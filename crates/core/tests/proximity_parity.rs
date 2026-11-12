//! Behaviour parity with `macos/Sources/SignalLock/ProximityMonitor.swift` and
//! `windows/Services/ProximityMonitor.cs`, with injected time. Every test
//! builds a `Sim` whose clock is a plain offset from one fixed `Instant`, so
//! nothing depends on wall-clock time.
//!
//! The one deliberate deviation is the rounding of the smoothed RSSI (ADR-014):
//! nearest, halves away from zero. See `rounding_*` below.

use std::time::{Duration, Instant};

use signallock_core::{AppSettings, Decision, ProximityMonitor, ProximityState};

struct Sim {
    monitor: ProximityMonitor,
    t0: Instant,
}

impl Sim {
    fn new(settings: AppSettings) -> Self {
        Self {
            monitor: ProximityMonitor::new(settings),
            t0: Instant::now(),
        }
    }

    /// Defaults (-80 dBm, 20 s, window 5) with the window overridden.
    fn with_window(window: u32) -> Self {
        Self::new(AppSettings {
            rssi_smoothing_window: window,
            ..AppSettings::default()
        })
    }

    fn at(&self, millis: u64) -> Instant {
        self.t0 + Duration::from_millis(millis)
    }

    fn sample(&mut self, millis: u64, rssi: i32) {
        let now = self.at(millis);
        self.monitor.on_sample(rssi, now);
    }

    fn tick(&mut self, millis: u64) -> Option<Decision> {
        let now = self.at(millis);
        self.monitor.tick(now)
    }

    /// Feeds `rssi` every 500 ms and ticks every 1000 ms over
    /// `[from_secs, to_secs]`. A sample at a tick instant is applied before
    /// the tick. Returns the seconds at which a lock was decided.
    fn run(&mut self, from_secs: u64, to_secs: u64, rssi: i32) -> Vec<u64> {
        let mut locks = Vec::new();
        for ms in (from_secs * 1000..=to_secs * 1000).step_by(500) {
            self.sample(ms, rssi);
            if ms % 1000 == 0 && self.tick(ms) == Some(Decision::Lock) {
                locks.push(ms / 1000);
            }
        }
        locks
    }
}

/// Window 1 makes the smoothed value equal to the latest sample, which keeps
/// the timing assertions exact.
fn confirmed_sim(window: u32) -> Sim {
    let mut sim = Sim::with_window(window);
    sim.sample(0, -60);
    assert_eq!(sim.tick(0), None);
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    sim
}

