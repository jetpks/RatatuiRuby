# frozen_string_literal: true

#--
# SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
# SPDX-License-Identifier: LGPL-3.0-or-later
#++

module RatatuiRuby
  ##
  # :singleton-method: __input_seam_fds
  # Internal native primitive (the Rust fd seam): the raw fd Hash
  # <tt>{ wake: Integer, tty: Integer | nil }</tt>. The Seam module owns
  # the public contract; this stays underscore-named as the internal marker.

  ##
  # :singleton-method: __input_seam_write_wake
  # Internal native primitive (the Rust fd seam): one non-blocking wake
  # byte into the wake pipe. SyntheticEvents.push and the SIGWINCH watcher
  # are the only writers.

  # (Native methods implemented in Rust)

  ##
  # The fd-based event seam (the input-wait fds + wake pipe) — the public
  # contract layer over the underscore native primitives.
  #
  # Exposes, as selector-awaitable Integers, the fds a host reactor must
  # await so that waiting for input never happens inside this gem:
  #
  # * <tt>:tty</tt> — a fd referring to the SAME tty device crossterm's
  #   event source reads (the fork's own fd; the device input queue is
  #   shared, so its readiness is exactly what crossterm's read consumes).
  #   The fd is never read by this gem or the host. PRESENT only when a real
  #   tty exists; absent in the gem's headless/test mode.
  # * <tt>:wake</tt> — the read end of a fork-owned pipe. Written (one byte)
  #   by every {SyntheticEvents.push} and by the fork's SIGWINCH watcher
  #   (one byte per delivered SIGWINCH, via an independent signal-hook
  #   registration — crossterm's internal Signals set is untouched). Present
  #   at every seat.
  #
  # The host protocol: await readiness on the fds, then — for the tty fd —
  # call {poll_event} with <tt>timeout: 0.0</tt> (the unchanged non-blocking
  # ready-read); for the wake fd — read the bytes off it (or call
  # {drain_input_wake}, which never blocks). Reading the wake fd re-arms it.
  #
  # === Example
  #
  #--
  # SPDX-SnippetBegin
  # SPDX-FileCopyrightText: 2026 Kerrick Long
  # SPDX-License-Identifier: MIT-0
  #++
  #   fds = RatatuiRuby::Seam.input_wait_fds
  #   ready = select(fds.values, nil, nil, 1.0)
  #   if ready && ready[0].include?(wake_io)
  #     RatatuiRuby::Seam.drain_input_wake
  #   elsif ready # tty ready
  #     event = RatatuiRuby.poll_event(timeout: 0.0)
  #   end
  #--
  # SPDX-SnippetEnd
  #++
  module Seam

    ##
    # The input-wait fds (see the module docstring). Also reachable as
    # <tt>RatatuiRuby.input_wait_fds</tt> (the module_function extends the
    # enclosing module's singleton chain — the example's one-call form).
    #
    # The FIRST exposure eagerly initializes crossterm's global event source
    # (a warm-up <tt>poll_event(timeout: 0.0)</tt>): the source registers
    # its signals pipe lazily at its first poll, and a SIGWINCH delivered
    # before that registration would be lost. The warm-up is best-effort:
    # a seat without a tty cannot create the source (poll_event raises) and
    # the seam still exposes its wake fd — never a raise.
    #
    # @return [Hash] <tt>{ wake: Integer }</tt>, plus
    #   <tt>{ tty: Integer }</tt> when a real tty is exposed.
    def input_wait_fds
      raw = RatatuiRuby.__input_seam_fds
      warm_event_source_once
      fds = { wake: raw[:wake] }
      fds[:tty] = raw[:tty] unless raw[:tty].nil? || raw[:tty] <= 0
      fds
    end

    ##
    # Read-drains every byte buffered on the wake fd. Never blocks (the
    # pipe is non-blocking; EOF/error tails are structural no-ops).
    #
    # @return [Integer] the number of bytes drained.
    def drain_input_wake
      io = wake_io or return 0
      count = 0
      loop do
        chunk = io.read_nonblock(4096, exception: false)
        break if chunk.nil? || chunk == :wait_readable

        count += chunk.bytesize
      end
      count
    rescue EOFError, Errno::EIO, IOError
      count
    end

    # The eager warm-up (module docstring): one non-blocking poll so the
    # event source exists (and its signals pipe is registered) before any
    # WINCH can arrive. Runs once per process; the rescue is the headless
    # seat (crossterm cannot create a source without a tty) — by design the
    # exposure never raises.
    def warm_event_source_once
      return if @event_source_warmed

      @event_source_warmed = true
      RatatuiRuby.poll_event(timeout: 0.0)
    rescue StandardError
      # Headless: the source cannot exist yet. If a terminal appears later,
      # its first poll creates the source (the lazy init is crossterm's).
      nil
    end

    # The cached IO wrapper over the wake fd (autoclose: false — the fd is
    # fork-owned; GC must never close it).
    def wake_io
      @wake_io ||= begin
        fd = RatatuiRuby.__input_seam_fds[:wake]
        fd && fd >= 0 ? IO.for_fd(fd, autoclose: false) : nil
      end
    end
  end

  # The module-level entry (the documented contract name):
  # +RatatuiRuby.input_wait_fds+ / +RatatuiRuby.drain_input_wake+.
  extend Seam
end
