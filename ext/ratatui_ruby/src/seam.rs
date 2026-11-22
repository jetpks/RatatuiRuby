// SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
// SPDX-License-Identifier: LGPL-3.0-or-later

//! The fd-based event seam.
//!
//! Exposes the input-wait fds a host reactor's selector must await:
//!
//! * the **wake fd** — the read end of a fork-owned pipe, written by
//!   `SyntheticEvents.push` (one byte per push) and by the SIGWINCH watcher
//!   (one byte per delivered SIGWINCH, via `signal-hook`'s self-pipe
//!   registration);
//! * the **tty fd** — a fd referring to the SAME tty device crossterm's
//!   event source reads: stdin dup'd when stdin is a tty — the same open
//!   file description crossterm polls, so the readiness equivalence is
//!   exact. The exposed fd is NEVER read; waiting on it observes exactly
//!   the readiness crossterm's own read consumes. When stdin is not a tty
//!   (headless/probe seats) the tty fd is not exposed.
//!
//! In the gem's test mode (a `TestBackend` terminal installed on this
//! thread) the tty fd is not exposed — there is no real tty behind it. The
//! wake fd is exposed at every seat: a pipe works everywhere.
//!
//! The contract with the host is: await readiness on the exposed fds with a
//! selector, then call `RatatuiRuby.poll_event(timeout: 0.0)` (the
//! non-blocking ready-read — the crossterm poll/read under
//! `rb_thread_call_without_gvl`, unchanged) only when the tty fd is ready,
//! and drain the wake fd's bytes when it is ready. No GVL-held blocking
//! wait is added anywhere on the input path.
//!
//! On seats where crossterm's event source cannot initialize at all (this
//! macOS kqueue refuses EVFILT_READ on a `/dev/tty` fd when stdin is not a
//! tty — measured, with a standalone-crate reproduction), the watcher's
//! second registration records the SIGWINCH as a pending-resize atomic so
//! `poll_event` can still surface the signal as a Resize instead of
//! raising; see the no-source arm in `events.rs::poll_crossterm_without_gvl`.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};

use magnus::{Error, IntoValue, Ruby, Value};

/// The seam's process-global state, initialized at most once.
struct Seam {
    /// The wake pipe's read end (exposed; drained by the host).
    wake_read_fd: i32,
    /// The wake pipe's write end (fork-internal; the push wake byte).
    wake_write_fd: i32,
    /// The SIGWINCH registrations' ids (kept so the registrations live as
    /// long as the pipe; dropping a `SigId` does not unregister, but
    /// keeping them documents ownership).
    _sigwinch: Vec<signal_hook::SigId>,
}

static SEAM: OnceLock<Seam> = OnceLock::new();

/// The cached tty fd (-1 = none exposed: test mode, or no tty this seat).
static TTY_FD: AtomicI32 = AtomicI32::new(-2); // -2 = not yet computed

/// The pending-SIGWINCH flag, heap-held so the flag registration's
/// `Arc<AtomicBool>` and `take_pending_resize` share ONE instance. The
/// signal-handler side stores `true` (signal-hook's flag::register — its
/// entire action is a single atomic store, async-signal-safe); the read
/// side (GVL-held, never signal context) takes it, only in the no-source
/// poll class.
static PENDING_RESIZE: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Consumes the pending-SIGWINCH flag (read side; never signal context).
/// False when the seam was never initialized (no registrations happened).
pub fn take_pending_resize() -> bool {
    PENDING_RESIZE
        .get()
        .is_some_and(|flag| flag.swap(false, Ordering::SeqCst))
}

/// Creates the wake pipe, arms the SIGWINCH watcher on its write end, and
/// stores the pair process-globally. Idempotent; called from both seam
/// entries (`input_wait_fds` and the `SyntheticEvents.push` wake write).
fn ensure_seam() -> Option<&'static Seam> {
    if let Some(seam) = SEAM.get() {
        return Some(seam);
    }

    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid writable array for pipe(2)'s two outputs;
    // the call has no other preconditions.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        // A failed pipe(2) means the process cannot create fds at all;
        // degrade to a seam without a wake fd rather than panic across FFI.
        return None;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // Non-blocking + close-on-exec both ends: the drain never blocks, the
    // wake write never blocks (a full pipe just drops the byte — the read
    // end is readable, which is all any consumer observes), and spawned
    // children inherit neither end.
    for fd in [read_fd, write_fd] {
        // SAFETY: `fd` is a pipe end this function just created; F_GETFL/
        // F_SETFL/F_SETFD on it cannot be interrupted by anything else.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags != -1 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            let _ = libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }

    let pending = Arc::new(AtomicBool::new(false));
    let mut sigwinch = Vec::new();

    // The SIGWINCH watcher, first registration: signal-hook's self-pipe
    // registration writes one byte into the wake pipe's write end per
    // delivered SIGWINCH, straight from the signal context
    // (async-signal-safe: a single write(2)). crossterm's internal
    // `Signals` set is a SEPARATE registration for the same signal —
    // signal-hook supports independent registrations, and crossterm's is
    // untouched.
    if let Ok(id) =
        signal_hook::low_level::pipe::register_raw(signal_hook::consts::SIGWINCH, write_fd)
    {
        sigwinch.push(id);
    }

    // The same watcher, second registration: record the pending resize as
    // ONE atomic store from signal context (async-signal-safe — signal-
    // hook's flag::register's entire action is `flag.store(true, SeqCst)`)
    // so the no-source poll path can surface the WINCH as a Resize.
    if let Ok(id) = signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&pending))
    {
        sigwinch.push(id);
    }
    let _ = PENDING_RESIZE.set(pending);

    SEAM
        .set(Seam {
            wake_read_fd: read_fd,
            wake_write_fd: write_fd,
            // A failed registration only costs its wake/record behavior; the
            // pipe itself still works (SIGWINCH exists on every unix target).
            _sigwinch: sigwinch,
        })
        .ok();
    SEAM.get()
}