#[test]
fn steady_near_signal_never_locks() {
    let mut sim = Sim::with_window(5);
    let locks = sim.run(0, 300, -60);
    assert!(locks.is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    assert!(!sim.monitor.is_away());
}

#[test]
fn weak_signal_locks_once_after_exactly_away_delay() {
    let mut sim = confirmed_sim(1);
    // Weak sample at t=10 s starts the away condition at that tick.
    sim.sample(10_000, -90);
    assert_eq!(sim.tick(10_000), None);
    assert_eq!(
        sim.monitor.state(),
        ProximityState::Away {
            since: sim.at(10_000)
        }
    );
    // One millisecond short of the 20 s delay: no lock.
    sim.sample(29_999, -90);
    assert_eq!(sim.tick(29_999), None);
    // Exactly at the delay: the lock.
    sim.sample(30_000, -90);
    assert_eq!(sim.tick(30_000), Some(Decision::Lock));
    assert_eq!(sim.monitor.state(), ProximityState::Locked);
    assert!(sim.monitor.is_away());
    // And never again while the episode lasts.
    let more = sim.run(31, 120, -90);
    assert!(more.is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Locked);
}

#[test]
fn returning_before_the_delay_does_not_lock() {
    let mut sim = confirmed_sim(1);
    assert!(sim.run(1, 9, -60).is_empty());
    // 12 s of weak signal, then back before the 20 s delay.
    assert!(sim.run(10, 22, -90).is_empty());
    assert!(matches!(sim.monitor.state(), ProximityState::Away { .. }));
    assert!(sim.run(23, 60, -60).is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    assert!(!sim.monitor.is_away());
}

#[test]
fn a_new_away_episode_after_returning_locks_again_once() {
    let mut sim = confirmed_sim(1);
    assert_eq!(sim.run(1, 40, -90), vec![21]);
    assert_eq!(sim.monitor.state(), ProximityState::Locked);
    // Back near without an unlock: the episode ends and the trigger is re-armed.
    assert!(sim.run(41, 50, -60).is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    // The second walk-away starts at the first weak tick (t=51) and locks once.
    assert_eq!(sim.run(51, 120, -90), vec![71]);
}

#[test]
fn silence_anchor_locks_at_delay_after_last_sample_not_twice() {
    let mut sim = confirmed_sim(5);
    sim.run(1, 10, -60);
    // The device powers off: last sample at t=10 s, then nothing.
    let mut locks = Vec::new();
    for s in 11..=80u64 {
        if sim.tick(s * 1000) == Some(Decision::Lock) {
            locks.push(s);
        }
        if s == 30 {
            // silence == delay is not "more than" the delay yet.
            assert_eq!(sim.monitor.state(), ProximityState::Near);
        }
    }
    // First tick with silence > 20 s is t=31; the anchor is last_seen (t=10),
    // so the elapsed 21 s already exceeds the delay. Without the anchor this
    // would be 51 s (the delay paid twice).
    assert_eq!(locks, vec![31]);
    assert_eq!(sim.monitor.state(), ProximityState::Locked);
}

#[test]
fn silence_based_away_is_anchored_to_last_seen() {
    let mut sim = confirmed_sim(1);
    // Last sample at t=5 s; the first tick that sees the silence is t=26 s.
    sim.sample(5_000, -60);
    assert_eq!(sim.tick(5_000), None);
    assert_eq!(sim.tick(25_000), None);
    assert_eq!(sim.tick(26_000), Some(Decision::Lock));
    assert_eq!(sim.monitor.state(), ProximityState::Locked);
}

#[test]
fn signal_based_away_is_anchored_to_the_tick_time() {
    let mut sim = confirmed_sim(1);
    // The weak sample arrives at t=10.0 s but the first tick that sees it is
    // t=11 s; the anchor is that tick (now), not the sample time.
    sim.sample(10_000, -95);
    assert_eq!(sim.tick(11_000), None);
    assert_eq!(
        sim.monitor.state(),
        ProximityState::Away {
            since: sim.at(11_000)
        }
    );
    sim.sample(30_999, -95);
    assert_eq!(sim.tick(30_999), None);
    sim.sample(31_000, -95);
    assert_eq!(sim.tick(31_000), Some(Decision::Lock));
}

#[test]
fn a_stalled_tick_after_long_silence_locks_immediately() {
    let mut sim = confirmed_sim(1);
    sim.sample(10_000, -95);
    // No tick for 30 s. Silence (30 s) exceeds the 20 s delay, the anchor is
    // last_seen (t=10 s), so the elapsed 30 s already reaches the delay.
    assert_eq!(sim.tick(40_000), Some(Decision::Lock));
}

#[test]
fn a_stale_weak_sample_is_aged_out_before_the_signal_check() {
    // A long delay keeps the silence path out of play, so only buffer aging
    // can explain what happens at t=20 s.
    let mut sim = Sim::new(AppSettings {
        away_delay_seconds: 120,
        rssi_smoothing_window: 1,
        ..AppSettings::default()
    });
    sim.sample(0, -60);
    assert_eq!(sim.tick(0), None);
    sim.sample(10_000, -95);
    // 10 s old weak sample: aged out, so there is no signal-based away. Without
    // aging the smoothed -95 would start an away episode here.
    assert_eq!(sim.tick(20_000), None);
    assert_eq!(sim.monitor.smoothed_rssi(), None);
    assert_eq!(sim.monitor.state(), ProximityState::Near);
}

#[test]
fn stale_samples_are_dropped_after_three_seconds() {
    let mut sim = Sim::with_window(5);
    for i in 0..4u64 {
        sim.sample(i * 500, -50);
    }
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-50));
    // Newest sample at t=1.5 s. Exactly 3.000 s later the buffer survives
    // (the aging test is strictly "more than 3 s")...
    sim.tick(4_500);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-50));
    // ...and one millisecond later it is gone.
    sim.tick(4_501);
    assert_eq!(sim.monitor.smoothed_rssi(), None);
}

