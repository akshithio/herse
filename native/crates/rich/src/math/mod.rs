//! LaTeX math -> self-contained SVG.
//!
//! [`render_svg`] parses LaTeX math with a vendored, patched copy of the
//! pure-Rust [latex-rust](https://github.com/jscarr64/latex-rust) engine
//! (TeX Appendix G layout on the OpenType MATH table of the embedded STIX
//! Two Math font) and writes an SVG whose glyphs are `<path>`s: no font is
//! needed at paint time, everything is one color (`currentColor` by default,
//! so GPUI's `svg()` alpha-mask tinting just works), and the result carries
//! its size and baseline in em so it can sit inline with text.
//!
//! It is a pure function of its input (no global mutable state except
//! lazily-built read-only tables), `Send + Sync`, and cheap enough to call
//! per formula from `cx.background_spawn`; cache the [`MathSvg`] per source
//! string. Call [`warm_up`] once at startup on a background thread so the
//! first formula on screen doesn't pay for font loading.
//!
//! Before the engine sees the input, a front end applies what notebooks
//! expect from MathJax: `$`/`$$`/`\(`/`\[` delimiters, user macros
//! ([`Macros`], `\newcommand`, `\def`, `\DeclareMathOperator`, also carried
//! across cells), MathJax/amsmath aliases (`\bm`, `\lbrace`, `\pmod`,
//! `\coloneqq`, `\argmax`, …) and environment rewrites (`align*`, `alignat`,
//! `eqnarray`, `gathered`, `dcases`, `smallmatrix`, …). Equations are not
//! numbered unless asked ([`MathOptions::number_equations`]), matching
//! JupyterLab's MathJax configuration; `\tag{}` always shows.
//!
//! Unsupported or malformed input returns `Err` (never a partial or fake
//! render, never a panic); show the source in monospace in that case.
//! [`remove_math`] finds the math in a Markdown cell exactly like
//! JupyterLab, including `\begin{align}` blocks written without `$$`.

mod front;
mod markdown;
mod svg;
// Vendored code keeps its upstream formatting.
#[rustfmt::skip]
#[allow(clippy::all)]
pub(crate) mod tex;

use std::fmt;
use std::sync::OnceLock;

pub use front::Macros;
pub use markdown::{remove_math, replace_math};

/// A rendered formula.
#[derive(Clone, Debug, PartialEq)]
pub struct MathSvg {
    /// Complete SVG document (`width`/`height` in `em`, glyphs as paths).
    pub svg: String,
    /// Width of the SVG in em (ink and TeX box, plus a small padding).
    pub width_em: f32,
    /// Height of the SVG in em.
    pub height_em: f32,
    /// Distance from the bottom of the SVG up to the math baseline, in em:
    /// for inline placement put the SVG's bottom `baseline_em` below the
    /// text baseline.
    pub baseline_em: f32,
    /// TeX box width in em (what the formula advances by in TeX).
    pub box_width_em: f32,
    /// Offset of the TeX box's left edge from the SVG's left edge, in em.
    pub box_left_em: f32,
}

impl MathSvg {
    /// The SVG with `width`/`height` in pixels instead of em, for renderers
    /// that rasterize at the intrinsic size. GPUI's `img()` rasterizes SVG at
    /// intrinsic size × 2 (usvg resolves `1em` as 12px), so pass
    /// `font_px * window_scale / 2.0` for a pixel-exact raster. (GPUI's
    /// `svg()` element rasterizes at the element size and needs nothing.)
    pub fn svg_px(&self, px_per_em: f32) -> String {
        let w = self.width_em * px_per_em;
        let h = self.height_em * px_per_em;
        let Some(start) = self.svg.find(" width=\"") else {
            return self.svg.clone();
        };
        let Some(end) = self.svg[start..].find(" viewBox=").map(|e| start + e) else {
            return self.svg.clone();
        };
        format!(
            "{} width=\"{w:.2}\" height=\"{h:.2}\"{}",
            &self.svg[..start],
            &self.svg[end..]
        )
    }
}

/// Why a formula could not be rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MathError {
    /// Malformed LaTeX (unbalanced braces, missing arguments, …).
    Parse(String),
    /// Valid LaTeX this renderer does not implement (unknown command or
    /// environment, mhchem, TikZ, …).
    Unsupported(String),
    /// Input over [`MAX_INPUT_BYTES`].
    TooLarge,
    /// Macro expansion did not terminate in reasonable time.
    TooComplex,
    /// Renderer bug (reported instead of panicking).
    Internal(String),
}

