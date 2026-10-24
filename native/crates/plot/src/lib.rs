//! Static SVG rendering of the interactive charts notebooks store as JSON:
//! Plotly figures (`application/vnd.plotly.v1+json`) and Vega-Lite specs
//! (`application/vnd.vegalite.v*+json`, or the spec Altair embeds in its
//! `text/html` output), and of Mermaid diagrams (```` ```mermaid ```` fences
//! in Markdown, `text/vnd.mermaid` outputs).
//!
//! The goal is a faithful *picture* of what plotly.js / vega-embed would draw
//! on first load — same data mapping, axis ranges, ticks, labels, legend and
//! default look — without JavaScript. Anything this crate can't draw
//! faithfully makes the render functions return `None` so the caller can keep
//! its "interactive output" note: no chart is better than a wrong one.
//!
//! Output is plain SVG 1.1 (rects, paths, circles, text) that usvg/resvg
//! renders; text is `<text>` in [`RenderOptions::font_family`].

mod color;
mod format;
mod mermaid;
mod plotly;
mod schemes;
mod svg;
mod text;
mod ticks;
mod time;
mod util;
mod vegalite;

use std::panic::{AssertUnwindSafe, catch_unwind};

/// How to render a chart.
#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Available width in px. Charts with an explicit, smaller width keep it.
    pub width: f32,
    /// Height in px; default: the figure's own height, else 400.
    pub height: Option<f32>,
    /// Dark app background: transparent canvas, light text and grid.
    pub dark: bool,
    /// CSS font-family list used for every `<text>`.
    pub font_family: String,
    /// Sit on the page instead of a card of its own: no background unless
    /// the figure sets one itself, quiet ink grids, no white boxes. (Dark
    /// mode always does this.)
    pub blend: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 640.,
            height: None,
            dark: false,
            font_family: "Helvetica, Arial, sans-serif".into(),
            blend: false,
        }
    }
}

/// A rendered chart.
#[derive(Clone, Debug)]
pub struct Rendered {
    /// Standalone SVG document (`<svg width=.. height=.. viewBox=..>`).
    pub svg: String,
    pub width: f32,
    pub height: f32,
    /// Features that were skipped or approximated (e.g. "animation shown at
    /// its first frame"), for a muted caption under the image.
    pub notes: Vec<String>,
}

/// Renders the value of an `application/vnd.plotly.v1+json` bundle
/// (`{"data": [...], "layout": {...}}`).
pub fn render_plotly(figure: &serde_json::Value, opts: &RenderOptions) -> Option<Rendered> {
    guard(|| plotly::render(figure, &opts.sanitized()))
}

/// Renders a Vega-Lite spec (`application/vnd.vegalite.v*+json`, or one
/// returned by [`vegalite_spec_from_html`]).
pub fn render_vegalite(spec: &serde_json::Value, opts: &RenderOptions) -> Option<Rendered> {
    guard(|| vegalite::render(spec, &opts.sanitized()))
}

/// Finds the Vega-Lite spec Altair embeds in its `text/html` output
/// (`vegaEmbed(el, spec, opt)` / `(function(spec, embedOpt){...})({...}, {...})`).
pub fn vegalite_spec_from_html(html: &str) -> Option<serde_json::Value> {
    guard(|| vegalite::html::extract(html))
}

/// Renders Mermaid source (flowchart, sequence, pie, state, class or ER
/// diagram) at its natural size, scaled down to fit
/// [`RenderOptions::width`]; `RenderOptions::height` is ignored. `None` for
/// other diagram types and for syntax it can't draw faithfully.
pub fn render_mermaid(source: &str, opts: &RenderOptions) -> Option<Rendered> {
    guard(|| mermaid::render(source, &opts.sanitized()))
}

impl RenderOptions {
    fn sanitized(&self) -> RenderOptions {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        RenderOptions {
            width: fix(self.width, 120., 8000., 640.),
            height: self.height.map(|h| fix(h, 80., 8000., 400.)),
            dark: self.dark,
            blend: self.blend,
            font_family: if self.font_family.trim().is_empty() {
                "sans-serif".into()
            } else {
                self.font_family.clone()
            },
        }
    }
}

/// Last line of defence: the renderers are written not to panic, but a chart
/// is never worth taking the app down for.
fn guard<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    util::clear_typed_cache();
    let r = catch_unwind(AssertUnwindSafe(f)).ok().flatten();
    util::clear_typed_cache();
    r
}
