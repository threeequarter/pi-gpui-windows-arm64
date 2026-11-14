//! Embeds the Windows app icon (resources/windows/pi-gpui.rc). Other targets
//! get their icon from the bundle: Info.plist on macOS, the .desktop file on
//! Linux.

fn main() {
    println!("cargo:rerun-if-changed=resources/windows/pi-gpui.rc");
    println!("cargo:rerun-if-changed=resources/windows/pi-gpui.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

#[cfg(windows)]
fn embed_windows_icon() {
    embed_resource::compile("resources/windows/pi-gpui.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("the Windows icon resource did not compile");
}

#[cfg(not(windows))]
fn embed_windows_icon() {
    println!("cargo:warning=cross-compiled Windows builds have no app icon");
}
