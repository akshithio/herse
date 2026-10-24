//! Markdown cell source → what the renderer gets: math normalized
//! (see [`super::math::prepare`]), `attachment:` images inlined as data URIs
//! from the cell's attachments, relative image paths resolved against the
//! notebook's folder, and the inline HTML notebooks lean on that the renderer
//! doesn't style ([`html_fallbacks`]) rewritten to Markdown that looks alike.

use std::path::Path;

use herse_nb::Value;

/// `toggled`: the `<details>` blocks (by order in the cell) the reader opened
/// or closed, against how the source has them.
/// What a notebook's relative image paths are relative to: the folder it
/// is in, or the URL it was opened from.
#[derive(Clone, Copy)]
pub enum Base<'a> {
    Dir(&'a Path),
    Url(&'a str),
}

impl<'a> Base<'a> {
    /// A notebook opened from a link resolves against its URL; one on disk,
    /// against its folder; an untitled one has neither.
    pub fn of(dir: Option<&'a Path>, url: Option<&'a str>) -> Option<Self> {
        url.map(Base::Url).or(dir.map(Base::Dir))
    }
}

pub fn prepare(
    source: &str,
    attachments: Option<&Value>,
    base: Option<Base<'_>>,
    toggled: &std::collections::HashSet<usize>,
    dark: bool,
) -> String {
    let mut text = super::math::prepare(source);
    if text.contains('<') {
        if text.to_ascii_lowercase().contains("<details") {
            let mut counter = 0;
            text = fold_details(&text, toggled, &mut counter);
        }
        text = html_fallbacks(&text);
    }
    if text.contains("](") || text.contains("src=") {
        text = rewrite_image_urls(&text, |url| resolve(url, attachments, base));
    }
    if text.contains("mermaid") {
        text = mermaid_diagrams(&text, dark);
    }
    text
}

/// Whether a Markdown source has a Mermaid diagram, which is drawn for the
/// theme and so is drawn again when it changes.
pub fn has_mermaid(source: &str) -> bool {
    source.contains("```mermaid") || source.contains("~~~mermaid")
}

/// ```` ```mermaid ```` fences drawn as diagrams, as JupyterLab 4.1+ draws
/// them: each becomes a centred SVG image. A diagram the renderer can't draw
/// stays a code block; every other fence passes through untouched.
fn mermaid_diagrams(text: &str, dark: bool) -> String {
    use base64::Engine as _;
    let mut out = String::with_capacity(text.len());
    let mut lines = text.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let fence = trimmed.as_bytes().first().copied().filter(|&b| b == b'`' || b == b'~');
        let Some(fence) = fence.filter(|_| line.len() - trimmed.len() <= 3) else {
            out.push_str(line);
            continue;
        };
        let len = trimmed.bytes().take_while(|&b| b == fence).count();
        if len < 3 {
            out.push_str(line);
            continue;
        }
        let info = trimmed[len..].trim();
        let mut block = line.to_string();
        let mut body = String::new();
        for line in lines.by_ref() {
            block.push_str(line);
            let t = line.trim();
            if t.len() >= len && t.bytes().all(|b| b == fence) {
                break;
            }
            body.push_str(line);
        }
        let is_mermaid = info.split_whitespace().next().is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"));
        match is_mermaid.then(|| mermaid_svg(&body, dark)).flatten() {
            Some(svg) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
                out.push_str(&format!(
                    "\n<p align=\"center\"><img src=\"data:image/svg+xml;base64,{encoded}\" alt=\"Mermaid diagram\"></p>\n\n"
                ));
            }
            None => out.push_str(&block),
        }
    }
    out
}

/// A Mermaid diagram as SVG, at most a prose column wide.
pub fn mermaid_svg(source: &str, dark: bool) -> Option<String> {
    let options = herse_plot::RenderOptions {
        width: 760.,
        height: None,
        dark,
        font_family: "Helvetica Neue, Helvetica, Arial, sans-serif".into(),
        blend: true,
    };
    herse_plot::render_mermaid(source, &options).map(|r| r.svg)
}

/// The `href` of a `<details>` summary; the view toggles block `N` on click.
pub const DETAILS_LINK: &str = "herse-details:";

