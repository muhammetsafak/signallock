//! Persisted models: [`AppSettings`] and [`TrustedDevice`].
//!
//! On-disk format. Fields are written in camelCase (`rssiThreshold`,
//! `identifier`, ...), which is what Swift's `JSONEncoder` produced for the
//! macOS `UserDefaults` values. The Windows 0.1.0 files are PascalCase
//! (`System.Text.Json` without a naming policy), so every field also carries
//! a PascalCase read alias. Reading accepts both; writing is always camelCase,
//! so a legacy Windows file is rewritten in camelCase on its first save.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

/// Allowed RSSI threshold in dBm (reference UI: `-100...-40`).
pub const RSSI_THRESHOLD_RANGE: RangeInclusive<i32> = -100..=-40;
/// Allowed away delay in seconds (reference UI: `5...120`).
pub const AWAY_DELAY_SECONDS_RANGE: RangeInclusive<u32> = 5..=120;
/// Allowed smoothing window in samples (reference UI: `1...20`).
pub const RSSI_SMOOTHING_WINDOW_RANGE: RangeInclusive<u32> = 1..=20;

/// User-tunable settings.
///
/// Missing fields fall back to their defaults, so a partially written or
/// older file still loads. Loading does not clamp; callers that accept user
/// input use [`AppSettings::clamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    #[serde(alias = "RssiThreshold")]
    pub rssi_threshold: i32,
    #[serde(alias = "AwayDelaySeconds")]
    pub away_delay_seconds: u32,
    #[serde(alias = "RssiSmoothingWindow")]
    pub rssi_smoothing_window: u32,
    #[serde(alias = "MonitoringEnabled")]
    pub monitoring_enabled: bool,
    #[serde(alias = "StartAtLogin")]
    pub start_at_login: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            rssi_threshold: -80,
            away_delay_seconds: 20,
            rssi_smoothing_window: 5,
            monitoring_enabled: false,
            start_at_login: false,
        }
    }
}

impl AppSettings {
    /// Returns a copy with every numeric field forced into its allowed range.
    #[must_use]
    pub fn clamp(self) -> Self {
        Self {
            rssi_threshold: self
                .rssi_threshold
                .clamp(*RSSI_THRESHOLD_RANGE.start(), *RSSI_THRESHOLD_RANGE.end()),
            away_delay_seconds: self.away_delay_seconds.clamp(
                *AWAY_DELAY_SECONDS_RANGE.start(),
                *AWAY_DELAY_SECONDS_RANGE.end(),
            ),
            rssi_smoothing_window: self.rssi_smoothing_window.clamp(
                *RSSI_SMOOTHING_WINDOW_RANGE.start(),
                *RSSI_SMOOTHING_WINDOW_RANGE.end(),
            ),
            ..self
        }
    }
}

/// The single trusted device (ADR-004).
///
/// `identifier` is platform-specific: a CoreBluetooth UUID on macOS, a
/// 12-character upper-case hex Bluetooth address on Windows and Linux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedDevice {
    #[serde(alias = "Identifier")]
    pub identifier: String,
    #[serde(alias = "Name")]
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_references() {
        let s = AppSettings::default();
        assert_eq!(s.rssi_threshold, -80);
        assert_eq!(s.away_delay_seconds, 20);
        assert_eq!(s.rssi_smoothing_window, 5);
        assert!(!s.monitoring_enabled);
        assert!(!s.start_at_login);
    }

    #[test]
    fn defaults_are_inside_ranges() {
        let s = AppSettings::default();
        assert_eq!(s, s.clamp());
    }

    #[test]
    fn clamp_forces_values_into_range() {
        let low = AppSettings {
            rssi_threshold: -300,
            away_delay_seconds: 0,
            rssi_smoothing_window: 0,
            ..AppSettings::default()
        }
        .clamp();
        assert_eq!(low.rssi_threshold, -100);
        assert_eq!(low.away_delay_seconds, 5);
        assert_eq!(low.rssi_smoothing_window, 1);

        let high = AppSettings {
            rssi_threshold: 10,
            away_delay_seconds: 9_999,
            rssi_smoothing_window: 500,
            monitoring_enabled: true,
            start_at_login: true,
        }
        .clamp();
        assert_eq!(high.rssi_threshold, -40);
        assert_eq!(high.away_delay_seconds, 120);
        assert_eq!(high.rssi_smoothing_window, 20);
        assert!(high.monitoring_enabled && high.start_at_login);
    }

    #[test]
    fn writes_camel_case() {
        let json = serde_json::to_value(AppSettings::default()).unwrap();
        let obj = json.as_object().unwrap();
        for key in [
            "rssiThreshold",
            "awayDelaySeconds",
            "rssiSmoothingWindow",
            "monitoringEnabled",
            "startAtLogin",
        ] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(obj.len(), 5);

        let device = TrustedDevice {
            identifier: "ID".into(),
            name: "N".into(),
        };
        let json = serde_json::to_value(&device).unwrap();
        assert_eq!(json["identifier"], "ID");
        assert_eq!(json["name"], "N");
    }

    /// Output of Swift's `JSONEncoder` for the macOS `AppSettings` struct.
    #[test]
    fn reads_camel_case_swift_sample() {
        let json = r#"{"rssiThreshold":-72,"awayDelaySeconds":30,"monitoringEnabled":true,"startAtLogin":true,"rssiSmoothingWindow":8}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s,
            AppSettings {
                rssi_threshold: -72,
                away_delay_seconds: 30,
                rssi_smoothing_window: 8,
                monitoring_enabled: true,
                start_at_login: true,
            }
        );
    }

    /// Output of `System.Text.Json` (`WriteIndented`, no naming policy) for the
    /// Windows 0.1.0 `AppSettings` class.
    #[test]
    fn reads_pascal_case_windows_sample() {
        let json = "{\r\n  \"RssiThreshold\": -65,\r\n  \"AwayDelaySeconds\": 45,\r\n  \"RssiSmoothingWindow\": 3,\r\n  \"MonitoringEnabled\": true,\r\n  \"StartAtLogin\": true\r\n}";
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s,
            AppSettings {
                rssi_threshold: -65,
                away_delay_seconds: 45,
                rssi_smoothing_window: 3,
                monitoring_enabled: true,
                start_at_login: true,
            }
        );
    }

    #[test]
    fn pascal_case_round_trips_to_camel_case() {
        let json = r#"{"RssiThreshold":-65,"AwayDelaySeconds":45,"RssiSmoothingWindow":3,"MonitoringEnabled":true,"StartAtLogin":false}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        let rewritten = serde_json::to_string(&s).unwrap();
        assert!(rewritten.contains("rssiThreshold"));
        assert!(!rewritten.contains("RssiThreshold"));
        assert_eq!(serde_json::from_str::<AppSettings>(&rewritten).unwrap(), s);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let s: AppSettings = serde_json::from_str(r#"{"rssiThreshold":-60}"#).unwrap();
        assert_eq!(s.rssi_threshold, -60);
        assert_eq!(s.away_delay_seconds, 20);
        assert_eq!(s.rssi_smoothing_window, 5);
    }

    #[test]
    fn trusted_device_reads_both_cases() {
        let camel: TrustedDevice =
            serde_json::from_str(r#"{"identifier":"AB-CD","name":"Watch"}"#).unwrap();
        let pascal: TrustedDevice =
            serde_json::from_str(r#"{"Identifier":"AB-CD","Name":"Watch"}"#).unwrap();
        assert_eq!(camel, pascal);
        assert_eq!(camel.identifier, "AB-CD");
        assert_eq!(camel.name, "Watch");
    }
}
