//! End-to-end: every sample renders (or declines) to valid SVG, the fixture
//! notebook's outputs behave, and no input — mutated, truncated or absurd —
//! makes the renderers panic.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use herse_plot::{RenderOptions, Rendered, render_plotly, render_vegalite, vegalite_spec_from_html};
use serde_json::{Value, json};

/// Samples we expect `None` for (can't be drawn faithfully).
const UNSUPPORTED: &[&str] = &["unsupported_violin", "unsupported_scatter3d", "unsupported_url_data"];

fn samples(kind: &str) -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(kind);
    let mut out: Vec<(String, Value)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| {
            let name = e.path().file_stem().unwrap().to_string_lossy().to_string();
            (name, serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Same compact number formatting the SVG writer uses (2 decimals, trimmed).
fn herse_plot_fmt(v: f32) -> String {
    let s = format!("{:.2}", v as f64);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

fn parse_svg(r: &Rendered) {
    let opt = resvg::usvg::Options::default();
    if let Err(e) = resvg::usvg::Tree::from_str(&r.svg, &opt) {
        panic!("invalid SVG: {e}\n{}", &r.svg[..r.svg.len().min(500)]);
    }
    assert!(r.width > 0. && r.height > 0.);
}

#[test]
fn every_sample_renders_valid_svg() {
    for dark in [false, true] {
        let opts = RenderOptions { dark, ..Default::default() };
        for (kind, f) in [("plotly", render_plotly as fn(&Value, &RenderOptions) -> Option<Rendered>), ("vegalite", render_vegalite)] {
            for (name, v) in samples(kind) {
                let r = f(&v, &opts);
                if UNSUPPORTED.contains(&name.as_str()) {
                    assert!(r.is_none(), "{kind}/{name} should be declined");
                    continue;
                }
                let r = r.unwrap_or_else(|| panic!("{kind}/{name} returned None"));
                parse_svg(&r);
                assert!(r.width <= opts.width + 0.5, "{kind}/{name} too wide: {}", r.width);
                if dark {
                    // Transparent canvas: no opaque full-size background rect.
                    let full = format!("<rect x=\"0\" y=\"0\" width=\"{}\"", herse_plot_fmt(r.width));
                    let bg = r.svg.lines().find(|l| l.starts_with(&full));
                    assert!(bg.is_none_or(|l| l.contains("fill-opacity") || l.contains("fill=\"none\"")), "{kind}/{name}: opaque background in dark mode: {bg:?}");
                }
            }
        }
    }
}

#[test]
fn fixture_notebook_outputs() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/plots.ipynb");
    let nb: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cells = nb["cells"].as_array().unwrap();
    let data = |ci: usize| -> Vec<&Value> { cells[ci]["outputs"].as_array().unwrap().iter().map(|o| &o["data"]).collect() };
    let opts = RenderOptions::default();
    // plotly express scatter and graph_objects bar render; the 3D surface is declined.
    for ci in [36, 37] {
        let fig = data(ci).into_iter().find_map(|d| d.get("application/vnd.plotly.v1+json")).unwrap();
        parse_svg(&render_plotly(fig, &opts).unwrap());
    }
    let surface = data(38).into_iter().find_map(|d| d.get("application/vnd.plotly.v1+json")).unwrap();
    assert!(render_plotly(surface, &opts).is_none());
    // Altair: spec from the HTML, then rendered.
    for ci in [40, 41, 42] {
        let html: String = data(ci)
            .into_iter()
            .find_map(|d| d.get("text/html"))
            .map(|h| match h {
                Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .unwrap();
        let spec = vegalite_spec_from_html(&html).unwrap_or_else(|| panic!("no spec in cell {ci}"));
        assert!(spec.get("datasets").is_some());
        parse_svg(&render_vegalite(&spec, &opts).unwrap());
    }
    // Bokeh's HTML holds no Vega-Lite spec.
    for d in data(44) {
        if let Some(Value::String(h)) = d.get("text/html") {
            assert!(vegalite_spec_from_html(h).is_none());
        }
    }
}

// ---------------------------------------------------------------- geometry

type Rect = (f64, f64, f64, f64);

/// Each plot frame (the view's `cell` stroke) with the bar rects drawn after
/// it, in document order. Rects starting right of the frame (legend swatches)
/// are left out.
fn frames_and_bars(svg: &str) -> Vec<(Rect, Vec<Rect>)> {
    let attr = |l: &str, k: &str| -> f64 {
        let p = format!(" {k}=\"");
        let i = l.find(&p).unwrap() + p.len();
        l[i..].split('"').next().unwrap().parse().unwrap()
    };
    let mut out: Vec<(Rect, Vec<Rect>)> = Vec::new();
    for l in svg.lines() {
        if l.starts_with("<rect") && l.contains("fill=\"none\"") {
            // Stroked half a pixel in, for crisp lines.
            out.push(((attr(l, "x") - 0.5, attr(l, "y") - 0.5, attr(l, "width"), attr(l, "height")), Vec::new()));
        } else if let (Some(d), Some(((fx, _, fw, _), bars))) = (l.strip_prefix("<path d=\"M").filter(|_| !l.contains("stroke=")), out.last_mut()) {
            // Bars are runs of `M{x} {y}h{w}v{h}h{-w}Z`.
            for run in d.split('"').next().unwrap().split('Z').map(|s| s.trim_start_matches('M')) {
                let n: Vec<f64> = run.split(['h', 'v', ' ']).filter_map(|x| x.parse().ok()).collect();
                if n.len() == 5 && run.chars().all(|c| c.is_ascii_digit() || "-. hv".contains(c)) && n[0] < *fx + *fw {
                    bars.push((n[0], n[1], n[2], n[3]));
                }
            }
        }
    }
    out
}

#[test]
fn aggregated_bars_stay_inside_the_plot() {
    // The vconcat sample's top view: count over an unbinned quantitative x,
    // one bar per hp value, the tallest a count of 5.
    let spec = samples("vegalite").into_iter().find(|s| s.0 == "vconcat").unwrap().1;
    let r = render_vegalite(&spec, &RenderOptions::default()).unwrap();
    let views = frames_and_bars(&r.svg);
    let ((_, fy, _, fh), bars) = &views[0];
    assert_eq!(bars.len(), 106, "one bar per distinct hp");
    for &(x, y, _, h) in bars {
        assert!(y >= fy - 0.01 && y + h <= fy + fh + 0.01, "bar at x={x} spans y {y}..{} outside the plot {fy}..{}", y + h, fy + fh);
    }
    // y domain: zero..max count, niced (0..5, as in Vega-Lite), so the count-5
    // bar reaches the top even though the last tick (step 2) is 4.
    let top = bars.iter().map(|b| b.1).fold(f64::INFINITY, f64::min);
    assert!((top - fy).abs() < 0.01, "tallest bar top {top}, plot top {fy}");
    // Same data, other aggregates, binned x and color-stacked bars.
    let q = |f: &str| json!({"field": f, "type": "quantitative"});
    let count = json!({"aggregate": "count", "type": "quantitative"});
    let binned = json!({"field": "hp", "bin": true, "type": "quantitative"});
    let color = json!({"field": "origin", "type": "nominal"});
    for enc in [
        json!({"x": q("hp"), "y": {"aggregate": "sum", "field": "mpg", "type": "quantitative"}}),
        json!({"x": q("hp"), "y": {"aggregate": "mean", "field": "mpg", "type": "quantitative"}}),
        json!({"x": binned, "y": count}),
        json!({"x": q("hp"), "y": count, "color": color}),
        json!({"x": binned, "y": count, "color": color}),
        json!({"y": q("hp"), "x": count, "color": color}),
    ] {
        let s = json!({"data": spec["data"], "datasets": spec["datasets"], "mark": "bar", "encoding": enc, "height": 100});
        let r = render_vegalite(&s, &RenderOptions::default()).unwrap();
        for ((fx, fy, fw, fh), bars) in frames_and_bars(&r.svg) {
            assert!(!bars.is_empty(), "{enc}: no bars");
            for (x, y, w, h) in bars {
                let inside = x >= fx - 0.01 && x + w <= fx + fw + 0.01 && y >= fy - 0.01 && y + h <= fy + fh + 0.01;
                assert!(inside, "{enc}: bar {:?} outside the plot {:?}", (x, y, w, h), (fx, fy, fw, fh));
            }
        }
    }
}

// ---------------------------------------------------------------- robustness

static PANICS: AtomicUsize = AtomicUsize::new(0);
static HOOK: Once = Once::new();
static LOCK: Mutex<()> = Mutex::new(());

fn count_panics() {
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            prev(info);
        }));
    });
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000. < p
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.next() as usize % v.len()]
    }
}