#[test]
fn aged_buffer_does_not_mask_a_fresh_weak_sample() {
    let mut sim = Sim::with_window(5);
    for i in 0..4u64 {
        sim.sample(i * 500, -50);
    }
    sim.tick(1_500);
    // Silence, then the buffer is aged away at t=5 s.
    sim.tick(5_000);
    assert_eq!(sim.monitor.smoothed_rssi(), None);
    // A weak sample is averaged only with itself, not with the stale -50s
    // (which would have given (4 * -50 + -90) / 5 = -58).
    sim.sample(5_100, -90);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-90));
}

#[test]
fn presence_gate_blocks_a_device_that_was_never_seen_near() {
    let mut sim = Sim::with_window(5);
    // No samples at all: nothing to evaluate.
    for s in 0..=60u64 {
        assert_eq!(sim.tick(s * 1000), None);
    }
    assert_eq!(sim.monitor.state(), ProximityState::Unknown);
    assert_eq!(sim.monitor.last_seen(), None);
    // One weak sample, then silence far beyond the 20 s delay. Without the
    // gate the silence path (anchored at last_seen) would lock here.
    sim.sample(61_000, -95);
    for s in 62..=300u64 {
        assert_eq!(sim.tick(s * 1000), None, "locked at {s}s without the gate");
    }
    assert_eq!(sim.monitor.state(), ProximityState::Unknown);
}

#[test]
fn presence_gate_blocks_a_device_that_was_only_ever_weak() {
    let mut sim = Sim::with_window(5);
    // Cold start with the device far away: weak samples alone never lock.
    assert!(sim.run(0, 120, -95).is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Unknown);
    // Silence must not lock either while the gate is closed.
    for s in 121..=300u64 {
        assert_eq!(sim.tick(s * 1000), None);
    }
    // Once the device is seen near, the gate opens for good...
    sim.sample(301_000, -50);
    sim.sample(301_100, -50);
    sim.sample(301_200, -50);
    sim.sample(301_300, -50);
    sim.sample(301_400, -50);
    assert_eq!(sim.tick(302_000), None);
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    // ...and a walk-away is then detected as usual.
    let mut locks = Vec::new();
    for s in 303..=360u64 {
        if sim.tick(s * 1000) == Some(Decision::Lock) {
            locks.push(s);
        }
    }
    assert_eq!(locks.len(), 1);
}

#[test]
fn arm_after_lock_resets_everything_and_closes_the_gate() {
    let mut sim = confirmed_sim(1);
    assert_eq!(sim.run(1, 40, -90), vec![21]);
    sim.monitor.arm();
    assert_eq!(sim.monitor.state(), ProximityState::Unknown);
    assert_eq!(sim.monitor.smoothed_rssi(), None);
    assert_eq!(sim.monitor.last_seen(), None);
    assert!(!sim.monitor.is_away());
}

#[test]
fn no_relock_loop_when_unlocked_with_the_device_still_away() {
    let mut sim = confirmed_sim(1);
    assert_eq!(sim.run(1, 40, -90), vec![21]);
    // The user unlocks at t=41 s; the device is still out of range, with the
    // odd stray weak advertisement at the edge of range.
    sim.monitor.arm();
    sim.sample(45_000, -92);
    sim.sample(60_000, -93);
    for s in 41..=400u64 {
        assert_eq!(sim.tick(s * 1000), None, "re-locked at {s}s");
    }
    assert_eq!(sim.monitor.state(), ProximityState::Unknown);
}

#[test]
fn rearmed_monitor_locks_again_after_the_device_is_seen_near() {
    let mut sim = confirmed_sim(1);
    assert_eq!(sim.run(1, 40, -90), vec![21]);
    sim.monitor.arm();
    // Back at the desk, then away again: a full new cycle.
    assert!(sim.run(41, 50, -60).is_empty());
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    assert_eq!(sim.run(51, 120, -90), vec![71]);
}

#[test]
fn smoothing_window_of_one_tracks_the_latest_sample() {
    let mut sim = Sim::with_window(1);
    sim.sample(0, -60);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-60));
    sim.sample(100, -73);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-73));
}

