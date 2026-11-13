//! JSON store for [`AppSettings`] and [`TrustedDevice`].
//!
//! Two files in a directory handed in by the caller: `settings.json` and
//! `trusted-device.json`. Writes are atomic (temp file in the same directory,
//! then rename). A corrupt file never panics: loading falls back to the
//! default value and logs a warning. Choosing the directory is the binary's
//! job, not this crate's.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::settings::{AppSettings, TrustedDevice};

/// File name of the settings file.
pub const SETTINGS_FILE: &str = "settings.json";
/// File name of the trusted-device file.
pub const TRUSTED_DEVICE_FILE: &str = "trusted-device.json";

const LOG_TARGET: &str = "app";

/// Distinguishes temp files of concurrent saves within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Handle on the config directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Creates a store rooted at `dir`. The directory is created lazily on the
    /// first save.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory this store reads from and writes to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Full path of `settings.json`.
    pub fn settings_path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Full path of `trusted-device.json`.
    pub fn trusted_device_path(&self) -> PathBuf {
        self.dir.join(TRUSTED_DEVICE_FILE)
    }

    /// Loads the settings. A missing, unreadable or corrupt file yields
    /// [`AppSettings::default`].
    pub fn load_settings(&self) -> AppSettings {
        load_json(&self.settings_path(), "settings").unwrap_or_default()
    }

    /// Persists the settings atomically. The failure is logged at `error` and
    /// returned; the caller keeps its in-memory value either way.
    pub fn save_settings(&self, settings: &AppSettings) -> io::Result<()> {
        self.save_json(&self.settings_path(), "settings", settings)
    }

    /// Loads the trusted device. `None` when there is none, or when the file
    /// is unreadable, corrupt or carries an empty identifier.
    pub fn load_trusted_device(&self) -> Option<TrustedDevice> {
        let device: TrustedDevice = load_json(&self.trusted_device_path(), "trusted device")?;
        if device.identifier.trim().is_empty() {
            tracing::warn!(
                target: LOG_TARGET,
                "Trusted device file has an empty identifier; ignoring it"
            );
            return None;
        }
        Some(device)
    }

    /// Persists the trusted device atomically; `None` removes the file.
    pub fn save_trusted_device(&self, device: Option<&TrustedDevice>) -> io::Result<()> {
        let path = self.trusted_device_path();
        match device {
            Some(device) => self.save_json(&path, "trusted device", device),
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => {
                    tracing::error!(
                        target: LOG_TARGET,
                        "Trusted device clear failed ({}): {e}",
                        path.display()
                    );
                    Err(e)
                }
            },
        }
    }

    fn save_json<T: Serialize>(&self, path: &Path, what: &str, value: &T) -> io::Result<()> {
        let result = serde_json::to_vec_pretty(value)
            .map_err(io::Error::other)
            .and_then(|bytes| write_atomic(&self.dir, path, &bytes));
        if let Err(e) = &result {
            tracing::error!(
                target: LOG_TARGET,
                "Saving {what} failed ({}): {e}",
                path.display()
            );
        }
        result
    }
}

/// Reads and parses `path`. Returns `None` (after logging) when the file is
/// missing, unreadable or does not parse.
fn load_json<T: DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Normal on a first launch and for a user without a trusted
            // device, so `info`; unreadable or corrupt files are `warn`.
            tracing::info!(
                target: LOG_TARGET,
                "No {what} file at {}; using defaults",
                path.display()
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                target: LOG_TARGET,
                "Reading {what} failed ({}): {e}; using defaults",
                path.display()
            );
            return None;
        }
    };
    // Tolerate a UTF-8 BOM that some Windows editors add.
    let bytes = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                target: LOG_TARGET,
                "Parsing {what} failed ({}): {e}; using defaults",
                path.display()
            );
            None
        }
    }
}

