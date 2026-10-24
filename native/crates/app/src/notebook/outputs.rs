//! Turning nbformat outputs into render-ready items.
//!
//! Everything expensive happens once per output, when its cell first scrolls
//! into view: base64 decode, image size sniffing, ANSI parsing, carriage
//! return folding, HTML/Markdown parsing (on a background thread inside
//! `TextViewState`). Rendering a frame then only clones `Arc`s.

use std::{collections::HashMap, ops::Range, rc::Rc, sync::Arc};

use base64::Engine as _;
use gpui_kit::{
    App, AppContext as _, Entity, FontStyle, FontWeight, HighlightStyle, Hsla, Image,
    ImageFormat, SharedString, StrikethroughStyle, UnderlineStyle, px, rgb,
};
use gpui_kit::component::text::TextViewState;
use herse_nb::{MimeBundle, MimeData, Output, StreamName};
use herse_widgets::{Kind as WidgetKind, WidgetState};
use herse_rich::ansi::{AnsiColor, AnsiStyle};

pub type Runs = Arc<[(Range<usize>, HighlightStyle)]>;

pub enum OutputItem {
    Text {
        text: SharedString,
        runs: Runs,
        lines: usize,
        kind: TextKind,
        /// Set when the text is too big to lay out whole: what to show folded.
        clip: Option<Clip>,
    },
    Image {
        image: Arc<Image>,
        width: f32,
        height: f32,
        /// What a natively drawn chart left out ("range slider omitted").
        caption: Option<SharedString>,
        /// `needs_background: light` (a transparent figure drawn for white):
        /// give it a white backing on a dark page, as Jupyter does.
        light_backing: bool,
    },
    Rich(Entity<TextViewState>),
    /// A DataFrame (or any plain HTML table), drawn as a native grid.
    Table(Arc<super::table::TableOutput>),
    /// Something we can't draw natively (widgets, JS); says so instead of
    /// pretending.
    Note(SharedString),
    /// An ipywidget drawn from the state saved in the notebook.
    Widget(Rc<WidgetUi>),
    /// A matplotlib `to_jshtml()` animation, played natively.
    Animation(Rc<super::animation::Animation>),
    /// HTML that only a browser can run (scripts, an embedded page): offered
    /// there instead of shown as `<IPython.core.display.HTML object>`.
    Browser {
        html: Arc<str>,
        /// An `<iframe>`'s page, opened directly.
        src: Option<SharedString>,
    },
    /// An `application/pdf` output, opened in the system viewer.
    Pdf(Arc<[u8]>),
    /// An output an extension draws that hasn't been allowed to run: a note
    /// saying so, what it would run, and Allow.
    ExtensionAsk { extension: String, name: SharedString, command: SharedString },
}

/// A widget tree plus what its nodes need prepared ahead of drawing, keyed
/// by [`herse_widgets::Widget::key`].
pub struct WidgetUi {
    pub root: herse_widgets::Widget,
    /// Every model in the tree, so a kernel update knows what to redraw.
    pub model_ids: Vec<String>,
    pub html: HashMap<u32, WidgetHtml>,
    /// An Output widget's outputs, built like a cell's.
    pub outputs: HashMap<u32, Vec<OutputItem>>,
    pub images: HashMap<u32, (Arc<Image>, f32, f32)>,
}

pub enum WidgetHtml {
    /// No markup, entities decoded (tqdm's labels).
    Plain(SharedString),
    Rich(Entity<TextViewState>),
}

/// Output widgets can display widgets that hold Output widgets; stop here.
const MAX_WIDGET_NESTING: usize = 6;

/// Head and tail of a huge text, as byte ranges into it.
#[derive(Clone, Debug)]
pub struct Clip {
    pub head: Range<usize>,
    pub tail: Range<usize>,
    pub hidden_lines: usize,
}

/// Folded outputs show this many lines from the start and from the end.
const HEAD_LINES: usize = 200;
const TAIL_LINES: usize = 40;
/// ...and never more than this many bytes, so one giant line can't be laid out whole.
const CLIP_BYTES: usize = 96 * 1024;

