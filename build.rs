//! Windows resource script, so the binary carries a manifest.
//!
//! Without this, wxWidgets prints at startup:
//!
//!     This application doesn't use a correct manifest specifying the use of
//!     Common Controls Library v6 ...
//!
//! That is not cosmetic. The manifest in `wx/msw/wx.rc` also carries the
//! DPI-awareness declaration, and without it Windows lies to us about screen
//! coordinates once the display is scaled — which is exactly the case where a
//! screen reader gets told the wrong place for a control.
//!
//! Why a build script and not "just let the library do it": Cargo compiles
//! Rust and runs build scripts; it has no resource-compilation step at all.
//! A `.rc` file has to go through `rc.exe` on Windows, which is what
//! `embed-resource` arranges. `wxdragon-sys` links a prebuilt static wxWidgets
//! and never compiles `wx.rc` into anything, so its own manifest cannot enter
//! our binary even if the crate wanted it to.
//!
//! Everything here is a no-op off Windows: the crate still builds with no
//! manifest work on Linux, and `cargo test --lib` in a container stays
//! untouched.

fn main() {
    #[cfg(windows)]
    windows_manifest();
}

#[cfg(windows)]
fn windows_manifest() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wireutils.rc");

    let mut res = winresource::WindowsResource::new();
    // A hand-written manifest rather than wx.rc: wx.rc also pulls in
    // wxWidgets' icon and version resources, which would duplicate entries
    // the linker already has from the static library. The content here is
    // the part of wx.rc that matters — Common Controls v6 and the DPI
    // declaration.
    res.set_manifest_file("wireutils.manifest");
    if let Err(e) = res.compile() {
        // Not fatal: a missing manifest degrades the app, it does not break
        // it, and failing the build on a resource-compiler hiccup would block
        // every other fix. The warning is the reminder, and it is visible.
        println!("cargo:warning=could not attach the Windows manifest: {e}");
    }
}
