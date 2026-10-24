//! Renders every Mermaid sample (tests/data/mermaid/*.mmd) to SVG + PNG,
//! light and dark.
//!
//! `cargo run -p herse-plot --release --example mermaid -- <out-dir> [name-filter]`
//!
//! Writes `<out>/<name>.svg`, `<out>/<name>.png` (on the app's white page)
//! and `<out>/<name>.dark.png` (on its dark page, `#191817`), prints
//! per-diagram CPU times, and lists samples that returned `None`
//! (`unsupported_*` samples are expected to).
use std::path::{Path, PathBuf};
use std::time::Duration;

use herse_plot::{RenderOptions, render_mermaid};
use resvg::tiny_skia::{Color, Pixmap, Transform};

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(args.first().map(String::as_str).unwrap_or("target/mermaid-gallery"));
    let filter = args.get(1).cloned();
    std::fs::create_dir_all(&out).expect("create out dir");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mermaid");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.retain(|f| f.extension().is_some_and(|e| e == "mmd"));
    files.sort();
    let opt = {
        let mut o = resvg::usvg::Options::default();
        o.fontdb_mut().load_system_fonts();
        o
    };
    let font = std::env::var("MERMAID_FONT").unwrap_or_else(|_| RenderOptions::default().font_family);
    let mut none = Vec::new();
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        if filter.as_ref().is_some_and(|flt| !name.contains(flt.as_str())) {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let light = RenderOptions { width: 900., font_family: font.clone(), ..Default::default() };
        let dark = RenderOptions { dark: true, ..light.clone() };
        let t0 = thread_cpu();
        let r = render_mermaid(&src, &light);
        let mut dt = thread_cpu() - t0;
        let Some(r) = r else {
            println!("{name:<28} None");
            none.push(name);
            continue;
        };
        for _ in 0..4 {
            let t0 = thread_cpu();
            let _ = render_mermaid(&src, &light);
            dt = dt.min(thread_cpu() - t0);
        }
        println!("{name:<28} {:>7.3} ms  {:>5}x{:<5} {:>7} B", dt.as_secs_f64() * 1e3, r.width, r.height, r.svg.len());
        std::fs::write(out.join(format!("{name}.svg")), &r.svg).unwrap();
        if let Some(p) = raster(&r.svg, &opt, Color::WHITE) {
            p.save_png(out.join(format!("{name}.png"))).unwrap();
        }
        if let Some(rd) = render_mermaid(&src, &dark)
            && let Some(p) = raster(&rd.svg, &opt, Color::from_rgba8(0x19, 0x18, 0x17, 255))
        {
            p.save_png(out.join(format!("{name}.dark.png"))).unwrap();
        }
    }
    println!("\nNone: {}", if none.is_empty() { "-".to_string() } else { none.join(", ") });
}

/// Rasterises at 2x, like a Retina display.
fn raster(svg: &str, opt: &resvg::usvg::Options, bg: Color) -> Option<Pixmap> {
    let tree = match resvg::usvg::Tree::from_str(svg, opt) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("usvg error: {e}");
            return None;
        }
    };
    let size = tree.size().to_int_size();
    let mut pm = Pixmap::new(size.width().max(1) * 2, size.height().max(1) * 2)?;
    pm.fill(bg);
    resvg::render(&tree, Transform::from_scale(2., 2.), &mut pm.as_mut());
    Some(pm)
}