fn clip_for(text: &str) -> Option<Clip> {
    let ht = herse_rich::stream::head_tail(text, HEAD_LINES, TAIL_LINES, CLIP_BYTES)?;
    let base = text.as_ptr() as usize;
    let head_start = ht.head.as_ptr() as usize - base;
    let tail_start = ht.tail.as_ptr() as usize - base;
    Some(Clip {
        head: head_start..head_start + ht.head.len(),
        tail: tail_start..tail_start + ht.tail.len(),
        hidden_lines: ht.hidden_lines,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Stdout,
    Stderr,
    Result,
    Error,
}

/// Mime types in the order a reader would want them, best first.
const PREFERRED: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "text/html",
    "text/markdown",
    "text/latex",
    "application/json",
    "text/plain",
];

/// `chart_width`: the width natively drawn charts are laid out to (the
/// notebook's content width at zoom 1).
pub fn build(
    outputs: &[Output],
    dark: bool,
    widgets: Option<&WidgetState>,
    chart_width: f32,
    cx: &mut App,
) -> Vec<OutputItem> {
    CHART_WIDTH.with(|w| w.set(chart_width));
    build_nested(outputs, dark, widgets, 0, cx)
}

thread_local! {
    /// The `chart_width` of the build in progress (one per notebook render).
    static CHART_WIDTH: std::cell::Cell<f32> = const { std::cell::Cell::new(820.) };
}

fn build_nested(
    outputs: &[Output],
    dark: bool,
    widgets: Option<&WidgetState>,
    depth: usize,
    cx: &mut App,
) -> Vec<OutputItem> {
    let mut items = Vec::with_capacity(outputs.len());
    let mut ix = 0;
    while ix < outputs.len() {
        match &outputs[ix] {
            Output::Stream { name, .. } => {
                // Consecutive chunks of one stream render as one block, the way
                // Jupyter shows them.
                let name = *name;
                let mut text = String::new();
                while let Some(Output::Stream { name: n, text: t }) = outputs.get(ix) {
                    if *n != name {
                        break;
                    }
                    herse_rich::stream::append(&mut text, t);
                    ix += 1;
                }
                let kind = match name {
                    StreamName::Stdout => TextKind::Stdout,
                    StreamName::Stderr => TextKind::Stderr,
                };
                items.push(ansi_item(&text, kind, dark));
                continue;
            }
            Output::Error {
                ename,
                evalue,
                traceback,
            } => {
                let text = if traceback.is_empty() {
                    format!("{ename}: {evalue}")
                } else {
                    traceback.join("\n")
                };
                items.push(ansi_item(&text, TextKind::Error, dark));
            }
            Output::DisplayData { data, metadata, .. }
            | Output::ExecuteResult { data, metadata, .. } => {
                if is_loader(data) {
                    ix += 1;
                    continue;
                }
                let widget = widgets
                    .filter(|_| depth < MAX_WIDGET_NESTING)
                    .and_then(|state| widget_item(data, state, dark, depth, cx));
                if let Some(item) = widget.or_else(|| mime_item(data, metadata, dark, cx)) {
                    items.push(item);
                }
            }
            Output::Unknown(map) => {
                let kind = map.get("output_type").and_then(|v| v.as_str()).unwrap_or("unknown");
                items.push(OutputItem::Note(format!("Unrecognized output ({kind})").into()));
            }
        }
        ix += 1;
    }
    items
}

fn mime_item(
    data: &MimeBundle,
    metadata: &serde_json::Map<String, serde_json::Value>,
    dark: bool,
    cx: &mut App,
) -> Option<OutputItem> {
    let widget = data.get("application/vnd.jupyter.widget-view+json").is_some();
    let interactive = data
        .0
        .iter()
        .find(|(mime, _)| mime.starts_with("application/vnd.") && mime.contains("+json"))
        .map(|(mime, _)| mime.clone());

    if let Some(chart) = chart_item(data, dark) {
        return Some(chart);
    }
    // A type an installed extension draws.
    if let Some(item) = extension_item(data, dark, cx) {
        return Some(item);
    }
    let mut browser_html: Option<&str> = None;
    if let Some(MimeData::Text(b64)) = data.get("application/pdf")
        && let Some(bytes) = decode_base64(b64).filter(|b| b.starts_with(b"%PDF"))
    {
        return Some(OutputItem::Pdf(bytes.into()));
    }
    // Math typesets crisply where a kernel's picture of it is blurry
    // (SymPy sends a PNG beside its LaTeX): prefer the LaTeX over images
    // when it is one formula the typesetter can set (JupyterLab ranks LaTeX
    // above images too).
    if let Some(MimeData::Text(tex)) = data.get("text/latex")
        && ["image/png", "image/jpeg", "image/svg+xml"].iter().any(|m| data.contains(m))
        && latex_typesets(tex)
    {
        let md = super::math::prepare(&latex_as_markdown(tex));
        return Some(OutputItem::Rich(cx.new(|cx| TextViewState::markdown(&md, cx).selectable(true))));
    }
    for &mime in PREFERRED {
        let Some(value) = data.get(mime) else { continue };
        match mime {
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
                if let Some(item) = image_item(mime, value, metadata) {
                    return Some(item);
                }
            }
            "image/svg+xml" => {
                if let MimeData::Text(svg) = value {
                    let image = Arc::new(Image::from_bytes(ImageFormat::Svg, svg.as_bytes().to_vec()));
                    let (width, height) = svg_size(svg).unwrap_or((480., 360.));
                    return Some(OutputItem::Image {
                        image,
                        width,
                        height,
                        caption: None,
                        light_backing: needs_light_background(metadata),
                    });
                }
            }
            "text/html" => {
                if let MimeData::Text(html) = value {
                    // A DataFrame: draw the grid ourselves (compact, aligned,
                    // Styler colours) instead of laying out generic HTML.
                    // Colab wraps every DataFrame in buttons and a script
                    // (convert to interactive, quick charts); the table inside
                    // is the output.
                    let colab_frame = html.contains("colab-df-container");
                    if let Some(table) = herse_table::parse(html).filter(|t| t.standalone || colab_frame) {
                        return Some(OutputItem::Table(Arc::new(super::table::TableOutput::new(table))));
                    }
                    if let Some(anim) = super::animation::parse(html) {
                        return Some(OutputItem::Animation(Rc::new(anim)));
                    }
                    // Altair's HTML: the Vega-Lite spec inside a vegaEmbed call.
                    if html.contains("vegaEmbed")
                        && let Some(spec) = herse_plot::vegalite_spec_from_html(html)
                        && let Some(chart) = chart_from(herse_plot::render_vegalite(&spec, &chart_options(dark, None)))
                    {
                        return Some(chart);
                    }
                    if is_bokeh_banner(html) {
                        return None;
                    }
                    // HTML that needs a browser (scripts, iframes) renders as
                    // garbage natively; fall through to a simpler type instead.
                    if needs_browser(html) {
                        // A library loader (plotly.js's init_notebook_mode):
                        // scripts with nothing to see; JupyterLab shows nothing.
                        if is_script_only(html) {
                            return None;
                        }
                        browser_html = Some(html);
                        continue;
                    }
                    let html = pad_empty_cells(html);
                    return Some(OutputItem::Rich(
                        cx.new(|cx| TextViewState::html(&html, cx).selectable(true)),
                    ));
                }
            }
            "text/markdown" => {
                if let MimeData::Text(md) = value {
                    let md = super::math::prepare(md);
                    return Some(OutputItem::Rich(
                        cx.new(|cx| TextViewState::markdown(&md, cx).selectable(true)),
                    ));
                }
            }
            "text/latex" => {
                if let MimeData::Text(tex) = value {
                    // Markdown math blocks go through the same math renderer.
                    let md = super::math::prepare(&latex_as_markdown(tex));
                    return Some(OutputItem::Rich(
                        cx.new(|cx| TextViewState::markdown(&md, cx).selectable(true)),
                    ));
                }
            }
            "application/json" => {
                let text = match value {
                    MimeData::Json(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                    MimeData::Text(t) => t.clone(),
                };
                return Some(plain_item(text, TextKind::Result));
            }
            "text/plain" => {
                if let MimeData::Text(text) = value {
                    if let Some(html) = browser_html
                        && !widget
                        && interactive.is_none()
                        && is_object_repr(text)
                    {
                        return Some(browser_item(html));
                    }
                    if is_object_repr(text)
                        && !widget
                        && interactive.is_none()
                        && let Some(kind) = data.0.iter().find_map(|(m, _)| unshown_kind(m))
                    {
                        return Some(OutputItem::Note(format!("{kind} — not shown here").into()));
                    }
                    let mut item = ansi_item(text, TextKind::Result, dark);
                    if widget || interactive.is_some() {
                        // Keep the fallback text but say why the real thing is missing.
                        if let OutputItem::Text { text, .. } = &item
                            && text.trim().is_empty()
                        {
                            item = note_for(widget, interactive.as_deref());
                        }
                    }
                    return Some(item);
                }
            }
            _ => {}
        }
    }
    if widget || interactive.is_some() {
        return Some(note_for(widget, interactive.as_deref()));
    }
    if let Some(html) = browser_html {
        return Some(browser_item(html));
    }
    data.0.first().map(|(mime, _)| OutputItem::Note(format!("{mime} output").into()))
}

/// An output an extension draws (see `crate::extensions`): its drawing once
/// it arrives, a note while it's on the way or if it failed.
fn extension_item(data: &MimeBundle, dark: bool, cx: &mut App) -> Option<OutputItem> {
    let (name, state) = crate::extensions::render_state(data, dark, cx)?;
    Some(match state {
        crate::extensions::RenderState::Pending => OutputItem::Note(format!("Drawing with {name}…").into()),
        crate::extensions::RenderState::Failed(why) => OutputItem::Note(format!("{name} couldn't draw this: {why}").into()),
        crate::extensions::RenderState::NeedsApproval { extension, command } => {
            OutputItem::ExtensionAsk { extension, name, command: command.into() }
        }
        crate::extensions::RenderState::Done(drawing) => match drawing {
            crate::extensions::Rendered::Svg(svg) => {
                let (width, height) = svg_size(&svg).unwrap_or((480., 360.));
                OutputItem::Image {
                    image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
                    width,
                    height,
                    caption: None,
                    light_backing: false,
                }
            }
            crate::extensions::Rendered::Png(bytes) => {
                let (width, height) =
                    imagesize::blob_size(&bytes).map_or((480., 360.), |s| (s.width as f32 / 2., s.height as f32 / 2.));
                OutputItem::Image {
                    image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
                    width,
                    height,
                    caption: None,
                    light_backing: false,
                }
            }
            crate::extensions::Rendered::Markdown(md) => {
                let md = super::math::prepare(&md);
                OutputItem::Rich(cx.new(|cx| TextViewState::markdown(&md, cx).selectable(true)))
            }
            crate::extensions::Rendered::Text(text) => plain_item(text, TextKind::Result),
        },
    })
}

/// Plotly figures and Vega-Lite specs, drawn natively as SVG. `None` when
/// the bundle has neither or the chart uses something herse-plot doesn't
/// draw (3D, maps, ...): the caller falls back to a note.
fn chart_item(data: &MimeBundle, dark: bool) -> Option<OutputItem> {
    // Borrowed when the notebook stored JSON (the usual case): figures run to
    // megabytes, not worth copying.
    fn json(v: &MimeData) -> Option<std::borrow::Cow<'_, serde_json::Value>> {
        match v {
            MimeData::Json(v) => Some(std::borrow::Cow::Borrowed(v)),
            MimeData::Text(t) => serde_json::from_str(t).ok().map(std::borrow::Cow::Owned),
        }
    }
    // JupyterLab 4.1+ draws `text/vnd.mermaid` (e.g. `display(Mermaid(...))`).
    if let Some(MimeData::Text(src)) = data.get("text/vnd.mermaid")
        && let Some(chart) = chart_from(herse_plot::render_mermaid(src, &chart_options(dark, None)))
    {
        return Some(chart);
    }
    if let Some(fig) = data.get("application/vnd.plotly.v1+json").and_then(json) {
        // plotly.js's own default height when the figure doesn't set one.
        let height = fig.get("layout").and_then(|l| l.get("height")).is_none().then_some(450.);
        if let Some(chart) = chart_from(herse_plot::render_plotly(&fig, &chart_options(dark, height))) {
            return Some(chart);
        }
    }
    let spec = data
        .0
        .iter()
        .find(|(m, _)| m.starts_with("application/vnd.vegalite.v") && m.ends_with("+json"))
        .and_then(|(_, v)| json(v))?;
    chart_from(herse_plot::render_vegalite(&spec, &chart_options(dark, None)))
}

