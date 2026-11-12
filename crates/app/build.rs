//! Build script. The `.slint` files are compiled here from Faz 05 on (and, on
//! Windows, the icon and the DPI-awareness manifest are embedded in Faz 06);
//! there is nothing to compile yet.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
