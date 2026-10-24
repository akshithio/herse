//! `herse-html`: a notebook as one self-contained HTML page, in Herse's look
//! (a page of warm paper and ink, prose in Charter, code and output in SF
//! Mono, light and dark following the system). It needs no Python, no
//! JavaScript and no network: images are inlined, math is drawn as SVG by
//! `herse-rich`, Plotly and Vega-Lite charts by `herse-plot`, and DataFrames
//! keep their HTML tables. Quick Look previews and Export as HTML both use it.
//!
//! ```
//! let nb = herse_nb::parse(br##"{"cells": [{"cell_type": "markdown", "metadata": {},
//!     "source": "# Hi $x^2$"}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"##).unwrap();
//! let page = herse_html::render(&nb, &herse_html::Options::default());
//! assert!(page.contains("<h1>Hi <span class=\"math\""));
//! ```

mod highlight;

use std::fmt::Write as _;

use herse_nb::{Cell, CellType, MimeBundle, Notebook, Output, StreamName, Value};
use herse_plot::{RenderOptions, Rendered};
use herse_rich::{ansi, math, stream};

/// How to render.
#[derive(Clone, Debug)]
pub struct Options {
    /// The page's title (the notebook's file name).
    pub title: String,
    /// Text outputs longer than this many lines keep their head and tail.
    pub max_output_lines: usize,
    /// Show code cells' sources; off makes a report of prose and results.
    pub inputs: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { title: "Notebook".into(), max_output_lines: 400, inputs: true }
    }
}

/// The notebook as a complete HTML document.
pub fn render(nb: &Notebook, opts: &Options) -> String {
    let bytes: usize = nb.cells.iter().map(|c| c.source.len() + c.outputs.len() * 256).sum();
    let mut out = String::with_capacity(bytes + 16 * 1024);
    out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    out.push_str("<meta name=\"color-scheme\" content=\"light dark\">");
    let _ = write!(out, "<title>{}</title><style>", escape(&opts.title));
    out.push_str(CSS);
    out.push_str(&ansi_css());
    out.push_str("</style></head><body><main>\n");
    let python = language(nb).is_none_or(|l| l.eq_ignore_ascii_case("python"));
    let mut macros = math::Macros::new();
    for cell in &nb.cells {
        render_cell(cell, python, &mut macros, opts, &mut out);
    }
    out.push_str("</main></body></html>\n");
    out
}

fn language(nb: &Notebook) -> Option<&str> {
    let meta = &nb.metadata;
    meta.get("kernelspec")
        .and_then(|k| k.get("language"))
        .or_else(|| meta.get("language_info").and_then(|l| l.get("name")))
        .and_then(Value::as_str)
}

/// The cell's hide flags: JupyterLab's and the classic notebook's folds,
/// and Jupyter Book's tags.
fn hidden(cell: &Cell) -> (bool, bool) {
    let jupyter = cell.metadata.get("jupyter");
    let flag = |key: &str| jupyter.and_then(|j| j.get(key)).and_then(Value::as_bool).unwrap_or(false);
    let tags: Vec<&str> = cell
        .metadata
        .get("tags")
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let tagged = |names: &[&str]| tags.iter().any(|t| names.contains(t));
    let all = tagged(&["remove-cell"]);
    let input = all || flag("source_hidden") || tagged(&["hide-input", "remove-input"]);
    let output = all
        || flag("outputs_hidden")
        || cell.metadata.get("collapsed").and_then(Value::as_bool).unwrap_or(false)
        || tagged(&["hide-output", "remove-output"]);
    (input, output)
}

