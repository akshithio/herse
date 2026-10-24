//! One open notebook: its document, the virtualized cell list, selection,
//! command/edit modes and (later) its kernel session.

pub mod agent_tools;
mod animation;
pub mod changes;
pub mod chart_style;
pub mod comments;
pub mod live;
pub mod document;
mod edit;
pub mod find;
pub mod frame_fetch;
pub mod highlight;
pub mod intellisense;
pub mod markdown;
pub mod math;
pub mod outputs;
pub mod preflight;
mod widget_view;
pub mod session;
pub mod table;

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenu},
    scroll::Scrollbar,
    text::{TextView, TextViewState},
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ListAlignment, ListState, MouseButton,
    MouseDownEvent, ObjectFit, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    StyledImage as _, Styled as _, Subscription, Task, Window, div, img, list,
    prelude::FluentBuilder as _, px,
};
use herse_kernel::{KernelEvent, KernelStatus};
use herse_nb::{Cell, CellType, Output};
use session::Session;

use crate::{actions::*, theme};
use document::{Document, SaveOutcome};
use highlight::Highlights;
use outputs::{OutputItem, TextKind};

/// Outputs longer than this render clipped, with a control to show the rest.
const MAX_OUTPUT_ITEMS: usize = 100;
/// Advance of one SF Mono / Menlo glyph at the code size, slightly generous.
fn mono_advance() -> f32 {
    theme::mono_size().as_f32() * 0.62
}
/// The cell editor's built-in text inset at gpui-kit's default input size (measured).
const EDITOR_PAD_LEFT: gpui_kit::Pixels = px(12.);
const EDITOR_PAD_Y: gpui_kit::Pixels = px(8.);
const MAX_VIEWER_LINE_BYTES: usize = 4096;
const AUTOSAVE_DELAY: Duration = Duration::from_millis(1000);

pub enum NotebookEvent {
    /// Title, dirty flag, or status line changed.
    Changed,
    /// A link in Markdown pointed at another notebook (and maybe a heading in it).
    OpenNotebook(PathBuf, Option<String>),
    /// A link pointed at a notebook on the web.
    OpenLink(String),
    /// A figure was clicked: show it large.
    ShowImage(Arc<gpui_kit::Image>, f32, f32),
    /// A DataFrame output's Open: show it full size.
    OpenFrame(OpenFrame),
}

/// Which DataFrame output to open full size.
pub struct OpenFrame {
    pub table: Arc<herse_table::Table>,
    pub cell_uid: u64,
    pub output: usize,
}

impl EventEmitter<NotebookEvent> for NotebookView {}

pub struct NotebookView {
    pub doc: Document,
    cells: Vec<CellUi>,
    list: ListState,
    selected: usize,
    /// Other end of a shift-extended selection.
    anchor: Option<usize>,
    editing: Option<Editing>,
    focus_handle: FocusHandle,
    grammar: SharedString,
    next_uid: u64,
    undo: Vec<Step>,
    /// What undo took back, for redo; any new edit clears it.
    redo: Vec<Step>,
    autosave: Option<Task<()>>,
    pub status: Option<SharedString>,
    status_task: Option<Task<()>>,
    _highlight_task: Option<Task<()>>,
    find: Option<FindBar>,
    session: Option<Session>,
    _kernel_task: Option<Task<()>>,
    stdin: Option<Entity<InputState>>,
    _stdin_subscription: Option<Subscription>,
    /// Re-render once a second while something runs, for the elapsed timers.
    _tick: Option<Task<()>>,
    /// The list's width at the last layout.
    viewport_width: gpui_kit::Pixels,
    /// Whether derived colours (syntax runs, ANSI) were built for a dark theme.
    built_dark: bool,
    _theme_observer: Option<Subscription>,
    /// Cells inside a collapsed heading section; recomputed when collapse
    /// state or structure changes.
    hidden: Rc<Vec<bool>>,
    hidden_dirty: bool,
    /// Bumped on every change to the document's cells.
    doc_rev: u64,
    outline_cache: std::cell::RefCell<Option<(u64, crate::sidebar::Outline)>>,
    /// Widget models: saved in the notebook metadata (parsed on first need)
    /// plus those the kernel opened this session.
    widget_state: Option<Rc<herse_widgets::WidgetState>>,
    widget_state_loaded: bool,
    /// The kernel changed widgets since the last save: write their state.
    widgets_dirty: bool,
    /// Code cells show line numbers (`Shift-L`; cells can override with `L`).
    line_numbers: bool,
    /// The widget slider being dragged (its model id).
    widget_drag: Option<String>,
    /// Widget text fields being typed in, by model id.
    widget_inputs: HashMap<String, (Entity<InputState>, Subscription)>,
    /// A plain left press on a cell (its uid, where): a release in place on
    /// code edits it there.
    press: Option<(u64, gpui_kit::Point<gpui_kit::Pixels>)>,
    /// A press landed on a cell (set by the cell, read by the page behind it).
    cell_pressed: bool,
    /// The press landed in the open editor (set by the editor, read by its cell).
    press_in_editor: bool,
    /// The editor caret the page last scrolled for: (cell uid, byte offset).
    followed_caret: Option<(u64, usize)>,
    /// A failure shown above the notebook until dismissed.
    problem: Option<Problem>,
    /// Threads about cells, and the panel showing them.
    comments: comments::Comments,
    /// The live session this notebook is in, if any.
    live: Option<live::Live>,
    /// Set while recording a change the session already has.
    live_quiet: bool,
    /// The name an untitled notebook goes by: the host's name for it in a
    /// live session, or the file name of the link it was opened from.
    name_hint: Option<String>,
    /// The URL of a notebook opened from a link. Its relative images and
    /// links resolve against it.
    source_url: Option<String>,
    /// The notebook as of the last commit, when it is in a git repository.
    baseline: Option<Rc<changes::Baseline>>,
    /// What changed since then, as of edit `changes_rev`.
    changes: changes::Changes,
    changes_rev: Option<u64>,
    /// Changed cells showing their old lines (by uid), and the places
    /// showing the cells removed there (by the index they sit above).
    diff_open: std::collections::HashSet<u64>,
    removed_open: std::collections::HashSet<usize>,
    show_changes: bool,
    baseline_task: Option<Task<()>>,
    /// Imports the kernel lacks, and installs under way.
    preflight: preflight::State,
    /// Where a coding agent is working, and the timer that clears the mark.
    agent_mark: Option<agent_tools::AgentMark>,
    agent_mark_task: Option<Task<()>>,
    /// Bumped on every selection, so a heading picked in the outline is
    /// marked only until the selection moves.
    select_count: u64,
    picked_heading: Option<(u64, usize)>,
    /// The extension drawings outputs were built with (see `extensions`).
    ext_generation: u64,
    /// The notebook's names for completion, as of an edit and when built.
    word_index: std::cell::RefCell<Option<(u64, Instant, Rc<intellisense::WordIndex>)>>,
}

/// A failure the reader has to see and may want to copy, such as a kernel
/// that won't start or died: it stays above the notebook, selectable, until
/// dismissed. The status line's messages pass in a few seconds.
struct Problem {
    title: SharedString,
    detail: SharedString,
    kind: ProblemKind,
}

#[derive(Clone, Copy, PartialEq)]
enum ProblemKind {
    /// The kernel is at fault: offer another; a kernel that starts clears it.
    Kernel,
    /// Saving, reloading, exporting: the next save or reload that works clears it.
    File,
    /// A link that wouldn't open: stays until dismissed.
    Link,
}

struct FindBar {
    input: Entity<InputState>,
    /// The replace row's input, while it is open (⌥⌘F).
    replace: Option<Entity<InputState>>,
    needle: find::Needle,
    hits: Vec<find::Hit>,
    current: Option<usize>,
    force_case: bool,
    _subscription: Subscription,
}

struct CellUi {
    uid: u64,
    /// Bumped on every source edit; highlights computed for an older revision are dropped.
    rev: u64,
    highlights: Option<(u64, Highlights)>,
    markdown: Option<Entity<TextViewState>>,
    outputs: Option<Rc<Vec<OutputItem>>>,
    outputs_hidden: bool,
    /// The input is folded to a one-line placeholder (`jupyter.source_hidden`).
    source_hidden: bool,
    /// `L` on this cell; `None` follows the notebook's `Shift-L` setting.
    line_numbers: Option<bool>,
    /// `<details>` blocks (by order) the reader opened or closed.
    details_toggled: HashSet<usize>,
    outputs_expanded: bool,
    /// What the rendered-markdown highlights were last computed for.
    find_mark: Option<(String, bool, usize)>,
    /// Wall time of the last completed run.
    last_run: Option<Duration>,
    /// Longest source line, in chars (recomputed when `rev` changes).
    max_line_chars: std::cell::Cell<usize>,
    max_line_rev: std::cell::Cell<u64>,
    /// Expanded huge outputs: line offsets and scroll state, per output index.
    line_starts: HashMap<usize, Arc<[usize]>>,
    line_scrolls: HashMap<usize, gpui_kit::UniformListScrollHandle>,
    /// Horizontal scroll of wide tables, per output index.
    table_scrolls: HashMap<usize, gpui_kit::ScrollHandle>,
    /// The fix for this cell's error, as of an edit (`doc_rev`).
    fix: Option<(u64, Option<preflight::Fix>)>,
}

struct Editing {
    uid: u64,
    editor: Entity<EditorState>,
    /// The cell's source when editing began: what undo goes back to once
    /// the editor is closed.
    before: String,
    _subscription: Subscription,
    /// Follows the caret (see [`NotebookView::follow_caret`]).
    _observation: Subscription,
}

/// Undo for structural edits: `removed` cells used to live at `at`, and
/// `inserted` cells now occupy that spot.
struct UndoEntry {
    at: usize,
    removed: Vec<Cell>,
    inserted: usize,
    selected: usize,
}

/// One step of the notebook's history.
enum Step {
    Cells(UndoEntry),
    /// The comment threads as they were before a comment was posted,
    /// answered, resolved or deleted.
    Comments(Vec<herse_collab::comments::Thread>),
}

#[derive(Default)]
struct CellClipboard(Vec<Cell>);
impl gpui_kit::Global for CellClipboard {}

impl NotebookView {
    pub fn new(doc: Document, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let count = doc.nb.cells.len();
        let grammar: SharedString = highlight::grammar_for(doc.nb.language().unwrap_or("python")).into();
        let mut this = Self {
            cells: Vec::with_capacity(count),
            list: ListState::new(count, ListAlignment::Top, px(150.)),
            selected: 0,
            anchor: None,
            editing: None,
            focus_handle: cx.focus_handle(),
            grammar,
            next_uid: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            autosave: None,
            status: None,
            status_task: None,
            _highlight_task: None,
            find: None,
            session: None,
            _kernel_task: None,
            stdin: None,
            _stdin_subscription: None,
            _tick: None,
            viewport_width: px(0.),
            built_dark: false,
            _theme_observer: None,
            hidden: Rc::new(Vec::new()),
            hidden_dirty: true,
            doc_rev: 0,
            outline_cache: std::cell::RefCell::new(None),
            widget_state: None,
            widget_state_loaded: false,
            widgets_dirty: false,
            line_numbers: crate::settings::line_numbers(),
            widget_drag: None,
            widget_inputs: HashMap::new(),
            press: None,
            cell_pressed: false,
            press_in_editor: false,
            followed_caret: None,
            problem: None,
            comments: comments::Comments::load(&doc.nb),
            live: None,
            live_quiet: false,
            name_hint: None,
            source_url: None,
            baseline: None,
            changes: Default::default(),
            changes_rev: None,
            diff_open: Default::default(),
            removed_open: Default::default(),
            show_changes: crate::settings::show_changes(),
            baseline_task: None,
            preflight: Default::default(),
            agent_mark: None,
            agent_mark_task: None,
            word_index: Default::default(),
            ext_generation: 0,
            select_count: 0,
            picked_heading: None,
            doc,
        };
        for _ in 0..count {
            let ui = this.new_cell_ui();
            this.cells.push(ui);
        }
        this.collapse_from_metadata();
        this.built_dark = cx.theme().is_dark();
        this._theme_observer = Some(cx.observe_global::<gpui_kit::component::Theme>(|this, cx| this.theme_changed(cx)));
        this.schedule_highlights(cx);
        this.refresh_baseline(cx);
        this.check_packages(cx);
        let _ = window;
        this
    }

    /// Fold what the notebook says is folded: JupyterLab's
    /// `jupyter.source_hidden` / `outputs_hidden`, the classic notebook's
    /// `collapsed`, and Jupyter Book's `hide-input` / `hide-output` tags.
    /// Toggling them later is view state; the file isn't rewritten.
    fn collapse_from_metadata(&mut self) {
        for (cell, ui) in self.doc.nb.cells.iter().zip(&mut self.cells) {
            let m = &cell.metadata;
            let jupyter = |k: &str| {
                m.get("jupyter").and_then(|j| j.get(k)).and_then(|v| v.as_bool()).unwrap_or(false)
            };
            let tagged = |t: &str| {
                m.get("tags")
                    .and_then(|v| v.as_array())
                    .is_some_and(|tags| tags.iter().any(|x| x.as_str() == Some(t)))
            };
            let collapsed = m.get("collapsed").and_then(|v| v.as_bool()).unwrap_or(false);
            ui.source_hidden = jupyter("source_hidden") || tagged("hide-input");
            ui.outputs_hidden = cell.cell_type == CellType::Code
                && !cell.outputs.is_empty()
                && (jupyter("outputs_hidden") || collapsed || tagged("hide-output"));
        }
    }

    /// Light/dark switched (by the user or by macOS): syntax runs and ANSI
    /// colours were resolved for the old theme, so rebuild them.
    fn theme_changed(&mut self, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        if dark == self.built_dark {
            return;
        }
        self.built_dark = dark;
        for ui in &mut self.cells {
            ui.highlights = None;
            ui.outputs = None;
            ui.line_starts.clear();
        }
        // Figures drawn from now on take the new theme's ink.
        if let Some(session) = self.session.as_ref().filter(|s| s.python)
            && let Some(code) = chart_style::theme_code(dark)
        {
            session.kernel.execute_with(&code, session::silent());
        }
        // Mermaid diagrams are drawn in the theme's ink.
        for ix in 0..self.doc.nb.cells.len() {
            if markdown::has_mermaid(&self.doc.nb.cells[ix].source) {
                self.refresh_markdown(ix, cx);
            }
        }
        self.schedule_highlights(cx);
        cx.notify();
    }

    fn new_cell_ui(&mut self) -> CellUi {
        self.next_uid += 1;
        CellUi {
            uid: self.next_uid,
            rev: 0,
            highlights: None,
            markdown: None,
            outputs: None,
            outputs_hidden: false,
            source_hidden: false,
            line_numbers: None,
            details_toggled: HashSet::new(),
            outputs_expanded: false,
            find_mark: None,
            last_run: None,
            max_line_chars: std::cell::Cell::new(0),
            max_line_rev: std::cell::Cell::new(u64::MAX),
            line_starts: HashMap::new(),
            line_scrolls: HashMap::new(),
            table_scrolls: HashMap::new(),
            fix: None,
        }
    }

    pub fn title(&self) -> String {
        match (&self.doc.path, &self.name_hint) {
            (None, Some(title)) => title.clone(),
            _ => self.doc.title(),
        }
    }

    /// Text sizes changed: every cell's height is stale.
    pub fn zoom_changed(&mut self, cx: &mut Context<Self>) {
        self.list.remeasure();
        cx.notify();
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.doc.path.as_ref()
    }

    pub fn cell_count(&self) -> usize {
        self.doc.nb.cells.len()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Headings for the outline panel (cached per document revision) and the
    /// index of the heading whose section holds the selected cell.
    pub fn outline(&self) -> (crate::sidebar::Outline, Option<usize>) {
        let mut cache = self.outline_cache.borrow_mut();
        let outline = match &*cache {
            Some((rev, outline)) if *rev == self.doc_rev => outline.clone(),
            _ => {
                let outline: crate::sidebar::Outline = Rc::new(self.headings());
                *cache = Some((self.doc_rev, outline.clone()));
                outline
            }
        };
        // The section you're in: the selected cell's own first heading when it
        // has some (a title cell with a run of headings is at its title, not
        // its last subheading), else the last heading before it.
        let first_here = outline.partition_point(|(cell, _, _)| *cell < self.selected);
        // A heading picked in the outline stays marked until the selection moves.
        if let Some((count, heading)) = self.picked_heading
            && count == self.select_count
            && outline.get(heading).is_some_and(|(cell, _, _)| *cell == self.selected)
        {
            return (outline, Some(heading));
        }
        let current = if outline.get(first_here).is_some_and(|(cell, _, _)| *cell == self.selected) {
            Some(first_here)
        } else {
            first_here.checked_sub(1)
        };
        (outline, current)
    }

    /// Markdown headings as (cell index, level, text), in document order.
    pub fn headings(&self) -> Vec<(usize, usize, SharedString)> {
        let mut out = Vec::new();
        for (ix, cell) in self.doc.nb.cells.iter().enumerate() {
            if cell.cell_type != CellType::Markdown {
                continue;
            }
            let mut in_fence = false;
            for line in cell.source.lines() {
                let t = line.trim_start();
                if t.starts_with("```") || t.starts_with("~~~") {
                    in_fence = !in_fence;
                    continue;
                }
                if in_fence {
                    continue;
                }
                let level = t.bytes().take_while(|&b| b == b'#').count();
                if (1..=6).contains(&level) && t.as_bytes().get(level) == Some(&b' ') {
                    let text = t[level..].trim().trim_end_matches('#').trim();
                    if !text.is_empty() {
                        out.push((ix, level, SharedString::from(text.to_string())));
                    }
                }
            }
        }
        out
    }

    /// Open or close a Markdown cell's `<details>` block.
    fn toggle_details(&mut self, ix: usize, block: usize, cx: &mut Context<Self>) {
        let Some(ui) = self.cells.get_mut(ix) else { return };
        if !ui.details_toggled.remove(&block) {
            ui.details_toggled.insert(block);
        }
        let cell = &self.doc.nb.cells[ix];
        let dark = cx.theme().is_dark();
        let source = markdown::prepare(&cell.source, cell.attachments.as_ref(), markdown::Base::of(self.doc.directory(), self.source_url.as_deref()), &ui.details_toggled, dark);
        if let Some(state) = &ui.markdown {
            state.update(cx, |s, cx| s.set_text(&source, cx));
        }
        cx.notify();
    }

    /// Markdown links: `#heading` jumps within the notebook, a notebook (a
    /// relative `.ipynb`, or one on GitHub, nbviewer, Colab…) opens in
    /// Herse, everything else goes to the browser.
    pub fn follow_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(anchor) = url.strip_prefix('#') {
            self.jump_to_anchor(anchor, window, cx);
            return;
        }
        // A notebook from the web: relative links point next to it.
        let url = match &self.source_url {
            Some(base) if !url.contains("://") && !url.starts_with("mailto:") => crate::remote::join(base, url),
            _ => url.to_string(),
        };
        let url = url.as_str();
        if (url.starts_with("https://") || url.starts_with("http://")) && crate::remote::is_notebook_link(url) {
            cx.emit(NotebookEvent::OpenLink(url.to_string()));
            return;
        }
        let (target, anchor) = match url.split_once('#') {
            Some((t, a)) => (t, Some(a.to_string())),
            None => (url, None),
        };
        let is_local = !target.contains("://") || target.starts_with("file://");
        if is_local && target.to_ascii_lowercase().ends_with(".ipynb") {
            let raw = target.strip_prefix("file://").unwrap_or(target);
            let decoded = raw.replace("%20", " ");
            let path = match self.doc.directory() {
                Some(dir) if !decoded.starts_with('/') => dir.join(&decoded),
                _ => PathBuf::from(&decoded),
            };
            cx.emit(NotebookEvent::OpenNotebook(path, anchor));
            return;
        }
        if is_local && !target.starts_with("file://") && !target.starts_with("mailto:") {
            // A relative link to some other file: show it in Finder.
            if let Some(dir) = self.doc.directory() {
                cx.reveal_path(&dir.join(target));
            }
            return;
        }
        cx.open_url(url);
    }

