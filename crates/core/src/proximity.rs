//! Proximity decision engine (ADR-005, ADR-010, ADR-014).
//!
//! `ProximityMonitor` is a direct port of `macos/Sources/SignalLock/ProximityMonitor.swift`
//! and `windows/Services/ProximityMonitor.cs`. It keeps a moving-average RSSI
//! buffer and a last-seen timestamp and emits exactly one [`Decision::Lock`]
//! per "away episode". Time is injected: callers pass `Instant`s into
//! [`ProximityMonitor::on_sample`] and [`ProximityMonitor::tick`]; this module
//! never reads the clock. The owner calls `tick` about once per second, like
//! the 1 s timer of the references.
//!
//! ## Rounding (deliberate deviation, user decision 2026-11-12, ADR-014)
//!
//! The smoothed RSSI is the window average rounded to the nearest integer,
//! halves away from zero (`f64::round`, e.g. -80.5 becomes -81). The references
//! differ from each other and from this: Swift truncates toward zero
//! (`sum / count`, so -80.5 becomes -80) and C# uses `Math.Round` (halves to
//! even, so -80.5 becomes -80 and -79.5 becomes -80). The difference is at most
//! 1 dBm. Every other branch below is ported unchanged.
//!
//! ## ADR-010 guards
//!
//! 1. Silence anchor: a silence-based away starts at `last_seen`, not at `now`,
//!    so the away delay is not paid twice when advertisements simply stop.
//! 2. Buffer aging: at the start of each tick the buffer is cleared when the
//!    last sample is more than 3 s old ([`BUFFER_STALE_AFTER`]).
//! 3. Presence gate: a one-way flag, reset by [`ProximityMonitor::arm`], set
//!    the first time the smoothed RSSI reaches the threshold. While it is
//!    unset, `tick` evaluates nothing, so an unlock with the device still out
//!    of range cannot re-lock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::settings::AppSettings;

const LOG_TARGET: &str = "proximity";

/// A buffer whose newest sample is older than this is stale and is cleared at
/// the start of a tick. Tied to typical BLE advertising cadence, not to any
/// user setting (ADR-010).
pub const BUFFER_STALE_AFTER: Duration = Duration::from_secs(3);

/// Where the monitor currently stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProximityState {
    /// The presence gate is closed: the trusted device has not been seen near
    /// since the last [`ProximityMonitor::arm`] (or since creation). Nothing is
    /// evaluated in this state.
    Unknown,
    /// The device was confirmed near and no away condition is running.
    Near,
    /// An away condition (weak signal or silence) started at `since` and the
    /// away delay has not elapsed yet. `since` is `last_seen` for a
    /// silence-based away and the tick time for a signal-based one.
    Away { since: Instant },
    /// The away delay elapsed and [`Decision::Lock`] was emitted for this
    /// episode. Stays until the device is near again or [`ProximityMonitor::arm`].
    Locked,
}

/// What the owner must do after a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Lock the machine now. Emitted at most once per away episode.
    Lock,
}

/// The proximity state machine. See the module documentation.
#[derive(Debug, Clone)]
pub struct ProximityMonitor {
    settings: AppSettings,
    buffer: VecDeque<i32>,
    last_seen: Option<Instant>,
    away_since: Option<Instant>,
    is_away: bool,
    has_triggered_lock: bool,
    has_confirmed_presence: bool,
}

impl ProximityMonitor {
    /// Creates an armed monitor (the equivalent of the references' `start()`).
    pub fn new(settings: AppSettings) -> Self {
        Self {
            settings,
            buffer: VecDeque::new(),
            last_seen: None,
            away_since: None,
            is_away: false,
            has_triggered_lock: false,
            has_confirmed_presence: false,
        }
    }

    /// The settings currently in effect.
    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Replaces the settings and trims the buffer if the window shrank.
    pub fn update_settings(&mut self, settings: AppSettings) {
        self.settings = settings;
        let window = self.window();
        while self.buffer.len() > window {
            self.buffer.pop_front();
        }
    }

