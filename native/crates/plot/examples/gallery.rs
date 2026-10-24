//! Renders every sample chart (tests/data, the fixture notebook's outputs and
//! synthesized 100k-point data) to SVG + PNG, light and dark.
//!
//! `cargo run -p herse-plot --release --example gallery -- <out-dir> [name-filter] [--ref <dir>]`
//!
//! Writes `<out>/<name>.svg`, `<out>/<name>.png`, `<out>/<name>.dark.png`
//! (on a dark backdrop) and, when `--ref <dir>` holds `plotly/<name>.svg` /
//! `vegalite/<name>.svg` references (made by tests/data/reference/
//! plotly_ref.py and vegalite_ref.cjs with the real plotly.js / vega),
//! `<out>/<name>.cmp.png` with ours on the left and the reference on the
//! right. Prints per-chart CPU times and notes, and lists charts that
//! returned `None`.
use std::path::{Path, PathBuf};
use std::time::Duration;

use herse_plot::{RenderOptions, Rendered, render_plotly, render_vegalite, vegalite_spec_from_html};
use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, Transform};
use serde_json::Value;

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plotly,
    Vega,
}

struct Sample {
    name: String,
    kind: Kind,
    value: Value,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = PathBuf::from("target/plot-gallery");
    let mut filter: Option<String> = None;
    let mut ref_dir: Option<PathBuf> = None;
    let mut i = 0;
    let mut positional = 0;
    while i < args.len() {
        if args[i] == "--ref" {
            ref_dir = args.get(i + 1).map(PathBuf::from);
            i += 2;
            continue;
        }
        match positional {
            0 => out = PathBuf::from(&args[i]),
            _ => filter = Some(args[i].clone()),
        }
        positional += 1;
        i += 1;
    }
    std::fs::create_dir_all(&out).expect("create out dir");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut samples = Vec::new();
    for (dir, kind) in [("tests/data/plotly", Kind::Plotly), ("tests/data/vegalite", Kind::Vega)] {
        let mut files: Vec<_> = std::fs::read_dir(root.join(dir)).into_iter().flatten().flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            if f.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = f.file_stem().unwrap().to_string_lossy().to_string();
            let value: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
            samples.push(Sample { name, kind, value });
        }
    }
    samples.extend(fixture_samples(&root.join("../../fixtures/plots.ipynb")));
    samples.extend(synthesized());

    let opt = {
        let mut o = resvg::usvg::Options::default();
        o.fontdb_mut().load_system_fonts();
        o
    };
    let mut failed = Vec::new();
    let mut total = Duration::ZERO;
    let mut count = 0;
    for s in &samples {
        if let Some(f) = &filter
            && !s.name.contains(f.as_str())
        {
            continue;
        }
        let light = RenderOptions::default();
        let dark = RenderOptions { dark: true, ..RenderOptions::default() };
        let render = |o: &RenderOptions| -> (Option<Rendered>, Duration) {
            let t0 = thread_cpu();
            let r = match s.kind {
                Kind::Plotly => render_plotly(&s.value, o),
                Kind::Vega => render_vegalite(&s.value, o),
            };
            (r, thread_cpu() - t0)
        };
        let (r, dt) = render(&light);
        let Some(r) = r else {
            println!("{:<34} None", s.name);
            failed.push(s.name.clone());
            continue;
        };
        // Re-time (warm caches) and keep the faster.
        let (_, dt2) = render(&light);
        let dt = dt.min(dt2);
        total += dt;
        count += 1;
        println!(
            "{:<34} {:>7.2} ms  {:>4}x{:<4} {:>7} B{}",
            s.name,
            dt.as_secs_f64() * 1e3,
            r.width,
            r.height,
            r.svg.len(),
            if r.notes.is_empty() { String::new() } else { format!("  notes: {}", r.notes.join("; ")) }
        );
        std::fs::write(out.join(format!("{}.svg", s.name)), &r.svg).unwrap();
        let png = raster(&r.svg, &opt, None);
        if let Some(p) = &png {
            p.save_png(out.join(format!("{}.png", s.name))).unwrap();
        }
        let (rd, _) = render(&dark);
        if let Some(rd) = rd {
            std::fs::write(out.join(format!("{}.dark.svg", s.name)), &rd.svg).unwrap();
            if let Some(p) = raster(&rd.svg, &opt, Some(Color::from_rgba8(30, 31, 34, 255))) {
                p.save_png(out.join(format!("{}.dark.png", s.name))).unwrap();
            }
        }
        if let (Some(dir), Some(ours)) = (&ref_dir, &png) {
            let sub = match s.kind {
                Kind::Plotly => "plotly",
                Kind::Vega => "vegalite",
            };
            let rp = dir.join(sub).join(format!("{}.svg", s.name));
            if let Ok(svg) = std::fs::read_to_string(&rp)
                && let Some(theirs) = raster(&svg, &opt, None)
            {
                let w = ours.width() + theirs.width() + 12;
                let h = ours.height().max(theirs.height());
                let mut cmp = Pixmap::new(w, h).unwrap();
                cmp.fill(Color::from_rgba8(200, 30, 30, 255));
                cmp.draw_pixmap(0, 0, ours.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                cmp.draw_pixmap((ours.width() + 12) as i32, 0, theirs.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                cmp.save_png(out.join(format!("{}.cmp.png", s.name))).unwrap();
            }
        }
    }
    println!(
        "\n{count} rendered, mean {:.2} ms; None: {}",
        if count > 0 { total.as_secs_f64() * 1e3 / count as f64 } else { 0. },
        if failed.is_empty() { "-".to_string() } else { failed.join(", ") }
    );
}

fn raster(svg: &str, opt: &resvg::usvg::Options, bg: Option<Color>) -> Option<Pixmap> {
    let tree = match resvg::usvg::Tree::from_str(svg, opt) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("usvg error: {e}");
            return None;
        }
    };
    let size = tree.size().to_int_size();
    let mut pm = Pixmap::new(size.width().max(1), size.height().max(1))?;
    pm.fill(bg.unwrap_or(Color::WHITE));
    resvg::render(&tree, Transform::identity(), &mut pm.as_mut());
    Some(pm)
}