fn chart_options(dark: bool, height: Option<f32>) -> herse_plot::RenderOptions {
    herse_plot::RenderOptions {
        width: CHART_WIDTH.with(|w| w.get()),
        height,
        dark,
        font_family: "Helvetica Neue, Helvetica, Arial, sans-serif".into(),
        blend: crate::settings::blend_charts(),
    }
}

fn chart_from(rendered: Option<herse_plot::Rendered>) -> Option<OutputItem> {
    let r = rendered?;
    (r.width > 0. && r.height > 0.).then(|| OutputItem::Image {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, r.svg.into_bytes())),
        width: r.width,
        height: r.height,
        caption: (!r.notes.is_empty()).then(|| r.notes.join(" · ").into()),
        light_backing: false,
    })
}

fn needs_light_background(metadata: &serde_json::Map<String, serde_json::Value>) -> bool {
    metadata.get("needs_background").and_then(|v| v.as_str()) == Some("light")
}

/// A widget view whose model the notebook saved, unless it's one we can't
/// draw (third-party widgets), which falls back to the plain mime types.
fn widget_item(data: &MimeBundle, state: &WidgetState, dark: bool, depth: usize, cx: &mut App) -> Option<OutputItem> {
    let view = match data.get(herse_widgets::VIEW_MIME)? {
        MimeData::Json(v) => v.clone(),
        MimeData::Text(t) => serde_json::from_str(t).ok()?,
    };
    let root = state.view(herse_widgets::model_id(&view)?)?;
    if matches!(root.kind, WidgetKind::Unsupported { .. }) {
        return None;
    }
    let mut ui = WidgetUi {
        root,
        model_ids: Vec::new(),
        html: HashMap::new(),
        outputs: HashMap::new(),
        images: HashMap::new(),
    };
    let root = ui.root.clone();
    prepare_widget(&root, &mut ui, state, dark, depth, cx);
    Some(OutputItem::Widget(Rc::new(ui)))
}