#[test]
fn smoothing_window_at_its_maximum_keeps_only_twenty_samples() {
    let mut sim = Sim::with_window(20);
    for i in 0..20u64 {
        sim.sample(i * 100, -60);
    }
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-60));
    // The 21st sample pushes the first -60 out: average of 19 * -60 and -80.
    // (19 * -60 - 80) / 20 = -61.0
    sim.sample(2_000, -80);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-61));
    // Twenty more -80s replace the whole window.
    for i in 0..20u64 {
        sim.sample(2_100 + i * 10, -80);
    }
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-80));
}

#[test]
fn rounding_half_below_zero_goes_away_from_zero() {
    // (-80 + -81) / 2 = -80.5 -> -81. Swift would say -80 (truncation) and C#
    // -80 (banker's rounding).
    let mut sim = Sim::with_window(2);
    sim.sample(0, -80);
    sim.sample(100, -81);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-81));
}

#[test]
fn rounding_below_a_half_stays_put() {
    // (4 * -80 + -82) / 5 = -80.4 -> -80.
    let mut sim = Sim::with_window(5);
    for (i, rssi) in [-80, -80, -80, -80, -82].into_iter().enumerate() {
        sim.sample(i as u64 * 100, rssi);
    }
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-80));
}

#[test]
fn rounding_half_above_goes_away_from_zero_too() {
    // (-79 + -80) / 2 = -79.5 -> -80. Swift truncates to -79; C# rounds the
    // half to the even -80; away from zero also gives -80.
    let mut sim = Sim::with_window(2);
    sim.sample(0, -79);
    sim.sample(100, -80);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-80));
}

#[test]
fn rounding_changes_the_away_decision_at_the_threshold_edge() {
    // Threshold -80. Average -80.5 rounds to -81, which is below the threshold,
    // so the signal path starts an away episode. (Swift's -80 would not.)
    let mut sim = Sim::new(AppSettings {
        rssi_smoothing_window: 2,
        ..AppSettings::default()
    });
    sim.sample(0, -70);
    sim.sample(100, -70);
    assert_eq!(sim.tick(1_000), None);
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    sim.sample(1_100, -80);
    sim.sample(1_200, -81);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-81));
    assert_eq!(sim.tick(1_300), None);
    assert!(matches!(sim.monitor.state(), ProximityState::Away { .. }));
}

#[test]
fn smoothed_equal_to_the_threshold_counts_as_near() {
    let mut sim = Sim::with_window(1);
    sim.sample(0, -80);
    assert_eq!(sim.monitor.state(), ProximityState::Near);
    assert_eq!(sim.run(1, 120, -80), Vec::<u64>::new());
    assert_eq!(sim.monitor.state(), ProximityState::Near);
}

#[test]
fn update_settings_trims_the_buffer_and_applies_new_threshold() {
    let mut sim = Sim::with_window(5);
    for (i, rssi) in [-50, -50, -50, -90, -90].into_iter().enumerate() {
        sim.sample(i as u64 * 100, rssi);
    }
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-66));
    // Window shrinks to 2: only the last two samples (-90, -90) remain.
    sim.monitor.update_settings(AppSettings {
        rssi_smoothing_window: 2,
        ..AppSettings::default()
    });
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-90));
    assert_eq!(sim.monitor.settings().rssi_smoothing_window, 2);
}

#[test]
fn away_delay_setting_is_respected() {
    let mut sim = Sim::new(AppSettings {
        away_delay_seconds: 5,
        rssi_smoothing_window: 1,
        ..AppSettings::default()
    });
    sim.sample(0, -60);
    assert_eq!(sim.tick(0), None);
    sim.sample(1_000, -90);
    assert_eq!(sim.tick(1_000), None);
    sim.sample(5_999, -90);
    assert_eq!(sim.tick(5_999), None);
    sim.sample(6_000, -90);
    assert_eq!(sim.tick(6_000), Some(Decision::Lock));
}

#[test]
fn zero_window_setting_behaves_as_one() {
    // The references clamp the window with max(1, ...) when ingesting.
    let mut sim = Sim::with_window(0);
    sim.sample(0, -60);
    sim.sample(100, -70);
    assert_eq!(sim.monitor.smoothed_rssi(), Some(-70));
}
