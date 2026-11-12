//! One-time import of the Swift app's `UserDefaults` values into the JSON
//! store (ADR-003 superseded by ADR-014).
//!
//! The Swift app stored two JSON documents as `Data` under the versioned keys
//! `SignalLock.Settings.v1` and `SignalLock.TrustedDevice.v1` in its bundle's
//! defaults domain, `com.signallock.app`. The Rust app ships under the same
//! bundle ID, so `NSUserDefaults.standardUserDefaults` is that domain. The
//! legacy keys are read, never deleted.
//!
//! Idempotent: once `settings.json` exists the import does nothing, so a
//! trusted device the user removes later is not resurrected from the old key.
//! `settings.json` is written last, so an import interrupted between the two
//! files is retried on the next launch.

use objc2_foundation::{NSString, NSUserDefaults};
use signallock_core::{AppSettings, Store, TrustedDevice};

use super::super::{MigrationOutcome, PlatformError, Result};
use crate::logging;

pub const SETTINGS_KEY: &str = "SignalLock.Settings.v1";
pub const TRUSTED_DEVICE_KEY: &str = "SignalLock.TrustedDevice.v1";

pub fn migrate_legacy(store: &Store) -> Result<MigrationOutcome> {
    migrate_with(store, read_defaults_data)
}

/// `migrate_legacy` with the `UserDefaults` read injected, for tests.
fn migrate_with(store: &Store, read: impl Fn(&str) -> Option<Vec<u8>>) -> Result<MigrationOutcome> {
    // `settings.json` is the completion marker: `import` writes it last, so a
    // run that died half-way leaves no marker and is retried.
    if store.settings_path().exists() {
        return Ok(MigrationOutcome::AlreadyMigrated);
    }
    import(
        store,
        read(SETTINGS_KEY).as_deref(),
        read(TRUSTED_DEVICE_KEY).as_deref(),
    )
}

fn read_defaults_data(key: &str) -> Option<Vec<u8>> {
    let defaults = NSUserDefaults::standardUserDefaults();
    let data = defaults.dataForKey(&NSString::from_str(key))?;
    Some(data.to_vec())
}