/// `<details>` blocks (solutions in course notebooks) folded the way a
/// browser shows them: the summary as a link that opens and closes the block,
/// the body only when open.
fn fold_details(text: &str, toggled: &std::collections::HashSet<usize>, counter: &mut usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines = text.split_inclusive('\n').peekable();
    let mut in_fence = false;
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if in_fence || !trimmed.to_ascii_lowercase().starts_with("<details") {
            out.push_str(line);
            continue;
        }
        // Gather the block to its matching close.
        let mut block = String::from(line);
        let mut depth = opens_minus_closes(line);
        while depth > 0 {
            let Some(next) = lines.next() else { break };
            depth += opens_minus_closes(next);
            block.push_str(next);
        }
        let index = *counter;
        *counter += 1;
        let lower = block.to_ascii_lowercase();
        let tag_end = lower.find('>').map_or(block.len(), |i| i + 1);
        let open_attr = lower[..tag_end].split_whitespace().any(|w| w.trim_end_matches('>') == "open");
        let open = open_attr != toggled.contains(&index);
        let (summary, body_start) = match (lower.find("<summary"), lower.find("</summary>")) {
            (Some(s), Some(e)) if s < e => {
                let inner_start = lower[s..].find('>').map_or(e, |i| s + i + 1).min(e);
                (strip_tags(&block[inner_start..e]), e + "</summary>".len())
            }
            _ => ("Details".to_string(), tag_end),
        };
        let body_end = lower.rfind("</details>").filter(|&e| e >= body_start).unwrap_or(block.len());
        let summary = summary.trim().replace('[', "\\[").replace(']', "\\]");
        let summary = if summary.is_empty() { "Details".to_string() } else { summary };
        out.push_str(&format!(
            "\n[{} {summary}]({DETAILS_LINK}{index})\n\n",
            if open { "▾" } else { "▸" }
        ));
        if open {
            // Nested blocks keep counting in reading order.
            out.push_str(&fold_details(&block[body_start..body_end], toggled, counter));
            out.push_str("\n\n");
        } else {
            // Still number the nested ones, so toggling an outer block
            // doesn't renumber what follows.
            *counter += block[body_start..body_end].to_ascii_lowercase().matches("<details").count();
        }
    }
    out
}

fn opens_minus_closes(line: &str) -> isize {
    let lower = line.to_ascii_lowercase();
    lower.matches("<details").count() as isize - lower.matches("</details>").count() as isize
}

pub(super) fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Outside code: `<sub>`/`<sup>` → Unicode sub/superscripts (when every
/// character has one: H₂O, x², Aᵀ), `<kbd>` → inline code, Bootstrap alert
/// boxes (`<div class="alert alert-info">`, common in course notebooks) →
/// a tinted callout (`> [!jupyter-info]`, which keeps the Markdown inside
/// the box), and `<summary>` → a bold line (the details stay open).
fn html_fallbacks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    let mut alert = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || !line.contains('<') && !alert {
            out.push_str(line);
            continue;
        }
        let mut line = inline_fallbacks(line);
        let lower = line.to_ascii_lowercase();
        if !alert && let Some(kind) = alert_kind(&lower) {
            // `<div class="alert ...">` opens the callout; text after the tag
            // on the same line is its first line.
            let open_end = lower.find('>').map_or(line.len(), |i| i + 1);
            let rest = line[open_end..].to_string();
            out.push_str("\n> [!jupyter-");
            out.push_str(kind);
            out.push_str("]\n> ");
            alert = true;
            line = rest;
        }
        if alert {
            let lower = line.to_ascii_lowercase();
            let (body, closed) = match lower.find("</div>") {
                Some(i) => (line[..i].to_string(), true),
                None => (line.clone(), false),
            };
            let body = body.trim_end_matches('\n');
            if !body.trim().is_empty() {
                out.push_str(body.trim());
                out.push_str(if closed { "\n" } else { "\n> " });
            }
            if closed {
                alert = false;
                out.push('\n');
            }
            continue;
        }
        out.push_str(&line);
    }
    out
}

/// The callout a `<div class="alert alert-…">` line opens.
fn alert_kind(lower_line: &str) -> Option<&'static str> {
    let t = lower_line.trim_start();
    if !t.starts_with("<div") {
        return None;
    }
    let tag = &t[..t.find('>')?];
    if !tag.contains("alert") {
        return None;
    }
    Some(if tag.contains("alert-warning") {
        "warning"
    } else if tag.contains("alert-danger") || tag.contains("alert-error") {
        "danger"
    } else if tag.contains("alert-success") {
        "success"
    } else {
        "info"
    })
}