fn prepare_widget(w: &herse_widgets::Widget, ui: &mut WidgetUi, state: &WidgetState, dark: bool, depth: usize, cx: &mut App) {
    ui.model_ids.push(w.model_id.clone());
    match &w.kind {
        WidgetKind::Box { children, .. }
        | WidgetKind::Tabs { children, .. }
        | WidgetKind::Accordion { children, .. } => {
            for child in children {
                prepare_widget(child, ui, state, dark, depth, cx);
            }
        }
        WidgetKind::Stack { child: Some(child) } => prepare_widget(child, ui, state, dark, depth, cx),
        WidgetKind::Html { html, math } => {
            let prepared = match herse_widgets::html::as_plain_text(html) {
                Some(text) if !*math || !text.contains('$') => WidgetHtml::Plain(text.into()),
                _ if *math => {
                    let md = super::math::prepare(html);
                    WidgetHtml::Rich(cx.new(|cx| TextViewState::markdown(&md, cx).selectable(true)))
                }
                _ => {
                    let html = pad_empty_cells(html);
                    WidgetHtml::Rich(cx.new(|cx| TextViewState::html(&html, cx).selectable(true)))
                }
            };
            ui.html.insert(w.key, prepared);
        }
        WidgetKind::Output { outputs } => {
            let outputs: Vec<Output> = outputs
                .iter()
                .filter_map(|o| herse_nb::output_from_iopub(o.get("output_type")?.as_str()?, o))
                .collect();
            let items = build_nested(&outputs, dark, Some(state), depth + 1, cx);
            for item in &items {
                if let OutputItem::Widget(inner) = item {
                    ui.model_ids.extend(inner.model_ids.iter().cloned());
                }
            }
            ui.outputs.insert(w.key, items);
        }
        WidgetKind::Image { format, data, width, height } => {
            let format = match format.as_str() {
                "png" => ImageFormat::Png,
                "jpeg" | "jpg" => ImageFormat::Jpeg,
                "gif" => ImageFormat::Gif,
                "webp" => ImageFormat::Webp,
                "svg" | "svg+xml" => ImageFormat::Svg,
                _ => return,
            };
            let natural = imagesize::blob_size(data).ok().map(|s| (s.width as f32, s.height as f32));
            let size = match (width, height, natural) {
                (Some(w), Some(h), _) => Some((*w, *h)),
                (Some(w), None, Some((nw, nh))) => Some((*w, w * nh / nw.max(1.))),
                (None, Some(h), Some((nw, nh))) => Some((h * nw / nh.max(1.), *h)),
                (None, None, n) => n,
                _ => None,
            };
            if let Some((iw, ih)) = size.filter(|(a, b)| *a > 0. && *b > 0.) {
                ui.images.insert(w.key, (Arc::new(Image::from_bytes(format, data.clone())), iw, ih));
            }
        }
        _ => {}
    }
}