/// Computes (once) the exposed tty fd, mirroring crossterm's own tty_fd()
/// choice so readiness is observed on the same device its source reads:
/// stdin when it is a tty (dup'd — the same file description), else an own
/// `/dev/tty` open. Test mode (a TestBackend terminal) exposes nothing.
fn ensure_tty_fd(_ruby: &Ruby) -> i32 {
    let cached = TTY_FD.load(Ordering::Relaxed);
    if cached != -2 {
        return cached;
    }

    let computed = compute_tty_fd();
    TTY_FD.store(computed, Ordering::Relaxed);
    computed
}

fn compute_tty_fd() -> i32 {
    // Test mode: no real tty exists behind the TestBackend — expose nothing.
    if crate::terminal::with_query(|q| q.is_test_mode()).unwrap_or(false) {
        return -1;
    }

    // The exposed tty fd mirrors crossterm's own stdin choice: when stdin is
    // a tty, crossterm POLLS STDIN, and the dup below is the SAME open file
    // description — exact readiness equivalence. When stdin is not a tty
    // (headless/probe seats), no tty fd is exposed: the host awaits the
    // wake fd only (the § headless degrade — no raise, wake path
    // functional). An own /dev/tty open is deliberately NOT exposed: this
    // macOS kqueue refuses EVFILT_READ on a /dev/tty fd when stdin is not a
    // tty (measured EINVAL), and a sometimes-non-readable exposed fd would
    // make the host's await non-deterministic.
    // SAFETY: isatty/dup on stdin are plain, interruption-free libc calls.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        // SAFETY: dup(2) of a valid fd; the duplicate lives for the
        // process's lifetime (deliberately never closed — the seam owns it).
        let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
        if fd != -1 {
            set_cloexec(fd);
            return fd;
        }
    }
    -1
}

fn set_cloexec(fd: i32) {
    // SAFETY: fcntl(2) on an fd this module just created.
    unsafe {
        let _ = libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// `RatatuiRuby.__input_seam_fds` — the seam's fd exposure primitive.
///
/// Returns a Ruby Hash `{ wake: Integer, tty: Integer | nil }`: the wake
/// pipe's read end (created here if this is the first seam touch) and the
/// tty fd (nil in test mode or without a tty). The Ruby seam layer shapes
/// the public contract on top.
pub fn input_seam_fds(ruby: &Ruby) -> Result<Value, Error> {
    let hash = ruby.hash_new();

    let wake = ensure_seam().map_or(-1, |seam| seam.wake_read_fd);
    hash.aset(ruby.to_symbol("wake"), wake)?;
    hash.aset(ruby.to_symbol("tty"), ensure_tty_fd(ruby))?;

    Ok(hash.into_value_with(ruby))
}

/// `RatatuiRuby.__input_seam_write_wake` — writes one wake byte into the
/// wake pipe (non-blocking; best-effort). This is what makes
/// `SyntheticEvents.push` self-waking. Creating the pipe here too keeps
/// "every push wakes" true even for a push that precedes the first
/// `input_wait_fds` call: the byte is in the pipe when the host starts
/// awaiting.
pub fn write_wake(ruby: &Ruby) -> Result<Value, Error> {
    if let Some(seam) = ensure_seam() {
        let byte = 1u8;
        // SAFETY: `seam.wake_write_fd` is a valid pipe end owned by the seam
        // static; a one-byte write(2) to it cannot be a problem.
        unsafe {
            libc::write(
                seam.wake_write_fd,
                (&byte as *const u8).cast::<c_void>(),
                1,
            );
        }
    }
    Ok(ruby.qnil().into_value_with(ruby))
}

#[cfg(test)]
mod seam_tests {
    /// The no-source seat contract (the degrade, not a raise): whatever
    /// this seat does, the poll either succeeds or fails with EXACTLY the
    /// cached-init class crossterm uses when its event source is missing —
    /// the narrow class the no-source arm in events.rs matches.
    #[test]
    fn poll_failure_is_exactly_the_no_source_class_or_success() {
        match ratatui::crossterm::event::poll(std::time::Duration::ZERO) {
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
