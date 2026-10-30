//! Embeds the icon, the application manifest (per-monitor DPI awareness v2,
//! common controls v6) and version info into TedCat.exe, and delay-loads
//! Media Foundation.

/// Missing on Windows N and KN editions until the Media Feature Pack is
/// installed. Delay-loaded so TedCat.exe still starts there (screenshots and
/// the gesture work); recording checks for them first (`record/sys.rs`) and
/// says what is missing instead.
const DELAY_LOADED: [&str; 2] = ["mfplat.dll", "mfreadwrite.dll"];

fn main() {
    println!("cargo:rerun-if-changed=resources/app.rc");
    println!("cargo:rerun-if-changed=resources/app.manifest");
    println!("cargo:rerun-if-changed=resources/tedcat.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("resources/app.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to compile Windows resources");
        // MSVC's link.exe converts the imports (windows-rs declares them with
        // raw-dylib, which rustc turns into an ordinary import library) into
        // delay-load imports; delayimp.lib provides the helper. The GNU
        // toolchain has no delay loading, and only MSVC builds are released.
        //
        // Cargo applies `-bins` link arguments to the bin's unit-test harness
        // as well (a build script cannot tell the two links apart), and no
        // test calls into Media Foundation, so link.exe would warn there that
        // the /DELAYLOAD is unused (LNK4199). That warning is silenced; the
        // smoke test reads the release TedCat.exe's import table to prove
        // both DLLs really are delay-loaded (step recording-delay-load).
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            for dll in DELAY_LOADED {
                println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
            }
            println!("cargo:rustc-link-arg-bins=delayimp.lib");
            println!("cargo:rustc-link-arg-bins=/IGNORE:4199");
        }
    }
}
