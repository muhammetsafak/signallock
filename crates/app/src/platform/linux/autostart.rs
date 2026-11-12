//! Login autostart through an XDG autostart entry:
//! `$XDG_CONFIG_HOME/autostart/signallock.desktop` (fallback
//! `~/.config/autostart/`), written when enabled and deleted when disabled
//! (`.ssot/07-linux-plan.md`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::super::{Autostart, PlatformError, Result};
use crate::logging;

pub const DESKTOP_FILE_NAME: &str = "signallock.desktop";

/// Where the entry lives under the user's config directory.
pub fn desktop_file_path(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(DESKTOP_FILE_NAME)
}

/// The command the entry starts: `$APPIMAGE` inside an AppImage (the mounted
/// binary path changes on every run, the `.AppImage` file does not), else the
/// running executable.
pub fn resolve_exec(appimage: Option<OsString>, current_exe: PathBuf) -> PathBuf {
    match appimage {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => current_exe,
    }
}

/// Quotes one `Exec` argument as the Desktop Entry specification requires:
/// a plain argument stays as is; anything with a space or a reserved
/// character is wrapped in double quotes with `"`, `` ` ``, `$` and `\`
/// escaped, and the backslash escaping of string values applied on top
/// (so a literal backslash becomes four). A literal `%` is written `%%`
/// so that it is not read as a field code.
pub fn quote_exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    let plain = arg.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '%' | '+' | ':' | ',')
    });
    if plain {
        return arg;
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        match c {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Whether `exec` can be written into an `Exec=` line. A line break would end
/// the line and start a new key, so such a path is refused.
pub fn exec_is_representable(exec: &Path) -> bool {
    !exec.to_string_lossy().contains(['\n', '\r'])
}

/// Contents of the `.desktop` file for `exec`.
pub fn desktop_entry(exec: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=SignalLock\n\
         Comment=Locks the computer when a trusted Bluetooth device moves out of range\n\
         Exec={}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        quote_exec_arg(&exec.to_string_lossy())
    )
}

/// Autostart entry under a config home directory.
#[derive(Debug, Clone)]
pub struct LinuxAutostart {
    config_home: PathBuf,
}

impl LinuxAutostart {
    /// Uses `$XDG_CONFIG_HOME`, falling back to `~/.config`.
    pub fn new() -> Self {
        let config_home = directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".config"));
        Self { config_home }
    }

    /// Uses an explicit config home (tests).
    pub fn with_config_home(config_home: PathBuf) -> Self {
        Self { config_home }
    }

    fn path(&self) -> PathBuf {
        desktop_file_path(&self.config_home)
    }
}

impl Default for LinuxAutostart {
    fn default() -> Self {
        Self::new()
    }
}

impl Autostart for LinuxAutostart {
    fn is_enabled(&self) -> bool {
        self.path().is_file()
    }

    fn set_enabled(&self, on: bool) -> Result<()> {
        let path = self.path();
        let result = if on {
            std::env::current_exe().and_then(|current| {
                let exec = resolve_exec(std::env::var_os("APPIMAGE"), current);
                if !exec_is_representable(&exec) {
                    return Err(io::Error::other(
                        "the executable path contains a line break",
                    ));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, desktop_entry(&exec))
            })
        } else {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        };
        result.map_err(|e| {
            let message = format!("autostart entry {} failed: {e}", path.display());
            tracing::warn!(target: logging::APP, "{message}");
            PlatformError::new(message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;

    #[test]
    fn desktop_entry_has_the_expected_content() {
        let entry = desktop_entry(Path::new("/opt/signallock/signallock"));
        assert_eq!(
            entry,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=SignalLock\n\
             Comment=Locks the computer when a trusted Bluetooth device moves out of range\n\
             Exec=/opt/signallock/signallock\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
    }

    #[test]
    fn exec_with_spaces_is_quoted() {
        let entry = desktop_entry(Path::new("/home/me/My Apps/SignalLock.AppImage"));
        assert!(entry.contains("Exec=\"/home/me/My Apps/SignalLock.AppImage\"\n"));
    }

    #[test]
    fn exec_quoting_follows_the_desktop_entry_spec() {
        assert_eq!(quote_exec_arg("/usr/bin/signallock"), "/usr/bin/signallock");
        assert_eq!(quote_exec_arg("/a b/c"), "\"/a b/c\"");
        assert_eq!(quote_exec_arg("/a\"b"), "\"/a\\\\\"b\"");
        assert_eq!(quote_exec_arg("/a$b"), "\"/a\\\\$b\"");
        assert_eq!(quote_exec_arg("/a\\b"), "\"/a\\\\\\\\b\"");
        assert_eq!(quote_exec_arg("/100%/x"), "/100%%/x");
    }

    #[test]
    fn exec_paths_with_line_breaks_are_refused() {
        assert!(exec_is_representable(Path::new(
            "/opt/signallock/signallock"
        )));
        assert!(!exec_is_representable(Path::new("/opt/a\nExec=evil")));
        assert!(!exec_is_representable(Path::new("/opt/a\rb")));
    }

    #[test]
    fn appimage_path_wins_over_the_mounted_executable() {
        let mounted = PathBuf::from("/tmp/.mount_SignalXYZ/usr/bin/signallock");
        assert_eq!(
            resolve_exec(
                Some(OsString::from("/home/me/SignalLock.AppImage")),
                mounted.clone()
            ),
            PathBuf::from("/home/me/SignalLock.AppImage")
        );
        assert_eq!(resolve_exec(None, mounted.clone()), mounted);
        assert_eq!(
            resolve_exec(Some(OsString::new()), mounted.clone()),
            mounted
        );
    }

    #[test]
    fn enable_writes_and_disable_removes_the_file() {
        let home = TestDir::new("linux-autostart");
        let autostart = LinuxAutostart::with_config_home(home.path().to_path_buf());
        let file = home.path().join("autostart").join("signallock.desktop");
        assert!(!autostart.is_enabled());

        autostart.set_enabled(true).unwrap();
        assert!(autostart.is_enabled());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("[Desktop Entry]\n"));
        assert!(content.contains("\nExec="));
        autostart.set_enabled(true).unwrap();

        autostart.set_enabled(false).unwrap();
        assert!(!autostart.is_enabled());
        assert!(!file.exists());
        autostart.set_enabled(false).unwrap();
    }
}