fn junk(r: &mut Lcg) -> Value {
    let choices = [
        Value::Null,
        json!(0),
        json!(-0.0),
        json!(1e308),
        json!(-1e308),
        json!(5e-324),
        json!(i64::MIN),
        json!(""),
        json!("abc"),
        json!("2020-13-45"),
        json!("NaN"),
        json!("Infinity"),
        json!("#zzz"),
        json!("rgb(1,2"),
        json!("%{y:.2f"),
        json!("<b>unclosed"),
        json!([]),
        json!({}),
        json!([null, "x", 1, [2], {"a": 1}]),
        json!({"dtype": "f8", "bdata": "AAAA"}),
        json!({"dtype": "i4", "bdata": "!!!", "shape": "9999999, 9999999"}),
        json!(true),
        json!([[1, 2], [3]]),
    ];
    r.pick(&choices).clone()
}

fn mutate(v: &mut Value, r: &mut Lcg, depth: usize) {
    if depth > 12 {
        return;
    }
    if r.chance(0.04) {
        *v = junk(r);
        return;
    }
    match v {
        Value::Array(a) => {
            if r.chance(0.2) && !a.is_empty() {
                let n = r.next() as usize % a.len();
                a.truncate(n);
            }
            if r.chance(0.05) {
                a.push(junk(r));
            }
            let stride = (a.len() / 64).max(1);
            for x in a.iter_mut().step_by(stride) {
                mutate(x, r, depth + 1);
            }
        }
        Value::Object(o) => {
            let keys: Vec<String> = o.keys().cloned().collect();
            for k in keys {
                if r.chance(0.05) {
                    o.remove(&k);
                } else if let Some(x) = o.get_mut(&k) {
                    mutate(x, r, depth + 1);
                }
            }
        }
        _ => {}
    }
}