/// Plotly / Altair outputs stored in fixtures/plots.ipynb.
fn fixture_samples(path: &Path) -> Vec<Sample> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(nb) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    let mut out = Vec::new();
    let cells = nb["cells"].as_array().cloned().unwrap_or_default();
    for (ci, cell) in cells.iter().enumerate() {
        for (oi, o) in cell["outputs"].as_array().into_iter().flatten().enumerate() {
            let data = &o["data"];
            if let Some(v) = data.get("application/vnd.plotly.v1+json") {
                out.push(Sample { name: format!("nb_cell{ci}_{oi}_plotly"), kind: Kind::Plotly, value: v.clone() });
            } else if let Some(h) = data.get("text/html") {
                let html = match h {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
                    _ => continue,
                };
                if html.len() > 1_000_000 {
                    continue;
                }
                if let Some(spec) = vegalite_spec_from_html(&html) {
                    out.push(Sample { name: format!("nb_cell{ci}_{oi}_altair"), kind: Kind::Vega, value: spec });
                } else if html.contains("bokeh") || html.contains("Bokeh") {
                    out.push(Sample { name: format!("nb_cell{ci}_{oi}_bokeh_html"), kind: Kind::Vega, value: Value::String(html) });
                }
            }
        }
    }
    out
}

struct Lcg(u64);
impl Lcg {
    fn uniform(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2. * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for ch in bytes.chunks(3) {
        let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// 100k-point charts (kept out of tests/data to keep the repo small).
fn synthesized() -> Vec<Sample> {
    let mut r = Lcg(42);
    let n = 100_000;
    let xs: Vec<f64> = (0..n).map(|_| r.normal()).collect();
    let ys: Vec<f64> = (0..n).map(|_| r.normal()).collect();
    let scatter = serde_json::json!({
        "data": [{"type": "scattergl", "mode": "markers", "x": xs, "y": ys, "marker": {"size": 3}}],
        "layout": {"title": {"text": "100k points"}}
    });
    let mut acc = 0.;
    let walk: Vec<f64> = (0..n)
        .map(|_| {
            acc += r.normal();
            acc
        })
        .collect();
    let line = serde_json::json!({
        "data": [{"type": "scatter", "mode": "lines", "y": walk}],
        "layout": {"title": {"text": "100k-point line"}}
    });
    // plotly >= 6 typed arrays.
    let bx: Vec<u8> = (0..n).flat_map(|i| (i as f64).to_le_bytes()).collect();
    let by: Vec<u8> = walk.iter().flat_map(|v| (*v as f32).to_le_bytes()).collect();
    let typed = serde_json::json!({
        "data": [{"type": "scatter", "mode": "lines", "x": {"dtype": "f8", "bdata": b64(&bx)}, "y": {"dtype": "f4", "bdata": b64(&by)}}],
        "layout": {}
    });
    let vl_rows: Vec<Value> = (0..20_000).map(|_| serde_json::json!({"x": r.normal(), "y": r.normal()})).collect();
    let vl = serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "data": {"values": vl_rows},
        "mark": {"type": "circle", "size": 8, "opacity": 0.4},
        "encoding": {"x": {"field": "x", "type": "quantitative"}, "y": {"field": "y", "type": "quantitative"}}
    });
    vec![
        Sample { name: "big_scatter_100k".into(), kind: Kind::Plotly, value: scatter },
        Sample { name: "big_line_100k".into(), kind: Kind::Plotly, value: line },
        Sample { name: "big_line_typed_arrays".into(), kind: Kind::Plotly, value: typed },
        Sample { name: "big_vl_points_20k".into(), kind: Kind::Vega, value: vl },
    ]
}
