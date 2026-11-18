//! `cargo run -p gk-icon` writes `assets/logo/glotzkiste.ico` (16, 20, 24,
//! 32, 40, 48, 64, 128, 256 px) and `assets/logo/glotzkiste-256.png` from
//! `assets/logo/glotzkiste.svg`. Each size is rendered from the vector, not
//! scaled down from a bitmap, so small sizes stay sharp. Both also go to
//! `crates/glotzkiste/res/`, the copies the app embeds (a published crate
//! carries only files inside it).

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let svg_path = root.join("assets/logo/glotzkiste.svg");
    let data = std::fs::read(&svg_path).unwrap_or_else(|e| panic!("{}: {e}", svg_path.display()));
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).expect("valid SVG");
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 20, 24, 32, 40, 48, 64, 128, 256] {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let s = size as f32 / tree.size().width().max(tree.size().height());
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(s, s), &mut pixmap.as_mut());
        // tiny-skia is premultiplied; ICO and PNG want straight alpha.
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for p in pixmap.pixels() {
            let c = p.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        if size == 256 {
            pixmap.save_png(root.join("assets/logo/glotzkiste-256.png")).expect("png");
            pixmap.save_png(root.join("crates/glotzkiste/res/glotzkiste-256.png")).expect("png");
        }
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        // Classic bitmaps up to 128 px, PNG only for 256 px: some tools (the
        // NSIS setup's icon replacement among them) take no PNG entries.
        let entry = if size >= 256 {
            ico::IconDirEntry::encode_as_png(&image)
        } else {
            ico::IconDirEntry::encode_as_bmp(&image)
        };
        dir.add_entry(entry.expect("encode"));
    }
    let out = root.join("assets/logo/glotzkiste.ico");
    let file = std::fs::File::create(&out).expect("create ico");
    dir.write(file).expect("write ico");
    std::fs::copy(&out, root.join("crates/glotzkiste/res/glotzkiste.ico")).expect("copy ico");
    println!("wrote {} and its copy in crates/glotzkiste/res", out.display());
}
