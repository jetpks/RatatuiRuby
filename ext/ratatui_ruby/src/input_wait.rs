// SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
// SPDX-License-Identifier: LGPL-3.0-or-later

//! The input waiters — the platform-neutral magnus boundary over the
//! per-platform halves: `input_wait/unix.rs` (all of the fd machinery,
//! wholly behind `#[cfg(unix)]`) and `input_wait/stub.rs` (the
//! unavailable-seat answers, wholly behind `#[cfg(not(unix))]`).
//!
//! Exposes the fds a host reactor's selector must await so that waiting for
//! input never happens inside this gem:
//!
//! * the **wake fd** — the read end of a process-owned pipe, written one
//!   byte per synthetic push and one byte per delivered resize signal;
//! * the **tty fd** — a fd referring to the SAME tty device the event
//!   source reads (a copy of the stdin file description), so its readiness
//!   is exactly what the source's own read consumes. The exposed fd is
//!   NEVER read by the gem or the host.
//!
//! The per-platform halves are reached only through this module, and the
//! boundary between them is pure data: `WaitFds`, plain bools, and byte
//! counts. On a seat with no fd-based waiting (no unix; a failed pipe
//! creation) the wake fd is `None` and the Ruby layer collapses the whole
//! answer to `{}` — the documented degrade, never a raise.
//!
//! The contract with the host is: await readiness on the exposed fds with
//! a selector, then call `RatatuiRuby.poll_event(timeout: 0.0)` (the
//! non-blocking ready-read under `rb_thread_call_without_gvl`, unchanged)
//! only when the tty fd is ready, and drain the wake fd's bytes when it is
//! ready. No GVL-held blocking wait is added anywhere on the input path.

use magnus::{Error, IntoValue, Ruby, Value};

#[cfg(not(unix))]
mod stub;
#[cfg(unix)]
mod unix;

#[cfg(not(unix))]
use self::stub as platform;
#[cfg(unix)]
use self::unix as platform;

/// The pure-data boundary between this module and the per-platform halves:
/// the fds a host reactor awaits, `None` = unavailable at this seat.
pub(crate) struct WaitFds {
    /// The wake pipe's read end (`None` = unavailable at this seat).
    pub wake: Option<i32>,
    /// The exposed tty fd (`None` = no real tty exposed at this call).
    pub tty: Option<i32>,
}

/// `RatatuiRuby._input_wait_fds` — the raw fd Hash
/// `{ wake: Integer | nil, tty: Integer | nil }` (nil = unavailable).
///
/// The FIRST call performs the event-source warm-up (exactly one
/// non-consuming poll — see the unix half) and the once-init of the wake
/// pipe. The Ruby layer owns the public contract on top.
pub fn input_wait_fds(ruby: &Ruby) -> Result<Value, Error> {
    // The warm-up rides the exposure path ONLY: a host that never asks for
    // the waiters never needs the source initialized, and the warm-up
    // regression test can deterministically witness the process's first
    // exposure.
    platform::warm();

    let (wake, tty) = platform::fds();
    let waiters = WaitFds { wake, tty };

    let hash = ruby.hash_new();
    hash.aset(ruby.to_symbol("wake"), waiters.wake)?;
    hash.aset(ruby.to_symbol("tty"), waiters.tty)?;
    Ok(hash.into_value_with(ruby))
}

/// `RatatuiRuby._write_input_wake` — writes one wake byte into the wake
/// pipe (non-blocking; best-effort). This is what makes
/// `SyntheticEvents.push` self-waking. Creating the pipe here too keeps
/// "every push wakes" true even for a push that precedes the first
/// `_input_wait_fds` call: the byte is in the pipe when the host starts
/// awaiting. Does NOT warm the event source — the warm-up belongs to the
/// exposure path only.
pub fn write_wake(ruby: &Ruby) -> Value {
    let _written = platform::write_wake();
    ruby.qnil().into_value_with(ruby)
}

/// `RatatuiRuby._test_terminal_installed?` — true exactly while a test
/// terminal is installed on the CALLING thread (the thread-local terminal
/// store answers on the caller's own thread). Platform-neutral.
pub fn test_terminal_installed(_ruby: &Ruby) -> bool {
    crate::terminal::with_query(|q| q.is_test_mode()).unwrap_or(false)
}

/// Consumes the pending-resize flag recorded by the unix watcher (read
/// side; never signal context). False when nothing is pending, and false
/// at every seat without the unix machinery. The no-source poll arm in
/// `events.rs` calls this to surface a recorded resize signal as a Resize
/// event instead of raising.
pub fn take_pending_resize() -> bool {
    platform::take_pending_resize()
}
