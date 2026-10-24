//! LaTeX in Markdown, rendered natively: formulas are typeset to SVG by
//! `herse_rich::math` while the Markdown parses on a background thread,
//! then drawn as images sized to the surrounding text and sitting on its
//! baseline. A formula that fails to typeset stays as its source text.

use std::{cell::RefCell, collections::HashMap, sync::Arc, sync::OnceLock};

use gpui_kit::component::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, markdown_ast as mdast,
};
use gpui_kit::{
    App, Hsla, Image, ImageFormat, IntoElement, ObjectFit, ParentElement as _, Pixels, Styled as _,
    StyledImage as _, Window, div, img, px,
};
use herse_rich::math::MathSvg;

/// Markdown extensions with both math plugins, built once: reusing the same
/// configuration lets text views keep their parsed documents.
pub fn extensions() -> MarkdownExtensions {
    static EXT: OnceLock<MarkdownExtensions> = OnceLock::new();
    EXT.get_or_init(|| {
        MarkdownExtensions::default()
            .plugin(InlineMath)
            .plugin(DisplayMath)
            .parser_revision(1)
    })
    .clone()
}

/// Rewrite math the way JupyterLab finds it into what the Markdown parser
/// understands: `\(…\)` → `$…$`, `\[…\]` and bare `\begin{align}…` → a `$$`
/// block. Code spans and fences are left alone.
pub fn prepare(markdown: &str) -> String {
    if !markdown.contains('$') && !markdown.contains('\\') {
        return markdown.to_string();
    }
    let markdown = single_backslash_delimiters(markdown);
    let (masked, pieces) = herse_rich::math::remove_math(&markdown);
    if pieces.is_empty() {
        return markdown.into_owned();
    }
    herse_rich::math::replace_math(&masked, |n| {
        let Some(piece) = pieces.get(n) else { return String::new() };
        let (inner, display) = herse_rich::math::strip_delimiters(piece);
        match display {
            // Pandoc's rule: inline math neither starts nor ends with a
            // space, so "$5 today and $10 tomorrow" stays prose.
            Some(false) if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) => {
                piece.replace('$', "\\$")
            }
            Some(false) => format!("${inner}$"),
            // Display math becomes its own block, as it renders.
            Some(true) => format!("\n$$\n{}\n$$\n", inner.trim()),
            // `\begin{env}…\end{env}` with no delimiters.
            None => format!("\n$$\n{}\n$$\n", piece.trim()),
        }
    })
}

/// `\(…\)` and `\[…\]` written with one backslash, as in LaTeX. JupyterLab
/// only reads them doubled (Markdown eats one backslash) and shows the rest
/// as "(\LaTeX)", which no author meant: outside code, make them dollar math.
fn single_backslash_delimiters(markdown: &str) -> std::borrow::Cow<'_, str> {
    if !markdown.contains("\\(") && !markdown.contains("\\[") {
        return markdown.into();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    for line in markdown.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if in_fence || t.starts_with("```") || t.starts_with("~~~") {
            out.push_str(line);
            continue;
        }
        out.push_str(&line_delimiters(line));
    }
    out.into()
}