fn note_for(widget: bool, mime: Option<&str>) -> OutputItem {
    if widget {
        return OutputItem::Note("Interactive widget — shown live only in a browser".into());
    }
    let mime = mime.unwrap_or("interactive");
    let name = match mime {
        m if m.starts_with("application/vnd.plotly") => "Plotly chart".to_string(),
        m if m.starts_with("application/vnd.vegalite") || m.starts_with("application/vnd.vega.") => "Vega chart".into(),
        m if m.starts_with("application/vnd.bokehjs") => "Bokeh plot".into(),
        m if m.starts_with("application/vnd.holoviews") => "HoloViews plot".into(),
        m => format!("Interactive output ({m})"),
    };
    OutputItem::Note(format!("{name} — shown live only in a browser").into())
}

/// Library loaders (BokehJS, HoloViews, Panel): scripts that set up a browser
/// and draw nothing themselves.
fn is_loader(data: &MimeBundle) -> bool {
    !data.0.is_empty()
        && data.0.iter().all(|(m, _)| {
            m == "application/javascript" || m.ends_with("_load.v0+json") || m.ends_with("_load.v1+json")
        })
}

/// pandas writes the index column's header as `<th></th>`; the HTML parser
/// drops empty cells, which shifts every header one column left.
fn pad_empty_cells(html: &str) -> String {
    html.replace("></th>", ">&nbsp;</th>").replace("></td>", ">&nbsp;</td>")
}

