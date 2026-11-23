// SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
// SPDX-License-Identifier: LGPL-3.0-or-later

//! The unix machinery behind the input waiters.
//!
//! Everything here is wholly behind `#[cfg(unix)]` (declared by the parent
//! module) — none of it is compiled on Windows. All fd plumbing rides the
//! safe `rustix` API (no `libc` anywhere, and every call is a safe,
//! bounds-checked binding) and the resize-signal watcher rides
//! `signal-hook`'s self-pipe registration.
//!
//! The state, both initialized at most once via `OnceLock::get_or_init`
//! (never check-then-set — two racing first-callers would otherwise create
//! two pipes and leave permanent duplicate signal registrations):
//!
//! * the **seat** — the wake pipe plus the two watcher registrations;
//! * the **tty fd** — computed ONCE from the stdin-is-a-tty fact (a
//!   process-level truth), cached as just the fd; the test-mode gate is a
//!   thread-local predicate and is applied per call at the boundary above,
//!   never baked into the cache.
//!
//! The **warm-up** — exactly one non-consuming `poll(Duration::ZERO)` that
//! constructs the event source and registers its internal signals pipe —
//! rides its own `OnceLock` and is triggered only by the exposure path:
//! `poll(0)` alone constructs the reader and registers the signals pipe,
//! and consumes nothing (only `read()` hands events out), so a queued
//! event survives the first exposure.
//!
//! On seats where the event source cannot initialize at all (this macOS
//! kqueue refuses `EVFILT_READ` on a `/dev/tty` fd when stdin is not a tty
//! — measured, with a standalone-crate reproduction), the watcher's second
//! registration records the resize signal as a pending-resize atomic so
//! `poll_event` can still surface the signal as a Resize instead of
//! raising; see the no-source arm in `events.rs::poll_crossterm_without_gvl`.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[cfg(target_vendor = "apple")]
use rustix::fs::{fcntl_setfl, OFlags};
use rustix::io::{dup, fcntl_setfd, write, FdFlags};
#[cfg(target_vendor = "apple")]
use rustix::pipe::pipe;
#[cfg(not(target_vendor = "apple"))]
use rustix::pipe::{pipe_with, PipeFlags};
use rustix::stdio::stdin;

/// The one wake byte per push / per delivered resize signal.
const WAKE_BYTE: u8 = 1;

/// The seat's process-global state, initialized at most once. `None` = the
/// seat cannot host the waiters at all (the pipe could not be created) —
/// the degrade, never a panic across the magnus boundary.
struct Seat {
    /// The wake pipe's read end (exposed; drained by the host).
    wake_read: OwnedFd,
    /// The wake pipe's write end (process-internal; the wake byte).
    wake_write: OwnedFd,
    /// The resize-signal registrations' ids (kept so the registrations live
    /// as long as the pipe; dropping a `SigId` does not unregister, but
    /// keeping them documents ownership).
    _sigwinch: Vec<signal_hook::SigId>,
}

static SEAT: OnceLock<Option<Seat>> = OnceLock::new();

/// The cached tty fd, computed once from the stdin-is-a-tty fact. Held as
/// an `OwnedFd` that lives for the process's lifetime (deliberately never
/// closed — the gem owns it).
static TTY_FD: OnceLock<Option<OwnedFd>> = OnceLock::new();

/// The warm-up: exactly one non-consuming poll, at most once per process.
static WARM: OnceLock<()> = OnceLock::new();

/// The pending-resize flag, heap-held so the flag registration's
/// `Arc<AtomicBool>` and `take_pending_resize` share ONE instance. The
/// signal-handler side stores `true` (signal-hook's flag registration —
/// its entire action is a single atomic store, async-signal-safe); the
/// read side (GVL-held, never signal context) takes it, only in the
/// no-source poll class.
static PENDING_RESIZE: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The per-seat answer: `(wake fd, tty fd)`, `None` = unavailable.
///
/// The tty is gated PER CALL on the thread-local test-mode predicate (a
/// `TestBackend` terminal means no real tty exists behind the exposure) —
/// the cached fd itself is a process-level fact and is never poisoned by
/// whichever thread happens to call first.
pub(super) fn fds() -> (Option<i32>, Option<i32>) {
    let wake = seat().map(|s| s.wake_read.as_raw_fd());
    let tty = if crate::terminal::with_query(|q| q.is_test_mode()).unwrap_or(false) {
        None
    } else {
        tty_fd()
    };
    (wake, tty)
}

/// The event-source warm-up, triggered ONLY by the exposure path: exactly
/// one `poll(Duration::ZERO)` with NO `read()` — poll alone constructs the
/// reader and registers its signals pipe, and consumes nothing (only
/// `read()` hands staged events out). Best-effort: a no-source seat fails
/// here and the failure is swallowed — the warm-up never raises.
pub(super) fn warm() {
    WARM.get_or_init(|| {
        let _polled = ratatui::crossterm::event::poll(Duration::ZERO);
    });
}

