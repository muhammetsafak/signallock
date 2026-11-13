//! Build script: compiles the Slint windows and, for Windows targets, embeds
//! the exe icon, version info and the DPI-awareness manifest.
//!
//! The three Slint files are compiled separately (each one is its own entry
//! point and is included by `src/ui/mod.rs` through its own generated module).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    slint_build::compile("src/ui/settings.slint").expect("failed to compile settings.slint");
    slint_build::compile("src/ui/device_picker.slint")
        .expect("failed to compile device_picker.slint");
    slint_build::compile("src/ui/update.slint").expect("failed to compile update.slint");
    embed_windows_resources();
}

/// Per-monitor DPI awareness (the Slint windows must not be bitmap-scaled by
/// Windows) and the Windows 10/11 compatibility entry. The app never needs
/// elevation, matching the installer's `PrivilegesRequired=lowest`.
#[cfg(windows)]
const WINDOWS_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Compiled in only when the build runs on Windows (the Windows package is
/// built on Windows, in CI); the resource compiler (`rc.exe`) is needed.
#[cfg(windows)]
fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    const ICON: &str = "../../packaging/windows/SignalLock.ico";
    println!("cargo:rerun-if-changed={ICON}");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(ICON);
    resource.set_manifest(WINDOWS_MANIFEST);
    resource.set("ProductName", "SignalLock");
    // Task Manager shows FileDescription as the process name.
    resource.set("FileDescription", "SignalLock");
    resource.set("OriginalFilename", "SignalLock.exe");
    resource
        .compile()
        .expect("failed to embed the Windows icon and manifest");
}

#[cfg(not(windows))]
fn embed_windows_resources() {}