impl fmt::Display for MathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MathError::Parse(m) => write!(f, "LaTeX error: {m}"),
            MathError::Unsupported(m) => write!(f, "unsupported: {m}"),
            MathError::TooLarge => write!(f, "formula too large"),
            MathError::TooComplex => write!(f, "macro expansion too deep"),
            MathError::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for MathError {}

pub type Result<T, E = MathError> = std::result::Result<T, E>;

/// Fill used for glyphs and rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fill {
    /// `currentColor`: the SVG takes the surrounding text color (and GPUI's
    /// `svg()` element ignores fills anyway and tints the mask).
    #[default]
    CurrentColor,
    Rgb(u8, u8, u8),
}

/// Options for [`render_svg_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MathOptions<'a> {
    /// Display style (`$$`, block math) instead of text style (`$`).
    /// Delimiters in the input override it, and display environments
    /// (`align`, `equation`, `gather`, …) always render as display.
    pub display: bool,
    /// Macros defined by earlier formulas (see [`Macros`]).
    pub macros: Option<&'a Macros>,
    pub fill: Fill,
    /// Keep `\color`/`\textcolor`/`\colorbox` colors. Off by default: the
    /// output is one color, and a `\colorbox` background would cover the
    /// text in an alpha-mask renderer.
    pub colors: bool,
    /// Number `equation`/`align`/`gather` rows like LaTeX. Off by default,
    /// as in JupyterLab (MathJax `tags: 'none'`); `\tag{}` always shows.
    pub number_equations: bool,
}

/// Inputs longer than this (after delimiter stripping) are refused; layout
/// cost grows faster than linearly on very long single-row formulas.
pub const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_EXPANDED_BYTES: usize = 32 * 1024;

/// Render LaTeX math to SVG. `display` picks display (block) or text
/// (inline) style; `$…$` / `$$…$$` / `\(…\)` / `\[…\]` delimiters in
/// `latex` are stripped and take precedence.
pub fn render_svg(latex: &str, display: bool) -> Result<MathSvg> {
    render_svg_with(
        latex,
        &MathOptions {
            display,
            ..Default::default()
        },
    )
}

/// [`render_svg`] with all options.
pub fn render_svg_with(latex: &str, opts: &MathOptions<'_>) -> Result<MathSvg> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_inner(latex, opts))) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            Err(MathError::Internal(msg))
        }
    }
}

/// Load the font and build the lookup tables (a few ms). Optional; call it
/// once on a background thread at startup.
pub fn warm_up() {
    if let Ok(font) = font() {
        let _ = render_inner(
            r"\sum_{i=1}^n \frac{\sqrt{x_i}}{\left(\alpha + \mathbb{R}\right)}",
            &MathOptions::default(),
        );
        let _ = font;
    }
}

fn font() -> Result<&'static tex::MathFont> {
    static FONT: OnceLock<Option<tex::MathFont>> = OnceLock::new();
    FONT.get_or_init(|| tex::MathFont::stix_two_math().ok())
        .as_ref()
        .ok_or_else(|| MathError::Internal("embedded math font failed to load".into()))
}

/// Strip one pair of math delimiters. Returns the inner text and the style
/// they imply (`Some(true)` display, `Some(false)` inline, `None` if there
/// were none).
pub fn strip_delimiters(latex: &str) -> (&str, Option<bool>) {
    let s = latex.trim();
    let pairs: [(&str, &str, bool); 6] = [
        ("$$", "$$", true),
        ("\\[", "\\]", true),
        ("\\\\[", "\\\\]", true),
        ("\\(", "\\)", false),
        ("\\\\(", "\\\\)", false),
        ("$", "$", false),
    ];
    for (open, close, display) in pairs {
        if s.len() >= open.len() + close.len() && s.starts_with(open) && s.ends_with(close) {
            let inner = &s[open.len()..s.len() - close.len()];
            // `$a$ and $b$` is not one formula.
            if open == "$" && inner.contains('$') && !inner.contains("\\$") {
                break;
            }
            return (inner, Some(display));
        }
    }
    (s, None)
}