/// Decodes the legacy JSON documents and writes them to `store`. Split from
/// the `UserDefaults` read so that it is testable with recorded samples.
fn import(
    store: &Store,
    settings_json: Option<&[u8]>,
    device_json: Option<&[u8]>,
) -> Result<MigrationOutcome> {
    let settings = settings_json.and_then(|bytes| decode::<AppSettings>(bytes, "settings"));
    let device = device_json
        .and_then(|bytes| decode::<TrustedDevice>(bytes, "trusted device"))
        .filter(|device| !device.identifier.trim().is_empty());

    if settings.is_none() && device.is_none() {
        return Ok(MigrationOutcome::NothingToImport);
    }

    // The device goes first and `settings.json` last: it doubles as the
    // "migration complete" marker (see `migrate_with`), and it is written even
    // when only a device existed.
    if let Some(device) = &device {
        store.save_trusted_device(Some(device)).map_err(|e| {
            PlatformError::new(format!("writing migrated trusted device failed: {e}"))
        })?;
    }
    store
        .save_settings(&settings.unwrap_or_default())
        .map_err(|e| PlatformError::new(format!("writing migrated settings failed: {e}")))?;
    tracing::info!(
        target: logging::APP,
        "Imported legacy UserDefaults (settings: {}, trusted device: {})",
        settings.is_some(),
        device.is_some()
    );
    Ok(MigrationOutcome::Imported {
        settings: settings.is_some(),
        trusted_device: device.is_some(),
    })
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Option<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                target: logging::APP,
                "Legacy {what} in UserDefaults is not valid JSON for the new model, skipping it: {e}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;

    /// `JSONEncoder().encode(AppSettings)` output of
    /// `macos/Sources/SignalLock/SettingsStore.swift` (camelCase keys).
    const SWIFT_SETTINGS: &str = r#"{"rssiThreshold":-72,"awayDelaySeconds":45,"monitoringEnabled":true,"startAtLogin":true,"rssiSmoothingWindow":8}"#;

    /// `JSONEncoder().encode(TrustedDevice)` output of `TrustedDeviceStore.swift`.
    const SWIFT_DEVICE: &str =
        r#"{"identifier":"7F1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9","name":"Muhammet's iPhone"}"#;

    /// A fake `UserDefaults` holding the Swift samples.
    fn legacy_reader(key: &str) -> Option<Vec<u8>> {
        match key {
            SETTINGS_KEY => Some(SWIFT_SETTINGS.as_bytes().to_vec()),
            TRUSTED_DEVICE_KEY => Some(SWIFT_DEVICE.as_bytes().to_vec()),
            _ => None,
        }
    }

    #[test]
    fn swift_json_samples_decode_losslessly() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());

        let outcome = import(
            &store,
            Some(SWIFT_SETTINGS.as_bytes()),
            Some(SWIFT_DEVICE.as_bytes()),
        )
        .unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: true,
                trusted_device: true
            }
        );
        assert_eq!(
            store.load_settings(),
            AppSettings {
                rssi_threshold: -72,
                away_delay_seconds: 45,
                rssi_smoothing_window: 8,
                monitoring_enabled: true,
                start_at_login: true,
            }
        );
        assert_eq!(
            store.load_trusted_device(),
            Some(TrustedDevice {
                identifier: "7F1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9".into(),
                name: "Muhammet's iPhone".into(),
            })
        );
    }

    #[test]
    fn second_run_is_a_no_op() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());
        import(
            &store,
            Some(SWIFT_SETTINGS.as_bytes()),
            Some(SWIFT_DEVICE.as_bytes()),
        )
        .unwrap();
        let before = std::fs::read(store.settings_path()).unwrap();

        let again = migrate_with(&store, legacy_reader).unwrap();

        assert_eq!(again, MigrationOutcome::AlreadyMigrated);
        assert_eq!(std::fs::read(store.settings_path()).unwrap(), before);
    }

    #[test]
    fn removed_trusted_device_is_not_resurrected() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());
        import(
            &store,
            Some(SWIFT_SETTINGS.as_bytes()),
            Some(SWIFT_DEVICE.as_bytes()),
        )
        .unwrap();
        store.save_trusted_device(None).unwrap();

        assert_eq!(
            migrate_with(&store, legacy_reader).unwrap(),
            MigrationOutcome::AlreadyMigrated
        );
        assert_eq!(store.load_trusted_device(), None);
    }

    #[test]
    fn import_interrupted_after_the_device_is_retried() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());
        // State left by a run that wrote the device but died before settings.
        store
            .save_trusted_device(Some(&TrustedDevice {
                identifier: "STALE".into(),
                name: "stale".into(),
            }))
            .unwrap();

        let outcome = migrate_with(&store, legacy_reader).unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: true,
                trusted_device: true
            }
        );
        assert_eq!(store.load_settings().rssi_threshold, -72);
        assert_eq!(
            store.load_trusted_device().unwrap().identifier,
            "7F1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"
        );
    }

    #[test]
    fn settings_file_is_written_after_the_device_file() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());
        // Make the settings write fail: a directory sits where the file goes.
        std::fs::create_dir(store.settings_path()).unwrap();

        let result = import(
            &store,
            Some(SWIFT_SETTINGS.as_bytes()),
            Some(SWIFT_DEVICE.as_bytes()),
        );

        assert!(result.is_err());
        // The device was already written, and with no completed settings file
        // there is nothing that marks the migration as done.
        assert!(store.trusted_device_path().is_file());
    }

    #[test]
    fn device_without_settings_writes_default_settings_too() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());

        let outcome = import(&store, None, Some(SWIFT_DEVICE.as_bytes())).unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: false,
                trusted_device: true
            }
        );
        assert!(store.settings_path().exists());
        assert_eq!(store.load_settings(), AppSettings::default());
    }

    #[test]
    fn nothing_to_import_writes_nothing() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());

        assert_eq!(
            import(&store, None, None).unwrap(),
            MigrationOutcome::NothingToImport
        );
        assert!(!store.settings_path().exists());
        assert!(!store.trusted_device_path().exists());
    }

    #[test]
    fn corrupt_legacy_values_are_skipped_not_fatal() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());

        let outcome = import(&store, Some(b"not json"), Some(SWIFT_DEVICE.as_bytes())).unwrap();

        assert_eq!(
            outcome,
            MigrationOutcome::Imported {
                settings: false,
                trusted_device: true
            }
        );
    }

    #[test]
    fn legacy_device_with_empty_identifier_is_ignored() {
        let dir = TestDir::new("macos-migrate");
        let store = Store::new(dir.path().to_path_buf());

        let outcome = import(&store, None, Some(br#"{"identifier":" ","name":"x"}"#)).unwrap();

        assert_eq!(outcome, MigrationOutcome::NothingToImport);
    }
}