/// `<sub>`, `<sup>`, `<kbd>` and `<summary>` within one line, outside code spans.
fn inline_fallbacks(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(lt) = rest.find(['<', '`']) {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with('`') {
            // Copy a code span through untouched.
            let ticks = rest.len() - rest.trim_start_matches('`').len();
            let fence = &rest[..ticks];
            match rest[ticks..].find(fence) {
                Some(end) => {
                    out.push_str(&rest[..ticks + end + ticks]);
                    rest = &rest[ticks + end + ticks..];
                }
                None => {
                    out.push_str(rest);
                    rest = "";
                }
            }
            continue;
        }
        let replaced = ["sub", "sup", "kbd", "summary"].iter().find_map(|tag| {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if !rest.get(..open.len())?.eq_ignore_ascii_case(&open) {
                return None;
            }
            let inner_end = rest.to_ascii_lowercase().find(&close)?;
            let inner = &rest[open.len()..inner_end];
            if inner.contains('<') && *tag != "summary" {
                return None;
            }
            let new = match *tag {
                "sub" => scripted(inner, SUBSCRIPTS)?,
                "sup" => scripted(inner, SUPERSCRIPTS)?,
                "kbd" if !inner.contains('`') && !inner.is_empty() => format!("`{inner}`"),
                // HTML, not `**`: it sits in an HTML block, which isn't Markdown.
                "summary" => format!("<b>▾ {}</b><br>", inner.trim()),
                _ => return None,
            };
            Some((new, inner_end + close.len()))
        });
        match replaced {
            Some((new, consumed)) => {
                out.push_str(&new);
                rest = &rest[consumed..];
            }
            None => {
                out.push('<');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

const SUPERSCRIPTS: &[(char, char)] = &[
    ('0', '⁰'), ('1', '¹'), ('2', '²'), ('3', '³'), ('4', '⁴'), ('5', '⁵'), ('6', '⁶'), ('7', '⁷'), ('8', '⁸'),
    ('9', '⁹'), ('+', '⁺'), ('-', '⁻'), ('−', '⁻'), ('=', '⁼'), ('(', '⁽'), (')', '⁾'), ('a', 'ᵃ'), ('b', 'ᵇ'),
    ('c', 'ᶜ'), ('d', 'ᵈ'), ('e', 'ᵉ'), ('f', 'ᶠ'), ('g', 'ᵍ'), ('h', 'ʰ'), ('i', 'ⁱ'), ('j', 'ʲ'), ('k', 'ᵏ'),
    ('l', 'ˡ'), ('m', 'ᵐ'), ('n', 'ⁿ'), ('o', 'ᵒ'), ('p', 'ᵖ'), ('r', 'ʳ'), ('s', 'ˢ'), ('t', 'ᵗ'), ('u', 'ᵘ'),
    ('v', 'ᵛ'), ('w', 'ʷ'), ('x', 'ˣ'), ('y', 'ʸ'), ('z', 'ᶻ'), ('T', 'ᵀ'), ('*', '*'), (' ', ' '),
];

const SUBSCRIPTS: &[(char, char)] = &[
    ('0', '₀'), ('1', '₁'), ('2', '₂'), ('3', '₃'), ('4', '₄'), ('5', '₅'), ('6', '₆'), ('7', '₇'), ('8', '₈'),
    ('9', '₉'), ('+', '₊'), ('-', '₋'), ('−', '₋'), ('=', '₌'), ('(', '₍'), (')', '₎'), ('a', 'ₐ'), ('e', 'ₑ'),
    ('h', 'ₕ'), ('i', 'ᵢ'), ('j', 'ⱼ'), ('k', 'ₖ'), ('l', 'ₗ'), ('m', 'ₘ'), ('n', 'ₙ'), ('o', 'ₒ'), ('p', 'ₚ'),
    ('r', 'ᵣ'), ('s', 'ₛ'), ('t', 'ₜ'), ('u', 'ᵤ'), ('v', 'ᵥ'), ('x', 'ₓ'), (' ', ' '),
];

/// `inner` in sub/superscript characters, or `None` if one has no form.
fn scripted(inner: &str, table: &[(char, char)]) -> Option<String> {
    if inner.is_empty() || inner.chars().count() > 24 {
        return None;
    }
    inner.chars().map(|c| table.iter().find(|(from, _)| *from == c).map(|(_, to)| *to)).collect()
}

fn resolve(url: &str, attachments: Option<&Value>, base: Option<Base<'_>>) -> Option<String> {
    if let Some(name) = url.strip_prefix("attachment:") {
        let bundle = attachments?.get(name)?.as_object()?;
        let (mime, data) = bundle.iter().find(|(m, _)| m.starts_with("image/"))?;
        let data = match data {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts.iter().filter_map(|p| p.as_str()).collect(),
            _ => return None,
        };
        if mime == "image/svg+xml" && data.trim_start().starts_with('<') {
            // Some writers store SVG attachments as text. Its spaces are
            // markup (`<svg xmlns=…`), so encode it whole, before any
            // whitespace is dropped.
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(data.as_bytes());
            return Some(format!("data:{mime};base64,{encoded}"));
        }
        let data: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        return Some(format!("data:{mime};base64,{data}"));
    }
    let is_relative = !url.is_empty()
        && !url.contains("://")
        && !url.starts_with("data:")
        && !url.starts_with('#')
        && !url.starts_with("mailto:");
    if !is_relative {
        return None;
    }
    let dir = match base? {
        Base::Url(base) => return Some(crate::remote::join(base, url)),
        Base::Dir(dir) => dir,
    };
    let path = if url.starts_with('/') {
        std::path::PathBuf::from(url)
    } else {
        dir.join(url.split(['?', '#']).next().unwrap_or(url))
    };
    // `localhost` as the authority: GPUI fetches through an `http::Uri`, which
    // rejects `file:///path` (a scheme with an empty authority).
    Some(format!("file://localhost{}", crate::http::percent_encode_path(&path.to_string_lossy())))
}

/// Apply `f` to every image URL (`![alt](url "t")` and `<img src="url">`),
/// outside fenced code blocks. `None` keeps the URL as written.
fn rewrite_image_urls(text: &str, f: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line(line, &f));
    }
    out
}

fn rewrite_line(line: &str, f: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let md = rest.find("![").and_then(|i| rest[i..].find("](").map(|j| (i, i + j + 2)));
        let html = find_src(rest);
        let next = match (md, html) {
            (Some((_, a)), Some((b, q))) if b < a => Some((b, Some(q))),
            (Some((_, a)), _) => Some((a, None)),
            (None, Some((b, q))) => Some((b, Some(q))),
            (None, None) => None,
        };
        let Some((start, quote)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = match quote {
            Some(q) => after.find(q).unwrap_or(after.len()),
            None => after.find([')', ' ', '\t']).unwrap_or(after.len()),
        };
        let url = after[..end].trim_start_matches('<').trim_end_matches('>');
        match f(url) {
            Some(new) => out.push_str(&new),
            None => out.push_str(&after[..end]),
        }
        rest = &after[end..];
    }
}

/// Position just past `src="` / `src='` and the closing quote.
fn find_src(s: &str) -> Option<(usize, char)> {
    let i = s.find("src=")?;
    let q = s[i + 4..].chars().next()?;
    (q == '"' || q == '\'').then_some((i + 5, q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_attachment_images() {
        let atts: Value = serde_json::json!({"a.png": {"image/png": "iVBORw0KGgo=\n"}});
        let src = "![x](attachment:a.png) and ![y](figs/p.png \"t\") <img src=\"z.jpg\"> ![r](https://e.com/i.png)";
        let out = prepare(src, Some(&atts), Some(Base::Dir(Path::new("/nb"))), &Default::default(), false);
        assert!(out.contains("](data:image/png;base64,iVBORw0KGgo=)"), "{out}");
        assert!(out.contains("](file://localhost/nb/figs/p.png \"t\")"), "{out}");
        assert!(out.contains("src=\"file://localhost/nb/z.jpg\""), "{out}");
        assert!(out.contains("](https://e.com/i.png)"), "{out}");
        // Opened from a link: relative images come from next to it.
        let base = Base::Url("https://raw.githubusercontent.com/o/r/main/nbs/a.ipynb");
        let out = prepare(src, Some(&atts), Some(base), &Default::default(), false);
        assert!(out.contains("](https://raw.githubusercontent.com/o/r/main/nbs/figs/p.png \"t\")"), "{out}");
        assert!(out.contains("src=\"https://raw.githubusercontent.com/o/r/main/nbs/z.jpg\""), "{out}");
        assert!(out.contains("](data:image/png;base64,iVBORw0KGgo=)"), "{out}");
    }

    #[test]
    fn inline_html_fallbacks() {
        let p = |s: &str| prepare(s, None, None, &Default::default(), false);
        assert_eq!(p("H<sub>2</sub>O and x<sup>2</sup>, A<sup>T</sup>, 10<sup>-3</sup>"), "H₂O and x², Aᵀ, 10⁻³");
        assert_eq!(p("press <kbd>Ctrl</kbd>+<kbd>C</kbd>"), "press `Ctrl`+`C`");
        // No form for 'Q': left as HTML.
        assert_eq!(p("x<sup>Q</sup>"), "x<sup>Q</sup>");
        // Code spans and fences keep their HTML.
        assert_eq!(p("`<sub>2</sub>` and <sub>2</sub>"), "`<sub>2</sub>` and ₂");
        assert_eq!(p("```\n<sup>2</sup>\n```\n"), "```\n<sup>2</sup>\n```\n");
        assert_eq!(p("<summary>Solution</summary>"), "<b>▾ Solution</b><br>");
        let alert = p("<div class=\"alert alert-block alert-info\">\n<b>Tip:</b> use it.\n</div>\nafter");
        assert!(alert.contains("> [!jupyter-info]\n> <b>Tip:</b> use it."), "{alert}");
        assert!(alert.trim_end().ends_with("after"), "{alert}");
        let one = p("<div class=\"alert alert-warning\">Careful.</div>");
        assert!(one.contains("> [!jupyter-warning]\n> Careful."), "{one}");
        assert!(!one.contains("</div>"), "{one}");
        let danger = p("<div class=\"alert alert-danger\">\n\n**Stop.**\n\n</div>");
        assert!(danger.contains("> [!jupyter-danger]"), "{danger}");
    }

    #[test]
    fn mermaid_fences() {
        let p = |s: &str| prepare(s, None, None, &Default::default(), false);
        let drawn = p("Before\n\n```mermaid\ngraph TD\n  A --> B\n```\n\nAfter");
        assert!(drawn.contains("<img src=\"data:image/svg+xml;base64,"), "{drawn}");
        assert!(drawn.starts_with("Before") && drawn.trim_end().ends_with("After"), "{drawn}");
        assert!(!drawn.contains("```"), "{drawn}");
        // A diagram it can't draw stays the code it was.
        let kept = "```mermaid\nnot a diagram at all\n```\n";
        assert_eq!(p(kept), kept);
        // Other fences, even ones that mention Mermaid, are untouched.
        let other = "```python\nprint('```mermaid')\n```\n";
        assert_eq!(p(other), other);
        assert!(has_mermaid("x\n~~~mermaid\ngraph LR\n~~~"));
    }

    #[test]
    fn details_fold() {
        let src = "Intro\n\n<details>\n<summary>Solution</summary>\n\nThe **answer**.\n</details>\n\nAfter";
        let closed = prepare(src, None, None, &Default::default(), false);
        assert!(closed.contains("[▸ Solution](herse-details:0)"), "{closed}");
        assert!(!closed.contains("answer") && closed.contains("After"), "{closed}");
        let open = prepare(src, None, None, &[0].into_iter().collect(), false);
        assert!(open.contains("[▾ Solution](herse-details:0)") && open.contains("The **answer**."), "{open}");
        assert!(!open.contains("</details>"), "{open}");
        // `open` in the source starts it open; toggling closes it.
        let o = "<details open><summary>Hint</summary>Try x.</details>\n";
        assert!(prepare(o, None, None, &Default::default(), false).contains("Try x."));
        assert!(!prepare(o, None, None, &[0].into_iter().collect(), false).contains("Try x."));
        // Nested blocks number in reading order whether or not the outer is open.
        let nested = "<details><summary>A</summary>\n<details><summary>B</summary>b</details>\n</details>\n<details><summary>C</summary>c</details>\n";
        let closed = prepare(nested, None, None, &Default::default(), false);
        assert!(closed.contains("](herse-details:2)") && !closed.contains("[▸ B]"), "{closed}");
        let outer_open = prepare(nested, None, None, &[0].into_iter().collect(), false);
        assert!(outer_open.contains("[▸ B](herse-details:1)") && outer_open.contains("[▸ C](herse-details:2)"), "{outer_open}");
        // In a code fence: untouched.
        let fenced = "```\n<details>x</details>\n```\n";
        assert_eq!(prepare(fenced, None, None, &Default::default(), false), fenced);
    }

    #[test]
    fn fences_untouched() {
        let src = "```\n![x](a.png)\n```\n";
        assert_eq!(prepare(src, None, Some(Base::Dir(Path::new("/nb"))), &Default::default(), false), src);
    }
}