/// One line: `\(x\)` → `$x$`, `\[x\]` → `$$x$$`, skipping code spans and
/// doubled backslashes (`\\(` is JupyterLab's own spelling, handled later).
fn line_delimiters(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        match b[i] {
            b'`' => {
                // Skip a code span: up to the same run of backticks.
                let run = b[i..].iter().take_while(|&&c| c == b'`').count();
                let fence = &line[i..i + run];
                i += run;
                if let Some(end) = line[i..].find(fence) {
                    i += end + run;
                }
            }
            b'\\' if i + 1 < b.len() => {
                let doubled = i > 0 && b[i - 1] == b'\\';
                let (open, close, dollars) = match b[i + 1] {
                    b'(' => ("\\(", "\\)", "$"),
                    b'[' => ("\\[", "\\]", "$$"),
                    _ => ("", "", ""),
                };
                if !open.is_empty() && !doubled
                    && let Some(end) = line[i + 2..].find(close)
                {
                    let inner = line[i + 2..i + 2 + end].trim();
                    out.push_str(&line[last..i]);
                    out.push_str(dollars);
                    out.push_str(inner);
                    out.push_str(dollars);
                    i += 2 + end + close.len();
                    last = i;
                    continue;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out.push_str(&line[last..]);
    out
}

struct MathData {
    math: MathSvg,
    /// Content hash of the SVG: the raster cache key.
    hash: u64,
}

fn typeset(latex: &str, display: bool) -> Option<MarkdownNode> {
    // The logos MathJax knows and the typesetter doesn't.
    let source = if latex.contains("TeX") {
        std::borrow::Cow::Owned(latex.replace("\\LaTeX", "\\mathrm{LaTeX}").replace("\\TeX", "\\mathrm{TeX}"))
    } else {
        std::borrow::Cow::Borrowed(latex)
    };
    let math = herse_rich::math::render_svg(&source, display).ok()?;
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        math.svg.hash(&mut h);
        h.finish()
    };
    let name = if display { DISPLAY } else { INLINE };
    Some(MarkdownNode::new(name, MathData { math, hash }).text(latex.to_string()))
}

const INLINE: &str = "herse-math-inline";
const DISPLAY: &str = "herse-math-display";

struct InlineMath;

impl MarkdownPlugin for InlineMath {
    fn name(&self) -> &str {
        INLINE
    }

    fn parse(&self, node: &mdast::Node, _cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let mdast::Node::InlineMath(m) = node else { return None };
        // Unclaimed (failed) math falls back to the literal source, so prose
        // like "$5 and $10" is unharmed.
        typeset(&m.value, false)
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let data = node.data::<MathData>()?;
        let font = context.font_size();
        let color = context.text_style().color;
        let image = image_for(data, font, color, window.scale_factor());
        let m = &data.math;
        let _ = cx;
        let el = img(image)
            .w(font * m.width_em)
            .h(font * m.height_em)
            .flex_none();
        Some(InlineElement::new(el).with_baseline(font * (m.height_em - m.baseline_em)))
    }
}

struct DisplayMath;

impl MarkdownPlugin for DisplayMath {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        DISPLAY
    }

    fn parse(&self, node: &mdast::Node, _cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let mdast::Node::Math(m) = node else { return None };
        typeset(&m.value, true)
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Some(data) = node.data::<MathData>() else {
            return div().into_any_element();
        };
        let style = window.text_style();
        let font = style.font_size.to_pixels(window.rem_size()) * 1.1;
        let m = &data.math;
        let image = image_for(data, font, style.color, window.scale_factor());
        div()
            .w_full()
            .flex()
            .justify_center()
            .py(px(6.))
            .child(
                img(image)
                    .w(font * m.width_em)
                    .h(font * m.height_em)
                    .max_w_full()
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element()
    }
}

thread_local! {
    static IMAGES: RefCell<HashMap<(u64, u32, u32), Arc<Image>>> = RefCell::new(HashMap::new());
}

/// One raster per (formula, colour, size), reused every frame.
fn image_for(data: &MathData, font: Pixels, color: Hsla, scale: f32) -> Arc<Image> {
    let math = &data.math;
    let rgb = color.to_rgb();
    let hex = ((rgb.r * 255.).round() as u32) << 16 | ((rgb.g * 255.).round() as u32) << 8 | (rgb.b * 255.).round() as u32;
    let size_key = (font.as_f32() * scale * 4.).round() as u32;
    let key = (data.hash, hex, size_key);
    IMAGES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(image) = cache.get(&key) {
            return image.clone();
        }
        if cache.len() > 8192 {
            cache.clear();
        }
        // GPUI rasterizes an SVG at its intrinsic size times two; size the
        // document so that lands on device pixels.
        let svg = math
            .svg_px(font.as_f32() * scale / 2.)
            .replace("currentColor", &format!("#{hex:06x}"));
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()));
        cache.insert(key, image.clone());
        image
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_backslash_delimiters_become_math() {
        assert_eq!(line_delimiters(r"Also \(\LaTeX\) here"), r"Also $\LaTeX$ here");
        assert_eq!(line_delimiters(r"Bracket: \[ x^2 + y^2 \]"), r"Bracket: $$x^2 + y^2$$");
        // Code spans and JupyterLab's doubled spelling are left alone.
        assert_eq!(line_delimiters(r"`\(x\)` code"), r"`\(x\)` code");
        assert_eq!(line_delimiters(r"\\(x\\)"), r"\\(x\\)");
    }

    #[test]
    fn prices_are_not_math() {
        let out = prepare("it costs $5 today and $10 tomorrow.");
        assert!(!out.contains("$5 today and $1") || out.contains("\\$5"), "{out}");
        assert!(prepare("so $E = mc^2$ holds").contains("$E = mc^2$"));
    }
}