fn render_cell(cell: &Cell, python: bool, macros: &mut math::Macros, opts: &Options, out: &mut String) {
    let (hide_input, hide_output) = hidden(cell);
    match cell.cell_type {
        CellType::Markdown => {
            if hide_input {
                return;
            }
            out.push_str("<section class=\"cell md\">");
            out.push_str(&markdown_html(&cell.source, cell.attachments.as_ref(), macros));
            out.push_str("</section>\n");
        }
        CellType::Raw => {
            if !hide_input && !cell.source.is_empty() {
                let _ = writeln!(out, "<section class=\"cell raw\"><pre>{}</pre></section>", escape(&cell.source));
            }
        }
        CellType::Code => {
            let show_input = opts.inputs && !hide_input;
            let show_output = !hide_output && !cell.outputs.is_empty();
            if !show_input && !show_output {
                return;
            }
            out.push_str("<section class=\"cell code\">");
            if show_input {
                let prompt = match cell.execution_count {
                    Some(n) => format!("[{n}]"),
                    None => "[ ]".into(),
                };
                let source = if python { highlight::python(&cell.source) } else { escape(&cell.source) };
                let _ = write!(
                    out,
                    "<div class=\"in\"><span class=\"prompt\">{prompt}</span><pre class=\"src\"><code>{}</code></pre></div>",
                    if source.is_empty() { " ".into() } else { source }
                );
            }
            if show_output {
                out.push_str("<div class=\"out\">");
                for output in &cell.outputs {
                    output_html(output, macros, opts, out);
                }
                out.push_str("</div>");
            }
            out.push_str("</section>\n");
        }
    }
}

fn output_html(output: &Output, macros: &mut math::Macros, opts: &Options, out: &mut String) {
    match output {
        Output::Stream { name, text } => {
            let text = stream::normalize(text);
            let class = if *name == StreamName::Stderr { "stream stderr" } else { "stream" };
            let _ = write!(out, "<pre class=\"{class}\">{}</pre>", ansi_html(&fold(&text, opts.max_output_lines)));
        }
        Output::Error { ename, evalue, traceback } => {
            let text = if traceback.is_empty() { format!("{ename}: {evalue}") } else { traceback.join("\n") };
            let _ = write!(out, "<pre class=\"error\">{}</pre>", ansi_html(&fold(&text, opts.max_output_lines)));
        }
        Output::DisplayData { data, metadata, .. } | Output::ExecuteResult { data, metadata, .. } => {
            mime_html(data, metadata, macros, opts, out);
        }
        Output::Unknown(_) => {}
    }
}

/// The richest representation of a mime bundle this page can show.
fn mime_html(data: &MimeBundle, metadata: &herse_nb::Map, macros: &mut math::Macros, opts: &Options, out: &mut String) {
    if let Some(fig) = data.get("application/vnd.plotly.v1+json").and_then(|d| d.as_json())
        && let Some(chart) = chart_html(|o| herse_plot::render_plotly(fig, o))
    {
        out.push_str(&chart);
        return;
    }
    if let Some((_, spec)) = data.0.iter().find(|(m, _)| m.starts_with("application/vnd.vegalite.v"))
        && let Some(spec) = spec.as_json()
        && let Some(chart) = chart_html(|o| herse_plot::render_vegalite(spec, o))
    {
        out.push_str(&chart);
        return;
    }
    if let Some(svg) = data.text("image/svg+xml") {
        let _ = write!(out, "<div class=\"img svg\">{}</div>", clean_html(svg));
        return;
    }
    for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
        if let Some(b64) = data.text(mime) {
            let b64: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            // A figure saved at 2x says its display size in the metadata.
            let size = metadata.get(mime);
            let dim = |key: &str| size.and_then(|s| s.get(key)).and_then(Value::as_f64);
            let mut style = String::new();
            if let Some(w) = dim("width") {
                let _ = write!(style, "width:{w}px;");
            }
            if let Some(h) = dim("height").filter(|_| dim("width").is_none()) {
                let _ = write!(style, "height:{h}px;");
            }
            let _ = write!(out, "<div class=\"img\"><img src=\"data:{mime};base64,{b64}\" style=\"{style}\" alt=\"\"></div>");
            return;
        }
    }
    if let Some(html) = data.text("text/html") {
        if let Some(spec) = herse_plot::vegalite_spec_from_html(html)
            && let Some(chart) = chart_html(|o| herse_plot::render_vegalite(&spec, o))
        {
            out.push_str(&chart);
            return;
        }
        let clean = clean_html(html);
        // Outputs that are only scripts (an interactive widget's loader)
        // leave nothing: show the plain text instead.
        if has_visible_content(&clean) || !data.contains("text/plain") {
            let class = if clean.contains("dataframe") || clean.contains("<table") { "html df" } else { "html" };
            let _ = write!(out, "<div class=\"{class}\">{clean}</div>");
            return;
        }
    }
    if let Some(md) = data.text("text/markdown") {
        let _ = write!(out, "<div class=\"md\">{}</div>", markdown_html(md, None, macros));
        return;
    }
    if let Some(tex) = data.text("text/latex") {
        let tex = tex.trim();
        if tex.starts_with('$') || tex.starts_with("\\[") || tex.starts_with("\\(") || tex.starts_with("\\begin") {
            out.push_str(&math_html(tex, macros, true));
            return;
        }
    }
    if let Some(json) = data.get("application/json").and_then(|d| d.as_json()) {
        let pretty = serde_json::to_string_pretty(json).unwrap_or_default();
        let _ = write!(out, "<pre class=\"stream\">{}</pre>", escape(&fold(&pretty, opts.max_output_lines)));
        return;
    }
    if let Some(text) = data.text("text/plain") {
        let _ = write!(out, "<pre class=\"result\">{}</pre>", ansi_html(&fold(text, opts.max_output_lines)));
    }
}