/// Writes `bytes` to a sibling temp file, flushes it to disk and renames it
/// over `path`. The temp file is removed if any step fails.
fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_os_string();
    // Unique per process and per call, so concurrent saves never share (and
    // truncate) one temp file.
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);

    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    thread_local! {
        static CAPTURED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Writes each test thread's log output into that thread's own buffer.
    struct ThreadBuf;
    impl Write for ThreadBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            CAPTURED.with(|b| b.borrow_mut().extend_from_slice(data));
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Runs `f` and returns the formatted log lines it produced on this thread.
    ///
    /// A scoped (`with_default`) subscriber is flaky under parallel tests:
    /// `tracing` caches each callsite's interest globally, and another thread
    /// hitting the same callsite with no subscriber can cache "never". A
    /// process-wide subscriber, installed once, keeps every callsite enabled;
    /// a thread-local buffer keeps each test's output separate.
    fn capture_logs(f: impl FnOnce()) -> String {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(|| ThreadBuf)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in this test binary");
        });
        CAPTURED.with(|b| b.borrow_mut().clear());
        f();
        let bytes = CAPTURED.with(|b| std::mem::take(&mut *b.borrow_mut()));
        String::from_utf8(bytes).unwrap()
    }

    fn store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("SignalLock"));
        (dir, store)
    }

    fn device() -> TrustedDevice {
        TrustedDevice {
            identifier: "0A1B2C3D-0000-1111-2222-333344445555".into(),
            name: "Pixel Watch".into(),
        }
    }

    #[test]
    fn settings_round_trip() {
        let (_guard, store) = store();
        let settings = AppSettings {
            rssi_threshold: -70,
            away_delay_seconds: 33,
            rssi_smoothing_window: 7,
            monitoring_enabled: true,
            start_at_login: true,
        };
        store.save_settings(&settings).unwrap();
        assert_eq!(store.load_settings(), settings);
    }

    #[test]
    fn trusted_device_round_trip_and_clear() {
        let (_guard, store) = store();
        assert_eq!(store.load_trusted_device(), None);
        store.save_trusted_device(Some(&device())).unwrap();
        assert_eq!(store.load_trusted_device(), Some(device()));
        store.save_trusted_device(None).unwrap();
        assert_eq!(store.load_trusted_device(), None);
        assert!(!store.trusted_device_path().exists());
        // Clearing twice is fine.
        store.save_trusted_device(None).unwrap();
    }

    #[test]
    fn missing_files_give_defaults() {
        let (_guard, store) = store();
        assert_eq!(store.load_settings(), AppSettings::default());
        assert_eq!(store.load_trusted_device(), None);
    }

    #[test]
    fn missing_files_log_at_info_not_warn() {
        let (_guard, store) = store();
        let logs = capture_logs(|| {
            store.load_settings();
            store.load_trusted_device();
        });
        assert!(logs.contains(" INFO "), "logs: {logs}");
        assert!(!logs.contains(" WARN "), "logs: {logs}");
    }

    #[test]
    fn corrupt_files_log_at_warn() {
        let (_guard, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.settings_path(), "{ not json").unwrap();
        fs::write(store.trusted_device_path(), "{ not json").unwrap();
        let logs = capture_logs(|| {
            store.load_settings();
            store.load_trusted_device();
        });
        assert_eq!(logs.matches(" WARN ").count(), 2, "logs: {logs}");
    }

    #[test]
    fn corrupt_files_give_defaults() {
        let (_guard, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.settings_path(), "{ not json").unwrap();
        fs::write(store.trusted_device_path(), [0xFF, 0xFE, 0x00]).unwrap();
        assert_eq!(store.load_settings(), AppSettings::default());
        assert_eq!(store.load_trusted_device(), None);

        fs::write(store.settings_path(), "").unwrap();
        fs::write(store.trusted_device_path(), r#"{"identifier":"x"}"#).unwrap();
        assert_eq!(store.load_settings(), AppSettings::default());
        assert_eq!(store.load_trusted_device(), None);
    }

    #[test]
    fn empty_identifier_is_no_device() {
        let (_guard, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.trusted_device_path(),
            r#"{"identifier":"  ","name":"x"}"#,
        )
        .unwrap();
        assert_eq!(store.load_trusted_device(), None);
    }

    #[test]
    fn reads_swift_camel_case_files() {
        let (_guard, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.settings_path(),
            r#"{"rssiThreshold":-72,"awayDelaySeconds":30,"monitoringEnabled":true,"startAtLogin":false,"rssiSmoothingWindow":8}"#,
        )
        .unwrap();
        fs::write(
            store.trusted_device_path(),
            r#"{"identifier":"11111111-2222-3333-4444-555555555555","name":"AirPods"}"#,
        )
        .unwrap();
        let settings = store.load_settings();
        assert_eq!(settings.rssi_threshold, -72);
        assert_eq!(settings.away_delay_seconds, 30);
        assert_eq!(settings.rssi_smoothing_window, 8);
        assert!(settings.monitoring_enabled);
        assert_eq!(
            store.load_trusted_device().unwrap().identifier,
            "11111111-2222-3333-4444-555555555555"
        );
    }

    #[test]
    fn reads_windows_pascal_case_files_and_rewrites_camel_case() {
        let (_guard, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        // UTF-8 BOM included on purpose; System.Text.Json output is indented.
        let mut settings_file = vec![0xEF, 0xBB, 0xBF];
        settings_file.extend_from_slice(
            b"{\r\n  \"RssiThreshold\": -65,\r\n  \"AwayDelaySeconds\": 45,\r\n  \"RssiSmoothingWindow\": 3,\r\n  \"MonitoringEnabled\": true,\r\n  \"StartAtLogin\": true\r\n}",
        );
        fs::write(store.settings_path(), settings_file).unwrap();
        fs::write(
            store.trusted_device_path(),
            "{\r\n  \"Identifier\": \"AABBCCDDEEFF\",\r\n  \"Name\": \"Mi Band\"\r\n}",
        )
        .unwrap();

        let settings = store.load_settings();
        assert_eq!(settings.rssi_threshold, -65);
        assert_eq!(settings.away_delay_seconds, 45);
        assert_eq!(settings.rssi_smoothing_window, 3);
        assert!(settings.monitoring_enabled && settings.start_at_login);
        let device = store.load_trusted_device().unwrap();
        assert_eq!(device.identifier, "AABBCCDDEEFF");
        assert_eq!(device.name, "Mi Band");

        // First save rewrites both files in camelCase, values intact.
        store.save_settings(&settings).unwrap();
        store.save_trusted_device(Some(&device)).unwrap();
        let settings_text = fs::read_to_string(store.settings_path()).unwrap();
        let device_text = fs::read_to_string(store.trusted_device_path()).unwrap();
        assert!(settings_text.contains("\"rssiThreshold\""));
        assert!(!settings_text.contains("RssiThreshold"));
        assert!(device_text.contains("\"identifier\""));
        assert!(!device_text.contains("Identifier"));
        assert_eq!(store.load_settings(), settings);
        assert_eq!(store.load_trusted_device(), Some(device));
    }

    #[test]
    fn save_replaces_existing_file_and_leaves_no_temp_file() {
        let (_guard, store) = store();
        store.save_settings(&AppSettings::default()).unwrap();
        let changed = AppSettings {
            rssi_threshold: -55,
            ..AppSettings::default()
        };
        store.save_settings(&changed).unwrap();
        assert_eq!(store.load_settings(), changed);
        let names: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![SETTINGS_FILE.to_string()]);
    }

    #[test]
    fn concurrent_saves_all_succeed_and_leave_a_valid_file() {
        let (_guard, store) = store();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        let settings = AppSettings {
                            rssi_threshold: -100 + i,
                            away_delay_seconds: 5 + j,
                            ..AppSettings::default()
                        };
                        store.save_settings(&settings).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        // The file is one of the written values, never a mix, and no temp
        // file is left behind.
        let loaded = store.load_settings();
        assert!((-100..-92).contains(&loaded.rssi_threshold));
        assert!((5..30).contains(&loaded.away_delay_seconds));
        let names: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![SETTINGS_FILE.to_string()]);
    }

    #[test]
    fn save_failure_is_returned_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        // The "directory" is a regular file, so create_dir_all must fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "x").unwrap();
        let store = Store::new(blocker.join("SignalLock"));
        assert!(store.save_settings(&AppSettings::default()).is_err());
        assert!(store.save_trusted_device(Some(&device())).is_err());
        // Loading from such a store still falls back to defaults.
        assert_eq!(store.load_settings(), AppSettings::default());
    }
}
