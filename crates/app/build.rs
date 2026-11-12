//! Build script: compiles the Slint windows. The two files are compiled
//! separately (each one is its own entry point and is included by
//! `src/ui/mod.rs` through its own generated module). On Windows the icon and
//! the DPI-awareness manifest are embedded in Faz 06.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    slint_build::compile("src/ui/settings.slint").expect("failed to compile settings.slint");
    slint_build::compile("src/ui/device_picker.slint")
        .expect("failed to compile device_picker.slint");
}
