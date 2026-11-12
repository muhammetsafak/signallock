//! One-time conversion of the C# app's files to the new format.
//!
//! The legacy app wrote `%APPDATA%\SignalLock\settings.json` and
//! `trusted-device.json` with `System.Text.Json` defaults, i.e. PascalCase
//! keys (`RssiThreshold`, `Identifier`, ...). The new store lives in the same
//! directory and writes camelCase; the core serde types also read PascalCase
//! through aliases. This step reads each legacy file once and rewrites it as
//! camelCase, so a later app version can drop the aliases.
//!
//! Idempotent: a file whose keys are already camelCase is left alone. A file
//! that cannot be parsed is left untouched too (the store then falls back to
//! defaults at load time, as it did before).

use std::fs;
use std::path::Path;

use serde_json::Value;
use signallock_core::{AppSettings, Store, TrustedDevice};

use super::super::{MigrationOutcome, PlatformError, Result};
use crate::logging;

pub fn migrate_legacy(store: &Store) -> Result<MigrationOutcome> {
    let settings = rewrite::<AppSettings>(&store.settings_path(), "settings", |value| {
        store.save_settings(value)
    })?;
    let device =
        rewrite::<TrustedDevice>(&store.trusted_device_path(), "trusted device", |value| {
            store.save_trusted_device(Some(value))
        })?;

    if settings || device {
        tracing::info!(
            target: logging::APP,
            "Converted legacy PascalCase files (settings: {settings}, trusted device: {device})"
        );
        Ok(MigrationOutcome::Imported {
            settings,
            trusted_device: device,
        })
    } else {
        Ok(MigrationOutcome::AlreadyMigrated)
    }
}

/// Rewrites one file if it is in the legacy format. Returns whether it did.
fn rewrite<T: serde::de::DeserializeOwned>(
    path: &Path,
    what: &str,
    save: impl FnOnce(&T) -> std::io::Result<()>,
) -> Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            tracing::warn!(
                target: logging::APP,
                "Cannot read legacy {what} file {}: {e}",
                path.display()
            );
            return Ok(false);
        }
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);

    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
        tracing::warn!(
            target: logging::APP,
            "Legacy {what} file {} is not a JSON object; leaving it as is",
            path.display()
        );
        return Ok(false);
    };
    if !map.keys().any(|key| is_pascal_case(key)) {
        return Ok(false);
    }

    let value: T = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                target: logging::APP,
                "Legacy {what} file {} does not fit the new model; leaving it as is: {e}",
                path.display()
            );
            return Ok(false);
        }
    };
    save(&value).map_err(|e| PlatformError::new(format!("rewriting {what} failed: {e}")))?;
    Ok(true)
}

fn is_pascal_case(key: &str) -> bool {
    key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;

    /// `JsonSerializer.Serialize(settings, new() { WriteIndented = true })`
    /// output of `windows/Services/SettingsStore.cs`.
    const WINDOWS_SETTINGS: &str = "{\r\n  \"RssiThreshold\": -65,\r\n  \"AwayDelaySeconds\": 30,\r\n  \"RssiSmoothingWindow\": 7,\r\n  \"MonitoringEnabled\": true,\r\n  \"StartAtLogin\": true\r\n}";

    const WINDOWS_DEVICE: &str =
        "{\r\n  \"Identifier\": \"A1B2C3D4E5F6\",\r\n  \"Name\": \"Galaxy Watch\"\r\n}";

    fn expected_settings() -> AppSettings {
        AppSettings {
            rssi_threshold: -65,
            away_delay_seconds: 30,
            rssi_smoothing_window: 7,
            monitoring_enabled: true,
            start_at_login: true,
        }
    }

    fn expected_device() -> TrustedDevice {
        TrustedDevice {
            identifier: "A1B2C3D4E5F6".into(),
            name: "Galaxy Watch".into(),
        }
    }

    fn setup(dir: &TestDir) -> Store {
        fs::write(dir.path().join("settings.json"), WINDOWS_SETTINGS).unwrap();
        fs::write(dir.path().join("trusted-device.json"), WINDOWS_DEVICE).unwrap();
        Store::new(dir.path().to_path_buf())
    }

    #[test]
    fn pascal_case_files_are_converted_without_loss() {
        let dir = TestDir::new("windows-migrate");
        let store = setup(&dir);

        let outcome = migrate_legacy(&store).unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: true,
                trusted_device: true
            }
        );
        assert_eq!(store.load_settings(), expected_settings());
        assert_eq!(store.load_trusted_device(), Some(expected_device()));

        // The files on disk are camelCase now.
        let settings: Value =
            serde_json::from_slice(&fs::read(store.settings_path()).unwrap()).unwrap();
        assert_eq!(settings["rssiThreshold"], -65);
        assert!(settings.get("RssiThreshold").is_none());
        let device: Value =
            serde_json::from_slice(&fs::read(store.trusted_device_path()).unwrap()).unwrap();
        assert_eq!(device["identifier"], "A1B2C3D4E5F6");
        assert!(device.get("Identifier").is_none());
    }

    #[test]
    fn second_run_is_a_no_op() {
        let dir = TestDir::new("windows-migrate");
        let store = setup(&dir);
        migrate_legacy(&store).unwrap();
        let settings_after_first = fs::read(store.settings_path()).unwrap();
        let device_after_first = fs::read(store.trusted_device_path()).unwrap();

        let outcome = migrate_legacy(&store).unwrap();

        assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
        assert_eq!(
            fs::read(store.settings_path()).unwrap(),
            settings_after_first
        );
        assert_eq!(
            fs::read(store.trusted_device_path()).unwrap(),
            device_after_first
        );
    }

    #[test]
    fn missing_files_are_not_created() {
        let dir = TestDir::new("windows-migrate");
        let store = Store::new(dir.path().to_path_buf());

        assert_eq!(
            migrate_legacy(&store).unwrap(),
            MigrationOutcome::AlreadyMigrated
        );
        assert!(!store.settings_path().exists());
        assert!(!store.trusted_device_path().exists());
    }

    #[test]
    fn only_the_legacy_file_is_rewritten() {
        let dir = TestDir::new("windows-migrate");
        let store = Store::new(dir.path().to_path_buf());
        fs::write(dir.path().join("settings.json"), WINDOWS_SETTINGS).unwrap();
        store.save_trusted_device(Some(&expected_device())).unwrap();

        let outcome = migrate_legacy(&store).unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: true,
                trusted_device: false
            }
        );
    }

    #[test]
    fn utf8_bom_is_tolerated() {
        let dir = TestDir::new("windows-migrate");
        let store = Store::new(dir.path().to_path_buf());
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(WINDOWS_DEVICE.as_bytes());
        fs::write(dir.path().join("trusted-device.json"), bytes).unwrap();

        migrate_legacy(&store).unwrap();

        assert_eq!(store.load_trusted_device(), Some(expected_device()));
    }

    #[test]
    fn unparsable_file_is_left_untouched() {
        let dir = TestDir::new("windows-migrate");
        let store = Store::new(dir.path().to_path_buf());
        // Negative delay does not fit the `u32` field of the new model.
        let odd = "{\"RssiThreshold\": -65, \"AwayDelaySeconds\": -3}";
        fs::write(dir.path().join("settings.json"), odd).unwrap();

        let outcome = migrate_legacy(&store).unwrap();

        assert_eq!(outcome, MigrationOutcome::AlreadyMigrated);
        assert_eq!(fs::read_to_string(store.settings_path()).unwrap(), odd);
    }
}
