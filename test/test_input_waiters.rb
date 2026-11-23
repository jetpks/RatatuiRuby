# frozen_string_literal: true

#--
# SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
# SPDX-License-Identifier: LGPL-3.0-or-later
#++

require "test_helper"

# The input waiters (wake pipe + tty fd exposure) — the CONTRACT tests.
#
# The SIGWINCH → wake fd → Resize round-trip (the contract's deterministic
# instrument) runs at EVERY seat: at a real-tty seat it is crossterm's own
# delivery (the byte-identity path); at a seat where crossterm's event
# source cannot initialize (this macOS kqueue refuses EVFILT_READ on
# /dev/tty when stdin is not a tty — measured), the watcher's pending flag
# surfaces the signal (the § headless degrade: no raise, the wake path
# functional). The tests are event-gated (bounded wait_readable, never a
# sleep bet) and self-settling (each leaves the wake pipe, the pending
# flag, and crossterm's queue empty).
class TestInputWaiters < Minitest::Test
  include RatatuiRuby::TestHelper

  # The warm-up regression test below must witness the process's FIRST
  # exposure to `input_waiters` — pin this class's order to alpha so
  # `test_0...` deterministically sorts first.
  i_suck_and_my_tests_are_order_dependent!

  # A test terminal installed on this thread short-circuits poll_event
  # (nil, no crossterm) — the waiters contract needs the real event path.
  def require_real_event_path
    waiters = RatatuiRuby.input_waiters
    skip "fd-based waiting is unavailable at this seat — nothing to await" if waiters.empty?

    return unless RatatuiRuby.__send__(:_test_terminal_installed?)

    skip "a test terminal is active on this thread — the waiters need the real event path"
  end

  def waiters
    RatatuiRuby.input_waiters
  end

  def wake_io
    @wake_io ||= waiters.fetch(:wake)
  end

  # Leaves the process settled: wake-pipe bytes drained, pending flag
  # consumed, crossterm's queue empty (nothing leaks into another test).
  def settle
    RatatuiRuby.drain_input_wake
    5.times do
      event = RatatuiRuby.poll_event(timeout: 0.0)
      break unless event.is_a?(RatatuiRuby::Event::Resize)
    end
  end

  # --- the warm-up (the finding-6 regression: nothing similar existed) ----

  def test_0_warm_up_leaves_queued_events_intact_across_the_first_exposure
    RatatuiRuby.inject_test_event("key", { code: "a" })
    RatatuiRuby.input_waiters # the process's first exposure — fires the warm-up
    event = RatatuiRuby.poll_event(timeout: 0.0)
    assert_instance_of RatatuiRuby::Event::Key, event,
      "the first exposure's warm-up must not swallow a queued event"
    assert_equal "a", event.code, "the exact queued event survives, not just an event"
  ensure
    RatatuiRuby.clear_events
  end

  # --- the contract -------------------------------------------------------

  def test_a_delivered_sigwinch_wakes_the_wake_fd
    require_real_event_path
    settle # earlier tests in this process may have left wake bytes/pending
    waiters
    refute wake_io.wait_readable(0), "clean start: no pending wake byte"

    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "the delivered SIGWINCH must wake the wake fd"
  ensure
    settle
  end

  def test_a_delivered_sigwinch_surfaces_a_resize_from_the_ready_read
    require_real_event_path
    waiters # first exposure also warms the event source (the lazy-init race)
    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "the delivered SIGWINCH must wake the wake fd"

    event = RatatuiRuby.poll_event(timeout: 0.0)
    assert_kind_of RatatuiRuby::Event::Resize, event,
      "the WINCH must surface as Resize from the non-blocking ready-read " \
        "(crossterm's own delivery at a real seat; the watcher's pending flag at a no-source seat)"
  ensure
    settle
  end

  def test_the_wake_fd_re_arms_after_a_drain
    require_real_event_path
    waiters
    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "the first WINCH wakes the fd"
    assert_operator RatatuiRuby.drain_input_wake, :>, 0, "the drain reads the byte(s)"

    refute wake_io.wait_readable(0), "the drain re-arms the pipe (nothing pending)"

    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "a WINCH after the drain wakes the fd again"
  ensure
    settle
  end

  def test_a_synthetic_push_wakes_the_wake_fd
    require_real_event_path
    settle # earlier tests in this process may have left wake bytes/pending
    waiters
    refute wake_io.wait_readable(0), "clean start: no pending wake byte"

    RatatuiRuby::SyntheticEvents.push(RatatuiRuby::Event::Sync.new)
    assert wake_io.wait_readable(1.0), "SyntheticEvents.push must wake the wake fd"
  ensure
    RatatuiRuby::SyntheticEvents.clear
    settle
  end

  def test_the_ready_read_is_nonblocking_with_nothing_pending
    require_real_event_path
    settle
    200.times do
      assert_instance_of RatatuiRuby::Event::None, RatatuiRuby.poll_event(timeout: 0.0),
        "a timeout-0 poll with nothing pending returns None (never raises, never blocks forever)"
    end
  ensure
    settle
  end

  # --- the IO contract ----------------------------------------------------

  def test_input_waiters_exposes_io_objects_with_the_documented_shape
    waiters = RatatuiRuby.input_waiters
    assert_instance_of Hash, waiters, "the documented shape: a Hash"
    assert(
      waiters.keys.all? { |key| %i[wake tty].include?(key) },
      "the keys are exactly :wake and/or :tty"
    )

    return if waiters.empty? # unavailable seat: exactly {} — the documented degrade

    assert waiters.key?(:wake), ":wake is present on every fd-capable seat"
    assert_instance_of IO, waiters[:wake], "the waiters are IO objects, not raw Integer fds"
    return unless waiters.key?(:tty)

    assert_instance_of IO, waiters[:tty], "the tty waiter is an IO object too"
  end

  def test_input_waiters_memoizes_the_same_io_objects_across_calls
    require_real_event_path
    first = RatatuiRuby.input_waiters
    second = RatatuiRuby.input_waiters
    assert_same first[:wake], second[:wake],
      "memoized: repeated calls yield the same IO object"
    return unless first.key?(:tty)

    assert_same first.fetch(:tty), second.fetch(:tty), "memoized: the tty IO too"
  end

  def test_input_waiters_ios_never_autoclose_the_process_owned_fds
    require_real_event_path
    waiters.each_value do |io|
      refute io.autoclose?, "the gem owns the fd; GC must never close it"
    end
  end

  # --- the guard honesty (replaces the dead poll_event probe) -------------

  def test_the_test_terminal_guard_reports_the_thread_local_truth
    refute RatatuiRuby.__send__(:_test_terminal_installed?),
      "no test terminal on this thread (the guard's honest baseline)"

    with_test_terminal do
      assert RatatuiRuby.__send__(:_test_terminal_installed?),
        "a TestBackend terminal IS installed on this thread"
    end

    refute RatatuiRuby.__send__(:_test_terminal_installed?), "restored: the terminal is gone again"
  end
end
