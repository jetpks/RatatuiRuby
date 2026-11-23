# frozen_string_literal: true

#--
# SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
# SPDX-License-Identifier: LGPL-3.0-or-later
#++

module RatatuiRuby
  ##
  # :singleton-method: _input_wait_fds
  # Internal native primitive (the Rust input waiters): the raw fd Hash
  # <tt>{ wake: Integer | nil, tty: Integer | nil }</tt> (nil = unavailable).
  # The first call performs the event-source warm-up and the once-init of
  # the wake pipe. The Ruby layer owns the public contract on top.

  ##
  # :singleton-method: _write_input_wake
  # Internal native primitive (the Rust input waiters): one non-blocking
  # wake byte into the wake pipe, creating the pipe if this is the first
  # touch. Best-effort: never raises, and never warms the event source.

  ##
  # :singleton-method: _test_terminal_installed?
  # Internal native primitive (the Rust input waiters): true exactly while
  # a test terminal is installed on the CALLING thread (a thread-local
  # fact).

  # (Native methods implemented in Rust)

  ##
  # The input waiters: the fds a host reactor awaits so that waiting for
  # input never happens inside this gem.
  #
  # Fiber-based hosts (Async, io-event) want to react to input and to
  # terminal resizes without burning CPU on a poller thread and without
  # this gem ever holding the GVL in a blocking wait. That means the host
  # needs something its selector can await — but the fds behind the event
  # source are the gem's business, not the host's.
  #
  # Without the waiters, a host has two bad choices: poll {poll_event} on a
  # timer (latency plus burned cycles), or run a dedicated poller thread
  # (complexity plus a second thread poking the terminal).
  #
  # This method hands the host waitable fds, as <tt>IO</tt> objects:
  #
  # * <tt>:wake</tt> — the read end of a process-owned pipe. Written (one
  #  byte) by every {SyntheticEvents.push} and by the SIGWINCH watcher (one
  #  byte per delivered SIGWINCH, via an independent signal-hook
  #  registration — crossterm's internal Signals set is untouched).
  # * <tt>:tty</tt> — an <tt>IO</tt> referring to the SAME tty device
  #  crossterm's event source reads (a dup of stdin when stdin is a tty —
  #  the same open file description, so the readiness equivalence is
  #  exact). The fd is never read by this gem or the host. PRESENT only
  #  when a real tty is exposed on this call: absent under an installed
  #  test terminal, and absent when stdin is not a tty (headless seats).
  #
  # The host protocol: await readiness on the waiters, then — for the tty —
  # call {poll_event} with <tt>timeout: 0.0</tt> (the unchanged
  # non-blocking ready-read); for the wake — read the bytes off it (or call
  # {drain_input_wake}, which never blocks). Reading the wake fd re-arms it.
  #
  # The FIRST exposure also warms crossterm's global event source (exactly
  # one non-blocking poll that consumes nothing), so a SIGWINCH delivered
  # before the source's lazy first poll is never lost — and no queued event
  # is swallowed either.
  #
  # On seats where fd-based waiting is unavailable (Windows; a failed pipe
  # creation) this returns exactly <tt>{}</tt> and never raises — hosts
  # fall back to {poll_event} with a timeout there.
  #
  # @return [Hash] <tt>{ wake: IO }</tt>, plus <tt>{ tty: IO }</tt> when a
  #  real tty is exposed; exactly <tt>{}</tt> where fd-based waiting is
  #  unavailable.
  #
  # === Example
  #
  #--
  # SPDX-SnippetBegin
  # SPDX-FileCopyrightText: 2026 Kerrick Long
  # SPDX-License-Identifier: MIT-0
  #++
  #   waiters = RatatuiRuby.input_waiters # obtain BEFORE relying on synthetic wake
  #
  #   loop do
  #     ready = IO.select(waiters.values, nil, nil, 1.0)
  #     next unless ready
  #
  #     if ready[0].include?(waiters[:tty])
  #       event = RatatuiRuby.poll_event(timeout: 0.0) # the non-blocking ready-read
  #       handle(event) unless event.none?
  #     end
  #     RatatuiRuby.drain_input_wake if ready[0].include?(waiters[:wake])
  #   end
  #
  #--
  # SPDX-SnippetEnd
  #++
  def self.input_waiters
    raw = _input_wait_fds
    wake = raw[:wake]
    return {} if wake.nil? # unavailable seat (no fds here): exactly {} — the degrade

    waiters = { wake: waiter_io(wake) }
    tty = raw[:tty]
    waiters[:tty] = waiter_io(tty) unless tty.nil?
    waiters
  end

  ##
  # Read-drains every byte buffered on the wake waiter.
  #
  # The pipe is non-blocking, so the drain never waits: EOF and error tails
  # are structural no-ops, and the pipe is re-armed for the next wake.
  #
  # Returns 0 on seats where fd-based waiting is unavailable (exactly
  # <tt>{}</tt>) — nothing to drain, never a raise.
  #
  # @return [Integer] the number of bytes drained.
  #
  # === Example
  #
  #--
  # SPDX-SnippetBegin
  # SPDX-FileCopyrightText: 2026 Kerrick Long
  # SPDX-License-Identifier: MIT-0
  #++
  #   if wake.wait_readable(0) # awaited via the waiters hash
  #     puts "woken by #{RatatuiRuby.drain_input_wake} byte(s)"
  #   end
  #
  #--
  # SPDX-SnippetEnd
  #++
  def self.drain_input_wake
    count = 0
    wake = input_waiters[:wake]
    return 0 if wake.nil? # unavailable seat: exactly {} — nothing to drain

    loop do
      chunk = wake.read_nonblock(4096, exception: false)
      break unless chunk.is_a?(String)

      count += chunk.bytesize
    end
    count
  rescue Errno::EIO, IOError
    count || 0
  end

  # The memoized IO wrapper over an exposed fd (autoclose: false — the fd
  # is process-owned; neither GC nor an explicit IO#close may close it).
  def self.waiter_io(fd)
    @input_waiter_ios ||= {} #: Hash[Integer, IO]
    @input_waiter_ios[fd] ||= IO.for_fd(fd, autoclose: false)
  end
  private_class_method :waiter_io

  # (Native methods implemented in Rust)
  private_class_method :_input_wait_fds, :_write_input_wake, :_test_terminal_installed?
end
