//! Embeds libmpv (zstd-compressed) so Glotzkiste ships as one executable.
//! `GK_LIBMPV` names the DLL (set in `.cargo/config.toml`); without it the
//! build embeds nothing and the app looks for libmpv next to itself.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=GK_LIBMPV");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dest = out.join("libmpv.zst");
    let meta = out.join("libmpv.meta");
    let Some(src) = std::env::var_os("GK_LIBMPV").map(PathBuf::from).filter(|p| p.is_file()) else {
        std::fs::write(&dest, b"").expect("write empty");
        std::fs::write(&meta, b"0 0").expect("write meta");
        println!("cargo:warning=GK_LIBMPV not set: libmpv is not embedded");
        return;
    };
    println!("cargo:rerun-if-changed={}", src.display());
    let raw = std::fs::read(&src).expect("read libmpv");
    // FNV-1a of the DLL names the extracted file, so a changed DLL never
    // reuses an old extraction.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &raw {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let stamp = format!("{h:016x} {}", raw.len());
    // Reuse the compressed file when the DLL did not change.
    if std::fs::read_to_string(&meta).ok().as_deref() == Some(stamp.as_str()) && dest.is_file() {
        return;
    }
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 12).expect("zstd");
    let _ = enc.multithread(4);
    enc.write_all(&raw).expect("compress");
    let packed = enc.finish().expect("finish");
    std::fs::write(&dest, packed).expect("write libmpv.zst");
    std::fs::write(&meta, stamp).expect("write meta");
}