    /// Headings match an anchor the way Jupyter and GitHub slug them.
    pub fn jump_to_anchor(&mut self, anchor: &str, window: &mut Window, cx: &mut Context<Self>) {
        let want = anchor.replace("%20", " ");
        let slug = |s: &str| -> String {
            s.trim()
                .to_lowercase()
                .chars()
                .filter_map(|c| match c {
                    ' ' | '-' => Some('-'),
                    c if c.is_alphanumeric() || c == '_' => Some(c),
                    _ => None,
                })
                .collect()
        };
        let want_slug = slug(&want);
        let hit = self
            .headings()
            .into_iter()
            .find(|(_, _, text)| text.as_ref() == want || slug(text) == want_slug)
            .map(|(cell, _, _)| cell)
            // `<a id="...">` anchors in a cell's HTML.
            .or_else(|| {
                let needle1 = format!("id=\"{want}\"");
                let needle2 = format!("name=\"{want}\"");
                self.doc.nb.cells.iter().position(|c| c.source.contains(&needle1) || c.source.contains(&needle2))
            });
        match hit {
            Some(cell) => self.jump_to_cell(cell, window, cx),
            None => self.flash(format!("No heading “{want}” in this notebook"), cx),
        }
    }

    /// Arriving from workspace search: show the cell with the query highlighted.
    pub fn reveal_search(&mut self, ix: usize, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_cell(ix, window, cx);
        self.on_find(&Find, window, cx);
        if let Some(bar) = &self.find {
            let input = bar.input.clone();
            let query = query.to_string();
            input.update(cx, |state, cx| state.set_value(query, window, cx));
        }
        let q = query.to_string();
        self.update_find(&q, cx);
        // Prefer the first hit in the target cell over the first after the selection.
        if let Some(bar) = &mut self.find
            && let Some(pos) = bar.hits.iter().position(|h| h.cell == ix)
        {
            bar.current = Some(pos);
        }
        self.focus_handle.focus(window, cx);
    }

    // ---------------------------------------------------------------- collapsible headings

    /// Heading level of a markdown cell whose first line is a heading.
    fn heading_level(&self, ix: usize) -> Option<usize> {
        let cell = self.doc.nb.cells.get(ix)?;
        if cell.cell_type != CellType::Markdown {
            return None;
        }
        let line = cell.source.lines().find(|l| !l.trim().is_empty())?.trim_start();
        let level = line.bytes().take_while(|&b| b == b'#').count();
        ((1..=6).contains(&level) && line.as_bytes().get(level) == Some(&b' ')).then_some(level)
    }

    /// JupyterLab keeps collapse state in `metadata.heading_collapsed`.
    fn is_collapsed(&self, ix: usize) -> bool {
        self.doc.nb.cells.get(ix).is_some_and(|c| {
            c.metadata.get("heading_collapsed").and_then(|v| v.as_bool()).unwrap_or(false)
        })
    }

    fn ensure_hidden(&mut self) {
        if !self.hidden_dirty && self.hidden.len() == self.doc.nb.cells.len() {
            return;
        }
        let n = self.doc.nb.cells.len();
        let mut hidden = vec![false; n];
        // Stack of collapsed heading levels covering the current position.
        let mut collapsed_at: Option<usize> = None;
        #[allow(clippy::needless_range_loop)] // `ix` also indexes the cells
        for ix in 0..n {
            let level = self.heading_level(ix);
            if let (Some(l), Some(c)) = (level, collapsed_at)
                && l <= c
            {
                collapsed_at = None;
            }
            if collapsed_at.is_some() {
                hidden[ix] = true;
                continue;
            }
            if let Some(l) = level
                && self.is_collapsed(ix)
            {
                collapsed_at = Some(l);
            }
        }
        self.hidden = Rc::new(hidden);
        self.hidden_dirty = false;
    }

    fn is_hidden(&self, ix: usize) -> bool {
        self.hidden.get(ix).copied().unwrap_or(false)
    }

    /// How many cells a collapsed heading hides.
    fn hidden_under(&self, ix: usize) -> usize {
        self.hidden[ix + 1..].iter().take_while(|h| **h).count()
    }

    fn set_collapsed(&mut self, ix: usize, collapsed: bool, cx: &mut Context<Self>) {
        if self.heading_level(ix).is_none() || self.is_collapsed(ix) == collapsed {
            return;
        }
        let meta = &mut self.doc.nb.cells[ix].metadata;
        if collapsed {
            meta.insert("heading_collapsed".into(), serde_json::Value::Bool(true));
        } else {
            meta.remove("heading_collapsed");
        }
        self.hidden_dirty = true;
        self.ensure_hidden();
        // Every cell after the heading may have changed height (to or from zero).
        let n = self.doc.nb.cells.len();
        self.list.splice(ix..n, n - ix);
        self.mark_dirty(cx);
        cx.notify();
    }

    /// The heading whose section contains `ix` (itself if it is one).
    fn section_heading(&self, ix: usize) -> Option<usize> {
        (0..=ix).rev().find(|&i| self.heading_level(i).is_some())
    }

    fn on_collapse(&mut self, _: &CollapseSection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.section_heading(self.selected) else { return };
        if h != self.selected || self.is_collapsed(h) {
            // Already collapsed, or inside a section: go to its heading.
            self.select(h, false, cx);
            if h == self.selected && !self.is_collapsed(h) {
                self.set_collapsed(h, true, cx);
            }
            return;
        }
        self.set_collapsed(h, true, cx);
    }

    fn on_expand(&mut self, _: &ExpandSection, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_collapsed(self.selected) {
            self.set_collapsed(self.selected, false, cx);
        }
    }

    fn on_collapse_all(&mut self, _: &CollapseAllHeadings, _: &mut Window, cx: &mut Context<Self>) {
        for ix in 0..self.doc.nb.cells.len() {
            if self.heading_level(ix).is_some() {
                self.doc.nb.cells[ix].metadata.insert("heading_collapsed".into(), serde_json::Value::Bool(true));
            }
        }
        self.after_bulk_collapse(cx);
    }

    fn on_expand_all(&mut self, _: &ExpandAllHeadings, _: &mut Window, cx: &mut Context<Self>) {
        for cell in &mut self.doc.nb.cells {
            cell.metadata.remove("heading_collapsed");
        }
        self.after_bulk_collapse(cx);
    }

    fn has_error(&self, ix: usize) -> bool {
        self.doc.nb.cells[ix].outputs.iter().any(|o| o.is_error())
    }

    fn step_error(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.doc.nb.cells.len();
        let found = if forward {
            (self.selected + 1..n).find(|&i| self.has_error(i))
        } else {
            (0..self.selected).rev().find(|&i| self.has_error(i))
        };
        match found {
            Some(ix) => self.jump_to_cell(ix, window, cx),
            None => {
                let total = (0..n).filter(|&i| self.has_error(i)).count();
                self.flash(
                    match total {
                        0 => "No errors in this notebook".to_string(),
                        _ => format!("No more errors {} ({total} in total)", if forward { "below" } else { "above" }),
                    },
                    cx,
                );
            }
        }
    }

    fn on_next_error(&mut self, _: &NextError, window: &mut Window, cx: &mut Context<Self>) {
        self.step_error(true, window, cx);
    }

    fn on_prev_error(&mut self, _: &PrevError, window: &mut Window, cx: &mut Context<Self>) {
        self.step_error(false, window, cx);
    }

