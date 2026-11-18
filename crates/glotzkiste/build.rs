//! Embeds the exe icon. For development builds, `GK_LIBMPV` (set in the
//! local `.cargo/config.toml`) names the pinned libmpv-2.dll, which is copied
//! next to the built executable so `cargo run` plays. Packages ship the DLL
//! themselves (scripts/package.ps1).

use std::path::PathBuf;

fn main() {
    // The exe icon (Explorer, Alt-Tab); the window and taskbar icons are
    // loaded from this same resource at runtime.
    println!("cargo:rerun-if-changed=glotzkiste.rc");
    println!("cargo:rerun-if-changed=res/glotzkiste.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && let Err(e) = embed_resource::compile("glotzkiste.rc", embed_resource::NONE).manifest_optional()
    {
        println!("cargo:warning=exe icon not embedded: {e}");
    }
    println!("cargo:rerun-if-env-changed=GK_LIBMPV");
    let Some(src) = std::env::var_os("GK_LIBMPV").map(PathBuf::from).filter(|p| p.is_file()) else { return };
    println!("cargo:rerun-if-changed={}", src.display());
    // OUT_DIR is <target>/<profile>/build/<crate>[-/]<hash>/out: the
    // profile folder is the parent of `build`.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let Some(bin) = out.ancestors().find(|a| a.file_name().is_some_and(|n| n == "build")).and_then(|b| b.parent())
    else {
        return;
    };
    let dest = bin.join("libmpv-2.dll");
    let same = |a: &std::path::Path, b: &std::path::Path| matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.len() == y.len());
    if !same(&src, &dest)
        && let Err(e) = std::fs::copy(&src, &dest)
    {
        // A running copy of the app keeps the DLL locked; the old one stays.
        println!("cargo:warning=libmpv not copied to {}: {e}", dest.display());
    }
}
