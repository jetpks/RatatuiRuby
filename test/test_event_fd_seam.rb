# frozen_string_literal: true

# SPDX-FileCopyrightText: 2026 Kerrick Long <me@kerricklong.com>
# SPDX-License-Identifier: LGPL-3.0-or-later

require "test_helper"

# The fd-based event seam (input-wait fds + wake pipe) — the CONTRACT tests.
#
# The SIGWINCH → wake fd → Resize round-trip (the contract's deterministic
# instrument) runs at EVERY seat: at a real-tty seat it is crossterm's own
# delivery (the byte-identity path); at a seat where crossterm's event
# source cannot initialize (this macOS kqueue refuses EVFILT_READ on
# /dev/tty when stdin is not a tty — measured), the seam's own watcher
# surfaces the signal (the § headless degrade: no raise, the wake path
# functional). The tests are event-gated (bounded wait_readable, never a
# sleep bet) and self-settling (each leaves the wake pipe, the pending
# flag, and crossterm's queue empty).
class TestEventFdSeam < Minitest::Test
  # A test terminal left over from an earlier test short-circuits
  # poll_event (nil, no crossterm) — the seam contract needs the real path.
  def require_real_event_path
    probe = begin
      RatatuiRuby.poll_event(timeout: 0.0)
    rescue StandardError
      nil
    end
    return unless probe.nil?

    skip "a test terminal is active on this thread — the seam needs the real event path"
  end

  def fds
    RatatuiRuby.input_wait_fds
  end

  def wake_io
    @wake_io ||= IO.for_fd(fds.fetch(:wake), autoclose: false)
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

  # --- the contract -------------------------------------------------------

  def test_input_wait_fds_exposes_integer_fds_with_the_documented_shape
    require_real_event_path
    raw = fds
    assert_kind_of Hash, raw, "the documented shape: a Hash"
    wake = raw.fetch(:wake)
    assert_kind_of Integer, wake, "the wake fd is an Integer"
    assert_operator wake, :>, 2, "the wake fd is a real fd"
    tty = raw[:tty]
    if tty
      assert_kind_of Integer, tty, "the tty fd is an Integer"
      assert_operator tty, :>, 2, "the tty fd is a real fd (never stdin/stdout/stderr)"
    else
      # Headless seat: no tty exposed, no raise — the documented degrade.
      assert true
    end
  end

  def test_a_delivered_sigwinch_wakes_the_wake_fd
    require_real_event_path
    settle # earlier tests in this process may have left wake bytes/pending
    fds
    refute wake_io.wait_readable(0), "clean start: no pending wake byte"

    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "the delivered SIGWINCH must wake the wake fd"
  ensure
    settle
  end

  def test_a_delivered_sigwinch_surfaces_a_resize_from_the_ready_read
    require_real_event_path
    fds # first exposure also warms the event source (the lazy-init race)
    Process.kill("WINCH", Process.pid)
    assert wake_io.wait_readable(1.0), "the delivered SIGWINCH must wake the wake fd"

    event = RatatuiRuby.poll_event(timeout: 0.0)
    assert_kind_of RatatuiRuby::Event::Resize, event,
      "the WINCH must surface as Resize from the non-blocking ready-read " \
      "(crossterm's own delivery at a real seat; the seam's watcher at a no-source seat)"
  ensure
    settle
  end

  def test_the_wake_fd_re_arms_after_a_drain
    require_real_event_path
    fds
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
    fds
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
end
