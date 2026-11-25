//! Embeds the app icon. GPUI loads icon resource 1 for the window and taskbar, and Explorer
//! shows it on the exe. Regenerate the .ico with `assets/brand/make-ico.ps1`.

fn main() {
    println!("cargo:rerun-if-changed=assets/brand/zeroclip.rc");
    println!("cargo:rerun-if-changed=assets/brand/zeroclip.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && let Err(e) = embed_resource::compile("assets/brand/zeroclip.rc", embed_resource::NONE)
            .manifest_optional()
    {
        panic!("embedding the app icon failed: {e}");
    }
}