/// A chart drawn for both appearances; the page shows the one that fits.
fn chart_html(render: impl Fn(&RenderOptions) -> Option<Rendered>) -> Option<String> {
    let opts = |dark| RenderOptions {
        width: 780.,
        dark,
        font_family: "-apple-system, 'SF Pro Text', Helvetica, Arial, sans-serif".into(),
        blend: true,
        ..Default::default()
    };
    let light = render(&opts(false))?;
    Some(match render(&opts(true)) {
        Some(dark) => format!(
            "<div class=\"chart\"><div class=\"for-light\">{}</div><div class=\"for-dark\">{}</div></div>",
            light.svg, dark.svg
        ),
        None => format!("<div class=\"chart\">{}</div>", light.svg),
    })
}

/// Markdown (GitHub's dialect, with raw HTML) to HTML, with math as SVG and
/// `attachment:` images inlined.
pub fn markdown_html(source: &str, attachments: Option<&Value>, macros: &mut math::Macros) -> String {
    let source = inline_attachments(source, attachments);
    let (text, maths) = math::remove_math(&source);
    let options = markdown::Options {
        parse: markdown::ParseOptions::gfm(),
        compile: markdown::CompileOptions {
            allow_dangerous_html: true,
            allow_dangerous_protocol: false,
            gfm_tagfilter: true,
            ..markdown::CompileOptions::gfm()
        },
    };
    let html = markdown::to_html_with_options(&text, &options)
        .unwrap_or_else(|_| format!("<pre>{}</pre>", escape(&source)));
    let html = clean_html(&html);
    if maths.is_empty() {
        return html;
    }
    math::replace_math(&html, |i| maths.get(i).map(|m| math_html(m, macros, false)).unwrap_or_default())
}

fn math_html(piece: &str, macros: &mut math::Macros, block: bool) -> String {
    macros.scan(piece);
    let (_, delimited) = math::strip_delimiters(piece);
    let display = delimited.unwrap_or(block) || piece.trim_start().starts_with("\\begin");
    let opts = math::MathOptions { display, macros: Some(macros), ..Default::default() };
    match math::render_svg_with(piece, &opts) {
        Ok(svg) if display => format!("<div class=\"math-display\">{}</div>", svg.svg),
        Ok(svg) => format!("<span class=\"math\" style=\"vertical-align:-{:.3}em\">{}</span>", svg.baseline_em, svg.svg),
        Err(_) => format!("<code class=\"math-error\">{}</code>", escape(piece)),
    }
}