fn is_display_env(s: &str) -> bool {
    let Some(rest) = s.trim_start().strip_prefix("\\begin{") else {
        return false;
    };
    let name = rest.split('}').next().unwrap_or("");
    matches!(
        name.trim_end_matches('*'),
        "align"
            | "equation"
            | "gather"
            | "multline"
            | "eqnarray"
            | "alignat"
            | "flalign"
            | "xalignat"
    )
}

fn render_inner(latex: &str, opts: &MathOptions<'_>) -> Result<MathSvg> {
    use tex::*;
    let (inner, delim_display) = strip_delimiters(latex);
    let inner = inner.trim();
    if inner.len() > MAX_INPUT_BYTES {
        return Err(MathError::TooLarge);
    }
    let display = delim_display.unwrap_or(opts.display) || is_display_env(inner);
    let font = font()?;
    // MathJax breaks lines at a top-level `\\` (Kalman-filter notebooks
    // rely on it); stack the lines like `gathered` (or `aligned` with `&`).
    let wrapped;
    let inner = match top_level_breaks(inner) {
        (true, false) => {
            wrapped = format!("\\begin{{gathered}}{inner}\\end{{gathered}}");
            &wrapped[..]
        }
        (true, true) => {
            wrapped = format!("\\begin{{aligned}}{inner}\\end{{aligned}}");
            &wrapped[..]
        }
        _ => inner,
    };
    let expanded = if inner.contains('\\') || inner.contains('%') {
        front::expand(inner, opts.macros)?
    } else {
        inner.to_string()
    };
    if expanded.len() > MAX_EXPANDED_BYTES {
        return Err(MathError::TooLarge);
    }
    if expanded.trim().is_empty() {
        // e.g. a formula that only defines macros: nothing to draw.
        return svg::write(&MathBox::empty(), font, opts.fill, opts.colors);
    }
    // A top-level `\tag` belongs to the display, as in MathJax.
    let expanded =
        if display && expanded.contains("\\tag") && !expanded.trim_start().starts_with("\\begin") {
            format!("\\begin{{equation}}{expanded}\\end{{equation}}")
        } else {
            expanded
        };
    let (ast, _colors) =
        parse_with_options(&expanded, &ParseOptions::default()).map_err(map_parse)?;
    let style = if display {
        MathStyle::Display
    } else {
        MathStyle::Text
    };
    let mut numbering = NumberingState::new(NumberingConfig {
        auto: opts.number_equations,
        ..Default::default()
    });
    let tree =
        layout_with_numbering_and_em_size_pt(&ast, font, style, &mut numbering, &Dim::from_i64(10))
            .map_err(map_err)?;
    svg::write(&tree, font, opts.fill, opts.colors)
}

/// (`\\` at brace/environment depth 0, `&` at depth 0).
fn top_level_breaks(s: &str) -> (bool, bool) {
    let b = s.as_bytes();
    let (mut depth, mut env) = (0i32, 0i32);
    let (mut br, mut amp) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let rest = &s[i + 1..];
                if rest.starts_with('\\') {
                    br |= depth == 0 && env == 0;
                    i += 2;
                    continue;
                }
                let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                match &rest[..name_len] {
                    "begin" => env += 1,
                    "end" => env -= 1,
                    "left" => depth += 1,
                    "right" => depth -= 1,
                    _ => {}
                }
                // Skip the command name, or the escaped char (`\{`, `\&`).
                i += 1 + name_len.max(1);
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'&' => amp |= depth == 0 && env == 0,
            _ => {}
        }
        i += 1;
    }
    (br, amp)
}

fn map_parse(e: tex::ParseError) -> MathError {
    use tex::ParseError as P;
    match e {
        P::Unsupported(m) => MathError::Unsupported(m),
        P::Unknown(m) => MathError::Unsupported(format!("unknown command {m}")),
        other => MathError::Parse(other.to_string()),
    }
}

fn map_err(e: tex::Error) -> MathError {
    match e {
        tex::Error::Parse(p) => map_parse(p),
        tex::Error::Unsupported { what } => MathError::Unsupported(what),
        tex::Error::Malformed { what } => MathError::Parse(what),
        tex::Error::Font(f) => MathError::Unsupported(f.to_string()),
        tex::Error::InvalidOption { what } => MathError::Internal(what),
    }
}

#[cfg(test)]
mod tests;