    fn set_all_outputs_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        for ui in &mut self.cells {
            ui.outputs_hidden = hidden;
        }
        let n = self.doc.nb.cells.len();
        self.list.splice(0..n, n);
        cx.notify();
    }

    fn on_hide_all_outputs(&mut self, _: &HideAllOutputs, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_outputs_hidden(true, cx);
    }

    fn on_show_all_outputs(&mut self, _: &ShowAllOutputs, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_outputs_hidden(false, cx);
    }

    /// Fold every code cell's input (a report-style read); Markdown stays.
    fn set_all_code_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        for (cell, ui) in self.doc.nb.cells.iter().zip(&mut self.cells) {
            if cell.cell_type == CellType::Code {
                ui.source_hidden = hidden;
            }
        }
        let n = self.doc.nb.cells.len();
        self.list.splice(0..n, n);
        cx.notify();
    }

    fn on_hide_all_code(&mut self, _: &HideAllCode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_code_hidden(true, cx);
    }

    fn on_show_all_code(&mut self, _: &ShowAllCode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_code_hidden(false, cx);
    }

    fn after_bulk_collapse(&mut self, cx: &mut Context<Self>) {
        self.hidden_dirty = true;
        self.ensure_hidden();
        let n = self.doc.nb.cells.len();
        self.list.reset(n);
        if self.is_hidden(self.selected)
            && let Some(h) = self.section_heading(self.selected)
        {
            self.selected = h;
        }
        self.mark_dirty(cx);
        cx.notify();
    }

    /// Go to the outline's heading `heading`, in cell `cell`. A cell can
    /// hold several headings (a title and its first section): the outline
    /// then marks the one clicked, not the cell's first.
    pub fn jump_to_heading(&mut self, cell: usize, heading: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_cell(cell, window, cx);
        self.picked_heading = Some((self.select_count, heading));
        cx.emit(NotebookEvent::Changed);
    }

    pub fn jump_to_cell(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.select(ix, false, cx);
        self.list.scroll_to(gpui_kit::ListOffset {
            item_ix: ix.min(self.doc.nb.cells.len().saturating_sub(1)),
            offset_in_item: px(0.),
        });
        self.focus_handle.focus(window, cx);
    }

    // ---------------------------------------------------------------- derived state

    /// Highlight every code cell on background threads, in parallel chunks,
    /// starting from the top so what's on screen colours first.
    fn schedule_highlights(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme().highlight_theme.clone();
        let grammar = self.grammar.to_string();
        let jobs: Vec<(u64, u64, String)> = self
            .doc
            .nb
            .cells
            .iter()
            .zip(&self.cells)
            .filter(|(cell, ui)| {
                cell.cell_type == CellType::Code
                    && ui.highlights.as_ref().is_none_or(|(rev, _)| *rev != ui.rev)
            })
            .map(|(cell, ui)| (ui.uid, ui.rev, cell.source.clone()))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let chunk = (jobs.len() / 24).clamp(16, 256);
        let task = cx.spawn(async move |this, cx| {
            let mut pending = Vec::new();
            for batch in jobs.chunks(chunk) {
                let batch = batch.to_vec();
                let theme = theme.clone();
                let grammar = grammar.clone();
                pending.push(cx.background_spawn(async move {
                    batch
                        .into_iter()
                        .map(|(uid, rev, src)| (uid, rev, highlight::highlight(&grammar, &src, &theme)))
                        .collect::<Vec<_>>()
                }));
            }
            for task in pending {
                let results = task.await;
                let Ok(()) = this.update(cx, |this, cx| {
                    for (uid, rev, hl) in results {
                        if let Some(ui) = this.cells.iter_mut().find(|ui| ui.uid == uid)
                            && ui.rev == rev
                        {
                            ui.highlights = Some((rev, hl));
                        }
                    }
                    cx.notify();
                }) else {
                    return;
                };
            }
        });
        self._highlight_task = Some(task);
    }

    fn widget_state(&mut self) -> Option<Rc<herse_widgets::WidgetState>> {
        if !self.widget_state_loaded {
            self.widget_state_loaded = true;
            self.widget_state = herse_widgets::WidgetState::from_notebook_metadata(&self.doc.nb.metadata).map(Rc::new);
        }
        self.widget_state.clone()
    }

    fn widget_state_mut(&mut self) -> &mut herse_widgets::WidgetState {
        self.widget_state();
        Rc::make_mut(self.widget_state.get_or_insert_with(Default::default))
    }

    /// Comm messages build the kernel's widgets; outputs a capturing Output
    /// widget takes go to it instead of the cell. True when consumed.
    fn route_widget_message(
        &mut self,
        parent_msg_id: Option<&str>,
        msg_type: &str,
        content: &serde_json::Value,
        changed: &mut HashSet<String>,
    ) -> bool {
        if msg_type.starts_with("comm_") {
            if let Some(id) = self.widget_state_mut().apply_comm(msg_type, content, parent_msg_id) {
                changed.insert(id);
            }
            return true;
        }
        let Some(parent) = parent_msg_id else { return false };
        if !self.widget_state.as_ref().is_some_and(|w| w.is_capturing(parent)) {
            return false;
        }
        match self.widget_state_mut().capture(parent, msg_type, content) {
            Some(id) => {
                changed.insert(id);
                true
            }
            None => false,
        }
    }

    /// Before a save: store the state of the widgets the outputs show, so the
    /// notebook reopens with them drawn (what nbclient and Colab do). Only
    /// after the kernel changed widgets, so other saves stay byte-exact.
    fn store_widget_state(&mut self) {
        if !self.widgets_dirty {
            return;
        }
        self.widgets_dirty = false;
        let Some(state) = self.widget_state.clone() else { return };
        let mut roots = Vec::new();
        for cell in &self.doc.nb.cells {
            for out in &cell.outputs {
                if let Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } = out
                    && let Some(herse_nb::MimeData::Json(view)) = data.get(herse_widgets::VIEW_MIME)
                    && let Some(id) = herse_widgets::model_id(view)
                {
                    roots.push(id.to_string());
                }
            }
        }
        match state.to_metadata(roots.iter().map(String::as_str)) {
            Some(saved) => {
                self.doc.nb.metadata.insert("widgets".into(), saved);
            }
            None => {
                self.doc.nb.metadata.shift_remove("widgets");
            }
        }
    }

    /// A widget the kernel closed (tqdm's `leave=False`) shows nothing; drop
    /// its output so the saved notebook doesn't bring back its stale text.
    fn drop_closed_widget_outputs(&mut self, changed: &HashSet<String>, touched: &mut HashSet<usize>) {
        let Some(state) = self.widget_state.clone() else { return };
        let closed: HashSet<&String> = changed.iter().filter(|id| state.is_closed(id)).collect();
        if closed.is_empty() {
            return;
        }
        for (ix, cell) in self.doc.nb.cells.iter_mut().enumerate() {
            let before = cell.outputs.len();
            cell.outputs.retain(|o| match o {
                Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => !matches!(
                    data.get(herse_widgets::VIEW_MIME),
                    Some(herse_nb::MimeData::Json(v)) if herse_widgets::model_id(v).is_some_and(|id| closed.contains(&id.to_string()))
                ),
                _ => true,
            });
            if cell.outputs.len() != before {
                touched.insert(ix);
            }
        }
    }

    /// Rebuild the outputs of cells showing any of `changed`.
    fn invalidate_widgets(&mut self, changed: &HashSet<String>) {
        if changed.is_empty() {
            return;
        }
        for ix in 0..self.cells.len() {
            let shows_changed = self.cells[ix].outputs.as_ref().is_some_and(|items| {
                items.iter().any(|item| match item {
                    OutputItem::Widget(ui) => ui.model_ids.iter().any(|id| changed.contains(id)),
                    _ => false,
                })
            });
            // A view drawn as its text fallback before its model existed.
            let views_changed = || {
                self.doc.nb.cells[ix].outputs.iter().any(|o| match o {
                    Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => data
                        .get(herse_widgets::VIEW_MIME)
                        .and_then(|v| match v {
                            herse_nb::MimeData::Json(v) => herse_widgets::model_id(v).map(|id| changed.contains(id)),
                            _ => None,
                        })
                        .unwrap_or(false),
                    _ => false,
                })
            };
            if shows_changed || (self.cells[ix].outputs.is_some() && views_changed()) {
                self.invalidate_outputs(ix);
            }
        }
    }

    fn ensure_derived(&mut self, ix: usize, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        let widgets = if self.cells[ix].outputs.is_none() { self.widget_state() } else { None };
        // Charts are laid out to the content width (at zoom 1: zoom scales them).
        let content = self.viewport_width.min(theme::CONTENT_MAX_WIDTH) - theme::GUTTER_WIDTH - theme::CELL_PAD_X * 2.;
        let chart_width = if content > px(0.) { (content.as_f32() / theme::zoom()).clamp(360., 1000.) } else { 820. };
        let cell = &self.doc.nb.cells[ix];
        let ui = &mut self.cells[ix];
        if cell.cell_type == CellType::Markdown && ui.markdown.is_none() {
            let dark = cx.theme().is_dark();
            let source =
                markdown::prepare(&cell.source, cell.attachments.as_ref(), markdown::Base::of(self.doc.directory(), self.source_url.as_deref()), &ui.details_toggled, dark);
            ui.markdown = Some(cx.new(|cx| TextViewState::markdown(&source, cx).selectable(true)));
        }
        if ui.outputs.is_none() {
            let items = if cell.outputs.is_empty() {
                Vec::new()
            } else {
                outputs::build(&cell.outputs, dark, widgets.as_deref(), chart_width, cx)
            };
            self.cells[ix].outputs = Some(Rc::new(items));
        }
    }

    fn invalidate_outputs(&mut self, ix: usize) {
        if let Some(ui) = self.cells.get_mut(ix) {
            ui.outputs = None;
            ui.line_starts.clear();
        }
    }

    // ---------------------------------------------------------------- rendering

    /// "+ Code" / "+ Markdown" after the last cell (or in an empty
    /// notebook), each with the keys that add one, from the live keymap.
    /// The keys add below the selected cell; the button, at the end.
    fn add_cell_button(&self, id: &'static str, cell_type: CellType, window: &Window, cx: &mut Context<Self>) -> gpui_kit::Stateful<gpui_kit::Div> {
        let (label, what, action): (&str, &str, &dyn gpui_kit::Action) = match cell_type {
            CellType::Markdown => ("+ Markdown", "a Markdown cell", &InsertMarkdownBelow),
            _ => ("+ Code", "a code cell", &InsertCodeBelow),
        };
        let keys = crate::finder::shortcut_text(action, Some(&self.focus_handle), window);
        crate::ui::ghost_button(id, label, cx)
            .gap(px(6.))
            .when_some(keys, |button, keys| {
                let tip: SharedString = format!("{keys} adds {what} below the selected cell").into();
                button
                    .child(crate::picker::keycaps(&keys, cx))
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                let at = this.doc.nb.cells.len();
                this.insert_cell(at, cell_type, true, window, cx);
            }))
    }

    fn render_cell(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let comment_badge = self.comment_badge(ix, cx);
        // Who else is on this cell, in a live session: their names, in their colors.
        let here: Vec<(SharedString, gpui_kit::Hsla)> = self
            .peers_at(ix)
            .into_iter()
            .map(|p| (SharedString::from(p.name.split_whitespace().next().unwrap_or(&p.name).to_string()), live::peer_color(&p.color)))
            .collect();
        let peer_pills = (!here.is_empty()).then(|| {
            div().mt(px(4.)).flex().flex_col().items_end().gap(px(2.)).children(here.into_iter().map(|(name, color)| {
                div()
                    .px(px(5.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .bg(color)
                    .text_size(px(10.))
                    .font_family(theme::UI_FONT)
                    .text_color(gpui_kit::white())
                    .child(name)
            }))
        });
        if ix >= self.doc.nb.cells.len() {
            return div().into_any_element();
        }
        self.ensure_hidden();
        if self.is_hidden(ix) {
            return div().h(px(0.)).into_any_element();
        }
        self.ensure_derived(ix, cx);
        if self.cells[ix].markdown.is_some() {
            self.sync_markdown_find(ix, cx);
        }
        // Changes since the last commit: the margin's mark, a changed
        // cell's old lines when asked, and cells removed above (or below
        // the last).
        let uid = self.cells[ix].uid;
        let change_marker = self.change_marker(ix, uid, cx);
        let cell_diff = self.cell_diff(ix, uid, cx);
        let removed_above = self.removed_note(ix, cx);
        let removed_below = if ix + 1 == self.doc.nb.cells.len() { self.removed_note(ix + 1, cx) } else { None };
        let heading = self.heading_level(ix);
        let collapsed = heading.is_some() && self.is_collapsed(ix);
        let hidden_count = if collapsed { self.hidden_under(ix) } else { 0 };
        let theme = cx.theme();
        let dark = theme.is_dark();
        let cell = &self.doc.nb.cells[ix];
        let ui = &self.cells[ix];
        let uid = ui.uid;
        let selected = self.is_selected(ix);
        let editing = self.editing.as_ref().filter(|e| e.uid == uid);
        let is_editing = editing.is_some();
        let muted = theme.muted_foreground;

        let run = self.run_state(uid);
        let prompt: SharedString = match cell.cell_type {
            CellType::Code => match (run, cell.execution_count) {
                (Some(_), _) => "[*]".into(),
                // Six digits and more don't fit the margin: thousands.
                (None, Some(n)) if n >= 100_000 => format!("[{}k]", n / 1000).into(),
                (None, Some(n)) => format!("[{n}]").into(),
                (None, None) => "[ ]".into(),
            },
            _ => "".into(),
        };
        let timing: Option<SharedString> = match run {
            Some(Some(started)) => Some(session::format_duration(started.elapsed()).into()),
            Some(None) => Some("queued".into()),
            None => ui.last_run.filter(|d| *d >= Duration::from_millis(100)).map(|d| session::format_duration(d).into()),
        };
        let group: SharedString = format!("cell-{uid}").into();
        let disclosure = heading.map(|_| {
            div()
                .id(("disclosure", uid))
                .size(px(18.))
                .mt(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                // Quiet until needed: shown when folded, or on hover.
                .when(!collapsed, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                .hover(|s| s.bg(theme.foreground.opacity(0.06)))
                .child(
                    gpui_kit::svg()
                        .path(if collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" })
                        .size(px(12.))
                        .text_color(if collapsed { theme.foreground } else { muted }),
                )
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    let c = this.is_collapsed(ix);
                    this.set_collapsed(ix, !c, cx);
                }))
        });
        // Run, beside the prompt: on hover or when selected (the original
        // Herse's margin control).
        let run_button = (cell.cell_type == CellType::Code && run.is_none()).then(|| {
            div()
                .id(("run", uid))
                .mt(px(3.))
                .ml_auto()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .when(!selected, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                .hover(|s| s.bg(theme.foreground.opacity(0.06)))
                .child(gpui_kit::svg().path("icons/play.svg").size(px(11.)).text_color(muted))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.select(ix, false, cx);
                    this.execute(ix..ix + 1, cx);
                }))
        });
        // Where you are: a bar of dew in the margin beside the selected cell
        // (a hint of ink on hover). The cell itself never changes size.
        let accent = theme::accent(cx);
        let bar = div().absolute().top(px(1.)).bottom(px(1.)).right(px(5.)).w(px(3.)).rounded(px(1.5));
        let bar = if selected {
            bar.bg(accent)
        } else {
            bar.bg(theme.foreground.opacity(0.12)).opacity(0.).group_hover(group.clone(), |s| s.opacity(1.))
        };
        // Stretched to the row's height (the flex default), so the bar spans
        // the whole cell, outputs included.
        // Add a cell below: a + at the foot of the margin, on hover.
        let add_below = crate::ui::icon_button(("add-below", uid), "icons/plus.svg", false, cx)
            .absolute()
            .left(px(0.))
            .bottom(px(0.))
            .opacity(0.)
            .group_hover(group.clone(), |s| s.opacity(1.))
            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Add a cell below  B").build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(ix) = this.cells.iter().position(|ui| ui.uid == uid) {
                    this.insert_cell(ix + 1, CellType::Code, true, window, cx);
                }
            }));
        let gutter = h_flex_start()
            .relative()
            .w(theme::GUTTER_WIDTH)
            .flex_none()
            .self_stretch()
            .gap(px(4.))
            .child(bar)
            .children(change_marker)
            .child(add_below)
            .when_some(disclosure, |d, disc| d.child(disc))
            .child(
                div()
                    .flex_1()
                    .pr(px(14.))
                    // The prompt's line is the code's first line (inside the
                    // block's border and padding).
                    .pt(theme::CELL_PAD_Y + px(1.))
                    .flex()
                    .flex_col()
                    .items_end()
                    .font_family(theme::mono_font())
                    .text_size(px(11.))
                    .line_height(theme::mono_line_height())
                    .text_color(if run.is_some() { accent } else { muted.opacity(0.8) })
                    .child(prompt)
                    .when_some(timing, |d, t| {
                        d.child(div().text_size(px(10.5)).line_height(px(14.)).text_color(muted.opacity(0.8)).child(t))
                    })
                    .when_some(run_button, |d, b| d.child(b))
                    .when_some(comment_badge, |d, b| d.child(b))
                    .when_some(peer_pills, |d, p| d.child(p)),
            );

        let body: AnyElement = match (cell.cell_type, editing) {
            (_, Some(editing)) => self.render_editor(&editing.editor, cell.cell_type, &cell.source, cx),
            (kind, None) if ui.source_hidden => {
                let lines = cell.source.lines().count();
                let what = match kind {
                    CellType::Code => "code",
                    CellType::Markdown => "text",
                    CellType::Raw => "raw",
                };
                div()
                    .id(("source-hidden", uid))
                    .mx(theme::CELL_PAD_X)
                    .my(px(4.))
                    .py(px(3.))
                    .px(px(8.))
                    .rounded(px(4.))
                    .bg(theme.foreground.opacity(0.04))
                    .text_size(px(12.))
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.foreground).bg(theme.foreground.opacity(0.07)))
                    .child(format!(
                        "⋯ {what} hidden ({lines} line{}) — show",
                        if lines == 1 { "" } else { "s" }
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = this.cells.get_mut(ix) {
                            ui.source_hidden = false;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            (CellType::Code, None) => self.render_code(ix, cx),
            (CellType::Markdown, None) => match &ui.markdown {
                Some(state) if !cell.source.trim().is_empty() => div()
                    .px(theme::CELL_PAD_X)
                    .py(px(2.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(gpui_kit::transparent_black())
                    // A readable measure for prose (~85 characters); code and
                    // outputs keep the full width.
                    .child(div().max_w(px(820. * theme::zoom())).font_family(theme::prose_font()).text_size(theme::prose_size()).child({
                        let this = cx.entity().downgrade();
                        TextView::new(state)
                            .markdown_extensions(math::extensions())
                            .on_link_click(move |url, _, window, cx| {
                                let url = url.to_string();
                                this.update(cx, |nb, cx| {
                                    match url.strip_prefix(markdown::DETAILS_LINK).and_then(|n| n.parse().ok()) {
                                        Some(block) => nb.toggle_details(ix, block, cx),
                                        None => nb.follow_link(&url, window, cx),
                                    }
                                })
                                .ok();
                            })
                    }))
                    .into_any_element(),
                _ => div()
                    .px(theme::CELL_PAD_X)
                    .py(theme::CELL_PAD_Y)
                    .text_color(muted)
                    .italic()
                    .child("Empty markdown cell")
                    .into_any_element(),
            },
            (CellType::Raw, None) => self.render_raw(ix, cx),
        };

        let outputs = self.render_outputs(ix, dark, window, cx);
        let stdin = self.render_stdin(uid, cx);
        let theme = cx.theme();
        let fold_note = (hidden_count > 0).then(|| {
            div()
                .id(("fold-note", uid))
                .mx(theme::CELL_PAD_X)
                .mb(px(4.))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(4.))
                .w_auto()
                .bg(theme.foreground.opacity(0.04))
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.foreground))
                .child(format!("{hidden_count} cell{} hidden", if hidden_count == 1 { "" } else { "s" }))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_collapsed(ix, false, cx);
                }))
        });

        // With the viewport width known, every box gets a definite width:
        // taffy then never measures text at max-content to resolve a flex
        // basis, which on long Markdown paragraphs was most of the layout cost.
        // (Read in `render`, not here: the list's state is borrowed while it
        // renders items.)
        let viewport = self.viewport_width;
        // Right-click on the margin or the source: the cell's commands, with
        // their keys. (Outputs keep their own menu; the editor keeps its own.)
        let menu = self.cell_menu(ix);
        let gutter = {
            let menu = menu.clone();
            gutter.id(("gutter", uid)).context_menu(move |m, w, c| menu(m, w, c))
        };
        let body: AnyElement = if is_editing {
            div()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.press_in_editor = true))
                .child(body)
                .into_any_element()
        } else {
            let menu = menu.clone();
            div().id(("source", uid)).child(body).context_menu(move |m, w, c| menu(m, w, c)).into_any_element()
        };
        let agent_mark = self.render_agent_mark(uid, cx);
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .child(body)
            .when_some(cell_diff, |d, diff| d.child(diff))
            .when_some(outputs, |d, o| d.child(o))
            .when_some(stdin, |d, s| d.child(s))
            .when_some(fold_note, |d, n| d.child(n))
            .when_some(agent_mark, |d, m| d.child(m));
        // Presses belong to the cell's column (margin and body); the page's
        // side margins and the gaps between cells are the page's, where a
        // click puts the editor away.
        let pressable = |d: gpui_kit::Div| {
            d.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| this.click_cell(ix, ev, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| this.right_click_cell(ix, window, cx)),
            )
        };
        let row = if viewport > px(0.) {
            let content = (viewport - px(32.)).min(theme::CONTENT_MAX_WIDTH).max(px(200.));
            let margin = ((viewport - content) / 2.).max(px(0.));
            div().w(viewport).pl(margin).py(px(5.)).when_some(removed_above, |d, note| d.child(div().w(content).child(note))).child(pressable(
                div()
                    .flex()
                    .flex_row()
                    .w(content)
                    .child(gutter)
                    .child(body.w(content - theme::GUTTER_WIDTH)),
            ))
        } else {
            // First frame, before the list has a size: flexible layout.
            div().w_full().flex().justify_center().px(px(16.)).py(px(5.)).child(pressable(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .max_w(theme::CONTENT_MAX_WIDTH)
                    .child(gutter)
                    .child(body.flex_1().min_w_0()),
            ))
        };
        // After the last cell: add one, either kind.
        let last = ix + 1 == self.doc.nb.cells.len();
        let row = row.when_some(removed_below, |row, note| row.child(div().pt(px(6.)).child(note)));
        let row = row.when(last, |row| {
            row.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(4.))
                    .pt(px(14.))
                    .pl(theme::GUTTER_WIDTH + theme::CELL_PAD_X - px(9.))
                    .child(self.add_cell_button("add-code-end", CellType::Code, window, cx))
                    .child(self.add_cell_button("add-markdown-end", CellType::Markdown, window, cx)),
            )
        });
        row.id(("cell", uid))
            .group(group)
            .into_any_element()
    }

    fn render_code(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let cell = &self.doc.nb.cells[ix];
        let ui = &self.cells[ix];
        if ui.max_line_rev.get() != ui.rev {
            let longest = cell.source.lines().map(|l| l.chars().count()).max().unwrap_or(0);
            ui.max_line_chars.set(longest);
            ui.max_line_rev.set(ui.rev);
        }
        let source: SharedString = if cell.source.is_empty() {
            " ".into()
        } else {
            cell.source.clone().into()
        };
        let syntax: &[(Range<usize>, gpui_kit::HighlightStyle)] =
            match ui.highlights.as_ref().filter(|(rev, _)| *rev == ui.rev) {
                Some((_, hl)) if !cell.source.is_empty() => hl,
                _ => &[],
            };
        let found = self.find_ranges_in_source(ix, &cell.source, theme.is_dark());
        // Selectable: drag to select and Cmd-C, without entering the editor.
        // The id carries the revision so a retained selection never copies
        // stale text.
        let id = ("code", (ui.uid << 24) | (ui.rev & 0xff_ffff));
        let text = if found.is_empty() {
            gpui_kit::base::SelectableText::new(id, source).with_highlights(syntax.iter().cloned())
        } else {
            gpui_kit::base::SelectableText::new(id, source)
                .with_highlights(gpui_kit::combine_highlights(syntax.iter().cloned(), found))
        };
        // Code doesn't wrap (as in JupyterLab, and as the editor shows it);
        // a long line scrolls the box sideways instead. The scroll container
        // measures its content every frame, so only cells that need it get one.
        let numbers = self.shows_line_numbers(ix).then(|| line_numbers(&cell.source));
        // The editor sizes its gutter from the shaped digits; use the font's
        // exact advance so the text lands on the same pixel.
        let numbers_width = numbers.as_ref().map_or(px(0.), |(_, digits)| {
            let ts = cx.text_system();
            let font = ts.resolve_font(&gpui_kit::font(theme::mono_font()));
            let advance = ts.advance(font, theme::mono_size(), '+').map_or(px(mono_advance()), |a| a.width);
            advance * *digits as f32
        });
        let width = self.viewport_width.min(theme::CONTENT_MAX_WIDTH) - theme::GUTTER_WIDTH - theme::CELL_PAD_X * 2. - numbers_width;
        let fits = width > px(0.) && ui.max_line_chars.get() as f32 * mono_advance() <= width.as_f32();
        let uid = ui.uid;
        let view = cx.entity().downgrade();
        let text = text.on_click(move |offset, _, window, cx| {
            view.update(cx, |this, cx| this.edit_at(uid, Some(offset), window, cx)).ok();
        });
        let code = mono().whitespace_nowrap().child(text);
        let content = if fits {
            code.into_any_element()
        } else {
            div()
                .id(("code-scroll", ui.uid))
                .overflow_x_scroll()
                .child(code)
                .into_any_element()
        };
        let boxed = code_box(code_background(cx), gpui_kit::transparent_black())
            .cursor_text()
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, ev, window, cx| this.release_on_code(uid, ev, window, cx)));
        match numbers {
            // Where the editor draws its gutter: the numbers start a margin
            // left of where unnumbered text starts, and the text follows them
            // after the same margin, so entering edit mode moves nothing.
            Some((numbers, _)) => boxed
                .pl(theme::CELL_PAD_X - LINE_NUMBER_MARGIN)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .child(
                            mono()
                                .flex_none()
                                .w(numbers_width + LINE_NUMBER_MARGIN)
                                .whitespace_nowrap()
                                .text_color(theme.muted_foreground)
                                .child(numbers),
                        )
                        .child(div().flex_1().min_w(px(0.)).child(content)),
                )
                .into_any_element(),
            None => boxed.child(content).into_any_element(),
        }
    }

    /// Find highlights for a cell's source, the current hit emphasized.
    fn find_ranges_in_source(&self, ix: usize, source: &str, dark: bool) -> Vec<(Range<usize>, gpui_kit::HighlightStyle)> {
        let Some(bar) = &self.find else { return Vec::new() };
        if bar.needle.is_empty() {
            return Vec::new();
        }
        let current = bar
            .current
            .and_then(|c| bar.hits.get(c))
            .filter(|h| h.cell == ix && h.target == find::Target::Source)
            .map(|h| h.range.clone());
        bar.needle
            .find_all(source)
            .into_iter()
            .map(|r| {
                let is_current = current.as_ref() == Some(&r);
                (r, find::match_style(is_current, dark))
            })
            .collect()
    }

    /// Keep a markdown cell's rendered-text highlights in step with the query.
    fn sync_markdown_find(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(state) = self.cells[ix].markdown.clone() else { return };
        let dark = cx.theme().is_dark();
        let want = self
            .find
            .as_ref()
            .filter(|b| !b.needle.is_empty())
            .map(|b| (b.needle.text.clone(), b.needle.case_sensitive));
        let rendered_len = state.read(cx).rendered_text().len();
        let mark = want.as_ref().map(|(t, c)| (t.clone(), *c, rendered_len));
        if self.cells[ix].find_mark == mark {
            return;
        }
        self.cells[ix].find_mark = mark;
        match want {
            Some((text, case)) => {
                let needle = find::Needle { text, case_sensitive: case };
                let ranges = needle.find_all(state.read(cx).rendered_text().as_str());
                let bg = find::match_background(false, dark);
                state.update(cx, |s, cx| {
                    let _ = s.set_range_highlights(
                        ranges.into_iter().map(|r| gpui_kit::component::text::RangeHighlight::new(r, bg)),
                        cx,
                    );
                });
            }
            None => state.update(cx, |s, cx| s.clear_range_highlights(cx)),
        }
    }

    fn render_raw(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let cell = &self.doc.nb.cells[ix];
        let source: SharedString = if cell.source.is_empty() {
            " ".into()
        } else {
            cell.source.clone().into()
        };
        // Raw text keeps a hairline: it is neither prose nor code.
        let border = theme.border;
        let uid = self.cells[ix].uid;
        let view = cx.entity().downgrade();
        let text = gpui_kit::base::SelectableText::new(("raw", (uid << 24) | (self.cells[ix].rev & 0xff_ffff)), source)
            .on_click(move |offset, _, window, cx| {
                view.update(cx, |this, cx| this.edit_at(uid, Some(offset), window, cx)).ok();
            });
        code_box(theme.background, border)
            .cursor_text()
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, ev, window, cx| this.release_on_code(uid, ev, window, cx)))
            .child(mono().text_color(theme.muted_foreground).child(text))
            .into_any_element()
    }

    fn render_editor(
        &self,
        editor: &Entity<EditorState>,
        cell_type: CellType,
        source: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let rows = source_rows(source);
        let theme = cx.theme();
        let bg = if cell_type == CellType::Code {
            code_background(cx)
        } else {
            theme.background
        };
        // The editor insets its text by its own padding (12px left, 8px top
        // and bottom at the default size); shrink ours by the same amount so
        // the text sits exactly where the static view drew it.
        let accent = theme::accent(cx);
        code_box(bg, accent)
            .shadow(vec![gpui_kit::BoxShadow {
                color: accent.opacity(0.18),
                offset: gpui_kit::point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }])
            .px(theme::CELL_PAD_X - EDITOR_PAD_LEFT)
            .py(theme::CELL_PAD_Y - EDITOR_PAD_Y)
            .child(
                mono().child(
                    // Same font, size and line height as the static view.
                    Editor::new(editor)
                        .appearance(false)
                        .bordered(false)
                        .font_family(theme::mono_font())
                        .text_size(theme::mono_size())
                        .line_height(theme::mono_line_height())
                        .h(theme::mono_line_height() * rows as f32 + EDITOR_PAD_Y * 2.),
                ),
            )
            .into_any_element()
    }

    fn render_outputs(
        &mut self,
        ix: usize,
        dark: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ui = &self.cells[ix];
        let items = ui.outputs.clone()?;
        if items.is_empty() {
            return None;
        }
        let theme = cx.theme();
        if ui.outputs_hidden {
            return Some(
                div()
                    .id(("outputs-hidden", ui.uid))
                    .px(theme::CELL_PAD_X)
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .child(format!("{} hidden output{} — press O to show", items.len(), if items.len() == 1 { "" } else { "s" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = this.cells.get_mut(ix) {
                            ui.outputs_hidden = false;
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let expanded = ui.outputs_expanded;
        let uid = ui.uid;
        let mut col = div().flex().flex_col().gap(px(6.)).pt(px(6.)).pb(px(2.));
        let shown = if expanded { items.len() } else { items.len().min(MAX_OUTPUT_ITEMS) };
        for (n, item) in items.iter().take(shown).enumerate() {
            let el = self.render_output_item(ix, n, item, dark, expanded, theme::CELL_PAD_X, cx);
            col = col.child(output_menu(item, el, uid * 1000 + n as u64));
        }
        if shown < items.len() {
            let theme = cx.theme();
            let more = items.len() - shown;
            col = col.child(
                div()
                    .id(("more-outputs", uid))
                    .mx(theme::CELL_PAD_X)
                    .py(px(3.))
                    .px(px(8.))
                    .rounded(px(4.))
                    .bg(theme.foreground.opacity(0.04))
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.foreground))
                    .child(format!("{} more outputs — show all", group_digits(more)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = this.cells.get_mut(ix) {
                            ui.outputs_expanded = true;
                        }
                        cx.notify();
                    })),
            );
        }
        if let Some(fix) = self.render_fix(ix, cx) {
            col = col.child(fix);
        }
        Some(col.into_any_element())
    }

    /// One output; `n` is its index among the cell's outputs (an Output
    /// widget's own outputs get indexes past those), `pad` its side padding.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_output_item(
        &mut self,
        ix: usize,
        n: usize,
        item: &OutputItem,
        dark: bool,
        expanded: bool,
        pad: gpui_kit::Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let uid = self.cells[ix].uid;
        match item {
            OutputItem::Text {
                text,
                runs,
                lines,
                kind,
                clip,
            } => {
                let (bg, fg) = match kind {
                    TextKind::Stderr => (Some(theme::error_background(dark).opacity(0.6)), None),
                    TextKind::Error => (Some(theme::error_background(dark)), None),
                    _ => (None, None::<gpui_kit::Hsla>),
                };
                let needle = self.find.as_ref().filter(|b| !b.needle.is_empty()).map(|b| &b.needle);
                let segment_id = uid * 1000 + n as u64;
                let segment = |range: Range<usize>| -> AnyElement {
                    let id = gpui_kit::ElementId::from(("out-text", (segment_id << 8) | (range.start as u64 & 0xff)));
                    mono()
                        .when_some(fg, |d, fg| d.text_color(fg))
                        .child(styled_segment(id, text, runs, range, needle, dark))
                        .into_any_element()
                };
                let body: AnyElement = match clip {
                    None => segment(0..text.len()),
                    Some(clip) if !expanded => {
                        let hidden = clip.hidden_lines;
                        div()
                            .flex()
                            .flex_col()
                            .child(segment(clip.head.clone()))
                            .child(
                                div()
                                    .id(("show-more", uid * 1000 + n as u64))
                                    .my(px(4.))
                                    .py(px(3.))
                                    .px(px(8.))
                                    .rounded(px(4.))
                                    .bg(theme.foreground.opacity(0.04))
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.foreground).bg(theme.foreground.opacity(0.07)))
                                    .child(format!(
                                        "{} of {} lines hidden — show all",
                                        group_digits(hidden),
                                        group_digits(*lines)
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(ui) = this.cells.get_mut(ix) {
                                            ui.outputs_expanded = true;
                                        }
                                        cx.notify();
                                    })),
                            )
                            .when(!clip.tail.is_empty(), |d| d.child(segment(clip.tail.clone())))
                            .into_any_element()
                    }
                    Some(_) => self.render_line_viewer(ix, n, text, runs, *lines, fg),
                };
                // Copy, on hover: the whole output (a traceback without its
                // colour codes), whatever is selected.
                let group: SharedString = format!("out-{segment_id}").into();
                let copy = text.clone();
                let copy_button = copy_chip(segment_id, group.clone(), "Copy output", cx, move |cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                    "Copied output"
                });
                div()
                    .relative()
                    .group(group)
                    .flex()
                    .flex_col()
                    .when_some(bg, |d, bg| d.bg(bg).rounded(px(8.)).py(px(6.)))
                    .px(pad)
                    .child(body)
                    .child(copy_button)
                    .into_any_element()
            }
            OutputItem::Image {
                image,
                width,
                height,
                caption,
                light_backing,
            } => {
                let muted = theme.muted_foreground;
                crate::images::touch(image, cx);
                let (image_c, w, h) = (image.clone(), *width, *height);
                let figure_id = uid * 1000 + n as u64;
                let group: SharedString = format!("figure-{figure_id}").into();
                let copy = image.clone();
                let copy_button = copy_chip(figure_id, group.clone(), "Copy image", cx, move |cx| {
                    cx.write_to_clipboard(ClipboardItem::new_image(&copy));
                    "Copied image"
                });
                div()
                    .px(pad)
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(
                        div()
                            .id(("figure", figure_id))
                            .relative()
                            .group(group)
                            .when(*light_backing && dark, |d| d.bg(gpui_kit::white()).rounded(px(6.)).p(px(6.)))
                            .cursor_pointer()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(NotebookEvent::ShowImage(image_c.clone(), w, h));
                            }))
                            .child(
                                img(image.clone())
                                    .w(px(*width * theme::zoom()))
                                    .max_w_full()
                                    .aspect_ratio(*width / *height)
                                    .object_fit(ObjectFit::Contain)
                                    .with_fallback(broken_image(muted)),
                            )
                            // After the image, so it paints on top of it.
                            .child(copy_button),
                    )
                    .when_some(caption.clone(), |d, c| {
                        d.child(div().mt(px(2.)).text_size(px(11.)).italic().text_color(muted).child(c))
                    })
                    .into_any_element()
            }
            OutputItem::Table(t) => {
                let available = (self.viewport_width.min(theme::CONTENT_MAX_WIDTH)
                    - theme::GUTTER_WIDTH
                    - theme::CELL_PAD_X * 2.)
                    .max(px(200.));
                let scroll = self.cells[ix].table_scrolls.entry(n).or_default().clone();
                let table_id = uid * 1000 + n as u64;
                let group: SharedString = format!("table-{table_id}").into();
                let copy = t.table.clone();
                let copy_button = copy_chip(table_id, group.clone(), "Copy as TSV", cx, move |cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(table_tsv(&copy)));
                    "Copied table"
                });
                let open = OpenFrame { table: t.table.clone(), cell_uid: uid, output: n };
                let open_button = table::open_chip(table_id, group.clone(), open, cx);
                div()
                    .px(pad)
                    .relative()
                    .group(group)
                    .child(table::render(t, ("table", table_id), available, &scroll, cx))
                    .child(open_button)
                    .child(copy_button)
                    .into_any_element()
            }
            OutputItem::Rich(state) => div()
                .px(pad)
                .max_w_full()
                .overflow_hidden()
                .child(TextView::new(state).markdown_extensions(math::extensions()))
                .into_any_element(),
            OutputItem::Widget(w) => {
                let w = w.clone();
                self.render_widget(ix, n, &w, dark, pad, cx)
            }
            OutputItem::Animation(anim) => {
                let anim = anim.clone();
                self.render_animation(uid, n, &anim, pad, cx)
            }
            OutputItem::Browser { html, src } => {
                let label: SharedString = match src {
                    Some(url) => {
                        let shown = url.split("://").nth(1).unwrap_or(url).trim_end_matches('/');
                        format!("Embedded page: {shown} — open in browser").into()
                    }
                    None if html.contains("<audio") => "Audio — play in browser".into(),
                    None if html.contains("<video") => "Video — play in browser".into(),
                    None => "Interactive HTML (needs a browser) — open in browser".into(),
                };
                let (html, src) = (html.clone(), src.clone());
                div()
                    .px(pad)
                    .child(
                        div()
                            .id(("open-in-browser", uid * 1000 + n as u64))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .child("⧉")
                            .child(label)
                            .on_click(move |_, _, cx| open_html_in_browser(&html, src.as_deref(), cx)),
                    )
                    .into_any_element()
            }
            OutputItem::Pdf(bytes) => {
                let bytes = bytes.clone();
                div()
                    .px(pad)
                    .child(
                        div()
                            .id(("open-pdf", uid * 1000 + n as u64))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .child("⧉")
                            .child(format!("PDF document ({}) — open", format_bytes(bytes.len())))
                            .on_click(move |_, _, cx| open_temp_file(&bytes, "pdf", cx)),
                    )
                    .into_any_element()
            }
            OutputItem::Note(note) => div()
                .px(pad)
                .text_size(px(12.))
                .italic()
                .text_color(theme.muted_foreground)
                .child(note.clone())
                .into_any_element(),
            // Allowing here is informed: the command it runs is right below.
            OutputItem::ExtensionAsk { extension, name, command } => {
                let extension = extension.clone();
                let id = uid * 1000 + n as u64;
                div()
                    .px(pad)
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .italic()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{name} draws this — Allow?")),
                            )
                            .child(
                                crate::ui::button(("extension-allow", id), "Allow", cx)
                                    .on_click(move |_, _, cx| crate::extensions::answer(&extension, true, cx)),
                            ),
                    )
                    .child(
                        div()
                            .font_family(theme::mono_font())
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .child(gpui_kit::base::SelectableText::new(
                                SharedString::from(format!("extension-ask-{id}")),
                                command.clone(),
                            )),
                    )
                    .into_any_element()
            }
        }
    }

    /// A huge text output, expanded: a fixed-height, virtualized, unwrapped
    /// line view, so 200,000 lines cost what's visible.
    #[allow(clippy::too_many_arguments)]
    fn render_line_viewer(
        &mut self,
        ix: usize,
        n: usize,
        text: &SharedString,
        runs: &outputs::Runs,
        lines: usize,
        fg: Option<gpui_kit::Hsla>,
    ) -> AnyElement {
        let ui = &mut self.cells[ix];
        let starts = ui
            .line_starts
            .entry(n)
            .or_insert_with(|| {
                let mut v = Vec::with_capacity(lines + 1);
                v.push(0);
                v.extend(memchr::memchr_iter(b'\n', text.as_bytes()).map(|i| i + 1));
                Arc::from(v)
            })
            .clone();
        let scroll = ui.line_scrolls.entry(n).or_default().clone();
        let count = starts.len();
        let text = text.clone();
        let runs = runs.clone();
        let height = theme::mono_line_height() * (count.min(32) as f32);
        div()
            .h(height)
            .w_full()
            .child(
                gpui_kit::uniform_list(("line-viewer", self.cells[ix].uid * 1000 + n as u64), count, move |range, _window, _cx| {
                    range
                        .map(|line| {
                            let start = starts[line];
                            let mut end = starts.get(line + 1).map_or(text.len(), |s| s.saturating_sub(1));
                            end = end.max(start);
                            let mut cut = None;
                            if end - start > MAX_VIEWER_LINE_BYTES {
                                let mut e = start + MAX_VIEWER_LINE_BYTES;
                                while !text.is_char_boundary(e) {
                                    e -= 1;
                                }
                                cut = Some(end - e);
                                end = e;
                            }
                            mono()
                                .whitespace_nowrap()
                                .when_some(fg, |d, fg| d.text_color(fg))
                                .child(styled_segment(
                                    gpui_kit::ElementId::from(("line", line)),
                                    &text,
                                    &runs,
                                    start..end,
                                    None,
                                    false,
                                ))
                                .when_some(cut, |d, rest| d.child(format!("  … +{} bytes", group_digits(rest))))
                                .into_any_element()
                        })
                        .collect()
                })
                .with_horizontal_sizing_behavior(gpui_kit::ListHorizontalSizingBehavior::Unconstrained)
                .track_scroll(&scroll)
                .size_full(),
            )
            .into_any_element()
    }

    // ---------------------------------------------------------------- kernel

    /// (kernel display name, status), status `None` when not started.
    pub fn kernel_state(&self) -> Option<(String, Option<KernelStatus>)> {
        match &self.session {
            Some(s) => Some((s.display_name.clone(), Some(s.status))),
            None => self.doc.nb.kernel_display_name().map(|n| (n.to_string(), None)),
        }
    }

    pub fn kernel_label(&self) -> Option<String> {
        match &self.session {
            Some(s) => Some(format!("{} · {}", s.display_name, session::status_label(s.status))),
            None => self.doc.nb.kernel_display_name().map(|n| format!("{n} · not started")),
        }
    }

    /// `Some(None)`: queued; `Some(Some(t))`: running since `t`.
    fn run_state(&self, uid: u64) -> Option<Option<Instant>> {
        let session = self.session.as_ref()?;
        session
            .running
            .values()
            .find(|r| r.cell_uid == uid)
            .map(|r| r.started_at)
    }

    fn pick_spec(&self) -> Option<herse_kernel::KernelSpec> {
        let specs = herse_kernel::discover();
        if let Some(name) = self.doc.nb.kernel_name()
            && let Some(spec) = specs.iter().find(|s| s.name == name)
        {
            return Some(spec.clone());
        }
        if let Some(lang) = self.doc.nb.language()
            && let Some(spec) = specs.iter().find(|s| s.language.eq_ignore_ascii_case(lang))
        {
            return Some(spec.clone());
        }
        herse_kernel::default_spec(&specs).cloned()
    }

    /// Where the kernel runs: beside the notebook, else in the home folder.
    fn kernel_cwd(&self) -> PathBuf {
        self.doc
            .directory()
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn ensure_kernel(&mut self, cx: &mut Context<Self>) -> bool {
        if self.session.is_some() {
            return true;
        }
        let Some(spec) = self.pick_spec() else {
            self.report(
                ProblemKind::Kernel,
                "No Jupyter kernels found",
                "Install one in the environment you work in:\n    python3 -m pip install ipykernel",
                cx,
            );
            return false;
        };
        let cwd = self.kernel_cwd();
        let python = spec.language.eq_ignore_ascii_case("python");
        let options = herse_kernel::LaunchOptions {
            env: if python { chart_style::kernel_env(cx.theme().is_dark()) } else { Vec::new() },
            ..Default::default()
        };
        match herse_kernel::Kernel::launch_with(&spec, &cwd, options) {
            Ok((kernel, events)) => {
                self.session = Some(Session {
                    kernel,
                    name: spec.name.clone(),
                    display_name: spec.display_name.clone(),
                    status: KernelStatus::Starting,
                    running: HashMap::new(),
                    clear_on_next: Vec::new(),
                    boot_ms: None,
                    pending_input: None,
                    python,
                    scratch: HashMap::new(),
                });
                self.resolve(ProblemKind::Kernel, cx);
                self._kernel_task = Some(cx.spawn(async move |this, cx| {
                    while let Some(batch) = session::next_batch(&events).await {
                        if this.update(cx, |this, cx| this.apply_kernel_events(batch, cx)).is_err() {
                            break;
                        }
                    }
                }));
                cx.emit(NotebookEvent::Changed);
                true
            }
            Err(err) => {
                self.report(ProblemKind::Kernel, format!("Couldn't start {}", spec.display_name), format!("{err:#}"), cx);
                false
            }
        }
    }

    fn uid_index(&self) -> HashMap<u64, usize> {
        self.cells.iter().enumerate().map(|(ix, ui)| (ui.uid, ix)).collect()
    }

    fn apply_kernel_events(&mut self, batch: Vec<KernelEvent>, cx: &mut Context<Self>) {
        let Some(_) = self.session.as_ref() else { return };
        let index = self.uid_index();
        let mut touched: HashSet<usize> = HashSet::new();
        let mut changed_doc = false;
        let mut widgets_changed: HashSet<String> = HashSet::new();
        // Cells that finished, for extensions listening (`cell.executed`).
        let mut executed: Vec<(usize, String)> = Vec::new();
        for event in batch {
            if let KernelEvent::Iopub {
                parent_msg_id,
                msg_type,
                content,
            } = &event
                && self.route_widget_message(parent_msg_id.as_deref(), msg_type, content, &mut widgets_changed)
            {
                continue;
            }
            let session = self.session.as_mut().expect("checked above");
            match event {
                KernelEvent::Status(status) => session.status = status,
                KernelEvent::Ready { timing, .. } => {
                    session.boot_ms = Some(timing.ready.as_secs_f32() * 1000.);
                    // Herse's chart style: retina figures (configuration only).
                    if session.python
                        && let Some(code) = chart_style::startup_code()
                    {
                        session.kernel.execute_with(code, session::silent());
                    }
                    // A (re)started kernel holds none of the old widgets.
                    if let Some(state) = self.widget_state.as_mut() {
                        Rc::make_mut(state).kernel_gone();
                    }
                }
                KernelEvent::Iopub {
                    parent_msg_id,
                    msg_type,
                    content,
                } => {
                    if let Some(scratch) = parent_msg_id.as_ref().and_then(|p| session.scratch.get_mut(p)) {
                        if let Some(output) = herse_nb::output_from_iopub(&msg_type, &content) {
                            push_output(&mut scratch.outputs, output);
                        }
                        continue;
                    }
                    let Some(run) = parent_msg_id.as_ref().and_then(|p| session.running.get_mut(p)) else {
                        if msg_type == "update_display_data" {
                            changed_doc |= self.update_display(&content, &mut touched);
                        }
                        continue;
                    };
                    let uid = run.cell_uid;
                    let Some(&ix) = index.get(&uid) else { continue };
                    match msg_type.as_str() {
                        "execute_input" => {
                            run.started_at = Some(Instant::now());
                            if let Some(n) = content.get("execution_count").and_then(|v| v.as_i64()) {
                                self.doc.nb.cells[ix].execution_count = Some(n);
                            }
                            touched.insert(ix);
                        }
                        "clear_output" => {
                            let wait = content.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
                            if wait {
                                session.clear_on_next.push(uid);
                            } else {
                                self.doc.nb.cells[ix].outputs.clear();
                                changed_doc = true;
                                touched.insert(ix);
                            }
                        }
                        "update_display_data" => {
                            changed_doc |= self.update_display(&content, &mut touched);
                        }
                        _ => {
                            if let Some(output) = herse_nb::output_from_iopub(&msg_type, &content) {
                                if let Some(pos) = session.clear_on_next.iter().position(|u| *u == uid) {
                                    session.clear_on_next.swap_remove(pos);
                                    self.doc.nb.cells[ix].outputs.clear();
                                }
                                push_output(&mut self.doc.nb.cells[ix].outputs, output);
                                changed_doc = true;
                                touched.insert(ix);
                            }
                        }
                    }
                }
                KernelEvent::ExecuteReply {
                    parent_msg_id,
                    status,
                    execution_count,
                } => {
                    if let Some(scratch) = session.scratch.get_mut(&parent_msg_id) {
                        scratch.done = true;
                        continue;
                    }
                    if let Some(run) = session.running.remove(&parent_msg_id)
                        && let Some(&ix) = index.get(&run.cell_uid)
                    {
                        if let Some(n) = execution_count {
                            self.doc.nb.cells[ix].execution_count = Some(n);
                        }
                        executed.push((ix, status.clone()));
                        // An aborted cell never ran: no time to report.
                        self.cells[ix].last_run = match (status.as_str(), run.started_at) {
                            ("aborted", _) | (_, None) => None,
                            (_, Some(started)) => Some(started.elapsed()),
                        };
                        changed_doc = true;
                        touched.insert(ix);
                    }
                    if session.pending_input.as_ref().is_some_and(|p| !session.running.values().any(|r| r.cell_uid == p.cell_uid)) {
                        session.pending_input = None;
                        self.stdin = None;
                    }
                }
                KernelEvent::InputRequest {
                    parent_msg_id,
                    prompt,
                    password,
                } => {
                    if let Some(run) = session.running.get(&parent_msg_id) {
                        session.pending_input = Some(session::InputPrompt {
                            cell_uid: run.cell_uid,
                            prompt,
                            password,
                        });
                        self.stdin = None;
                    }
                }
                KernelEvent::Died { reason } => {
                    session.running.clear();
                    for scratch in session.scratch.values_mut() {
                        scratch.done = true;
                    }
                    session.pending_input = None;
                    self.stdin = None;
                    let reason = reason.clone();
                    self.report(ProblemKind::Kernel, "The kernel died", reason, cx);
                }
                _ => {}
            }
        }
        if !widgets_changed.is_empty() {
            self.drop_closed_widget_outputs(&widgets_changed, &mut touched);
        }
        for ix in touched {
            self.invalidate_outputs(ix);
        }
        if !widgets_changed.is_empty() {
            self.invalidate_widgets(&widgets_changed);
            self.widgets_dirty = true;
            changed_doc = true;
        }
        if changed_doc {
            self.mark_dirty(cx);
        }
        for (ix, status) in executed {
            let detail = serde_json::json!({ "cell": ix + 1, "status": status });
            crate::extensions::notify("cell.executed", crate::extensions::describe(self), detail, cx);
        }
        self.update_tick(cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// `update_display_data`: replace every output carrying that display id.
    fn update_display(&mut self, content: &serde_json::Value, touched: &mut HashSet<usize>) -> bool {
        let Some(id) = content
            .get("transient")
            .and_then(|t| t.get("display_id"))
            .and_then(|v| v.as_str())
        else {
            return false;
        };
        let Some(new) = herse_nb::output_from_iopub("display_data", content) else { return false };
        let mut changed = false;
        for (ix, cell) in self.doc.nb.cells.iter_mut().enumerate() {
            for out in cell.outputs.iter_mut() {
                if out.display_id() == Some(id) {
                    *out = new.clone();
                    changed = true;
                    touched.insert(ix);
                }
            }
        }
        changed
    }

    fn update_tick(&mut self, cx: &mut Context<Self>) {
        let busy = self.session.as_ref().is_some_and(|s| s.is_busy());
        if !busy {
            self._tick = None;
            return;
        }
        if self._tick.is_some() {
            return;
        }
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let Ok(busy) = this.update(cx, |this, cx| {
                    cx.notify();
                    this.session.as_ref().is_some_and(|s| s.is_busy())
                }) else {
                    break;
                };
                if !busy {
                    break;
                }
            }
        }));
    }

    /// Queue code cells for execution, in order. Markdown and raw cells are skipped.
    fn execute(&mut self, cells: Range<usize>, cx: &mut Context<Self>) {
        if self.hold_for_install(cells.clone(), cx) {
            return;
        }
        let has_code = self.doc.nb.cells[cells.clone()].iter().any(|c| c.cell_type == CellType::Code);
        if !has_code || !self.ensure_kernel(cx) {
            return;
        }
        let session = self.session.as_mut().expect("ensured");
        for ix in cells {
            let cell = &mut self.doc.nb.cells[ix];
            if cell.cell_type != CellType::Code {
                continue;
            }
            let uid = self.cells[ix].uid;
            // A cell already queued or running isn't queued twice.
            if session.running.values().any(|r| r.cell_uid == uid) {
                continue;
            }
            cell.outputs.clear();
            cell.execution_count = None;
            self.cells[ix].outputs = None;
            self.cells[ix].last_run = None;
            if cell.source.trim().is_empty() {
                continue;
            }
            let msg_id = session.kernel.execute(&cell.source);
            session.running.insert(
                msg_id,
                session::Run {
                    cell_uid: uid,
                    queued_at: Instant::now(),
                    started_at: None,
                },
            );
        }
        self.mark_dirty(cx);
        self.update_tick(cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    fn render_stdin(&mut self, uid: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self
            .session
            .as_ref()?
            .pending_input
            .as_ref()
            .filter(|p| p.cell_uid == uid)?;
        let (label, password) = (prompt.prompt.clone(), prompt.password);
        self.stdin.as_ref()?;
        let input = self.stdin.clone()?;
        let theme = cx.theme();
        let _ = password;
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(theme::CELL_PAD_X)
                .pt(px(6.))
                .child(mono().text_color(theme.muted_foreground).child(label))
                .child(div().flex_1().child(Input::new(&input)))
                .into_any_element(),
        )
    }

    fn ensure_stdin_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.session.as_ref().and_then(|s| s.pending_input.as_ref()) else { return };
        if self.stdin.is_some() {
            return;
        }
        let password = prompt.password;
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx);
            if password { state.masked(true) } else { state }
        });
        let sub = cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let value = input.read(cx).value().to_string();
                this.submit_stdin(value, window, cx);
            }
        });
        let handle = input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.stdin = Some(input);
        self._stdin_subscription = Some(sub);
        cx.notify();
    }

    fn submit_stdin(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else { return };
        let Some(prompt) = session.pending_input.take() else { return };
        session.kernel.input_reply(&value);
        // Echo the exchange into the output, as Jupyter does.
        let shown = if prompt.password { "········".to_string() } else { value };
        if let Some(ix) = self.cells.iter().position(|ui| ui.uid == prompt.cell_uid) {
            push_output(
                &mut self.doc.nb.cells[ix].outputs,
                Output::Stream {
                    name: herse_nb::StreamName::Stdout,
                    text: format!("{}{}\n", prompt.prompt, shown),
                },
            );
            self.invalidate_outputs(ix);
        }
        self.stdin = None;
        self._stdin_subscription = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn on_run(&mut self, _: &RunCell, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.execute(self.selection(), cx);
    }

    fn on_run_advance(&mut self, _: &RunAndAdvance, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        let range = self.selection();
        self.execute(range.clone(), cx);
        if range.end >= self.doc.nb.cells.len() {
            self.insert_cell(range.end, CellType::Code, true, window, cx);
        } else {
            self.select(range.end, false, cx);
        }
    }

    fn on_run_insert(&mut self, _: &RunAndInsert, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        let range = self.selection();
        self.execute(range.clone(), cx);
        self.insert_cell(range.end, CellType::Code, true, window, cx);
    }

    fn on_run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.execute(0..self.doc.nb.cells.len(), cx);
    }

    fn on_run_above(&mut self, _: &RunAbove, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.execute(0..self.selection().start, cx);
    }

    fn on_run_below(&mut self, _: &RunBelow, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.execute(self.selection().start..self.doc.nb.cells.len(), cx);
    }

    fn on_interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        if let Err(err) = session.kernel.interrupt() {
            self.report(ProblemKind::Kernel, "Couldn't interrupt the kernel", format!("{err:#}"), cx);
        } else {
            self.flash("Interrupted", cx);
        }
    }

    fn on_restart(&mut self, _: &Restart, _: &mut Window, cx: &mut Context<Self>) {
        self.restart_kernel(cx);
    }

    fn restart_kernel(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = &mut self.session else {
            return self.ensure_kernel(cx);
        };
        session.running.clear();
        for scratch in session.scratch.values_mut() {
            scratch.done = true;
        }
        session.pending_input = None;
        self.stdin = None;
        match session.kernel.restart() {
            Ok(()) => {
                self.resolve(ProblemKind::Kernel, cx);
                self.flash("Restarting kernel", cx);
                true
            }
            Err(err) => {
                self.report(ProblemKind::Kernel, "Couldn't restart the kernel", format!("{err:#}"), cx);
                false
            }
        }
    }

    fn on_restart_run_all(&mut self, _: &RestartAndRunAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        if self.restart_kernel(cx) {
            self.execute(0..self.doc.nb.cells.len(), cx);
        }
    }

    fn on_shutdown(&mut self, _: &ShutdownKernel, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            session.kernel.shutdown();
            self._kernel_task = None;
            self.stdin = None;
            self.flash("Kernel shut down", cx);
            cx.notify();
        }
    }

    /// Switch kernels: the next run launches `spec`.
    pub fn set_kernel(&mut self, spec: &herse_kernel::KernelSpec, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            session.kernel.shutdown();
            self._kernel_task = None;
        }
        let meta = &mut self.doc.nb.metadata;
        meta.insert(
            "kernelspec".into(),
            serde_json::json!({
                "name": spec.name,
                "display_name": spec.display_name,
                "language": spec.language,
            }),
        );
        let grammar: SharedString = highlight::grammar_for(&spec.language).into();
        if grammar != self.grammar {
            self.grammar = grammar;
            for ui in &mut self.cells {
                ui.highlights = None;
            }
            self.schedule_highlights(cx);
        }
        self.mark_dirty(cx);
        self.ensure_kernel(cx);
        self.check_packages(cx);
        cx.notify();
    }

    // ---------------------------------------------------------------- selection

    fn is_selected(&self, ix: usize) -> bool {
        let range = self.selection();
        range.contains(&ix)
    }

    pub fn selection(&self) -> Range<usize> {
        match self.anchor {
            Some(a) => a.min(self.selected)..a.max(self.selected) + 1,
            None => self.selected..self.selected + 1,
        }
    }

    fn select(&mut self, ix: usize, extend: bool, cx: &mut Context<Self>) {
        self.select_inner(ix, extend, true, cx);
    }

    /// Select cell `ix` (extending from the anchor with `extend`); `reveal`
    /// scrolls it into view, which keyboard moves want and clicks don't.
    fn select_inner(&mut self, ix: usize, extend: bool, reveal: bool, cx: &mut Context<Self>) {
        let count = self.doc.nb.cells.len();
        if count == 0 {
            return;
        }
        let ix = ix.min(count - 1);
        // Landing inside a collapsed section (find, search, outline) opens it.
        self.ensure_hidden();
        while self.is_hidden(ix) {
            let Some(h) = (0..ix).rev().find(|&i| self.is_collapsed(i) && !self.is_hidden(i)) else { break };
            self.set_collapsed(h, false, cx);
        }
        if extend {
            self.anchor.get_or_insert(self.selected);
        } else {
            self.anchor = None;
        }
        self.selected = ix;
        self.select_count += 1;
        if reveal {
            self.reveal(ix);
        }
        self.send_presence(cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// Bring a cell into view: nothing if it's fully visible, its top if it's
    /// taller than the viewport (a reader starts at the top), else the least scroll.
    fn reveal(&self, ix: usize) {
        let viewport = self.list.viewport_bounds();
        match self.list.bounds_for_item(ix) {
            Some(b) if b.top() >= viewport.top() && b.bottom() <= viewport.bottom() => {}
            Some(b) if b.size.height > viewport.size.height => {
                self.list.scroll_to(gpui_kit::ListOffset {
                    item_ix: ix,
                    offset_in_item: px(0.),
                });
            }
            // Below the view and never laid out (a jump to the end, a search
            // hit far down): the list counts unmeasured cells as zero height,
            // so "reveal" would stop short. Bring its top to the top instead.
            None if ix > self.list.logical_scroll_top().item_ix => {
                self.list.scroll_to(gpui_kit::ListOffset {
                    item_ix: ix,
                    offset_in_item: px(0.),
                });
            }
            _ => self.list.scroll_to_reveal_item(ix),
        }
    }

    fn click_cell(&mut self, ix: usize, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.cell_pressed = true;
        let editing_this = self
            .editing
            .as_ref()
            .is_some_and(|e| self.cells.get(ix).is_some_and(|ui| ui.uid == e.uid));
        // A press in the code is the editor's. Anywhere else on the cell (its
        // output, its margin) puts the editor away and selects the cell, as
        // in Jupyter, so ⌘⌫ then deletes it.
        if editing_this && std::mem::take(&mut self.press_in_editor) {
            return;
        }
        // A field inside the cell (an input() prompt, a widget's text box)
        // that took focus on this press keeps it.
        let claimed = window.default_prevented();
        if self.editing.is_some() {
            let field = claimed.then(|| window.focused(cx)).flatten();
            self.stop_editing(window, cx);
            if let Some(field) = field {
                field.focus(window, cx);
            }
        }
        let extend = ev.modifiers.shift;
        // The clicked cell is on screen: selecting it must not scroll it, or
        // the text under the pointer moves before the click lands.
        self.select_inner(ix, extend, false, cx);
        let uid = self.cells[ix].uid;
        self.press = (!extend && ev.click_count == 1).then_some((uid, ev.position));
        if ev.click_count >= 2 && self.doc.nb.cells[ix].cell_type == CellType::Markdown {
            // The double click selects a word of the rendered text (the text
            // view handles the press after this): edit with the cursor on
            // that word in the source, not at its start.
            cx.defer_in(window, move |this, window, cx| {
                let offset = this.cells.iter().position(|ui| ui.uid == uid).and_then(|ix| {
                    let state = this.cells[ix].markdown.clone()?;
                    let (range, word, shown) = {
                        let view = state.read(cx);
                        let range = view.selected_source_range();
                        let shown = range.clone().and_then(|r| view.source().get(r).map(str::to_string));
                        (range, view.selected_text(), shown)
                    };
                    state.update(cx, |s, cx| s.clear_selection(cx));
                    // The view shows a prepared copy of the source (attachments
                    // resolved, details folded): its range holds when the text
                    // there matches; otherwise find the word itself.
                    let source = &this.doc.nb.cells[ix].source;
                    match (range, shown) {
                        (Some(r), Some(shown)) if source.get(r.clone()) == Some(shown.as_str()) => Some(r.start),
                        _ => Some(word.trim()).filter(|w| !w.is_empty()).and_then(|w| source.find(w)),
                    }
                });
                this.edit_at(uid, offset, window, cx);
            });
        } else if ev.click_count >= 2 {
            self.start_editing(window, cx);
        } else if !claimed {
            self.focus_handle.focus(window, cx);
        }
    }

    /// A right-click acts on the cell under it: select it (keeping a
    /// selection it belongs to) so the menu's commands apply there.
    fn right_click_cell(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let uid = self.cells[ix].uid;
        if self.editing.as_ref().is_some_and(|e| e.uid == uid) {
            return;
        }
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        if !self.selection().contains(&ix) {
            self.select_inner(ix, false, false, cx);
        }
        if !window.default_prevented() {
            self.focus_handle.focus(window, cx);
        }
    }

    /// The cell's right-click menu. Items are the notebook's own actions, sent
    /// to its focus, so each shows its key.
    #[allow(clippy::type_complexity)]
    fn cell_menu(&self, ix: usize) -> Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu> {
        let focus = self.focus_handle.clone();
        let kind = self.doc.nb.cells[ix].cell_type;
        let has_outputs = !self.doc.nb.cells[ix].outputs.is_empty();
        let outputs_hidden = self.cells[ix].outputs_hidden;
        let source_hidden = self.cells[ix].source_hidden;
        let numbered = self.shows_line_numbers(ix);
        Rc::new(move |menu: PopupMenu, _, _| {
            let code = kind == CellType::Code;
            let menu = menu.action_context(focus.clone());
            let menu = if code {
                menu.menu("Run Cell", Box::new(RunCell))
                    .menu("Run All Above", Box::new(RunAbove))
                    .menu("Run This and Below", Box::new(RunBelow))
                    .separator()
            } else {
                menu.menu("Edit", Box::new(EnterEdit)).separator()
            };
            let menu = menu
                .menu("Cut", Box::new(CutCells))
                .menu("Copy", Box::new(CopyCells))
                .menu("Paste Below", Box::new(PasteCellsBelow))
                .menu("Delete", Box::new(DeleteCell))
                .separator()
                .menu("Insert Cell Above", Box::new(InsertAbove))
                .menu("Insert Cell Below", Box::new(InsertBelow))
                .menu("Move Up", Box::new(MoveCellUp))
                .menu("Move Down", Box::new(MoveCellDown))
                .menu("Merge With Below", Box::new(MergeBelow))
                .separator();
            let menu = match kind {
                CellType::Code => menu.menu("Change to Markdown", Box::new(ToMarkdown)).menu("Change to Raw", Box::new(ToRaw)),
                CellType::Markdown => menu.menu("Change to Code", Box::new(ToCode)).menu("Change to Raw", Box::new(ToRaw)),
                CellType::Raw => menu.menu("Change to Code", Box::new(ToCode)).menu("Change to Markdown", Box::new(ToMarkdown)),
            };
            let menu = menu.separator().menu(if source_hidden { "Show Input" } else { "Hide Input" }, Box::new(ToggleInput));
            if !code {
                return menu;
            }
            let menu = menu.menu(if numbered { "Hide Line Numbers" } else { "Show Line Numbers" }, Box::new(ToggleLineNumbers));
            if has_outputs {
                menu.menu(if outputs_hidden { "Show Output" } else { "Hide Output" }, Box::new(ToggleOutput))
                    .menu("Clear Output", Box::new(ClearOutputs))
            } else {
                menu
            }
        })
    }

    /// A click on code (or raw) source edits it with the cursor where the
    /// click landed (`offset` into the source), as in VS Code and Zed; a drag
    /// selects the text instead, for copying. Markdown keeps a double click,
    /// so its links and rendered text stay clickable.
    fn edit_at(&mut self, uid: u64, offset: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.press = None;
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == uid) else { return };
        if self.editing.as_ref().is_some_and(|e| e.uid == uid) {
            return;
        }
        if self.selected != ix || self.anchor.is_some() {
            self.select(ix, false, cx);
        }
        self.start_editing(window, cx);
        let Some(editing) = self.editing.as_ref().filter(|e| e.uid == uid) else { return };
        let source = &self.doc.nb.cells[ix].source;
        let mut at = offset.unwrap_or(source.len()).min(source.len());
        while !source.is_char_boundary(at) {
            at -= 1;
        }
        let before = &source[..at];
        let position = gpui_kit::component::input::Position {
            line: before.matches('\n').count() as u32,
            character: before.rsplit('\n').next().unwrap_or("").chars().count() as u32,
        };
        editing.editor.update(cx, |e, cx| e.set_cursor_position(position, window, cx));
    }

    /// A release on a code box's padding, after a press in place: edit, the
    /// cursor at the end. (A release on the text itself is `edit_at` with
    /// the exact spot, which runs first.)
    fn release_on_code(&mut self, uid: u64, ev: &gpui_kit::MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pressed, at)) = self.press.take() else { return };
        let moved = (ev.position.x - at.x).abs().max((ev.position.y - at.y).abs());
        if pressed == uid && moved < px(4.) {
            self.edit_at(uid, None, window, cx);
        }
    }

    // ---------------------------------------------------------------- editing

    fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_view_only() {
            self.flash("This live session is view-only", cx);
            return;
        }
        let ix = self.selected;
        let Some(uid) = self.cells.get(ix).map(|ui| ui.uid) else { return };
        if self.editing.as_ref().is_some_and(|e| e.uid == uid) {
            return;
        }
        // Moving straight on from another cell's editor (running, a click):
        // what was typed there becomes a step in the notebook's history.
        if let Some(previous) = self.editing.take() {
            self.record_typing(&previous);
        }
        let Some(cell) = self.doc.nb.cells.get(ix) else { return };
        self.anchor = None;
        let language = match cell.cell_type {
            CellType::Code => self.grammar.clone(),
            CellType::Markdown => "markdown".into(),
            CellType::Raw => "text".into(),
        };
        let source = cell.source.clone();
        let source_before = source.clone();
        let numbered = cell.cell_type == CellType::Code && self.shows_line_numbers(ix);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                // Four spaces, as Python, Jupyter and PEP 8 indent.
                .tab_size(gpui_kit::component::input::TabSize { tab_size: 4, hard_tabs: false })
                .line_number(numbered)
                .indent_guides(false)
                .folding(false)
                .soft_wrap(false)
                .default_value(source)
        });
        if cell.cell_type == CellType::Code {
            let provider = Rc::new(intellisense::KernelIntellisense {
                notebook: cx.entity().downgrade(),
            });
            editor.update(cx, |e, _| {
                e.lsp_mut().completion_provider = Some(provider.clone());
                e.lsp_mut().hover_provider = Some(provider);
            });
        }
        let subscription = cx.subscribe_in(&editor, window, move |this, editor, event: &InputEvent, _window, cx| {
            if matches!(event, InputEvent::Change) {
                this.editor_changed(uid, editor, cx);
            }
        });
        let observation = cx.observe_in(&editor, window, |this, _, window, cx| this.follow_caret(window, cx));
        let handle = editor.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.editing = Some(Editing {
            uid,
            editor,
            before: source_before,
            _subscription: subscription,
            _observation: observation,
        });
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// Keep the caret on screen as it moves (typing, arrows, a long paste):
    /// a cell's editor is as tall as its text, so the page has to scroll.
    /// Code doesn't wrap, so the caret sits at its line number times the line
    /// height below the cell's top. Only a moved caret scrolls; reading
    /// elsewhere doesn't snap back.
    fn follow_caret(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Live: the others see my cursor move.
        self.send_presence(cx);
        let Some(editing) = &self.editing else { return };
        let (uid, editor) = (editing.uid, editing.editor.clone());
        let state = editor.read(cx);
        let cursor = state.cursor();
        let at = (uid, cursor);
        if self.followed_caret == Some(at) {
            return;
        }
        // Entering the editor puts the caret where the click was: on screen.
        let first = self.followed_caret.is_none_or(|(u, _)| u != uid);
        self.followed_caret = Some(at);
        if first {
            return;
        }
        // Lines above the caret: newlines before it in the rope.
        let row = {
            let text = state.text();
            let cursor = cursor.min(text.len());
            text.slice(0..cursor).chars().filter(|&c| c == '\n').count() as f32
        };
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == uid) else { return };
        let (Some(cell), view) = (self.list.bounds_for_item(ix), self.list.viewport_bounds()) else { return };
        let line = theme::mono_line_height();
        // The row's padding and the block's border and padding sit above
        // the first line.
        let top = cell.top() + px(5.) + px(1.) + theme::CELL_PAD_Y + line * row;
        let bottom = top + line;
        let margin = line * 1.5;
        let delta = if bottom + margin > view.bottom() {
            bottom + margin - view.bottom()
        } else if top - margin < view.top() {
            top - margin - view.top()
        } else {
            return;
        };
        self.list.scroll_by(delta);
        cx.notify();
    }

    fn editor_changed(&mut self, uid: u64, editor: &Entity<EditorState>, cx: &mut Context<Self>) {
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == uid) else { return };
        let value = editor.read(cx).value();
        let cell = &mut self.doc.nb.cells[ix];
        if cell.source.as_str() == value.as_ref() {
            return;
        }
        let old_rows = source_rows(&cell.source);
        let old = std::mem::replace(&mut cell.source, value.to_string());
        let new_rows = source_rows(&cell.source);
        self.cells[ix].rev += 1;
        self.hidden_dirty = true;
        if self.live.is_some() {
            // Live: just the keystroke's edit, not a diff of the notebook.
            self.send_source_edit(ix, &old);
            self.quiet_dirty(cx);
        } else {
            self.mark_dirty(cx);
        }
        // The editor redraws itself; the notebook only needs to when the
        // cell changes height. Typing stays as cheap as the editor alone.
        if old_rows != new_rows {
            cx.notify();
        }
    }

    /// Re-render a Markdown cell's text after its source changed.
    fn refresh_markdown(&mut self, ix: usize, cx: &mut Context<Self>) {
        let cell = &self.doc.nb.cells[ix];
        if cell.cell_type != CellType::Markdown {
            return;
        }
        let dark = cx.theme().is_dark();
        let source =
            markdown::prepare(&cell.source, cell.attachments.as_ref(), markdown::Base::of(self.doc.directory(), self.source_url.as_deref()), &self.cells[ix].details_toggled, dark);
        if let Some(state) = &self.cells[ix].markdown {
            state.update(cx, |s, cx| s.set_text(&source, cx))
        }
    }

    /// Opened from a link: name the tab after the file, and resolve the
    /// notebook's relative images and links against where it came from.
    pub fn set_origin(&mut self, name: String, url: String, cx: &mut Context<Self>) {
        self.name_hint = Some(name);
        self.source_url = Some(url);
        for ix in 0..self.cells.len() {
            self.refresh_markdown(ix, cx);
        }
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// Where a notebook opened from a link came from.
    pub fn source_url(&self) -> Option<&str> {
        self.source_url.as_deref()
    }

    /// A link that wouldn't open, above the notebook until dismissed.
    pub fn report_link_error(&mut self, title: String, detail: String, cx: &mut Context<Self>) {
        self.report(ProblemKind::Link, title, detail, cx);
    }

    /// Draw every output again (a chart setting changed).
    pub fn restyle_outputs(&mut self, cx: &mut Context<Self>) {
        for ui in &mut self.cells {
            ui.outputs = None;
        }
        self.list.remeasure();
        cx.notify();
    }

    /// Open comment threads, for the tab strip's count.
    pub fn open_comment_count(&self) -> usize {
        self.comments.open_total()
    }

    /// Remember (or forget) where this notebook is published, in its
    /// metadata, and save that.
    pub fn set_share_info(&mut self, info: Option<&herse_share::ShareInfo>, cx: &mut Context<Self>) {
        match info {
            Some(info) => herse_share::write_share_info(&mut self.doc.nb, info),
            None => {
                herse_share::clear_share_info(&mut self.doc.nb);
            }
        }
        self.mark_dirty(cx);
        if self.doc.path.is_some() {
            self.save_now(false, cx);
        }
        cx.notify();
    }

    /// Set the sources of some cells in place (find and replace): one undo
    /// step brings them all back, and every other cell keeps its view state.
    fn set_sources(&mut self, edits: Vec<(usize, String)>, cx: &mut Context<Self>) {
        let (Some(lo), Some(hi)) = (edits.iter().map(|e| e.0).min(), edits.iter().map(|e| e.0).max()) else {
            return;
        };
        self.push_undo(UndoEntry {
            at: lo,
            removed: self.doc.nb.cells[lo..=hi].to_vec(),
            inserted: hi - lo + 1,
            selected: self.selected,
        });
        for (ix, source) in edits {
            self.doc.nb.cells[ix].source = source;
            self.cells[ix].rev += 1;
            self.refresh_markdown(ix, cx);
        }
        self.hidden_dirty = true;
        self.schedule_highlights(cx);
        self.mark_dirty(cx);
        cx.notify();
    }

    /// What was typed in a cell whose editor is closing, as one undo step:
    /// the editor's own history goes with it, the notebook's stays.
    fn record_typing(&mut self, editing: &Editing) {
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == editing.uid) else { return };
        let cell = &self.doc.nb.cells[ix];
        if cell.source == editing.before {
            return;
        }
        let mut before = cell.clone();
        before.source = editing.before.clone();
        self.push_undo(UndoEntry { at: ix, removed: vec![before], inserted: 1, selected: ix });
    }

    /// ⌘Z while typing: the cell's own edits first; past the first of them,
    /// leave the cell and carry on through the notebook's history.
    fn on_editor_undo(&mut self, _: &EditorUndo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.as_ref().map(|e| e.editor.clone()) else {
            cx.propagate();
            return;
        };
        if editor.read(cx).can_undo() {
            editor.update(cx, |e, cx| e.undo_edit(window, cx));
            return;
        }
        self.stop_editing(window, cx);
        self.on_undo(&UndoCellOp, window, cx);
    }

    /// ⌘⇧Z while typing: the cell's own undone edits, then the notebook's.
    fn on_editor_redo(&mut self, _: &EditorRedo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.as_ref().map(|e| e.editor.clone()) else {
            cx.propagate();
            return;
        };
        if editor.read(cx).can_redo() || self.redo.is_empty() {
            editor.update(cx, |e, cx| e.redo_edit(window, cx));
            return;
        }
        self.stop_editing(window, cx);
        self.on_redo(&RedoCellOp, window, cx);
    }

    fn stop_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else { return };
        self.record_typing(&editing);
        if let Some(ix) = self.cells.iter().position(|ui| ui.uid == editing.uid) {
            self.refresh_markdown(ix, cx);
        }
        self.schedule_highlights(cx);
        self.check_packages(cx);
        self.focus_handle.focus(window, cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        // A live notebook's change goes to the others too.
        if !self.live_quiet {
            self.sync_live();
        }
        self.doc_rev += 1;
        let was = self.doc.dirty;
        self.doc.dirty = true;
        if !was {
            cx.emit(NotebookEvent::Changed);
        }
        self.schedule_autosave(cx);
    }

    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        if self.doc.path.is_none() || self.doc.autosave_blocked {
            return;
        }
        self.autosave = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTOSAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_now(false, cx)).ok();
        }));
    }

    /// Explicit saves overwrite; auto-saves back off when the file changed on disk.
    pub fn save_now(&mut self, explicit: bool, cx: &mut Context<Self>) {
        if !self.doc.dirty && !explicit {
            return;
        }
        // herse-snap: tests drive real files and must never rewrite them.
        if std::env::var_os("HERSE_NO_SAVE").is_some() {
            return;
        }
        self.store_widget_state();
        match self.doc.save(None, !explicit) {
            Ok(SaveOutcome::Saved) => {
                self.resolve(ProblemKind::File, cx);
                self.refresh_baseline(cx);
                crate::extensions::notify("notebook.saved", crate::extensions::describe(self), serde_json::Value::Null, cx);
                if explicit {
                    self.flash("Saved", cx);
                }
            }
            Ok(SaveOutcome::Conflict) => {
                self.flash("Changed on disk — auto-save paused. Save to overwrite.", cx);
            }
            Err(err) => self.report(ProblemKind::File, "Couldn't save", format!("{err:#}"), cx),
        }
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// The file changed on disk. Our own save leaves the mtime we recorded, so
    /// that's ignored; a clean document reloads; unsaved edits are kept and
    /// auto-save pauses (the next explicit save overwrites).
    pub fn disk_changed(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.doc.path.clone() else { return };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.doc.disk_mtime {
            return;
        }
        if self.doc.dirty || self.editing.is_some() {
            self.doc.autosave_blocked = true;
            self.flash("Changed on disk — your unsaved edits are kept. Reload from Disk to discard them.", cx);
            return;
        }
        self.reload(cx);
    }

    /// Replace the document with the file's current contents, keeping the
    /// reader's place.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.doc.path.clone() else { return };
        match Document::open(&path) {
            Ok(doc) => {
                let top = self.list.logical_scroll_top();
                let selected = self.selected;
                self.doc = doc;
                let open = self.comments.open;
                self.comments = comments::Comments::load(&self.doc.nb);
                self.comments.open = open;
                self.widget_state = None;
                self.widget_state_loaded = false;
                self.widgets_dirty = false;
                self.editing = None;
                self.anchor = None;
                let count = self.doc.nb.cells.len();
                self.cells.clear();
                for _ in 0..count {
                    let ui = self.new_cell_ui();
                    self.cells.push(ui);
                }
                self.collapse_from_metadata();
                self.undo.clear();
                self.redo.clear();
                self.doc_rev += 1;
                self.hidden_dirty = true;
                self.list.reset(count);
                if count > 0 {
                    self.list.scroll_to(gpui_kit::ListOffset {
                        item_ix: top.item_ix.min(count - 1),
                        offset_in_item: top.offset_in_item,
                    });
                }
                self.selected = selected.min(count.saturating_sub(1));
                self.schedule_highlights(cx);
                if let Some(bar) = &self.find {
                    let text = bar.needle.text.clone();
                    self.update_find(&text, cx);
                }
                self.resolve(ProblemKind::File, cx);
                self.refresh_baseline(cx);
                self.flash("Reloaded from disk", cx);
            }
            Err(err) => self.report(ProblemKind::File, "Couldn't reload from disk", format!("{err:#}"), cx),
        }
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// Export through nbconvert, using the Python of this notebook's kernel
    /// (so it works without `jupyter` on PATH, as when launched from Finder).
    /// Export as HTML: the notebook as one self-contained page, drawn by
    /// Herse itself (herse-html, the same pages as Quick Look): no Python,
    /// unsaved edits included. It goes next to the notebook; one with no
    /// file yet (untitled, or opened from a link) asks where.
    fn on_export_html(&mut self, _: &ExportHtml, _: &mut Window, cx: &mut Context<Self>) {
        let nb = self.doc.nb.clone();
        let title = self.title();
        let beside = self.doc.path.as_ref().map(|p| p.with_extension("html"));
        let name = std::path::Path::new(&title).with_extension("html").to_string_lossy().into_owned();
        let downloads = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(|| PathBuf::from("."));
        let ask = beside.is_none().then(|| cx.prompt_for_new_path(&downloads, Some(&name)));
        cx.spawn(async move |this, cx| {
            let target = match (beside, ask) {
                (Some(path), _) => path,
                (None, Some(rx)) => match rx.await {
                    Ok(Ok(Some(path))) => path,
                    _ => return,
                },
                (None, None) => return,
            };
            let result = cx
                .background_spawn(async move {
                    let page = herse_html::render(&nb, &herse_html::Options { title, ..Default::default() });
                    std::fs::write(&target, page).map(|_| target).map_err(|e| e.to_string())
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(html) => {
                    cx.reveal_path(&html);
                    this.resolve(ProblemKind::File, cx);
                    this.flash(format!("Exported {}", html.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()), cx);
                }
                Err(e) => this.report(ProblemKind::File, "Couldn't export HTML", e, cx),
            })
            .ok();
        })
        .detach();
    }

    fn on_reload(&mut self, _: &ReloadFromDisk, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    pub fn save_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.store_widget_state();
        match self.doc.save(Some(path), false) {
            Ok(_) => {
                self.resolve(ProblemKind::File, cx);
                self.refresh_baseline(cx);
                self.flash("Saved", cx)
            }
            Err(err) => self.report(ProblemKind::File, "Couldn't save", format!("{err:#}"), cx),
        }
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// Show a failure above the notebook until dismissed (see [`Problem`]).
    fn report(
        &mut self,
        kind: ProblemKind,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let (title, detail) = (title.into(), detail.into());
        eprintln!("{title}: {detail}");
        self.problem = Some(Problem { title, detail, kind });
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// The trouble of `kind` is over (a kernel started, a save went through).
    fn resolve(&mut self, kind: ProblemKind, cx: &mut Context<Self>) {
        if self.problem.as_ref().is_some_and(|p| p.kind == kind) {
            self.problem = None;
            cx.emit(NotebookEvent::Changed);
            cx.notify();
        }
    }

    fn render_problem(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let problem = self.problem.as_ref()?;
        let mut actions = Vec::new();
        if problem.kind == ProblemKind::Kernel {
            actions.push(
                crate::ui::button("problem-kernel", "Choose Kernel…", cx)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ChangeKernel), cx))
                    .into_any_element(),
            );
        }
        let banner = crate::ui::problem_banner(
            "problem",
            problem.title.clone(),
            problem.detail.clone(),
            actions,
            cx.listener(|this, _, _, cx| this.flash("Copied", cx)),
            cx.listener(|this, _, _, cx| {
                this.problem = None;
                cx.emit(NotebookEvent::Changed);
                cx.notify();
            }),
            cx,
        );
        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(16.))
                .pt(px(12.))
                .child(div().w_full().max_w(theme::CONTENT_MAX_WIDTH).mx_auto().child(banner))
                .into_any_element(),
        )
    }

    pub fn flash(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(message.into());
        cx.emit(NotebookEvent::Changed);
        self.status_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            this.update(cx, |this, cx| {
                this.status = None;
                cx.emit(NotebookEvent::Changed);
            })
            .ok();
        }));
    }

    // ---------------------------------------------------------------- structure

    /// Replace `range` with `cells`, keeping the UI state, list and undo in step.
    fn splice_cells(&mut self, range: Range<usize>, cells: Vec<Cell>, record_undo: bool, cx: &mut Context<Self>) {
        let inserted = cells.len();
        let at = range.start;
        let removed = self.splice_raw(range, cells, cx);
        if record_undo {
            self.push_undo(UndoEntry { at, removed, inserted, selected: self.selected });
        }
    }

    /// [`Self::splice_cells`] without undo: returns the cells it took out.
    fn splice_raw(&mut self, range: Range<usize>, cells: Vec<Cell>, cx: &mut Context<Self>) -> Vec<Cell> {
        let inserted = cells.len();
        let removed: Vec<Cell> = self.doc.nb.cells.splice(range.clone(), cells).collect();
        let new_ui: Vec<CellUi> = (0..inserted).map(|_| self.new_cell_ui()).collect();
        self.cells.splice(range.clone(), new_ui);
        self.list.splice(range, inserted);
        self.hidden_dirty = true;
        self.schedule_highlights(cx);
        self.mark_dirty(cx);
        cx.notify();
        removed
    }

    /// A new edit: undoable, and the end of what redo could bring back.
    fn push_undo(&mut self, entry: UndoEntry) {
        self.push_step(Step::Cells(entry));
    }

    fn push_step(&mut self, step: Step) {
        self.redo.clear();
        self.undo.push(step);
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
    }

    fn new_cell(&self, cell_type: CellType) -> Cell {
        self.doc.nb.new_cell(cell_type)
    }

    fn insert_cell(&mut self, at: usize, cell_type: CellType, edit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        let at = at.min(self.doc.nb.cells.len());
        let cell = self.new_cell(cell_type);
        self.splice_cells(at..at, vec![cell], true, cx);
        self.select(at, false, cx);
        if edit {
            self.start_editing(window, cx);
        }
    }

    fn set_type(&mut self, cell_type: CellType, cx: &mut Context<Self>) {
        let range = self.selection();
        let cells: Vec<Cell> = self.doc.nb.cells[range.clone()]
            .iter()
            .map(|c| {
                let mut c = c.clone();
                if c.cell_type != cell_type {
                    c.cell_type = cell_type;
                    if cell_type != CellType::Code {
                        c.outputs.clear();
                        c.execution_count = None;
                    }
                }
                c
            })
            .collect();
        if cells.iter().zip(&self.doc.nb.cells[range.clone()]).all(|(a, b)| a.cell_type == b.cell_type) {
            return;
        }
        let selected = self.selected;
        let anchor = self.anchor;
        self.splice_cells(range, cells, true, cx);
        self.selected = selected;
        self.anchor = anchor;
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) -> Vec<Cell> {
        let range = self.selection();
        if range.is_empty() || self.doc.nb.cells.is_empty() {
            return Vec::new();
        }
        let removed = self.doc.nb.cells[range.clone()].to_vec();
        let start = range.start;
        self.splice_cells(range, Vec::new(), true, cx);
        self.anchor = None;
        let count = self.doc.nb.cells.len();
        self.selected = start.min(count.saturating_sub(1));
        removed
    }

    // ---------------------------------------------------------------- action handlers

    fn next_visible(&mut self, from: usize, forward: bool) -> usize {
        self.ensure_hidden();
        let n = self.doc.nb.cells.len();
        let mut ix = from;
        loop {
            let next = if forward { ix + 1 } else { ix.wrapping_sub(1) };
            if next >= n {
                return from;
            }
            ix = next;
            if !self.is_hidden(ix) {
                return ix;
            }
        }
    }
    fn on_select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.next_visible(self.selected, true);
        self.select(ix, false, cx);
    }
    fn on_select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.next_visible(self.selected, false);
        self.select(ix, false, cx);
    }
    fn on_extend_next(&mut self, _: &ExtendSelectionNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, true, cx);
    }
    fn on_extend_prev(&mut self, _: &ExtendSelectionPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), true, cx);
    }
    fn on_select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, false, cx);
    }
    fn on_select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(usize::MAX, false, cx);
    }
    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let h = self.list.viewport_bounds().size.height;
        self.list.scroll_by(h * 0.9);
        cx.notify();
    }
    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let h = self.list.viewport_bounds().size.height;
        self.list.scroll_by(-h * 0.9);
        cx.notify();
    }
    fn on_enter_edit(&mut self, _: &EnterEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.start_editing(window, cx);
    }
    fn on_exit_edit(&mut self, _: &ExitEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_editing(window, cx);
    }
    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.anchor.take().is_some() {
            cx.notify();
        }
    }
    fn on_insert_above(&mut self, _: &InsertAbove, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_cell(self.selection().start, CellType::Code, true, window, cx);
    }
    fn on_insert_below(&mut self, _: &InsertBelow, window: &mut Window, cx: &mut Context<Self>) {
        let at = if self.doc.nb.cells.is_empty() { 0 } else { self.selection().end };
        self.insert_cell(at, CellType::Code, true, window, cx);
    }
    fn on_insert_code_above(&mut self, _: &InsertCodeAbove, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_cell(self.selection().start, CellType::Code, true, window, cx);
    }
    fn on_insert_code_below(&mut self, _: &InsertCodeBelow, window: &mut Window, cx: &mut Context<Self>) {
        let at = if self.doc.nb.cells.is_empty() { 0 } else { self.selection().end };
        self.insert_cell(at, CellType::Code, true, window, cx);
    }
    fn on_insert_markdown_below(&mut self, _: &InsertMarkdownBelow, window: &mut Window, cx: &mut Context<Self>) {
        let at = if self.doc.nb.cells.is_empty() { 0 } else { self.selection().end };
        self.insert_cell(at, CellType::Markdown, true, window, cx);
    }
    fn on_to_markdown(&mut self, _: &ToMarkdown, _: &mut Window, cx: &mut Context<Self>) {
        self.set_type(CellType::Markdown, cx);
    }
    fn on_to_code(&mut self, _: &ToCode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_type(CellType::Code, cx);
    }
    fn on_to_raw(&mut self, _: &ToRaw, _: &mut Window, cx: &mut Context<Self>) {
        self.set_type(CellType::Raw, cx);
    }
    fn on_delete(&mut self, _: &DeleteCell, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection(cx);
    }
    fn on_cut(&mut self, _: &CutCells, _: &mut Window, cx: &mut Context<Self>) {
        let cells = self.delete_selection(cx);
        self.put_clipboard(cells, cx);
    }
    fn on_copy(&mut self, _: &CopyCells, _: &mut Window, cx: &mut Context<Self>) {
        let cells = self.doc.nb.cells[self.selection()].to_vec();
        let n = cells.len();
        self.put_clipboard(cells, cx);
        self.flash(format!("Copied {n} cell{}", if n == 1 { "" } else { "s" }), cx);
    }
    /// ⌘C in command mode: selected text when there is some (an output, a
    /// paragraph), else the selected cells.
    fn on_command_copy(&mut self, _: &CommandCopy, window: &mut Window, cx: &mut Context<Self>) {
        if gpui_kit::base::TextSelection::has_selection(window, cx) {
            cx.propagate();
            return;
        }
        self.on_copy(&CopyCells, window, cx);
    }
    /// ⌘X in command mode cuts cells, unless text is selected: then it does
    /// nothing (read-only text can't be cut), rather than cut the cell.
    fn on_command_cut(&mut self, _: &CommandCut, window: &mut Window, cx: &mut Context<Self>) {
        if gpui_kit::base::TextSelection::has_selection(window, cx) {
            return;
        }
        self.on_cut(&CutCells, window, cx);
    }
    /// ⌘V in command mode: cells copied here paste as cells; text from
    /// anywhere else becomes a new code cell below.
    fn on_command_paste(&mut self, _: &CommandPaste, window: &mut Window, cx: &mut Context<Self>) {
        let system = cx.read_from_clipboard().and_then(|c| c.text());
        let ours = cx
            .try_global::<CellClipboard>()
            .filter(|c| !c.0.is_empty())
            .map(|c| c.0.iter().map(|c| c.source.as_str()).collect::<Vec<_>>().join("\n\n"));
        match (ours, system) {
            (Some(ours), system) if system.as_deref().is_none_or(|s| s == ours) => {
                self.on_paste(&PasteCellsBelow, window, cx);
            }
            (_, Some(text)) if !text.trim().is_empty() => {
                let mut cell = self.new_cell(CellType::Code);
                cell.source = text.trim_end_matches('\n').to_string();
                let at = if self.doc.nb.cells.is_empty() { 0 } else { self.selection().end };
                self.splice_cells(at..at, vec![cell], true, cx);
                self.select(at, false, cx);
            }
            _ => {}
        }
    }
    fn put_clipboard(&mut self, cells: Vec<Cell>, cx: &mut Context<Self>) {
        let text = cells.iter().map(|c| c.source.as_str()).collect::<Vec<_>>().join("\n\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.set_global(CellClipboard(cells));
    }
    fn on_paste(&mut self, _: &PasteCellsBelow, _: &mut Window, cx: &mut Context<Self>) {
        let Some(clip) = cx.try_global::<CellClipboard>() else { return };
        if clip.0.is_empty() {
            return;
        }
        let mut cells = clip.0.clone();
        for c in &mut cells {
            if c.id.is_some() {
                c.id = Some(self.doc.nb.fresh_cell_id());
            }
        }
        let n = cells.len();
        let at = if self.doc.nb.cells.is_empty() { 0 } else { self.selection().end };
        self.splice_cells(at..at, cells, true, cx);
        self.select(at + n - 1, false, cx);
    }
    fn on_undo(&mut self, _: &UndoCellOp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.undo.pop() else {
            self.flash("Nothing to undo", cx);
            return;
        };
        let redo = self.revert_step(step, cx);
        self.redo.push(redo);
    }

    /// ⇧Z / ⌘⇧Z: put back what undo took back.
    fn on_redo(&mut self, _: &RedoCellOp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.redo.pop() else {
            self.flash("Nothing to redo", cx);
            return;
        };
        let undo = self.revert_step(step, cx);
        self.undo.push(undo);
    }

    /// Apply an undo or redo step; returns the step that reverses it.
    fn revert_step(&mut self, step: Step, cx: &mut Context<Self>) -> Step {
        match step {
            Step::Cells(entry) => Step::Cells(self.revert(entry, cx)),
            Step::Comments(threads) => {
                let now = std::mem::replace(&mut self.comments.threads, threads);
                // Show what came back (or went).
                self.comments.open = true;
                self.store_comments(cx);
                Step::Comments(now)
            }
        }
    }

    fn revert(&mut self, entry: UndoEntry, cx: &mut Context<Self>) -> UndoEntry {
        let selected = self.selected;
        let restored = entry.removed.len();
        let taken = self.splice_raw(entry.at..entry.at + entry.inserted, entry.removed, cx);
        self.anchor = None;
        self.select(entry.selected.min(self.doc.nb.cells.len().saturating_sub(1)), false, cx);
        UndoEntry { at: entry.at, removed: taken, inserted: restored, selected }
    }

    /// ⌘A in command mode: every cell.
    fn on_select_all_cells(&mut self, _: &SelectAllCells, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.doc.nb.cells.len();
        if n == 0 {
            return;
        }
        self.anchor = Some(0);
        self.selected = n - 1;
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }
    fn on_move_up(&mut self, _: &MoveCellUp, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        if range.start == 0 {
            return;
        }
        let mut block = self.doc.nb.cells[range.start - 1..range.end].to_vec();
        block.rotate_left(1);
        let anchor = self.anchor.map(|a| a - 1);
        let selected = self.selected - 1;
        self.splice_cells(range.start - 1..range.end, block, true, cx);
        self.anchor = anchor;
        self.select(selected, anchor.is_some(), cx);
        self.anchor = anchor;
    }
    fn on_move_down(&mut self, _: &MoveCellDown, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        if range.end >= self.doc.nb.cells.len() {
            return;
        }
        let mut block = self.doc.nb.cells[range.start..range.end + 1].to_vec();
        block.rotate_right(1);
        let anchor = self.anchor.map(|a| a + 1);
        let selected = self.selected + 1;
        self.splice_cells(range.start..range.end + 1, block, true, cx);
        self.select(selected, false, cx);
        self.anchor = anchor;
    }
    fn on_merge_below(&mut self, _: &MergeBelow, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        let range = if range.len() == 1 { range.start..(range.end + 1).min(self.doc.nb.cells.len()) } else { range };
        if range.len() < 2 {
            return;
        }
        let mut merged = self.doc.nb.cells[range.start].clone();
        for c in &self.doc.nb.cells[range.start + 1..range.end] {
            if !merged.source.is_empty() && !merged.source.ends_with('\n') {
                merged.source.push('\n');
            }
            merged.source.push_str(&c.source);
        }
        merged.outputs.clear();
        merged.execution_count = None;
        let start = range.start;
        self.splice_cells(range, vec![merged], true, cx);
        self.select(start, false, cx);
    }
    fn on_toggle_output(&mut self, _: &ToggleOutput, _: &mut Window, cx: &mut Context<Self>) {
        for ix in self.selection() {
            if let Some(ui) = self.cells.get_mut(ix) {
                ui.outputs_hidden = !ui.outputs_hidden;
            }
        }
        cx.notify();
    }
    fn shows_line_numbers(&self, ix: usize) -> bool {
        self.cells.get(ix).and_then(|ui| ui.line_numbers).unwrap_or(self.line_numbers)
    }

    fn on_toggle_line_numbers(&mut self, _: &ToggleLineNumbers, window: &mut Window, cx: &mut Context<Self>) {
        for ix in self.selection() {
            let on = !self.shows_line_numbers(ix);
            if let Some(ui) = self.cells.get_mut(ix) {
                ui.line_numbers = Some(on);
            }
        }
        self.sync_editor_line_numbers(window, cx);
        cx.notify();
    }

    pub(crate) fn on_toggle_all_line_numbers(&mut self, _: &ToggleAllLineNumbers, window: &mut Window, cx: &mut Context<Self>) {
        self.line_numbers = !self.line_numbers;
        for ui in &mut self.cells {
            ui.line_numbers = None;
        }
        crate::settings::set_line_numbers(self.line_numbers);
        self.sync_editor_line_numbers(window, cx);
        cx.notify();
    }

    /// The open editor draws its own gutter; keep it in step.
    fn sync_editor_line_numbers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.as_ref() else { return };
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == editing.uid) else { return };
        let on = self.shows_line_numbers(ix);
        editing.editor.update(cx, |e, cx| e.set_line_number(on, window, cx));
    }

    fn on_toggle_input(&mut self, _: &ToggleInput, _: &mut Window, cx: &mut Context<Self>) {
        for ix in self.selection() {
            if let Some(ui) = self.cells.get_mut(ix) {
                ui.source_hidden = !ui.source_hidden;
            }
        }
        cx.notify();
    }
    fn on_toggle_output_scroll(&mut self, _: &ToggleOutputScroll, _: &mut Window, cx: &mut Context<Self>) {
        for ix in self.selection() {
            if let Some(ui) = self.cells.get_mut(ix) {
                ui.outputs_expanded = !ui.outputs_expanded;
            }
        }
        cx.notify();
    }
    /// Clearing outputs is undoable: they may have taken an hour to make.
    fn record_cells(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let removed = self.doc.nb.cells[range.clone()].to_vec();
        self.push_undo(UndoEntry { at: range.start, removed, inserted: range.len(), selected: self.selected });
    }
    fn on_clear_outputs(&mut self, _: &ClearOutputs, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        if self.doc.nb.cells[range.clone()].iter().any(|c| !c.outputs.is_empty() || c.execution_count.is_some()) {
            self.record_cells(range);
        }
        for ix in self.selection() {
            let cell = &mut self.doc.nb.cells[ix];
            if cell.outputs.is_empty() && cell.execution_count.is_none() {
                continue;
            }
            cell.outputs.clear();
            cell.execution_count = None;
            self.invalidate_outputs(ix);
        }
        self.mark_dirty(cx);
        cx.notify();
    }
    fn on_clear_all_outputs(&mut self, _: &ClearAllOutputs, _: &mut Window, cx: &mut Context<Self>) {
        if self.doc.nb.cells.iter().any(|c| !c.outputs.is_empty() || c.execution_count.is_some()) {
            self.record_cells(0..self.doc.nb.cells.len());
        }
        for ix in 0..self.doc.nb.cells.len() {
            let cell = &mut self.doc.nb.cells[ix];
            cell.outputs.clear();
            cell.execution_count = None;
            self.invalidate_outputs(ix);
        }
        self.mark_dirty(cx);
        cx.notify();
    }
    fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = &self.find {
            let handle = bar.input.read(cx).focus_handle(cx);
            handle.focus(window, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in notebook"));
        let subscription = cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                this.update_find(&text, cx);
            }
        });
        let handle = input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.find = Some(FindBar {
            input,
            replace: None,
            needle: find::Needle::new("", false),
            hits: Vec::new(),
            current: None,
            force_case: false,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn update_find(&mut self, text: &str, cx: &mut Context<Self>) {
        let selected = self.selected;
        let Some(bar) = &mut self.find else { return };
        bar.needle = find::Needle::new(text, bar.force_case);
        bar.hits = find::search(&self.doc.nb, &bar.needle);
        // Start from the selection, the way a reader expects.
        bar.current = bar.hits.iter().position(|h| h.cell >= selected).or((!bar.hits.is_empty()).then_some(0));
        let target = bar.current.map(|c| bar.hits[c].cell);
        if let Some(cell) = target {
            self.selected = cell;
            self.anchor = None;
            self.reveal(cell);
        }
        cx.notify();
    }

    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(bar) = &mut self.find else { return };
        if bar.hits.is_empty() {
            return;
        }
        let n = bar.hits.len();
        let next = match bar.current {
            Some(c) if forward => (c + 1) % n,
            Some(c) => (c + n - 1) % n,
            None => 0,
        };
        bar.current = Some(next);
        let cell = bar.hits[next].cell;
        self.selected = cell;
        self.anchor = None;
        self.reveal(cell);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    fn on_find_next(&mut self, _: &find::FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(true, cx);
    }

    fn on_find_prev(&mut self, _: &find::FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(false, cx);
    }

    fn on_toggle_find_case(&mut self, _: &find::ToggleFindCase, _: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = &mut self.find else { return };
        bar.force_case = !bar.force_case;
        let text = bar.needle.text.clone();
        self.update_find(&text, cx);
    }

    fn on_close_find(&mut self, _: &find::CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// ⌥⌘F: open the replace row under the find field, or close it.
    fn on_toggle_replace(&mut self, _: &find::ToggleReplace, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.on_find(&Find, window, cx);
        }
        let Some(bar) = &mut self.find else { return };
        let focus = match bar.replace.take() {
            Some(_) => bar.input.read(cx).focus_handle(cx),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Replace"));
                let handle = input.read(cx).focus_handle(cx);
                bar.replace = Some(input);
                handle
            }
        };
        focus.focus(window, cx);
        cx.notify();
    }

    /// The replacement text and the editor committed, so sources are current.
    fn begin_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let with = self.find.as_ref()?.replace.as_ref()?.read(cx).value().to_string();
        if self.editing.is_some() {
            self.stop_editing(window, cx);
            if let Some(input) = self.find.as_ref().and_then(|bar| bar.replace.as_ref()) {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        Some(with)
    }

    /// Search again after a replacement and land on the first match after
    /// `from` (a cell and a byte offset in its source).
    fn research_from(&mut self, from: (usize, usize), cx: &mut Context<Self>) {
        let Some(bar) = &mut self.find else { return };
        bar.hits = find::search(&self.doc.nb, &bar.needle);
        bar.current = bar
            .hits
            .iter()
            .position(|h| (h.cell, h.range.start) >= from)
            .or((!bar.hits.is_empty()).then_some(0));
        if let Some(cell) = bar.current.map(|c| bar.hits[c].cell) {
            self.selected = cell;
            self.anchor = None;
            self.reveal(cell);
        }
        cx.notify();
    }

    /// ↩ in the replace field: replace the current match in a cell's source
    /// (matches in outputs are skipped) and move on to the next.
    fn on_replace_one(&mut self, _: &find::ReplaceOne, window: &mut Window, cx: &mut Context<Self>) {
        let Some(with) = self.begin_replace(window, cx) else { return };
        let Some(bar) = &self.find else { return };
        let n = bar.hits.len();
        let start = bar.current.unwrap_or(0);
        let Some(hit) = (0..n)
            .map(|i| &bar.hits[(start + i) % n])
            .find(|h| h.target == find::Target::Source)
            .cloned()
        else {
            if n > 0 {
                self.flash("Matches in outputs can't be replaced", cx);
            }
            return;
        };
        let mut source = self.doc.nb.cells[hit.cell].source.clone();
        if !on_char_boundaries(&source, &hit.range) {
            return;
        }
        source.replace_range(hit.range.clone(), &with);
        self.set_sources(vec![(hit.cell, source)], cx);
        self.research_from((hit.cell, hit.range.start + with.len()), cx);
    }

    /// ⌘↩ in the replace field: replace every match in every cell's source,
    /// as one undo step.
    fn on_replace_all(&mut self, _: &find::ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        let Some(with) = self.begin_replace(window, cx) else { return };
        let Some(bar) = &self.find else { return };
        let mut edits: Vec<(usize, String)> = Vec::new();
        let mut count = 0;
        // Back to front within a cell, so earlier ranges stay put.
        for hit in bar.hits.iter().rev().filter(|h| h.target == find::Target::Source) {
            if edits.last().is_none_or(|(cell, _)| *cell != hit.cell) {
                edits.push((hit.cell, self.doc.nb.cells[hit.cell].source.clone()));
            }
            if let Some((_, source)) = edits.last_mut()
                && on_char_boundaries(source, &hit.range)
            {
                source.replace_range(hit.range.clone(), &with);
                count += 1;
            }
        }
        if count == 0 {
            return;
        }
        let selected = self.selected;
        self.set_sources(edits, cx);
        self.research_from((selected, 0), cx);
        self.flash(format!("Replaced {count} {}", if count == 1 { "match" } else { "matches" }), cx);
    }

    fn render_find_bar(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let bar = self.find.as_ref()?;
        let theme = cx.theme();
        let count: SharedString = match (bar.needle.is_empty(), bar.current) {
            (true, _) => "".into(),
            (false, Some(c)) => format!("{} of {}", c + 1, bar.hits.len()).into(),
            (false, None) => "No results".into(),
        };
        let icon_button = |id: &'static str, icon: &'static str, on: bool| crate::ui::icon_button(id, icon, on, cx);
        let accent = theme::accent(cx);
        let replacing = bar.replace.is_some();
        let row = || div().h(px(34.)).flex().flex_row().items_center().gap(px(2.)).pr(px(5.));
        // A quiet text button, like "Aa": the replace row's actions.
        let text_button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex_none()
                .h(crate::ui::CONTROL)
                .px(px(7.))
                .flex()
                .items_center()
                .rounded(crate::ui::CONTROL_RADIUS)
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .hover(|s| s.bg(theme.foreground.opacity(0.06)).text_color(theme.foreground))
                .child(label)
        };
        // The replace row: its field under the find field, then replace this
        // match or all of them.
        let replace_row = bar.replace.as_ref().map(|input| {
            row()
                .key_context(find::FIND_REPLACE)
                .border_t_1()
                .border_color(theme.border.opacity(0.6))
                .pl(px(2.))
                .child(div().w(crate::ui::CONTROL).flex_none())
                .child(div().w(px(13.)).flex_none())
                .child(div().flex_1().min_w_0().child(Input::new(input).appearance(false)))
                .child(
                    text_button("replace-one", "Replace")
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Replace  ↩").build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.on_replace_one(&find::ReplaceOne, window, cx))),
                )
                .child(
                    text_button("replace-all", "All")
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Replace All  ⌘↩").build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.on_replace_all(&find::ReplaceAll, window, cx))),
                )
        });
        Some(
            div()
                .key_context(find::FIND_BAR)
                .absolute()
                .top(px(10.))
                .right(px(20.))
                .w(px(420.))
                .flex()
                .flex_col()
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .child(
                    row()
                        .pl(px(2.))
                        .child(
                            crate::ui::icon_button(
                                "find-replace-toggle",
                                if replacing { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" },
                                false,
                                cx,
                            )
                            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Replace  ⌥⌘F").build(window, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.on_toggle_replace(&find::ToggleReplace, window, cx))),
                        )
                        .child(gpui_kit::svg().path("icons/search.svg").size(px(13.)).flex_none().text_color(theme.muted_foreground))
                        .child(div().flex_1().min_w_0().child(Input::new(&bar.input).appearance(false)))
                        .child(
                            div()
                                .flex_none()
                                .px(px(4.))
                                .text_size(px(11.5))
                                .text_color(theme.muted_foreground)
                                .child(count),
                        )
                        .child(
                            div()
                                .id("find-case")
                                .flex_none()
                                .h(crate::ui::CONTROL)
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .rounded(crate::ui::CONTROL_RADIUS)
                                .cursor_pointer()
                                .text_size(px(11.))
                                .when(bar.needle.case_sensitive, |d| d.bg(accent.opacity(0.12)).text_color(accent))
                                .when(!bar.needle.case_sensitive, |d| d.text_color(theme.muted_foreground))
                                .hover(|s| s.bg(theme.foreground.opacity(0.06)))
                                .child("Aa")
                                .on_click(cx.listener(|this, _, window, cx| this.on_toggle_find_case(&find::ToggleFindCase, window, cx))),
                        )
                        .child(
                            icon_button("find-prev", "icons/chevron-up.svg", false)
                                .on_click(cx.listener(|this, _, window, cx| this.on_find_prev(&find::FindPrev, window, cx))),
                        )
                        .child(
                            icon_button("find-next", "icons/chevron-down.svg", false)
                                .on_click(cx.listener(|this, _, window, cx| this.on_find_next(&find::FindNext, window, cx))),
                        )
                        .child(
                            icon_button("find-close", "icons/close.svg", false)
                                .on_click(cx.listener(|this, _, window, cx| this.on_close_find(&find::CloseFind, window, cx))),
                        ),
                )
                .children(replace_row)
                .into_any_element(),
        )
    }

    /// The notebook's names for completion: rebuilt after an edit, at most
    /// once a second while typing (a name typed a moment ago can wait).
    pub(super) fn word_index(&self) -> Rc<intellisense::WordIndex> {
        let mut cache = self.word_index.borrow_mut();
        if let Some((rev, built, index)) = cache.as_ref()
            && (*rev == self.doc_rev || built.elapsed() < Duration::from_secs(1))
        {
            return index.clone();
        }
        let index = Rc::new(intellisense::WordIndex::build(
            self.doc.nb.cells.iter().filter(|c| c.cell_type == CellType::Code).map(|c| c.source.as_str()),
        ));
        *cache = Some((self.doc_rev, Instant::now(), index.clone()));
        index
    }

    /// Tab takes the suggestion when completion is showing; after a name it
    /// asks for completions (as JupyterLab does); anywhere else it indents.
    fn on_tab(&mut self, _: &TabComplete, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else { return };
        let state = editing.editor.read(cx);
        if state.completion_menu_state().open {
            window.dispatch_action(Box::new(gpui_kit::component::input::Enter { secondary: false, shift: false }), cx);
            return;
        }
        let cursor = state.cursor();
        let text = state.value();
        let before = text[..cursor.min(text.len())].chars().next_back();
        let wants_completion = before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '"' | '\''));
        if wants_completion {
            window.dispatch_action(Box::new(gpui_kit::component::input::ShowCompletions), cx);
        } else {
            window.dispatch_action(Box::new(gpui_kit::component::input::Indent), cx);
        }
    }

    /// Return in the editor starts a new line even while completion shows
    /// (Tab takes a suggestion): the menu closes and the key goes on.
    fn on_editor_return(&mut self, _: &EditorReturn, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = &self.editing
            && editing.editor.read(cx).completion_menu_state().open
        {
            editing.editor.update(cx, |e, cx| e.dismiss_lsp_overlays(cx));
        }
        cx.propagate();
    }

    /// ⌘/ in the editor: comment or uncomment the selected lines (or the
    /// cursor's), as one undoable edit, in the kernel language's syntax.
    fn on_toggle_comment(&mut self, _: &ToggleComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else { return };
        let Some(ix) = self.cells.iter().position(|ui| ui.uid == editing.uid) else { return };
        if self.doc.nb.cells[ix].cell_type != CellType::Code {
            return;
        }
        let prefix = edit::comment_prefix(&self.grammar);
        editing.editor.update(cx, |e, cx| {
            let text = e.value().to_string();
            let selected = e.selected_range();
            let cursor = e.cursor();
            let (range, new) = edit::toggle_comment(&text, selected.clone(), prefix);
            let delta = new.len() as isize - range.len() as isize;
            e.set_selected_range(range.clone(), cx);
            e.replace(new.clone(), window, cx);
            if selected.is_empty() {
                // One line changed: keep the cursor on the same character.
                let at = (cursor as isize + delta).max(range.start as isize) as usize;
                e.set_selected_range(at..at, cx);
            } else {
                e.set_selected_range(range.start..range.start + new.len(), cx);
            }
        });
    }

    /// Apply a line edit to the open editor as one undoable change.
    fn edit_lines(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&str, Range<usize>) -> Option<edit::Edit>,
    ) {
        let Some(editing) = &self.editing else { return };
        editing.editor.update(cx, |e, cx| {
            let text = e.value().to_string();
            let Some(edit) = f(&text, e.selected_range()) else { return };
            e.set_selected_range(edit.range, cx);
            e.replace(edit.text, window, cx);
            e.set_selected_range(edit.select, cx);
        });
    }
    fn on_move_line_up(&mut self, _: &MoveLineUp, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, range| edit::move_lines(text, range, true));
    }
    fn on_move_line_down(&mut self, _: &MoveLineDown, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, range| edit::move_lines(text, range, false));
    }
    fn on_delete_line(&mut self, _: &DeleteLine, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, range| Some(edit::delete_lines(text, range)));
    }
    fn on_copy_line_up(&mut self, _: &CopyLineUp, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, range| Some(edit::copy_lines(text, range, true)));
    }
    fn on_copy_line_down(&mut self, _: &CopyLineDown, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, range| Some(edit::copy_lines(text, range, false)));
    }
    fn on_select_line(&mut self, _: &SelectLine, _: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else { return };
        editing.editor.update(cx, |e, cx| {
            let range = edit::select_lines(&e.value(), e.selected_range());
            e.set_selected_range(range, cx);
        });
    }

    /// ⌘D in command mode: copies of the selected cells, right after them,
    /// with ids of their own.
    fn on_duplicate_cell(&mut self, _: &DuplicateCell, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.nb.cells.is_empty() {
            return;
        }
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        let range = self.selection();
        let copies: Vec<Cell> = self.doc.nb.cells[range.clone()]
            .iter()
            .map(|cell| Cell { id: self.new_cell(cell.cell_type).id, ..cell.clone() })
            .collect();
        let n = copies.len();
        self.splice_cells(range.end..range.end, copies, true, cx);
        self.anchor = None;
        self.select(range.end, false, cx);
        if n > 1 {
            self.select(range.end + n - 1, true, cx);
        }
    }

    fn on_split(&mut self, _: &SplitCell, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else { return };
        let cursor = editing.editor.read(cx).cursor();
        let ix = self.selected;
        let source = self.doc.nb.cells[ix].source.clone();
        let cursor = cursor.min(source.len());
        let (a, b) = source.split_at(cursor);
        let mut first = self.doc.nb.cells[ix].clone();
        first.source = a.trim_end_matches('\n').to_string();
        first.outputs.clear();
        let mut second = self.new_cell(first.cell_type);
        second.source = b.trim_start_matches('\n').to_string();
        self.stop_editing(window, cx);
        self.splice_cells(ix..ix + 1, vec![first, second], true, cx);
        self.select(ix + 1, false, cx);
        self.start_editing(window, cx);
    }
}