    /// Resets all away-detection state and closes the presence gate. Used for
    /// `start()` and for `rearm()` after the user unlocks the machine: the next
    /// walk-away must be preceded by seeing the device near again.
    pub fn arm(&mut self) {
        self.buffer.clear();
        self.last_seen = None;
        self.away_since = None;
        self.is_away = false;
        self.has_triggered_lock = false;
        self.has_confirmed_presence = false;
    }

    /// Feeds one RSSI sample (dBm) observed at `now`.
    pub fn on_sample(&mut self, rssi: i32, now: Instant) {
        self.last_seen = Some(now);
        self.buffer.push_back(rssi);
        let window = self.window();
        while self.buffer.len() > window {
            self.buffer.pop_front();
        }
        if !self.has_confirmed_presence
            && let Some(smoothed) = self.smoothed_rssi()
            && smoothed >= self.settings.rssi_threshold
        {
            self.has_confirmed_presence = true;
            tracing::info!(
                target: LOG_TARGET,
                "Presence confirmed (smoothed={smoothed} dBm)"
            );
        }
    }

    /// Evaluates the away conditions at `now`. Call about once per second.
    /// Returns `Some(Decision::Lock)` at most once per away episode.
    pub fn tick(&mut self, now: Instant) -> Option<Decision> {
        // Buffer aging (ADR-010 guard 2): runs before the presence gate, as in
        // both references.
        if let Some(last) = self.last_seen
            && now.saturating_duration_since(last) > BUFFER_STALE_AFTER
        {
            self.buffer.clear();
        }

        // Presence gate (ADR-010 guard 3).
        if !self.has_confirmed_presence {
            return None;
        }

        let delay = Duration::from_secs(u64::from(self.settings.away_delay_seconds));
        let away_by_signal = self
            .smoothed_rssi()
            .is_some_and(|smoothed| smoothed < self.settings.rssi_threshold);
        let away_by_silence = self
            .last_seen
            .is_some_and(|last| now.saturating_duration_since(last) > delay);

        if away_by_signal || away_by_silence {
            let since = match self.away_since {
                Some(since) => since,
                None => {
                    // Silence anchor (ADR-010 guard 1).
                    let since = match self.last_seen {
                        Some(last) if away_by_silence => last,
                        _ => now,
                    };
                    self.away_since = Some(since);
                    tracing::info!(
                        target: LOG_TARGET,
                        "Away condition started (signal={away_by_signal}, silence={away_by_silence})"
                    );
                    since
                }
            };
            let elapsed = now.saturating_duration_since(since);
            if elapsed >= delay {
                if !self.is_away {
                    self.is_away = true;
                    tracing::info!(
                        target: LOG_TARGET,
                        "Confirmed AWAY after {}s; will trigger lock",
                        elapsed.as_secs()
                    );
                }
                if !self.has_triggered_lock {
                    self.has_triggered_lock = true;
                    return Some(Decision::Lock);
                }
            }
        } else if self.is_away || self.away_since.is_some() {
            tracing::info!(target: LOG_TARGET, "Returned to NEAR; resetting away state");
            self.is_away = false;
            self.away_since = None;
            self.has_triggered_lock = false;
        }
        None
    }

    /// Current state, derived from the internal flags.
    pub fn state(&self) -> ProximityState {
        if !self.has_confirmed_presence {
            ProximityState::Unknown
        } else if self.is_away {
            ProximityState::Locked
        } else if let Some(since) = self.away_since {
            ProximityState::Away { since }
        } else {
            ProximityState::Near
        }
    }

    /// `true` once the away delay elapsed for the current episode (the
    /// references' `isAway`).
    pub fn is_away(&self) -> bool {
        self.is_away
    }

    /// Time of the newest sample since the last [`arm`](Self::arm).
    pub fn last_seen(&self) -> Option<Instant> {
        self.last_seen
    }

    /// Window average of the buffered samples, rounded to the nearest integer,
    /// halves away from zero. `None` while the buffer is empty.
    pub fn smoothed_rssi(&self) -> Option<i32> {
        if self.buffer.is_empty() {
            return None;
        }
        let sum: i64 = self.buffer.iter().map(|&v| i64::from(v)).sum();
        let average = sum as f64 / self.buffer.len() as f64;
        Some(average.round() as i32)
    }

    fn window(&self) -> usize {
        self.settings.rssi_smoothing_window.max(1) as usize
    }
}