/// `<IPython.core.display.HTML object>`, `<IPython.lib.display.IFrame at 0x…>`:
/// a placeholder repr, not worth showing over a way to see the real thing.
fn is_object_repr(text: &str) -> bool {
    let t = text.trim();
    t.len() < 200 && t.starts_with('<') && t.ends_with('>') && (t.contains(" object") || t.contains(" at 0x"))
}

/// What to call an output type we don't draw, when its text is only a repr.
fn unshown_kind(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "application/javascript" | "text/javascript" => "JavaScript output (runs in a browser)",
        "application/geo+json" => "GeoJSON map",
        "application/vnd.dataresource+json" => "Data resource table",
        m if m.starts_with("video/") => "Video",
        m if m.starts_with("audio/") => "Audio",
        _ => return None,
    })
}

fn browser_item(html: &str) -> OutputItem {
    OutputItem::Browser {
        html: html.into(),
        src: iframe_src(html).map(Into::into),
    }
}

/// The page of an output that is just an `<iframe src=…>` (IFrame, YouTubeVideo).
fn iframe_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    if lower.contains("<script") || lower.matches("<iframe").count() != 1 {
        return None;
    }
    let at = lower.find("<iframe")?;
    let tag_end = lower[at..].find('>')? + at;
    let tag = &html[at..tag_end];
    let i = tag.to_ascii_lowercase().find("src=")?;
    let rest = &tag[i + 4..];
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let url = &rest[1..rest[1..].find(quote)? + 1];
    let url = url.trim().replace("&amp;", "&");
    let url = if url.starts_with("//") { format!("https:{url}") } else { url };
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
}

/// HTML that is only `<script>`, `<style>`, `<link>` and `<meta>`: nothing a
/// browser would draw by itself.
/// Bokeh's `output_notebook()` banner: its logo and "Loading BokehJS ...",
/// which BokehJS replaces once it loads. It never loads here, so the line
/// would only mislead; the plot's own output says where it can be seen.
fn is_bokeh_banner(html: &str) -> bool {
    html.len() < 8 * 1024 && html.contains("bk-notebook-logo")
}

fn is_script_only(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return true;
        }
        let Some(after) = rest.strip_prefix('<') else { return false };
        let name: String = after.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        rest = match name.as_str() {
            "script" | "style" => {
                let close = format!("</{name}>");
                match rest.find(&close) {
                    Some(i) => &rest[i + close.len()..],
                    None => return true,
                }
            }
            "link" | "meta" => match rest.find('>') {
                Some(i) => &rest[i + 1..],
                None => return true,
            },
            _ => return false,
        };
    }
}