impl Focusable for NotebookView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotebookView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::images::collect(window, cx);
        self.update_changes();
        // An extension finished drawing an output: build outputs again.
        let generation = crate::extensions::render_generation(cx);
        if generation != self.ext_generation {
            self.ext_generation = generation;
            self.restyle_outputs(cx);
        }
        let comments_panel = self.render_comments_panel(window, cx);
        if self.stdin.is_none() && self.session.as_ref().is_some_and(|s| s.pending_input.is_some()) {
            self.ensure_stdin_input(window, cx);
        }
        self.viewport_width = self.list.viewport_bounds().size.width;
        let theme = cx.theme();
        // Typing into an output (an input() prompt, a widget's text field)
        // must not trigger single-key cell commands.
        let typing = self.stdin.as_ref().is_some_and(|i| i.read(cx).focus_handle(cx).is_focused(window))
            || self.widget_inputs.values().any(|(i, _)| i.read(cx).focus_handle(cx).is_focused(window));
        let context = if self.editing.is_some() {
            NOTEBOOK_EDIT
        } else if typing {
            crate::actions::NOTEBOOK_TEXT
        } else {
            NOTEBOOK_COMMAND
        };
        let empty = self.doc.nb.cells.is_empty();
        let find_bar = self.render_find_bar(cx);
        let problem = self.render_problem(cx);
        let preflight = self.render_preflight(cx);
        let theme = theme.clone();
        // Cell commands live on the focused surface only; the find bar is a
        // sibling, so typing in it never triggers single-key commands.
        let surface = div()
            .id("notebook-surface")
            .key_context(context)
            .track_focus(&self.focus_handle)
            .w_full()
            .flex_1()
            .min_h_0()
            // A click on the page around the cells (below the last one, in
            // the side margins) puts the editor away, as anywhere else would.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.press_in_editor = false;
                    if !std::mem::take(&mut this.cell_pressed) && this.editing.is_some() {
                        this.stop_editing(window, cx);
                        this.focus_handle.focus(window, cx);
                    }
                }),
            )
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_extend_next))
            .on_action(cx.listener(Self::on_extend_prev))
            .on_action(cx.listener(Self::on_select_first))
            .on_action(cx.listener(Self::on_select_last))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_enter_edit))
            .on_action(cx.listener(Self::on_exit_edit))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_insert_above))
            .on_action(cx.listener(Self::on_insert_below))
            .on_action(cx.listener(Self::on_to_markdown))
            .on_action(cx.listener(Self::on_to_code))
            .on_action(cx.listener(Self::on_to_raw))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_command_copy))
            .on_action(cx.listener(Self::on_command_cut))
            .on_action(cx.listener(Self::on_command_paste))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_merge_below))
            .on_action(cx.listener(Self::on_toggle_output))
            .on_action(cx.listener(Self::on_toggle_input))
            .on_action(cx.listener(|this, _: &StopTyping, window, cx| {
                this.focus_handle.focus(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(Self::on_hide_all_code))
            .on_action(cx.listener(Self::on_show_all_code))
            .on_action(cx.listener(Self::on_toggle_line_numbers))
            .on_action(cx.listener(Self::on_toggle_all_line_numbers))
            .on_action(cx.listener(Self::on_toggle_output_scroll))
            .on_action(cx.listener(Self::on_split))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_move_line_up))
            .on_action(cx.listener(Self::on_move_line_down))
            .on_action(cx.listener(Self::on_delete_line))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_editor_return))
            .on_action(cx.listener(Self::on_editor_undo))
            .on_action(cx.listener(Self::on_editor_redo))
            .on_action(cx.listener(Self::on_collapse))
            .on_action(cx.listener(Self::on_expand))
            .on_action(cx.listener(Self::on_collapse_all))
            .on_action(cx.listener(Self::on_expand_all))
            .on_action(cx.listener(Self::on_next_error))
            .on_action(cx.listener(Self::on_next_change))
            .on_action(cx.listener(Self::on_prev_change))
            .on_action(cx.listener(Self::on_toggle_changes))
            .on_action(cx.listener(Self::on_prev_error))
            .on_action(cx.listener(Self::on_hide_all_outputs))
            .on_action(cx.listener(Self::on_show_all_outputs))
            .when(empty, |d| {
                d.child(
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child("This notebook is empty")
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(4.))
                                .child(self.add_cell_button("empty-add-code", CellType::Code, window, cx))
                                .child(self.add_cell_button("empty-add-markdown", CellType::Markdown, window, cx)),
                        ),
                )
            })
            .when(!empty, |d| {
                d.child(
                    list(
                        self.list.clone(),
                        cx.processor(|this, ix, window, cx| this.render_cell(ix, window, cx)),
                    )
                    .size_full()
                    .pt(px(12.))
                    .pb(px(240.)),
                )
            });
        div()
            .id("notebook")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .text_size(theme::content_size())
            .bg(theme.background)
            .text_color(theme.foreground)
            // Commands that make sense from anywhere in the notebook, find bar included.
            .on_action(cx.listener(Self::on_insert_code_above))
            .on_action(cx.listener(Self::on_insert_code_below))
            .on_action(cx.listener(Self::on_insert_markdown_below))
            .on_action(cx.listener(Self::on_clear_outputs))
            .on_action(cx.listener(Self::on_clear_all_outputs))
            .on_action(cx.listener(Self::on_find))
            .on_action(cx.listener(Self::on_find_next))
            .on_action(cx.listener(Self::on_find_prev))
            .on_action(cx.listener(Self::on_close_find))
            .on_action(cx.listener(Self::on_toggle_find_case))
            .on_action(cx.listener(Self::on_toggle_replace))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_duplicate_cell))
            .on_action(cx.listener(Self::on_copy_line_up))
            .on_action(cx.listener(Self::on_copy_line_down))
            .on_action(cx.listener(Self::on_select_line))
            .on_action(cx.listener(Self::on_add_comment))
            .on_action(cx.listener(Self::on_toggle_comments))
            .on_action(cx.listener(Self::on_select_all_cells))
            .on_action(cx.listener(Self::on_replace_one))
            .on_action(cx.listener(Self::on_replace_all))
            .on_action(cx.listener(Self::on_run))
            .on_action(cx.listener(Self::on_run_advance))
            .on_action(cx.listener(Self::on_run_insert))
            .on_action(cx.listener(Self::on_run_all))
            .on_action(cx.listener(Self::on_run_above))
            .on_action(cx.listener(Self::on_run_below))
            .on_action(cx.listener(Self::on_interrupt))
            .on_action(cx.listener(Self::on_restart))
            .on_action(cx.listener(Self::on_restart_run_all))
            .on_action(cx.listener(Self::on_shutdown))
            .on_action(cx.listener(Self::on_reload))
            .on_action(cx.listener(Self::on_export_html))
            .when_some(problem, |d, p| d.child(p))
            .when_some(preflight, |d, p| d.child(p))
            .child(
                // The page and, when open, the comments beside it; the
                // scrollbar stays on the page's edge.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(surface)
                            .when(!empty, |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .right_0()
                                        .bottom_0()
                                        .w(px(12.))
                                        .child(Scrollbar::vertical(&self.list)),
                                )
                            }),
                    )
                    .children(comments_panel),
            )
            .when_some(find_bar, |d, bar| d.child(bar))
    }
}