/// Writes one wake byte into the wake pipe (non-blocking; best-effort).
/// Zero (not an error) when the seat has no wake pipe: the byte would have
/// no reader, and the degrade is the contract.
pub(super) fn write_wake() -> usize {
    seat().map_or(0, |seat| write_wake_byte(&seat.wake_write))
}

/// One wake byte into the pipe. Non-blocking and best-effort: a full pipe
/// just drops the byte — the read end stays readable, which is all any
/// consumer observes.
fn write_wake_byte(fd: &OwnedFd) -> usize {
    write(fd, &[WAKE_BYTE]).unwrap_or(0)
}

/// Consumes the pending-resize flag (read side; never signal context).
/// False when the seat was never initialized (no registrations happened).
pub(super) fn take_pending_resize() -> bool {
    PENDING_RESIZE
        .get()
        .is_some_and(|flag| flag.swap(false, Ordering::SeqCst))
}

/// The seat, initialized at most once. `None` is cached too: a process
/// that could not create one pipe is degraded, permanently and
/// deliberately, rather than panicking across the magnus boundary.
fn seat() -> Option<&'static Seat> {
    SEAT.get_or_init(create_seat).as_ref()
}

/// Creates the wake pipe, arms the resize-signal watcher on its write end,
/// and hands the pair back for the once-cache.
fn create_seat() -> Option<Seat> {
    let (wake_read, wake_write) = create_wake_pipe()?;

    let mut sigwinch = Vec::new();

    // The resize-signal watcher, first registration: signal-hook's
    // self-pipe registration writes one byte into the wake pipe's write
    // end per delivered signal, straight from the signal context
    // (async-signal-safe: a single `write(2)`). The event source's internal
    // signals set is a SEPARATE registration for the same signal —
    // signal-hook supports independent registrations, and crossterm's is
    // untouched. Registrations run in order, so the wake byte and the
    // source's own pipe byte are both written before the handler returns.
    if let Ok(id) = signal_hook::low_level::pipe::register_raw(
        signal_hook::consts::SIGWINCH,
        wake_write.as_raw_fd(),
    ) {
        sigwinch.push(id);
    }

    // The same watcher, second registration: record the pending resize as
    // ONE atomic store from signal context (async-signal-safe) so the
    // no-source poll path can surface the signal as a Resize.
    let pending = pending_resize_flag();
    if let Ok(id) = signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(pending))
    {
        sigwinch.push(id);
    }

    Some(Seat {
        wake_read,
        wake_write,
        // A failed registration only costs its wake/record behavior; the
        // pipe itself still works (the resize signal exists on every unix
        // target).
        _sigwinch: sigwinch,
    })
}

/// The pending-resize flag's shared instance (also used by the tests to
/// drive the store → take → take round-trip).
fn pending_resize_flag() -> &'static Arc<AtomicBool> {
    PENDING_RESIZE.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// Creates the wake pipe with both ends non-blocking and close-on-exec:
/// the drain never blocks, the wake write never blocks, and spawned
/// children inherit neither end.
#[cfg(not(target_vendor = "apple"))]
fn create_wake_pipe() -> Option<(OwnedFd, OwnedFd)> {
    // One safe call replaces pipe(2) plus both F_SETFL and both F_SETFD.
    let ends = pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC).ok()?;
    Some(ends)
}

/// The macOS twin of `create_wake_pipe`: rustix does not build its
/// `pipe_with` (the `pipe2` binding) for Apple targets, so `pipe()` plus
/// the two safe fcntl sets per end are the equivalent — every call still
/// a safe binding, and the flags are set BEFORE `register_raw` (a blocking
/// write from signal context would hang).
#[cfg(target_vendor = "apple")]
fn create_wake_pipe() -> Option<(OwnedFd, OwnedFd)> {
    let (wake_read, wake_write) = pipe().ok()?;
    for fd in [&wake_read, &wake_write] {
        let _set = fcntl_setfl(fd, OFlags::NONBLOCK);
        let _cloexec = fcntl_setfd(fd, FdFlags::CLOEXEC);
    }
    Some((wake_read, wake_write))
}

/// The cached tty fd: computed once, then just read.
fn tty_fd() -> Option<i32> {
    TTY_FD
        .get_or_init(|| compute_tty_fd(rustix::termios::isatty(stdin())))
        .as_ref()
        .map(rustix::fd::AsRawFd::as_raw_fd)
}