#[test]
fn mutated_inputs_never_panic() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    count_panics();
    let before = PANICS.load(Ordering::SeqCst);
    let mut r = Lcg(0x5eed);
    let opts = RenderOptions::default();
    for (kind, f) in [("plotly", render_plotly as fn(&Value, &RenderOptions) -> Option<Rendered>), ("vegalite", render_vegalite)] {
        for (_, v) in samples(kind) {
            for _ in 0..12 {
                let mut m = v.clone();
                mutate(&mut m, &mut r, 0);
                if let Some(out) = f(&m, &opts) {
                    parse_svg(&out);
                }
            }
        }
    }
    // Wrong shapes entirely.
    for v in [json!(null), json!(1), json!("x"), json!([]), json!({}), json!({"data": 1}), json!({"data": [1, null, "a"]}), json!({"layout": []}), json!({"mark": {}}), json!({"layer": [{}]}), json!({"hconcat": [1]})] {
        let _ = render_plotly(&v, &opts);
        let _ = render_vegalite(&v, &opts);
    }
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "a renderer panicked");
}

#[test]
fn odd_options_and_truncated_html_never_panic() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    count_panics();
    let before = PANICS.load(Ordering::SeqCst);
    let fig = samples("plotly").into_iter().find(|s| s.0 == "subplots_2x2").unwrap().1;
    let spec = samples("vegalite").into_iter().find(|s| s.0 == "facet_column").unwrap().1;
    for (w, h) in [(0., None), (f32::NAN, Some(f32::NAN)), (1e9, Some(1e9)), (10., Some(10.)), (-5., Some(-1.)), (300., Some(4000.))] {
        let o = RenderOptions { width: w, height: h, dark: true, font_family: String::new(), blend: false };
        if let Some(r) = render_plotly(&fig, &o) {
            parse_svg(&r);
        }
        if let Some(r) = render_vegalite(&spec, &o) {
            parse_svg(&r);
        }
    }
    let html = format!(
        "<div></div><script>(function(spec, embedOpt){{}})({}, {{\"mode\": \"vega-lite\"}});</script>",
        serde_json::to_string(&spec).unwrap()
    );
    assert!(vegalite_spec_from_html(&html).is_some());
    let mut cut = 0;
    while cut < html.len() {
        if html.is_char_boundary(cut) {
            let _ = vegalite_spec_from_html(&html[..cut]);
        }
        cut += 97;
    }
    let _ = vegalite_spec_from_html("{{{{{{{{{{\"$schema\": \"https://vega.github.io/schema/vega-lite/v5.json\"");
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "a renderer panicked");
}

fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
}

#[test]
fn hundred_thousand_points_are_fast_and_compact() {
    let mut r = Lcg(1);
    let n = 100_000;
    let xs: Vec<f64> = (0..n).map(|_| (r.next() % 100_000) as f64 / 1000.).collect();
    let ys: Vec<f64> = (0..n).map(|_| (r.next() % 100_000) as f64 / 1000.).collect();
    let fig = json!({"data": [{"type": "scattergl", "mode": "markers", "x": xs, "y": ys}], "layout": {}});
    let line = json!({"data": [{"type": "scatter", "mode": "lines", "y": ys}], "layout": {}});
    let opts = RenderOptions::default();
    let _ = render_plotly(&fig, &opts);
    let t0 = thread_cpu_ms();
    let out = render_plotly(&fig, &opts).unwrap();
    let dt = thread_cpu_ms() - t0;
    // Release builds take ~15 ms; leave room for debug builds and busy CI.
    let budget = if cfg!(debug_assertions) { 2000. } else { 150. };
    assert!(dt < budget, "100k scatter took {dt:.1} ms");
    assert!(out.svg.len() < 3_000_000, "svg is {} bytes", out.svg.len());
    let t0 = thread_cpu_ms();
    let out = render_plotly(&line, &opts).unwrap();
    let dt = thread_cpu_ms() - t0;
    assert!(dt < budget, "100k line took {dt:.1} ms");
    // Lines are decimated to ~4 points per pixel column.
    assert!(out.svg.len() < 200_000, "line svg is {} bytes", out.svg.len());
}

/// Longer fuzzing run: `FUZZ_ITERS=200 cargo test -p herse-plot --release --test render -- --ignored`
#[test]
#[ignore]
fn fuzz_long() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    count_panics();
    let before = PANICS.load(Ordering::SeqCst);
    let iters: usize = std::env::var("FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(100);
    let opts = RenderOptions::default();
    for seed in 0..4u64 {
        let mut r = Lcg(seed * 7919 + 3);
        for (kind, f) in [("plotly", render_plotly as fn(&Value, &RenderOptions) -> Option<Rendered>), ("vegalite", render_vegalite)] {
            for (name, v) in samples(kind) {
                for _ in 0..iters / 4 {
                    let mut m = v.clone();
                    // Heavier mutation: several passes.
                    for _ in 0..1 + (r.next() % 3) {
                        mutate(&mut m, &mut r, 0);
                    }
                    if std::env::var("FUZZ_VERBOSE").is_ok() {
                        eprintln!("{kind}/{name} seed {seed}");
                        if let Ok(p) = std::env::var("FUZZ_DUMP") { std::fs::write(p, m.to_string()).ok(); }
                    }
                    if let Some(out) = f(&m, &opts) {
                        parse_svg(&out);
                    }
                }
            }
        }
    }
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "a renderer panicked");
}