/// A whisper of the foreground over the page: reads as a code surface in
/// both light and dark without introducing a colour.
fn code_background(cx: &App) -> gpui_kit::Hsla {
    let theme = cx.theme();
    theme.background.blend(theme.foreground.opacity(if theme.is_dark() { 0.055 } else { 0.035 }))
}

fn h_flex_start() -> gpui_kit::Div {
    div().flex().flex_row().items_start()
}

fn mono() -> gpui_kit::Div {
    div()
        .font_family(theme::mono_font())
        .text_size(theme::mono_size())
        .line_height(theme::mono_line_height())
}

/// A code block. Its hairline border carries the cell's state: plain, strong
/// when selected, ink while editing. The width never changes, so switching
/// states moves nothing.
fn code_box(bg: gpui_kit::Hsla, border: gpui_kit::Hsla) -> gpui_kit::Div {
    div()
        .w_full()
        .rounded(px(8.))
        .bg(bg)
        .border_1()
        .border_color(border)
        .px(theme::CELL_PAD_X)
        .py(theme::CELL_PAD_Y)
}

/// The editor's gap between its line numbers and the text.
const LINE_NUMBER_MARGIN: gpui_kit::Pixels = px(6.);

/// One right-aligned number per row, padded to the editor's width (at least
/// three digits), and that digit count.
fn line_numbers(source: &str) -> (SharedString, usize) {
    use std::fmt::Write as _;
    let rows = source_rows(source);
    let digits = (rows.ilog10() as usize + 1).clamp(3, 7);
    let mut out = String::with_capacity(rows * (digits + 1));
    for n in 1..=rows {
        if n > 1 {
            out.push('\n');
        }
        let _ = write!(out, "{n:>digits$}");
    }
    (out.into(), digits)
}