fn needs_browser(html: &str) -> bool {
    let probe = &html[..html.floor_char_boundary(64 * 1024)];
    // Players too: natively they'd show only "Your browser does not
    // support the audio element".
    ["<script", "<iframe", "<audio", "<video"].iter().any(|tag| contains_ci(probe, tag))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// A single formula (`$…$`, `$$…$$`, `\[…\]`) the math typesetter sets.
fn latex_typesets(tex: &str) -> bool {
    let (inner, display) = herse_rich::math::strip_delimiters(tex.trim());
    display.is_some() && herse_rich::math::render_svg(inner, display == Some(true)).is_ok()
}

fn latex_as_markdown(tex: &str) -> String {
    let t = tex.trim();
    if t.starts_with('$') {
        t.to_string()
    } else {
        format!("$$\n{t}\n$$")
    }
}

fn image_item(
    mime: &str,
    value: &MimeData,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Option<OutputItem> {
    let MimeData::Text(b64) = value else { return None };
    let bytes = decode_base64(b64)?;
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        _ => ImageFormat::Webp,
    };
    let (px_w, px_h) = match imagesize::blob_size(&bytes) {
        Ok(size) => (size.width as f32, size.height as f32),
        Err(_) => return None,
    };
    // Retina figures carry their logical size in output metadata.
    let meta = metadata.get(mime);
    let meta_w = meta.and_then(|m| m.get("width")).and_then(|v| v.as_f64());
    let meta_h = meta.and_then(|m| m.get("height")).and_then(|v| v.as_f64());
    let pixel_ratio = px_h / px_w.max(1.);
    let (width, height) = match (meta_w, meta_h) {
        // A stated size of another shape than the pixels (IRkernel's 420×420
        // for any plot) would stretch the figure: keep its width, and the
        // pixels' shape.
        (Some(w), Some(h)) if ((h / w.max(1.)) as f32 - pixel_ratio).abs() > 0.02 * pixel_ratio.max(0.01) => {
            (w as f32, w as f32 * pixel_ratio)
        }
        (Some(w), Some(h)) => (w as f32, h as f32),
        (Some(w), None) => (w as f32, w as f32 * pixel_ratio),
        (None, Some(h)) => (h as f32 * px_w / px_h.max(1.), h as f32),
        (None, None) => (px_w, px_h),
    };
    Some(OutputItem::Image {
        image: Arc::new(Image::from_bytes(format, bytes)),
        width: width.max(1.),
        height: height.max(1.),
        caption: None,
        light_backing: needs_light_background(metadata),
    })
}

pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let engine = &base64::engine::general_purpose::STANDARD;
    if !text.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
        return engine.decode(text).ok();
    }
    let compact: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    engine.decode(compact).ok()
}

fn svg_size(svg: &str) -> Option<(f32, f32)> {
    let head = &svg[..svg.len().min(4096)];
    let start = head.find("<svg")?;
    let tag = &head[start..];
    let tag = &tag[..tag.find('>')?];
    let attr = |name: &str| -> Option<f32> {
        let key = format!(" {name}=\"");
        let i = tag.find(&key)? + key.len();
        let rest = &tag[i..];
        let value = &rest[..rest.find('"')?];
        let number: String = value
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let n: f32 = number.parse().ok()?;
        // matplotlib writes sizes in pt.
        Some(if value.ends_with("pt") { n * 4. / 3. } else { n })
    };
    match (attr("width"), attr("height")) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => {
            let key = "viewBox=\"";
            let i = tag.find(key)? + key.len();
            let rest = &tag[i..];
            let vb: Vec<f32> = rest[..rest.find('"')?]
                .split([' ', ','])
                .filter_map(|s| s.parse().ok())
                .collect();
            (vb.len() == 4).then(|| (vb[2], vb[3]))
        }
    }
}

fn plain_item(text: String, kind: TextKind) -> OutputItem {
    let lines = herse_rich::stream::line_count(&text).max(1);
    let clip = clip_for(&text);
    OutputItem::Text {
        text: text.into(),
        runs: Arc::from(Vec::new()),
        lines,
        kind,
        clip,
    }
}

