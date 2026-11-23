// SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
// SPDX-License-Identifier: LGPL-3.0-or-later

//! The unavailable seat (Windows): pure data, no external crates, no fds.
//!
//! Wholly behind `#[cfg(not(unix))]` (declared by the parent module). This
//! file must compile standalone for `x86_64-pc-windows-gnu` — the
//! portability scratch check `#[path]`-includes it into a dependency-less
//! crate. Everything answers "unavailable": the Ruby layer collapses the
//! waiters to exactly `{}` and hosts fall back to `poll_event(timeout:)`.

/// The unavailable-seat answer: no wake pipe, no exposed tty fd.
pub(super) fn fds() -> (Option<i32>, Option<i32>) {
    (None, None)
}

/// Nothing to write on an unavailable seat: zero bytes, never a raise.
pub(super) fn write_wake() -> usize {
    0
}

/// Nothing to warm on an unavailable seat: no event source exists here.
pub(super) fn warm() {}

/// Nothing is ever pending on an unavailable seat.
pub(super) fn take_pending_resize() -> bool {
    false
}