/// Whether `range` cuts `text` only between characters (a case-insensitive
/// match can, rarely, be found in lowercased text whose offsets differ).
fn on_char_boundaries(text: &str, range: &Range<usize>) -> bool {
    range.end <= text.len() && text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
}

/// Rows a source occupies when drawn: a trailing newline adds an empty line.
fn source_rows(source: &str) -> usize {
    source.split('\n').count().max(1)
}

/// Show an output that needs a browser there: an embedded page by its URL,
/// anything else from a temporary file (named by content, so reopening the
/// same output reuses it).
fn open_html_in_browser(html: &str, src: Option<&str>, cx: &mut App) {
    if let Some(url) = src {
        cx.open_url(url);
        return;
    }
    let page = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Notebook output</title></head>\n<body>\n{html}\n</body></html>\n"
    );
    open_temp_file(page.as_bytes(), "html", cx);
}

/// Write an output to a temporary file named by its content and open it with
/// the system's handler for `ext`.
fn open_temp_file(bytes: &[u8], ext: &str, cx: &mut App) {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let path = std::env::temp_dir().join(format!("herse-output-{:016x}.{ext}", hasher.finish()));
    if std::fs::write(&path, bytes).is_ok() {
        cx.open_url(&format!("file://{}", crate::http::percent_encode_path(&path.to_string_lossy())));
    }
}