fn inline_attachments(source: &str, attachments: Option<&Value>) -> String {
    let Some(map) = attachments.and_then(Value::as_object) else { return source.to_string() };
    let mut out = source.to_string();
    for (name, bundle) in map {
        let Some((mime, data)) = bundle.as_object().and_then(|b| b.iter().find(|(m, _)| m.starts_with("image/"))) else {
            continue;
        };
        let data: String = match data {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        let data: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        out = out.replace(&format!("attachment:{name}"), &format!("data:{mime};base64,{data}"));
    }
    out
}

/// Text past `max` lines keeps its head and tail, with a note between.
fn fold(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let lines = text.lines().count();
    if lines <= max || max == 0 {
        return text.into();
    }
    let head = max * 3 / 4;
    let tail = max - head;
    let mut out: Vec<&str> = text.lines().take(head).collect();
    let note = format!("… {} more lines …", lines - head - tail);
    out.push(&note);
    out.extend(text.lines().skip(lines - tail));
    out.join("\n").into()
}

/// Terminal colors as HTML: the 16 theme colors as classes (so dark mode
/// can recolor them), anything else inline.
fn ansi_html(text: &str) -> String {
    if !ansi::has_escapes(text) {
        return escape(text);
    }
    let styled = ansi::parse(text);
    let mut out = String::with_capacity(styled.text.len() + styled.spans.len() * 32);
    for (range, style) in styled.runs() {
        let piece = escape(&styled.text[range]);
        if style.is_default() {
            out.push_str(&piece);
            continue;
        }
        let (fg, bg) = if style.inverse { (style.bg, style.fg) } else { (style.fg, style.bg) };
        let mut classes: Vec<String> = Vec::new();
        let mut css = String::new();
        let mut color = |c: ansi::AnsiColor, prefix: &str, prop: &str, bright: bool| match c {
            ansi::AnsiColor::Named(n) | ansi::AnsiColor::Indexed(n) if n < 16 => {
                let n = if bright && n < 8 { n + 8 } else { n };
                classes.push(format!("{prefix}{n}"));
            }
            other => {
                let (r, g, b) = ansi::Palette::LIGHT.rgb(other);
                let _ = write!(css, "{prop}:rgb({r},{g},{b});");
            }
        };
        if let Some(c) = fg {
            color(c, "f", "color", style.bold);
        }
        if let Some(c) = bg {
            color(c, "g", "background", false);
        }
        for (on, class) in [(style.bold, "ab"), (style.italic, "ai"), (style.underline, "au"), (style.dim, "ad"), (style.strikethrough, "ax")] {
            if on {
                classes.push(class.into());
            }
        }
        let _ = write!(out, "<span class=\"{}\"", classes.join(" "));
        if !css.is_empty() {
            let _ = write!(out, " style=\"{css}\"");
        }
        let _ = write!(out, ">{piece}</span>");
    }
    out
}

fn ansi_css() -> String {
    let mut css = String::new();
    let rules = |css: &mut String, palette: &ansi::Palette| {
        for (i, (r, g, b)) in palette.colors.iter().enumerate() {
            let _ = write!(css, ".f{i}{{color:rgb({r},{g},{b})}}.g{i}{{background:rgb({r},{g},{b})}}");
        }
    };
    rules(&mut css, &ansi::Palette::LIGHT);
    css.push_str("@media (prefers-color-scheme: dark){");
    rules(&mut css, &ansi::Palette::DARK);
    css.push('}');
    css
}

/// Notebook HTML without what would run or reach out: scripts, frames,
/// embeds, and `<link>`/`<meta>`/`<base>` tags. (Quick Look runs no
/// JavaScript anyway; an exported page should behave the same.)
pub fn clean_html(html: &str) -> String {
    if !html.contains('<') {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(off) = lower[i..].find('<') {
        let at = i + off;
        out.push_str(&html[i..at]);
        let name: String = lower[at + 1..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        match name.as_str() {
            "script" | "iframe" | "object" | "noscript" | "template" => {
                let close = format!("</{name}");
                i = match lower[at..].find(&close) {
                    Some(c) => lower[at + c..].find('>').map_or(html.len(), |e| at + c + e + 1),
                    None => html.len(),
                };
            }
            "embed" | "link" | "meta" | "base" => {
                i = lower[at..].find('>').map_or(html.len(), |e| at + e + 1);
            }
            _ => {
                out.push('<');
                i = at + 1;
            }
        }
    }
    out.push_str(&html[i..]);
    out
}

/// Whether HTML shows anything: text outside tags, or an element that draws.
fn has_visible_content(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if ["<img", "<svg", "<table", "<canvas", "<video", "<pre"].iter().any(|t| lower.contains(t)) {
        return true;
    }
    let mut in_tag = false;
    let mut in_style = false;
    let mut i = 0;
    let b = lower.as_bytes();
    while i < b.len() {
        match b[i] {
            b'<' => {
                in_tag = true;
                if lower[i..].starts_with("<style") {
                    in_style = true;
                } else if lower[i..].starts_with("</style") {
                    in_style = false;
                }
            }
            b'>' => in_tag = false,
            c if !in_tag && !in_style && !c.is_ascii_whitespace() => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Text for HTML: `& < > " '` escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Herse's page: the palette is the app's theme (`herse-theme.json`).
const CSS: &str = r#"
:root{--paper:#fff;--ink:#1c1a17;--muted:#78736c;--rule:#e8e6e2;--code:#f7f6f4;--accent:#0b7a75;
--err:#b42318;--err-bg:rgba(220,38,38,.06);--kw:#9a3eaf;--str:#3d8540;--com:#a09a92;--num:#b2601e;--fn:#2f64c6}
@media (prefers-color-scheme:dark){:root{--paper:#191817;--ink:#eceae6;--muted:#9c968f;--rule:#2b2927;--code:#211f1d;
--accent:#5cc8be;--err:#f08080;--err-bg:rgba(240,128,128,.08);--kw:#cb8fdc;--str:#9dc889;--com:#77716a;--num:#e3a26b;--fn:#7da9f2}}
html{background:var(--paper)}
body{margin:0;background:var(--paper);color:var(--ink);font:16px/1.62 Charter,"Iowan Old Style",Georgia,serif;
-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}
main{max-width:820px;margin:0 auto;padding:36px 28px 72px 84px}
.cell{margin:0 0 14px;position:relative}
pre,code{font-family:"SF Mono",ui-monospace,Menlo,Consolas,monospace}
.in{position:relative}
.prompt{position:absolute;left:-60px;width:48px;text-align:right;top:10px;font:11px/1.55 "SF Mono",ui-monospace,Menlo,monospace;color:var(--muted)}
pre.src{margin:0;padding:10px 12px;background:var(--code);border-radius:8px;font-size:12.5px;line-height:1.55;overflow-x:auto;tab-size:4}
.out{margin-top:6px;padding:0 12px}
.out pre{margin:4px 0;font-size:12.5px;line-height:1.5;white-space:pre-wrap;word-break:break-word}
.out pre.stderr{background:var(--err-bg);padding:6px 10px;border-radius:6px}
.out pre.error{background:var(--err-bg);padding:8px 12px;border-radius:6px}
.out .img{margin:6px 0}
.out img{max-width:100%;height:auto;display:block}
@media (prefers-color-scheme:dark){.out img{background:#fff;border-radius:4px}}
.out .svg svg{max-width:100%;height:auto}
.chart svg{max-width:100%;height:auto}.for-dark{display:none}
@media (prefers-color-scheme:dark){.for-light{display:none}.for-dark{display:block}}
.html{overflow-x:auto;margin:6px 0;font:13px/1.45 -apple-system,"SF Pro Text",Helvetica,sans-serif}
table{border-collapse:collapse;font:13px/1.45 -apple-system,"SF Pro Text",Helvetica,sans-serif;font-variant-numeric:tabular-nums;margin:8px 0}
th,td{padding:5px 10px;border-bottom:1px solid var(--rule);vertical-align:top}
th{font-weight:600;text-align:left}
.df table{border:none}.df th,.df td{border-top:none;border-left:none;border-right:none;text-align:right}
.df thead th{border-bottom:1px solid var(--muted)}.df tbody th{text-align:left}
.md h1,.md h2,.md h3,.md h4{line-height:1.25;margin:1.1em 0 .4em;font-weight:700}
.md h1{font-size:2em;margin-top:.5em}.md h2{font-size:1.5em}.md h3{font-size:1.2em}
.md{overflow-wrap:break-word}.md code{overflow-wrap:anywhere}.md p{margin:.5em 0}.md ul,.md ol{padding-left:1.4em;margin:.5em 0}
.md a{color:var(--accent);text-decoration-thickness:1px;text-underline-offset:2px}
.md code{background:var(--code);padding:.12em .36em;border-radius:4px;font-size:.84em}
.md pre{background:var(--code);padding:10px 12px;border-radius:8px;overflow-x:auto;font-size:12.5px;line-height:1.55}
.md pre code{background:none;padding:0;font-size:inherit}
.md blockquote{margin:.6em 0;padding:.1em 0 .1em 14px;border-left:3px solid var(--rule);color:var(--muted)}
.md img{max-width:100%}.md hr{border:0;border-top:1px solid var(--rule);margin:1.4em 0}
.md table td,.md table th{text-align:left}
.raw pre{color:var(--muted);font-size:12.5px;white-space:pre-wrap}
.math svg{display:inline-block}.math-display{text-align:center;margin:.7em 0;overflow-x:auto}
.math-error{color:var(--err)}
.k{color:var(--kw)}.s{color:var(--str)}.c{color:var(--com);font-style:italic}.n,.o{color:var(--num)}.f,.b{color:var(--fn)}
.ab{font-weight:700}.ai{font-style:italic}.au{text-decoration:underline}.ad{opacity:.7}.ax{text-decoration:line-through}
@media (max-width:640px){main{padding:24px 16px 56px}.prompt{display:none}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn nb(cells: &str) -> Notebook {
        let json = format!(
            r#"{{"cells": [{cells}], "metadata": {{"kernelspec": {{"name": "python3", "language": "python", "display_name": "Python 3"}}}}, "nbformat": 4, "nbformat_minor": 5}}"#
        );
        herse_nb::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn a_notebook_becomes_a_page() {
        let page = render(
            &nb(r##"{"cell_type": "markdown", "metadata": {}, "source": "# Title\n\nSome *text* and $\\alpha$.\n\n| a | b |\n|---|---|\n| 1 | 2 |"},
               {"cell_type": "code", "metadata": {}, "execution_count": 3, "source": "print('hi')",
                "outputs": [{"output_type": "stream", "name": "stdout", "text": "hi\n"},
                            {"output_type": "execute_result", "execution_count": 3, "metadata": {},
                             "data": {"text/plain": "42", "text/html": "<table class=\"dataframe\"><tr><td>42</td></tr></table><script>evil()</script>"}}]}"##),
            &Options { title: "t.ipynb".into(), ..Default::default() },
        );
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<title>t.ipynb</title>"));
        assert!(page.contains("<h1>Title</h1>"));
        assert!(page.contains("<em>text</em>"));
        assert!(page.contains("<span class=\"math\""), "math becomes SVG");
        assert!(page.contains("<table>"), "GFM tables");
        assert!(page.contains("<span class=\"prompt\">[3]</span>"));
        assert!(page.contains("<span class=\"b\">print</span>"));
        assert!(page.contains("<pre class=\"stream\">hi\n</pre>"));
        assert!(page.contains("<div class=\"html df\"><table class=\"dataframe\">"));
        assert!(!page.contains("evil()"), "scripts are dropped");
    }

    #[test]
    fn images_errors_and_hidden_cells() {
        let page = render(
            &nb(r##"{"cell_type": "code", "metadata": {"jupyter": {"source_hidden": true}}, "execution_count": 1, "source": "secret_source()",
                "outputs": [{"output_type": "display_data", "metadata": {"image/png": {"width": 300}},
                             "data": {"image/png": "iVBORw0K\nGgo=", "text/plain": "<Figure>"}},
                            {"output_type": "error", "ename": "ValueError", "evalue": "bad",
                             "traceback": ["\u001b[0;31mValueError\u001b[0m: bad"]}]},
               {"cell_type": "code", "metadata": {"tags": ["remove-cell"]}, "execution_count": 2, "source": "gone()", "outputs": []}"##),
            &Options::default(),
        );
        assert!(!page.contains("secret_source"), "hidden input stays hidden");
        assert!(page.contains("<img src=\"data:image/png;base64,iVBORw0KGgo=\" style=\"width:300px;\""));
        assert!(page.contains("<span class=\"f1\">ValueError</span>: bad"), "{page}");
        assert!(!page.contains("gone()"));
    }

    #[test]
    fn long_outputs_fold_and_html_is_cleaned() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let folded = fold(&text, 400);
        assert!(folded.contains("line 0") && folded.contains("line 999"));
        assert!(folded.contains("… 600 more lines …"));
        assert_eq!(clean_html("<p>a</p><SCRIPT src=x>1</script ><iframe>z</iframe><link rel=x>b"), "<p>a</p>b");
        assert!(!has_visible_content("<style>p{}</style>\n<div></div>"));
        assert!(has_visible_content("<div>hi</div>"));
    }
}
