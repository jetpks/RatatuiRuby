<!--
  SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
  SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Input Waiters Design (`RatatuiRuby.input_waiters`)

This document describes the design of the input-waiter fds — the wake pipe and the
exposed tty fd — and the host protocol around them. It is for contributors and runtime
authors building fiber-based (Async, io-event) hosts on `ratatui_ruby`.

## Motivation: a host that awaits, never a gem that blocks

TUI apps need input. A host built on fibers wants to react to input the way it reacts
to everything else: by awaiting something its selector can watch. Two needs sit behind
this:

* **Keyboards and terminals are slow.** The host wants to do useful work while nobody
  is typing.
* **Resizes arrive out of band.** SIGWINCH is a signal, not a read on a device the
  host polls. A selector cannot await a signal.

Without waitable fds, a host has two bad choices: poll `poll_event` on a timer
(latency plus burned cycles), or run a dedicated poller thread (complexity plus a
second thread poking the terminal).

The input waiters solve both: `RatatuiRuby.input_waiters` returns the fds a selector
can await, and the gem never adds a GVL-held blocking wait anywhere on the input path.
The host awaits; the gem's `poll_event(timeout: 0.0)` does the ready-read.

## The two waiters

`RatatuiRuby.input_waiters` returns memoized `IO` objects with `autoclose: false`
(the fd is process-owned — see [Ownership](#ownership); `IO#fileno` is the escape
hatch for raw-integer selectors):

* **`:wake`** — the read end of a process-owned pipe, written one byte per
  `SyntheticEvents.push` (the synthetic rail is self-waking) and one byte per
  delivered SIGWINCH via an independent signal-hook self-pipe registration.
  Crossterm's internal Signals set is untouched.
* **`:tty`** — a dup of stdin when stdin is a tty: the SAME open file description
  crossterm's `tty_fd()` polls, so its readiness is exactly what crossterm's own read
  consumes. The fd is never read by the gem or the host — waiting on it observes
  readiness only. Absent when no real tty is exposed on the call: under an installed
  test terminal, and when stdin is not a tty.

Both waiters are memoized: repeated calls yield the same `IO` objects.

## The three seat classes

| Seat | `input_waiters` | Resize delivery |
| --- | --- | --- |
| Real tty (stdin is a tty) | `{ wake: IO, tty: IO }` | crossterm's own source (byte-identity path) |
| Headless, no source (stdin not a tty) | `{ wake: IO }` | the no-source arm: the watcher's pending flag surfaces SIGWINCH as a Resize |
| Windows (and unix seats where the pipe fails) | exactly `{}` | `poll_event(timeout:)` fallback; never raises |

The headless macOS no-source seat is a true no-source seat, not a bug: macOS kqueue
refuses `EVFILT_READ` on a `/dev/tty` fd when stdin is not a tty (EINVAL, reproduced
with a standalone probe at `tmp/kqueue_probe.rb`). There crossterm's event source
cannot initialize at all — crossterm 0.29 `event/read.rs:57` then fails every poll
with exactly `"Failed to initialize input reader"` — and the only Resize delivery
path is the no-source arm in `events.rs`, which consumes the watcher's pending-resize
flag. The string match is brittle but pinned: the Cargo.lock pins crossterm 0.29, and
the canary test in `input_wait/unix.rs` binds the class.

Because the wake pipe works on every unix seat, `:wake` is present on every
fd-capable seat, and `drain_input_wake` re-arms it after every drain.

## The host protocol

<!-- SPDX-SnippetBegin -->
<!--
  SPDX-FileCopyrightText: 2026 Kerrick Long
  SPDX-License-Identifier: MIT-0
-->
```ruby
waiters = RatatuiRuby.input_waiters # obtain BEFORE relying on synthetic wake
loop do
  ready = IO.select(waiters.values, nil, nil, 1.0)
  next unless ready

  if ready[0].include?(waiters[:tty])
    event = RatatuiRuby.poll_event(timeout: 0.0) # the non-blocking ready-read
    handle(event) unless event.none?
  end
  RatatuiRuby.drain_input_wake if ready[0].include?(waiters[:wake])
end
```
<!-- SPDX-SnippetEnd -->

The rules:

1. **Obtain the waiters before relying on synthetic wake.** The first exposure
   creates the wake pipe; a push that precedes the first exposure still wakes
   (the write creates the pipe), but the host cannot be awaiting what it has not
   asked for.
2. **On tty-ready, run the ready-read.** `poll_event(timeout: 0.0)` — unchanged
   public semantics, the crossterm poll/read under `rb_thread_call_without_gvl`.
3. **On wake-ready, drain.** `drain_input_wake` reads every buffered byte (the
   count), never blocks, and re-arms the pipe.
4. **On `{}` seats, fall back.** Windows and failed-pipe seats return exactly
   `{}`; the host awaits nothing and uses `poll_event(timeout:)`.

## The warm-up

Crossterm's global event source registers its signals pipe lazily, at its first
poll. A SIGWINCH delivered before that registration would be lost. The first
`input_waiters` exposure therefore warms the source: exactly one native
`crossterm::event::poll(Duration::ZERO)` WITHOUT `read()`.

Poll alone constructs the reader and registers the signals pipe — and consumes
nothing: crossterm stages pending events in an internal deque and only `read()`
hands them out. (An earlier design warmed with the public
`poll_event(timeout: 0.0)`, which poll+reads and discards — that swallowed one
queued event per process.) The warm-up rides its own `OnceLock`, runs at most once
per process, is triggered only by the fd-exposure path (not by the wake write), and
is best-effort: on a no-source seat the poll fails and the failure is swallowed.

## Ownership

The exposed fds are **process-owned**: the gem creates them and never closes them
for the process's lifetime. The `IO` objects wrap them with
`IO.for_fd(fd, autoclose: false)` so that neither GC nor an explicit `IO#close` can
close a gem-owned fd (which would hand the host delayed `EBADF`s and fd-reuse
misreads). Hosts await them and drain the wake end; nobody closes anything.

## Rust layout

* `ext/ratatui_ruby/src/input_wait.rs` — the platform-neutral magnus boundary: the
  pure-data `WaitFds` boundary type and the native entry points the Ruby layer sees
  (`_input_wait_fds`, `_write_input_wake`, `_test_terminal_installed?`) plus the
  internal pending-resize flag consume. Contains no unix-only code.
* `ext/ratatui_ruby/src/input_wait/unix.rs` — all unix machinery, wholly behind
  `#[cfg(unix)]`: the wake pipe via safe rustix calls (no `unsafe`, no `libc`), the
  two signal-hook registrations, the once-computed tty fd (test-mode gated per
  call), the warm-up, and the unit tests over the extracted pure helpers.
* `ext/ratatui_ruby/src/input_wait/stub.rs` — the unavailable-seat answers, wholly
  behind `#[cfg(not(unix))]`: pure data, compiles standalone for
  `x86_64-pc-windows-gnu` (the portability scratch check).

The Ruby layer (`lib/ratatui_ruby/input_waiters.rb`) owns the public contract on
top of the private natives `_input_wait_fds`, `_write_input_wake`, and
`_test_terminal_installed?`, and collapses any seat without a wake fd to exactly
`{}` — the degrade, never a raise.