fn format_bytes(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.0} KB", n as f64 / 1024.),
        n => format!("{:.1} MB", n as f64 / (1024. * 1024.)),
    }
}

/// What an image that fails to decode draws instead (`img` draws nothing).
pub(super) fn broken_image(color: gpui_kit::Hsla) -> impl Fn() -> AnyElement + 'static {
    move || {
        div()
            .text_size(px(12.))
            .italic()
            .text_color(color)
            .child("Image could not be decoded")
            .into_any_element()
    }
}

/// A table as tab-separated text, header rows first.
fn table_tsv(table: &herse_table::Table) -> String {
    table
        .header
        .iter()
        .chain(&table.body)
        .map(|row| {
            row.iter()
                .map(|c| if c.colspan == 0 { "" } else { c.text.as_str() })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An output's Copy chip: top-right, shown while the output (`group`) is
/// hovered. `copy` writes the clipboard and names what it copied.
fn copy_chip(
    id: u64,
    group: SharedString,
    tip: &'static str,
    cx: &Context<NotebookView>,
    copy: impl Fn(&mut App) -> &'static str + 'static,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    crate::ui::icon_button(("copy-output", id), "icons/copy.svg", false, cx)
        .absolute()
        .top(px(2.))
        .right(px(4.))
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .opacity(0.)
        .group_hover(group, |s| s.opacity(1.))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            let done = copy(cx);
            this.flash(done, cx);
        }))
}