pub fn ansi_item(text: &str, kind: TextKind, dark: bool) -> OutputItem {
    let normalized = herse_rich::stream::normalize(text);
    let styled = herse_rich::ansi::parse(&normalized);
    let mut s = styled.text;
    // A trailing newline would render as an empty last line.
    while s.ends_with('\n') {
        s.pop();
    }
    let len = s.len();
    let runs: Vec<(Range<usize>, HighlightStyle)> = styled
        .spans
        .into_iter()
        .filter_map(|span| {
            let range = span.range.start.min(len)..span.range.end.min(len);
            (!range.is_empty()).then(|| (range, ansi_highlight(&span.style, dark)))
        })
        .collect();
    let lines = herse_rich::stream::line_count(&s).max(1);
    let clip = clip_for(&s);
    OutputItem::Text {
        text: s.into(),
        runs: runs.into(),
        lines,
        kind,
        clip,
    }
}

fn ansi_highlight(style: &AnsiStyle, dark: bool) -> HighlightStyle {
    let mut fg = style.fg.map(|c| ansi_color(c, dark));
    let mut bg = style.bg.map(|c| ansi_color(c, dark));
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    // Text on the page (no fill of its own) must read on it: IPython's
    // 256-colour tracebacks pick dark blue and dark red for a white page.
    if bg.is_none() {
        fg = fg.map(|c| gpui_kit::base::text::readable_text_color(c, dark));
    }
    if style.dim {
        fg = fg.map(|c| c.opacity(0.6));
    }
    HighlightStyle {
        color: fg,
        background_color: bg,
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            ..Default::default()
        }),
        strikethrough: style.strikethrough.then(|| StrikethroughStyle {
            thickness: px(1.),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The 16 named colours, tuned to stay readable on our backgrounds rather
/// than copying a terminal's saturated defaults.
fn ansi_color(color: AnsiColor, dark: bool) -> Hsla {
    const LIGHT: [u32; 16] = [
        0x24292f, 0xc4321f, 0x1a7f37, 0x9a6700, 0x0a58ca, 0x8250df, 0x1b7c83, 0x6e7781,
        0x57606a, 0xd1242f, 0x2da44e, 0xbf8700, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
    ];
    const DARK: [u32; 16] = [
        0x6e7681, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
        0x8b949e, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xf0f6fc,
    ];
    match color {
        AnsiColor::Named(n) if n < 16 => {
            let table = if dark { &DARK } else { &LIGHT };
            rgb(table[n as usize]).into()
        }
        AnsiColor::Named(n) | AnsiColor::Indexed(n) => {
            if n < 16 {
                return ansi_color(AnsiColor::Named(n), dark);
            }
            let (r, g, b) = herse_rich::ansi::xterm_256_rgb(n);
            rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
        }
        AnsiColor::Rgb(r, g, b) => rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceback_colours_read_in_dark() {
        use gpui_kit::base::text::{ink_on, luminance, readable_text_color};
        // IPython's 256-colour blue (21) and dark red (124).
        for n in [21u8, 124] {
            let c = ansi_highlight(&AnsiStyle { fg: Some(AnsiColor::Indexed(n)), ..Default::default() }, true);
            assert!(luminance(c.color.unwrap()) >= 0.229, "colour {n}");
        }
        // Light mode keeps them (they read on white).
        let blue = ansi_highlight(&AnsiStyle { fg: Some(AnsiColor::Indexed(21)), ..Default::default() }, false);
        assert_eq!(blue.color, Some(ansi_color(AnsiColor::Indexed(21), false)));
        // Yellow darkens on white; a <mark>'s pale fill gets dark ink.
        assert!(luminance(readable_text_color(gpui_kit::rgb(0xffff00).into(), false)) <= 0.301);
        assert!(luminance(ink_on(gpui_kit::rgb(0xfef08a).into())) < 0.05);
    }

    #[test]
    fn script_only_html() {
        assert!(is_script_only("<script>window.PlotlyConfig = {};</script>\n<script src='x.js'></script>"));
        assert!(is_script_only("  <style>.a{}</style><link rel=\"stylesheet\" href=\"x\"> <script>1</script>"));
        assert!(!is_script_only("<div id=\"plot\"></div><script>draw()</script>"));
        assert!(!is_script_only("<script>1</script> Loading BokehJS ..."));
        assert!(is_bokeh_banner("<style>.bk-notebook-logo{}</style><div class=\"bk-notebook-logo\"></div><span>Loading BokehJS ...</span>"));
        assert!(!is_bokeh_banner("<div id=\"plot\"></div>"));
    }
}