/// Computes the exposed tty fd, mirroring the event source's own stdin
/// choice so readiness is observed on the same device its read consumes:
/// a copy of stdin when stdin is a tty (the same open file description —
/// exact readiness equivalence), else nothing. Parameterized on the
/// is-a-tty fact so the unit tests cover both branches.
fn compute_tty_fd(is_tty: bool) -> Option<OwnedFd> {
    if !is_tty {
        // stdin is not a tty (headless/probe seat): no tty fd is exposed.
        // An own `/dev/tty` open is deliberately NOT exposed: this macOS
        // kqueue refuses `EVFILT_READ` on a `/dev/tty` fd when stdin is
        // not a tty (measured EINVAL), and a sometimes-non-watchable
        // exposed fd would make the host's await non-deterministic.
        return None;
    }

    // The copy shares the same open file description; the gem owns it for
    // the process's lifetime (deliberately never closed).
    let fd = dup(stdin()).ok()?;
    // Close-on-exec: spawned children inherit no fds.
    fcntl_setfd(&fd, FdFlags::CLOEXEC).ok()?;
    Some(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use rustix::fs::{fcntl_getfl, OFlags};
    use rustix::io::fcntl_getfd;

    #[test]
    fn wake_pipe_ends_are_nonblocking_and_close_on_exec() {
        let (wake_read, wake_write) = create_wake_pipe().expect("a pipe is creatable in tests");

        for end in [&wake_read, &wake_write] {
            let status = fcntl_getfl(end).expect("F_GETFL on a pipe end cannot fail here");
            assert!(
                status.contains(OFlags::NONBLOCK),
                "both pipe ends must be non-blocking (the drain never blocks, the wake write never blocks)"
            );
            let descriptor = fcntl_getfd(end).expect("F_GETFD on a pipe end cannot fail here");
            assert!(
                descriptor.contains(FdFlags::CLOEXEC),
                "both pipe ends must be close-on-exec (spawned children inherit neither end)"
            );
        }
    }

    #[test]
    fn a_wake_write_makes_the_read_end_readable() {
        let (wake_read, wake_write) = create_wake_pipe().expect("a pipe is creatable in tests");

        // A clean start: a zero-timeout poll with POLLIN finds nothing.
        let mut clean = [PollFd::new(&wake_read, PollFlags::IN)];
        let clean_hits = poll(
            &mut clean,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .expect("poll on a pipe end cannot fail here");
        assert_eq!(clean_hits, 0, "clean start: no pending wake byte");

        assert_eq!(write_wake_byte(&wake_write), 1, "one wake byte written");

        // The same zero-timeout poll now reports the read end readable.
        let mut armed = [PollFd::new(&wake_read, PollFlags::IN)];
        let armed_hits = poll(
            &mut armed,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .expect("poll on a pipe end cannot fail here");
        assert_eq!(
            armed_hits, 1,
            "the wake write must make the read end readable"
        );
    }

    #[test]
    fn the_tty_compute_covers_both_branches_of_the_is_a_tty_fact() {
        // Not a tty: nothing exposed — the headless/probe seat's degrade.
        assert!(
            compute_tty_fd(false).is_none(),
            "without a tty, no tty fd is exposed (an own /dev/tty open is deliberately not exposed)"
        );

        // A tty: a dup'd fd with close-on-exec set, sharing stdin's file
        // description. Works whatever stdin actually is (cargo test's
        // stdin is a valid fd — a dup of it is a valid dup).
        let exposed = compute_tty_fd(true).expect("a tty seat exposes a dup'd fd");
        assert_ne!(
            exposed.as_raw_fd(),
            0,
            "the exposed fd is a dup, never stdin itself"
        );
        let descriptor = fcntl_getfd(&exposed).expect("F_GETFD on a fresh dup cannot fail here");
        assert!(
            descriptor.contains(FdFlags::CLOEXEC),
            "the dup'd tty fd must be close-on-exec"
        );
    }

    #[test]
    fn the_pending_resize_flag_round_trips_store_take_take() {
        let flag = pending_resize_flag();

        flag.store(true, Ordering::SeqCst);
        assert!(
            take_pending_resize(),
            "a stored resize surfaces exactly once"
        );
        assert!(
            !take_pending_resize(),
            "the take consumes the flag (a second take is a no-op)"
        );
    }

    /// The no-source seat contract (the degrade, not a raise): whatever
    /// this seat does, the poll either succeeds or fails with EXACTLY the
    /// cached-init class the no-source arm in `events.rs` matches —
    /// crossterm 0.29 `event/read.rs:57`, pinned by the Cargo.lock.
    #[test]
    fn poll_failure_is_exactly_the_no_source_class_or_success() {
        match ratatui::crossterm::event::poll(Duration::ZERO) {
            Ok(_) => {} // a seat with a working event source (or pending input)
            Err(e) => {
                assert_eq!(
                    e.to_string(),
                    "Failed to initialize input reader",
                    "the seat's poll failure must be the cached-init class the no-source arm matches"
                );
            }
        }
    }
}