/// Right-click on an output: copy it (or save a figure).
fn output_menu(item: &OutputItem, el: AnyElement, id: u64) -> AnyElement {
    use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
    enum Payload {
        Text(SharedString),
        Image(std::sync::Arc<gpui_kit::Image>),
        Table(std::sync::Arc<herse_table::Table>),
        None,
    }
    let payload = match item {
        OutputItem::Text { text, .. } => Payload::Text(text.clone()),
        OutputItem::Image { image, .. } => Payload::Image(image.clone()),
        OutputItem::Table(t) => Payload::Table(t.table.clone()),
        _ => Payload::None,
    };
    if matches!(payload, Payload::None) {
        return el;
    }
    let payload = Rc::new(payload);
    div()
        .id(("output", id))
        .child(el)
        .context_menu(move |menu, _window, _cx| {
            let p = payload.clone();
            match &*p {
                Payload::Text(text) => {
                    let text = text.clone();
                    menu.item(PopupMenuItem::new("Copy Output").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                    }))
                }
                Payload::Image(image) => {
                    let copy = image.clone();
                    let save = image.clone();
                    menu.item(PopupMenuItem::new("Copy Image").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_image(&copy));
                    }))
                    .item(PopupMenuItem::new("Save Image As…").on_click(move |_, _, cx| {
                        let image = save.clone();
                        let ext = match image.format() {
                            gpui_kit::ImageFormat::Jpeg => "jpg",
                            gpui_kit::ImageFormat::Svg => "svg",
                            gpui_kit::ImageFormat::Gif => "gif",
                            gpui_kit::ImageFormat::Webp => "webp",
                            _ => "png",
                        };
                        let dir = std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join("Downloads"))
                            .unwrap_or_else(|| PathBuf::from("."));
                        let rx = cx.prompt_for_new_path(&dir, Some(&format!("figure.{ext}")));
                        cx.spawn(async move |_| {
                            if let Ok(Ok(Some(path))) = rx.await {
                                let _ = std::fs::write(path, image.bytes());
                            }
                        })
                        .detach();
                    }))
                }
                Payload::Table(table) => {
                    let table = table.clone();
                    menu.item(PopupMenuItem::new("Copy as TSV").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(table_tsv(&table)));
                    }))
                }
                Payload::None => menu,
            }
        })
        .into_any_element()
}

/// Append an output, merging consecutive chunks of the same stream the way
/// Jupyter stores them.
fn push_output(outputs: &mut Vec<Output>, output: Output) {
    if let Output::Stream { name, text } = &output
        && let Some(Output::Stream { name: last, text: last_text }) = outputs.last_mut()
        && last == name
    {
        last_text.push_str(text);
        return;
    }
    outputs.push(output);
}

/// `text[range]` as selectable styled text: ANSI runs clipped to the range,
/// find matches on top.
fn styled_segment(
    id: gpui_kit::ElementId,
    text: &SharedString,
    runs: &outputs::Runs,
    range: Range<usize>,
    needle: Option<&find::Needle>,
    dark: bool,
) -> gpui_kit::base::SelectableText {
    let slice: SharedString = if range.start == 0 && range.end == text.len() {
        text.clone()
    } else {
        SharedString::from(text[range.clone()].to_string())
    };
    // Runs are sorted by start; skip to the first that can overlap.
    let first = runs.partition_point(|(r, _)| r.end <= range.start);
    let ansi = runs[first..]
        .iter()
        .take_while(|(r, _)| r.start < range.end)
        .map(|(r, s)| (r.start.max(range.start) - range.start..r.end.min(range.end) - range.start, *s))
        .filter(|(r, _)| !r.is_empty());
    let found: Vec<_> = needle
        .map(|n| n.find_all(&slice).into_iter().map(|r| (r, find::match_style(false, dark))).collect())
        .unwrap_or_default();
    let selectable = gpui_kit::base::SelectableText::new(id, slice);
    if found.is_empty() {
        selectable.with_highlights(ansi)
    } else {
        selectable.with_highlights(gpui_kit::combine_highlights(ansi, found))
    }
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

