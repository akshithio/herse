//! A table to look at, full size: a data file (CSV, Parquet, JSON, Excel,
//! SQLite…) opened from the sidebar, ⌘O or a drag, or a notebook's
//! DataFrame output opened from its Open chip. Both become a
//! [`herse_data::Frame`] drawn here.
//!
//! Speed is the point. A file is read on its own thread and its rows show
//! as they arrive (the first 512 in about a millisecond); the body is a
//! `uniform_list`, so only the rows on screen exist, and each row is one
//! canvas, so a row of 50 columns is a few shaped lines rather than 50
//! laid-out elements. Sorting, filtering and searching run in the
//! background and swap the row order in when done.
//!
//! The look follows DESIGN.md: rules, not grids; ink on paper; names in
//! semibold with their type in pencil underneath; dew only for state (the
//! sort arrow, an active filter, the selected cell, search hits).

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use gpui_kit::AppContext as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hitbox,
    HitboxBehavior, Hsla, InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Render, ScrollStrategy, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TextAlign, TextRun, UniformListScrollHandle, WeakEntity, Window, actions, canvas,
    div, fill, font, point, prelude::FluentBuilder as _, px, size, svg, uniform_list,
};
use herse_data::{Event, Filter, Flow, Format, Frame, Library, Loaded, Order, Profile, Shape, ViewSpec};

use crate::{notebook::NotebookView, theme};

actions!(
    frame,
    [
        FocusSearch,
        CopySelection,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        PageUp,
        PageDown,
        MoveToTop,
        MoveToBottom,
        ClearSearch,
        ApplyFilter,
        CancelFilter,
        EditFilter,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        SelectAll,
        ShowValue,
        Dismiss,
        UndoView,
        RedoView,
    ]
);

/// The viewer's key context.
pub const FRAME: &str = "FrameView";
const FRAME_SEARCH: &str = "FrameSearch";
const FRAME_FILTER: &str = "FrameFilter";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(FRAME);
    cx.bind_keys([
        KeyBinding::new("cmd-f", FocusSearch, ctx),
        KeyBinding::new("cmd-c", CopySelection, ctx),
        KeyBinding::new("up", MoveUp, ctx),
        KeyBinding::new("down", MoveDown, ctx),
        KeyBinding::new("left", MoveLeft, ctx),
        KeyBinding::new("right", MoveRight, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("cmd-up", MoveToTop, ctx),
        KeyBinding::new("cmd-down", MoveToBottom, ctx),
        KeyBinding::new("cmd-shift-l", EditFilter, ctx),
        // A text field inside keeps its own ⌘Z (its context is deeper).
        KeyBinding::new("cmd-z", UndoView, ctx),
        KeyBinding::new("cmd-shift-z", RedoView, ctx),
    ]);
    // Keys a text field wants for itself: only while the table has focus.
    let grid = Some("FrameView && !Input");
    cx.bind_keys([
        KeyBinding::new("shift-up", SelectUp, grid),
        KeyBinding::new("shift-down", SelectDown, grid),
        KeyBinding::new("shift-left", SelectLeft, grid),
        KeyBinding::new("shift-right", SelectRight, grid),
        KeyBinding::new("cmd-a", SelectAll, grid),
        KeyBinding::new("space", ShowValue, grid),
        KeyBinding::new("escape", Dismiss, grid),
    ]);
    let search = Some("FrameSearch > Input");
    cx.bind_keys([KeyBinding::new("escape", ClearSearch, search), KeyBinding::new("enter", MoveDown, search)]);
    let filter = Some("FrameFilter > Input");
    cx.bind_keys([KeyBinding::new("enter", ApplyFilter, filter), KeyBinding::new("escape", CancelFilter, filter)]);
}

const FONT_SIZE: f32 = 12.5;
const ROW_H: f32 = 26.;
const HEADER_H: f32 = 42.;
const PAD: f32 = 10.;
/// Average advance of the UI font at FONT_SIZE (tabular digits included).
const CHAR_W: f32 = 7.3;
/// The profile strip under the names, and its parts (at zoom 1).
const PROFILE_H: f32 = 52.;
const PROFILE_TOP: f32 = 2.;
const PROFILE_CHART_H: f32 = 30.;
const PROFILE_ROW: f32 = 13.;
const PROFILE_ROWS: usize = 3;
const PROFILE_MISSING_Y: f32 = 48.;
/// Average advance of a semibold column name.
const NAME_CHAR_W: f32 = 8.0;
/// Room after a column's name for the sort arrow and filter mark.
const MARKS: f32 = 30.;
const MIN_COLUMN: f32 = 56.;
const MAX_COLUMN_CHARS: u16 = 42;
/// How many rows Copy takes at most.
const COPY_LIMIT: usize = 100_000;

/// Where the table came from.
pub enum Source {
    File { path: PathBuf, format: Format, table: Option<String> },
    Output(OutputSource),
}

/// A DataFrame output of a notebook's cell.
pub struct OutputSource {
    pub notebook: WeakEntity<NotebookView>,
    pub notebook_title: String,
    pub cell_uid: u64,
    pub output: usize,
    /// The cell's number, from 1.
    pub cell_number: usize,
    /// What the frame is called (`df`, `Out[12]`, `Cell 4`).
    pub name: String,
    pub saved_rows: usize,
    pub total_rows: Option<usize>,
    pub total_columns: Option<usize>,
    pub truncated: bool,
    /// Every row is shown (Load All Rows finished).
    pub complete: bool,
    /// Leading index columns of the frame the kernel wrote.
    pub fetched_index: usize,
}

pub enum FrameEvent {
    /// The title changed.
    Changed,
    /// New Notebook from This File made one: open it.
    OpenNotebook(Rc<RefCell<Option<crate::notebook::document::Document>>>),
}

enum Load {
    Reading { progress: Option<f64>, started: Instant },
    Done { ms: f64 },
    Failed(SharedString),
}

enum Fetch {
    Idle,
    Running(Instant),
    Failed(SharedString),
}

/// Column positions: `xs[c]..xs[c + 1]` is column `c`, after a gutter of
/// row numbers; the first `sticky` columns (the gutter and any index
/// columns) stay put when the table scrolls sideways.
#[derive(Clone, Debug, Default)]
struct Geometry {
    xs: Arc<[f32]>,
    gutter: f32,
    sticky: usize,
    width: f32,
    digits: usize,
}

impl Geometry {
    fn sticky_width(&self) -> f32 {
        if self.sticky == 0 { self.gutter } else { self.xs[self.sticky] }
    }

    /// The column under content x `x` (or None for the gutter / past the end).
    fn column_at(&self, x: f32) -> Option<usize> {
        if x < self.gutter {
            return None;
        }
        let ix = self.xs.partition_point(|&c| c <= x);
        (ix >= 1 && ix < self.xs.len()).then(|| ix - 1)
    }
}

/// A column being resized by its header's right edge.
#[derive(Clone, Copy)]
struct Resize {
    column: usize,
    start_x: Pixels,
    start_width: f32,
}

struct FilterEdit {
    column: usize,
    input: Entity<InputState>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

pub struct FrameView {
    source: Source,
    frame: Arc<Frame>,
    load: Load,
    loaded: Option<Loaded>,
    spec: ViewSpec,
    /// The visible rows in order, when not every row in file order.
    order: Option<Arc<Vec<u32>>>,
    sorting: Option<Instant>,
    order_stale: bool,
    order_cancel: Arc<AtomicBool>,
    order_ms: Option<f64>,
    geometry: Geometry,
    /// Widths the reader set by dragging, by column.
    widths: Vec<Option<f32>>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    search: Entity<InputState>,
    filter: Option<FilterEdit>,
    hover_column: Option<usize>,
    resize: Rc<Cell<Option<Resize>>>,
    /// Each column's profile over every row, and over the rows in view
    /// when a filter or search hides some.
    profiles: Option<Arc<Vec<Option<Profile>>>>,
    view_profiles: Option<Arc<Vec<Option<Profile>>>>,
    profile_cancel: Arc<AtomicBool>,
    /// The part of a profile under the pointer, and where the pointer is.
    hover_part: Option<(usize, Part)>,
    pointer: gpui_kit::Point<Pixels>,
    /// Where the header was drawn (its top, for which line the pointer is on).
    header_top: Rc<Cell<Pixels>>,
    /// (visible row, column): the cell with the cursor.
    selected: Option<(usize, usize)>,
    /// The other corner of a range of cells (shift-click or drag).
    anchor: Option<(usize, usize)>,
    dragging: bool,
    summary: Option<Summary>,
    /// A cell's whole value, shown on a double-click: (file row, column, where).
    popover: Option<(usize, usize, gpui_kit::Point<Pixels>)>,
    message: Option<(SharedString, Instant)>,
    /// ⌘Z / ⌘⇧Z: the view before each change, with what the change was.
    undo: Vec<(ViewState, &'static str)>,
    redo: Vec<(ViewState, &'static str)>,
    /// The search field's edits since it took focus are one step.
    search_step: bool,
    /// The view before a column's edge was dragged.
    width_before: Option<ViewState>,
    fetch: Fetch,
    /// The reader closed the read error's banner.
    error_dismissed: bool,
    stop: Arc<AtomicBool>,
    _reader: Option<Task<()>>,
    _order_task: Option<Task<()>>,
    _profile_task: Option<Task<()>>,
    _summary_task: Option<Task<()>>,
    _ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What the reader changed about the view, for ⌘Z: the sort, the filters,
/// the search and the column widths (not the selection, scroll or hover).
#[derive(Clone)]
struct ViewState {
    sort: Option<(usize, Order)>,
    filters: Vec<Filter>,
    search: String,
    widths: Vec<Option<f32>>,
}

impl ViewState {
    fn same(&self, other: &ViewState) -> bool {
        let n = self.widths.len().max(other.widths.len());
        let width = |w: &[Option<f32>], i: usize| w.get(i).copied().flatten();
        self.sort == other.sort
            && self.filters.len() == other.filters.len()
            && self.filters.iter().zip(&other.filters).all(|(a, b)| a.column == b.column && a.text == b.text)
            && self.search.trim() == other.search.trim()
            && (0..n).all(|i| width(&self.widths, i) == width(&other.widths, i))
    }

    /// Only the widths differ: the rows stay as they are.
    fn same_rows(&self, other: &ViewState) -> bool {
        ViewState { widths: other.widths.clone(), ..self.clone() }.same(other)
    }
}

/// How many steps ⌘Z goes back.
const UNDO_LIMIT: usize = 200;

/// A part of a column's profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Bin(usize),
    Top(usize),
    Yes,
    No,
    Missing,
}

/// What a range of cells adds up to.
#[derive(Clone, Debug, PartialEq)]
struct Summary {
    cells: usize,
    numbers: usize,
    sum: f64,
    min: f64,
    max: f64,
}

impl EventEmitter<FrameEvent> for FrameView {}

impl Focusable for FrameView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for FrameView {
    fn drop(&mut self) {
        // The reader thread stops at its next chunk.
        self.stop.store(true, AtomicOrdering::Relaxed);
        self.order_cancel.store(true, AtomicOrdering::Relaxed);
        self.profile_cancel.store(true, AtomicOrdering::Relaxed);
    }
}

impl FrameView {
    fn new(source: Source, frame: Frame, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search rows"));
        let sub = cx.subscribe_in(&search, window, |this, state, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = state.read(cx).value().to_string();
                if text.trim() != this.spec.search.trim() {
                    // Typing is one step until the field loses focus.
                    if !this.search_step {
                        let before = this.view_state();
                        this.push_undo(before, "the search");
                        this.search_step = true;
                    }
                    this.spec.search = text;
                    this.clear_selection();
                    this.reorder(cx);
                }
            }
        });
        let search_focus = search.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&search_focus, window, |this, _, _| this.search_step = false);
        let mut view = FrameView {
            source,
            frame: Arc::new(frame),
            load: Load::Done { ms: 0. },
            loaded: None,
            spec: ViewSpec::default(),
            order: None,
            sorting: None,
            order_stale: false,
            order_cancel: Arc::new(AtomicBool::new(false)),
            order_ms: None,
            geometry: Geometry::default(),
            widths: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            search,
            filter: None,
            hover_column: None,
            resize: Rc::new(Cell::new(None)),
            profiles: None,
            view_profiles: None,
            profile_cancel: Arc::new(AtomicBool::new(false)),
            hover_part: None,
            pointer: Default::default(),
            header_top: Rc::new(Cell::new(px(0.))),
            selected: None,
            anchor: None,
            dragging: false,
            summary: None,
            popover: None,
            message: None,
            undo: Vec::new(),
            redo: Vec::new(),
            search_step: false,
            width_before: None,
            fetch: Fetch::Idle,
            error_dismissed: false,
            stop: Arc::new(AtomicBool::new(false)),
            _reader: None,
            _order_task: None,
            _profile_task: None,
            _summary_task: None,
            _ticker: None,
            _subscriptions: vec![sub, blur],
        };
        view.layout_columns();
        if view.frame.rows() > 0 {
            view.profile_all(cx);
        }
        view
    }

    /// A data file, read in the background.
    pub fn open_file(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let format = Format::of(&path).unwrap_or(Format::Csv { gz: false });
        let mut view = Self::new(Source::File { path, format, table: None }, Frame::default(), window, cx);
        view.start_reading(None, cx);
        view
    }

    /// A notebook's DataFrame output: the rows it saved.
    pub fn from_output(table: &herse_table::Table, source: OutputSource, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut source = source;
        let parsed = herse_data::from_table(table);
        source.saved_rows = parsed.saved_rows;
        source.total_rows = parsed.total_rows;
        source.total_columns = parsed.total_columns;
        source.truncated = parsed.truncated;
        Self::new(Source::Output(source), parsed.frame, window, cx)
    }

    pub fn path(&self) -> Option<&PathBuf> {
        match &self.source {
            Source::File { path, .. } => Some(path),
            Source::Output(_) => None,
        }
    }

    /// The table as read so far, for the assistant (⌘I) to read on its own
    /// thread.
    pub fn frame(&self) -> Arc<Frame> {
        self.frame.clone()
    }

    /// The tab's title.
    pub fn title(&self) -> String {
        match &self.source {
            Source::File { path, table, .. } => {
                let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                match table {
                    Some(t) if self.loaded.as_ref().is_some_and(|l| l.tables.len() > 1) => format!("{name} › {t}"),
                    _ => name,
                }
            }
            Source::Output(o) => format!("{} · {}", o.name, o.notebook_title),
        }
    }

    /// The file changed on disk: read it again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if matches!(self.source, Source::File { .. }) && !matches!(self.load, Load::Reading { .. }) {
            let table = match &self.source {
                Source::File { table, .. } => table.clone(),
                Source::Output(_) => None,
            };
            self.start_reading(table, cx);
        }
    }

    /// Renamed in the tree.
    pub fn set_path(&mut self, to: PathBuf, cx: &mut Context<Self>) {
        if let Source::File { path, .. } = &mut self.source {
            *path = to;
            cx.emit(FrameEvent::Changed);
            cx.notify();
        }
    }

    pub fn zoom_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    // ------------------------------------------------------------ reading

    fn start_reading(&mut self, table: Option<String>, cx: &mut Context<Self>) {
        let Source::File { path, .. } = &self.source else { return };
        let path = path.clone();
        self.stop.store(true, AtomicOrdering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        self.frame = Arc::new(Frame::default());
        self.order = None;
        self.profiles = None;
        self.view_profiles = None;
        self.clear_selection();
        self.widths.clear();
        self.forget_history();
        self.error_dismissed = false;
        self.load = Load::Reading { progress: None, started: Instant::now() };
        let started = Instant::now();
        let (tx, rx) = smol::channel::unbounded::<Msg>();
        let spawned = std::thread::Builder::new().name("herse-data".into()).spawn(move || {
            let result = herse_data::read(&path, table.as_deref(), &mut |event| {
                if stop.load(AtomicOrdering::Relaxed) || tx.send_blocking(Msg::Event(event)).is_err() {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            });
            let _ = tx.send_blocking(Msg::Done(result));
        });
        if let Err(e) = spawned {
            self.load = Load::Failed(format!("Couldn't start reading: {e}").into());
            return;
        }
        self._reader = Some(cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                if this.update(cx, |this, cx| this.apply(batch, started, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply(&mut self, batch: Vec<Msg>, started: Instant, cx: &mut Context<Self>) {
        let frame = Arc::make_mut(&mut self.frame);
        let mut relayout = false;
        let mut grew = false;
        for msg in batch {
            match msg {
                Msg::Event(Event::Columns(mut columns)) => {
                    // The index a kernel's frame was written with.
                    if let Source::Output(o) = &self.source {
                        for c in columns.iter_mut().take(o.fetched_index) {
                            c.index = true;
                        }
                    }
                    relayout |= columns.len() != frame.columns().len();
                    frame.set_columns(columns);
                }
                Msg::Event(Event::Chunk(chunk)) => {
                    relayout |= frame.rows() == 0;
                    frame.push_chunk(chunk);
                    grew = true;
                }
                Msg::Event(Event::Progress(p)) => {
                    if let Load::Reading { progress, .. } = &mut self.load {
                        *progress = Some(p);
                    }
                }
                Msg::Done(result) => {
                    let ms = started.elapsed().as_secs_f64() * 1000.;
                    match result {
                        Ok(loaded) => {
                            if let Source::File { table, .. } = &mut self.source
                                && table.is_none()
                            {
                                *table = loaded.sniffed.table.clone();
                            }
                            self.loaded = Some(loaded);
                            self.load = Load::Done { ms };
                            cx.emit(FrameEvent::Changed);
                        }
                        Err(e) => self.load = Load::Failed(e.0.into()),
                    }
                    relayout = true;
                    grew = true;
                    self.order_stale = true;
                }
            }
        }
        if relayout || digits(self.frame.rows()) != self.geometry.digits {
            self.layout_columns();
        }
        // Profiles of the first rows at once, of every row once read.
        let done = matches!(self.load, Load::Done { .. } | Load::Failed(_));
        if grew && (done || self.profiles.is_none()) && self.frame.rows() > 0 {
            self.profile_all(cx);
        }
        if grew && !self.spec.is_identity() {
            self.order_stale = true;
            // While reading, re-sort at most twice a second.
            let idle = self.sorting.is_none();
            let done = matches!(self.load, Load::Done { .. });
            if idle && (done || self.order_ms.is_none_or(|ms| ms < 150.)) {
                self.reorder(cx);
            }
        }
        cx.notify();
    }

    /// Column widths from the values read so far (and the reader's own).
    fn layout_columns(&mut self) {
        let frame = &self.frame;
        let n = frame.columns().len();
        self.widths.resize(n, None);
        let index_columns = frame.columns().iter().take_while(|c| c.index).count();
        let rows = match &self.source {
            Source::Output(o) => o.total_rows.unwrap_or(0).max(frame.rows()),
            Source::File { .. } => frame.rows(),
        };
        let digits = digits(rows);
        // Row numbers, unless the frame has its own labels (a pandas index).
        // Digits plus the thousands commas `group` adds.
        let chars = digits + (digits - 1) / 3;
        let gutter = if index_columns > 0 { 0. } else { (chars as f32 * CHAR_W * 0.95 + PAD * 2. + 4.).max(40.) };
        let mut xs = Vec::with_capacity(n + 1);
        let mut x = gutter;
        xs.push(x);
        for c in 0..n {
            // The name is semibold and shares its line with the sort and
            // filter marks; values have the column to themselves.
            let column = &frame.columns()[c];
            let name = column.name.chars().count().min(MAX_COLUMN_CHARS as usize) as f32 * NAME_CHAR_W + MARKS;
            let dtype = column.dtype.as_deref().map_or(5, |d| d.chars().count()) as f32 * CHAR_W * 0.9 + MARKS;
            let values = frame.stats(c).width.clamp(3, MAX_COLUMN_CHARS) as f32 * CHAR_W;
            let auto = name.max(dtype).max(values) + PAD * 2.;
            x += self.widths[c].unwrap_or(auto).max(MIN_COLUMN);
            xs.push(x);
        }
        self.geometry = Geometry { xs: xs.into(), gutter, sticky: index_columns, width: x + 24., digits };
    }

    // ------------------------------------------------------- view changes

    /// Sort, filters or search changed (or more rows arrived): compute the
    /// visible rows in the background.
    fn reorder(&mut self, cx: &mut Context<Self>) {
        self.order_cancel.store(true, AtomicOrdering::Relaxed);
        if self.spec.is_identity() {
            self.order = None;
            self.view_profiles = None;
            self.sorting = None;
            self.order_stale = false;
            cx.notify();
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.order_cancel = cancel.clone();
        let frame = self.frame.clone();
        let spec = self.spec.clone();
        let base = self.profiles.clone();
        let started = Instant::now();
        self.sorting = Some(started);
        self.order_stale = false;
        // Hidden rows: profile the ones in view too, in the same bins.
        let narrowed = !spec.filters.is_empty() || !spec.search.trim().is_empty();
        self._order_task = Some(cx.spawn(async move |this, cx| {
            let flag = cancel.clone();
            let rows_frame = frame.clone();
            let order = cx.background_spawn(async move { herse_data::compute_order(&rows_frame, &spec, &flag) }).await.map(Arc::new);
            if cancel.load(AtomicOrdering::Relaxed) {
                return;
            }
            // The rows first; their profiles follow.
            this.update(cx, |this, cx| {
                if !narrowed {
                    this.view_profiles = None;
                }
                this.order = order.clone();
                this.order_ms = Some(started.elapsed().as_secs_f64() * 1000.);
                this.sorting = None;
                if let Some((row, col)) = this.selected {
                    this.selected = (row < this.visible_rows()).then_some((row, col));
                }
                if this.order_stale {
                    this.reorder(cx);
                }
                cx.notify();
            })
            .ok();
            let (Some(rows), Some(base), true) = (order, base, narrowed) else { return };
            let flag = cancel.clone();
            let profiles = cx
                .background_spawn(async move {
                    base.iter()
                        .enumerate()
                        .map(|(c, p)| p.as_ref().and_then(|p| herse_data::profile_like(&frame, c, &rows, p, &flag)))
                        .collect::<Vec<_>>()
                })
                .await;
            if cancel.load(AtomicOrdering::Relaxed) {
                return;
            }
            this.update(cx, |this, cx| {
                this.view_profiles = Some(Arc::new(profiles));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Profile every column over every row, in the background.
    fn profile_all(&mut self, cx: &mut Context<Self>) {
        self.profile_cancel.store(true, AtomicOrdering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.profile_cancel = cancel.clone();
        let frame = self.frame.clone();
        self._profile_task = Some(cx.spawn(async move |this, cx| {
            let flag = cancel.clone();
            let profiles = cx
                .background_spawn(async move {
                    (0..frame.columns().len()).map(|c| herse_data::profile(&frame, c, None, &flag)).collect::<Vec<_>>()
                })
                .await;
            if cancel.load(AtomicOrdering::Relaxed) {
                return;
            }
            this.update(cx, |this, cx| {
                this.profiles = Some(Arc::new(profiles));
                // A filtered view needs its profiles in the new bins.
                if !this.spec.is_identity() {
                    this.reorder(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.anchor = None;
        self.summary = None;
        self.popover = None;
    }

    /// The header's height at zoom 1: names and types, then the profiles.
    fn header_height(&self) -> f32 {
        if self.frame.columns().is_empty() { HEADER_H } else { HEADER_H + PROFILE_H }
    }

    /// Something is under way that the meta line counts.
    fn busy(&self) -> bool {
        matches!(self.load, Load::Reading { .. }) || matches!(self.fetch, Fetch::Running(_)) || self.sorting.is_some()
    }

    fn visible_rows(&self) -> usize {
        match &self.order {
            Some(order) => order.len(),
            None if self.spec.is_identity() => self.frame.rows(),
            // Waiting for the first order: show nothing rather than the
            // unfiltered rows.
            None => 0,
        }
    }

    /// The file row shown at visible row `ix`.
    fn row_at(&self, ix: usize) -> Option<usize> {
        match &self.order {
            Some(order) => order.get(ix).map(|&r| r as usize),
            None => (ix < self.frame.rows()).then_some(ix),
        }
    }

    /// Click on a column's name: ascending, descending, then file order.
    fn cycle_sort(&mut self, column: usize, cx: &mut Context<Self>) {
        let before = self.view_state();
        self.spec.sort = match self.spec.sort {
            Some((c, Order::Ascending)) if c == column => Some((column, Order::Descending)),
            Some((c, Order::Descending)) if c == column => None,
            _ => Some((column, Order::Ascending)),
        };
        self.commit(before, "the sort");
        self.clear_selection();
        self.reorder(cx);
    }

    fn edit_filter(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.spec.filters.iter().find(|f| f.column == column).map(|f| f.text.clone()).unwrap_or_default();
        let kind = self.frame.kind(column);
        let hint = if kind.is_numeric() { "> 10   10..20   =3   null" } else { "text   !text   =exact   null" };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(hint).default_value(current));
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(edit) = &mut this.filter
            {
                edit.error = None;
                cx.notify();
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.filter = Some(FilterEdit { column, input, error: None, _subscription: sub });
        cx.notify();
    }

    fn apply_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.filter else { return };
        let text = edit.input.read(cx).value().to_string();
        let column = edit.column;
        let parsed = if text.trim().is_empty() {
            None
        } else {
            match Filter::parse(column, &text) {
                Ok(f) => Some(f),
                Err(e) => {
                    edit.error = Some(e.into());
                    cx.notify();
                    return;
                }
            }
        };
        let before = self.view_state();
        self.spec.filters.retain(|f| f.column != column);
        self.spec.filters.extend(parsed);
        self.commit(before, "the filter");
        self.filter = None;
        self.clear_selection();
        self.focus.focus(window, cx);
        self.reorder(cx);
    }

    fn remove_filter(&mut self, column: usize, cx: &mut Context<Self>) {
        let before = self.view_state();
        self.spec.filters.retain(|f| f.column != column);
        self.commit(before, "the filter");
        if self.filter.as_ref().is_some_and(|e| e.column == column) {
            self.filter = None;
        }
        self.reorder(cx);
    }

    fn flash(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let at = Instant::now();
        self.message = Some((text.into(), at));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            this.update(cx, |this, cx| {
                if this.message.as_ref().is_some_and(|(_, t)| *t == at) {
                    this.message = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn copy_as(&mut self, how: CopyAs, cx: &mut Context<Self>) {
        let order = self.order.clone();
        let rows = self.visible_rows();
        let text = match how {
            CopyAs::Csv => herse_data::to_csv(&self.frame, order.as_deref().map(|v| v.as_slice()), COPY_LIMIT),
            CopyAs::Tsv => herse_data::to_tsv(&self.frame, order.as_deref().map(|v| v.as_slice()), COPY_LIMIT),
            CopyAs::Markdown => herse_data::to_markdown(&self.frame, order.as_deref().map(|v| v.as_slice()), COPY_LIMIT),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let what = how.label();
        if rows > COPY_LIMIT {
            self.flash(format!("Copied the first {} of {} rows as {what}", group(COPY_LIMIT), group(rows)), cx);
        } else {
            self.flash(format!("Copied {} as {what}", plural(rows, "row")), cx);
        }
    }

    fn on_copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some((rows, cols)) = self.range() else {
            self.copy_as(CopyAs::Tsv, cx);
            return;
        };
        // A range copies as tab-separated lines, to paste into a sheet.
        let mut text = String::new();
        let mut cells = 0;
        let all = rows.len();
        for ix in rows.clone().take(COPY_LIMIT) {
            let Some(r) = self.row_at(ix) else { continue };
            let line: Vec<String> = cols
                .clone()
                .map(|c| if self.frame.is_null(r, c) { String::new() } else { self.frame.text(r, c).replace(['\t', '\n', '\r'], " ") })
                .collect();
            cells += line.len();
            text.push_str(&line.join("\t"));
            text.push('\n');
        }
        if cells == 1 {
            text.pop();
            let r = self.row_at(rows.start).unwrap_or(0);
            text = self.frame.text(r, cols.start).to_string();
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.flash(
            if cells == 1 {
                "Copied the cell".to_string()
            } else if all > COPY_LIMIT {
                format!("Copied the first {} of {} rows", group(COPY_LIMIT), group(all))
            } else {
                format!("Copied {}", plural(cells, "cell"))
            },
            cx,
        );
    }

    /// The selected rows (visible indices) and columns.
    fn range(&self) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
        let (r1, c1) = self.selected?;
        let (r0, c0) = self.anchor.unwrap_or((r1, c1));
        Some((r0.min(r1)..r0.max(r1) + 1, c0.min(c1)..c0.max(c1) + 1))
    }

    fn select(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.select_to(row, col, false, cx);
    }

    /// Put the cursor on a cell; `extend` keeps the range's other corner.
    fn select_to(&mut self, row: usize, col: usize, extend: bool, cx: &mut Context<Self>) {
        let rows = self.visible_rows();
        let cols = self.frame.columns().len();
        if rows == 0 || cols == 0 {
            return;
        }
        let (row, col) = (row.min(rows - 1), col.min(cols - 1));
        self.anchor = if extend { self.anchor.or(self.selected) } else { None };
        self.selected = Some((row, col));
        self.popover = None;
        self.update_summary(cx);
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        // Bring the column into view sideways.
        let state = self.scroll.0.borrow();
        let handle = &state.base_handle;
        let viewport = handle.bounds().size.width.as_f32();
        let offset = handle.offset();
        let (x0, x1) = (self.geometry.xs[col], self.geometry.xs[col + 1]);
        let sticky = self.geometry.sticky_width();
        let left = -offset.x.as_f32();
        let new_left = if col < self.geometry.sticky {
            left
        } else if x0 - sticky < left {
            x0 - sticky
        } else if x1 > left + viewport {
            x1 - viewport
        } else {
            left
        };
        if (new_left - left).abs() > 0.5 && viewport > 0. {
            handle.set_offset(point(px(-new_left.max(0.)), offset.y));
        }
        drop(state);
        cx.notify();
    }

    /// A click in a row: select the cell (shift extends the range, a
    /// double-click shows its whole value), and start a drag.
    fn row_mouse_down(&mut self, ix: usize, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(col) = self.geometry.column_at(self.content_x(ev.position.x)) else { return };
        self.focus.focus(window, cx);
        if ev.modifiers.shift {
            self.select_to(ix, col, true, cx);
            return;
        }
        self.select(ix, col, cx);
        self.dragging = true;
        if ev.click_count >= 2
            && let Some(r) = self.row_at(ix)
        {
            self.popover = Some((r, col, self.cell_point(ix, col)));
            self.dragging = false;
        }
    }

    /// Dragging across rows selects a range.
    fn row_drag(&mut self, ix: usize, x: Pixels, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let Some(col) = self.geometry.column_at(self.content_x(x)) else { return };
        if self.selected != Some((ix, col)) {
            self.select_to(ix, col, true, cx);
        }
    }

    /// Count, sum and mean of a range, in the background when it's big.
    fn update_summary(&mut self, cx: &mut Context<Self>) {
        let Some((rows, cols)) = self.range().filter(|(r, c)| r.len() * c.len() > 1) else {
            self.summary = None;
            self._summary_task = None;
            return;
        };
        let frame = self.frame.clone();
        let order = self.order.clone();
        let sum_up = move || {
            let mut s = Summary { cells: 0, numbers: 0, sum: 0., min: f64::INFINITY, max: f64::NEG_INFINITY };
            for ix in rows {
                let r = match &order {
                    Some(o) => o[ix] as usize,
                    None => ix,
                };
                for c in cols.clone() {
                    s.cells += 1;
                    if let Some(x) = frame.number(r, c).filter(|x| x.is_finite()) {
                        s.numbers += 1;
                        s.sum += x;
                        s.min = s.min.min(x);
                        s.max = s.max.max(x);
                    }
                }
            }
            s
        };
        let size = self.range().map_or(0, |(r, c)| r.len() * c.len());
        if size <= 50_000 {
            self.summary = Some(sum_up());
            self._summary_task = None;
            return;
        }
        self._summary_task = Some(cx.spawn(async move |this, cx| {
            let summary = cx.background_spawn(async move { sum_up() }).await;
            this.update(cx, |this, cx| {
                this.summary = Some(summary);
                cx.notify();
            })
            .ok();
        }));
    }

    // ------------------------------------------------------------ history

    fn view_state(&self) -> ViewState {
        ViewState {
            sort: self.spec.sort,
            filters: self.spec.filters.clone(),
            search: self.spec.search.clone(),
            widths: self.widths.clone(),
        }
    }

    fn push_undo(&mut self, before: ViewState, what: &'static str) {
        self.undo.push((before, what));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// A change is done: remember the view before it, if it changed.
    fn commit(&mut self, before: ViewState, what: &'static str) {
        self.search_step = false;
        if !before.same(&self.view_state()) {
            self.push_undo(before, what);
        }
    }

    /// New data (another sheet, a reload, every row from the kernel): the
    /// old steps may name columns that aren't there.
    fn forget_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.search_step = false;
        self.width_before = None;
    }

    /// Put the view back as it was; the search field shows its text again.
    fn restore(&mut self, state: ViewState, window: &mut Window, cx: &mut Context<Self>) {
        let rows_change = !state.same_rows(&self.view_state());
        if self.spec.search != state.search {
            let text = state.search.clone();
            self.search.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        self.spec.sort = state.sort;
        self.spec.filters = state.filters;
        self.spec.search = state.search;
        self.widths = state.widths;
        self.filter = None;
        self.search_step = false;
        self.layout_columns();
        if rows_change {
            self.clear_selection();
            self.reorder(cx);
        }
        cx.notify();
    }

    fn on_undo_view(&mut self, _: &UndoView, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.view_state();
        // A step that changed nothing in the end (typed, then deleted) is skipped.
        while let Some((state, what)) = self.undo.pop() {
            if state.same(&current) {
                continue;
            }
            self.redo.push((current, what));
            self.restore(state, window, cx);
            self.flash(format!("Undid {what}"), cx);
            return;
        }
        self.flash("Nothing to undo", cx);
    }

    fn on_redo_view(&mut self, _: &RedoView, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.view_state();
        while let Some((state, what)) = self.redo.pop() {
            if state.same(&current) {
                continue;
            }
            self.undo.push((current, what));
            self.restore(state, window, cx);
            self.flash(format!("Redid {what}"), cx);
            return;
        }
        self.flash("Nothing to redo", cx);
    }

    /// Shift-arrows: move the cursor and keep the range's other corner.
    fn extend_by(&mut self, dr: isize, dc: isize, cx: &mut Context<Self>) {
        let (row, col) = self.selected.unwrap_or((0, 0));
        let row = (row as isize + dr).max(0) as usize;
        let col = (col as isize + dc).max(0) as usize;
        self.select_to(row, col, true, cx);
    }

    /// Where a cell's bottom left is in the window, for a card under it.
    fn cell_point(&self, ix: usize, col: usize) -> gpui_kit::Point<Pixels> {
        let state = self.scroll.0.borrow();
        let b = state.base_handle.bounds();
        let offset = state.base_handle.offset();
        let z = theme::zoom();
        let x = b.left() + px(self.geometry.xs.get(col).copied().unwrap_or(0.) * z) + if col < self.geometry.sticky { px(0.) } else { offset.x };
        let y = b.top() + offset.y + px((ix as f32 + 1.) * ROW_H * z);
        point(x.max(b.left()), y.clamp(b.top(), b.bottom()))
    }

    fn move_by(&mut self, dr: isize, dc: isize, cx: &mut Context<Self>) {
        let (row, col) = self.selected.unwrap_or((0, 0));
        let row = (row as isize + dr).max(0) as usize;
        let col = (col as isize + dc).max(0) as usize;
        self.select(row, col, cx);
    }

    fn page(&self) -> isize {
        let h = self.scroll.0.borrow().base_handle.bounds().size.height.as_f32();
        ((h / (ROW_H * theme::zoom())) as isize - 1).max(1)
    }

    /// Content x (zoom 1) under window x `x`, and whether it's over the
    /// sticky columns.
    fn content_x(&self, x: Pixels) -> f32 {
        let state = self.scroll.0.borrow();
        let left = state.base_handle.bounds().left();
        let scroll = state.base_handle.offset().x;
        let z = theme::zoom();
        let local = (x - left).as_f32() / z;
        if local < self.geometry.sticky_width() { local } else { local - scroll.as_f32() / z }
    }

    /// Which line of the header the pointer is on: the name (`None`), or
    /// the profile (its y within the strip, at zoom 1).
    fn header_line(&self, y: Pixels) -> Option<f32> {
        let local = (y - self.header_top.get()).as_f32() / theme::zoom();
        (local >= HEADER_H).then_some(local - HEADER_H)
    }

    fn header_hover(&mut self, at: gpui_kit::Point<Pixels>, cx: &mut Context<Self>) {
        let x = self.content_x(at.x);
        let col = self.geometry.column_at(x);
        let (column, part) = match (col, self.header_line(at.y)) {
            (Some(c), Some(y)) => (None, self.part_at(c, x, y).map(|p| (c, p))),
            (c, None) => (c, None),
            (None, _) => (None, None),
        };
        if column != self.hover_column || part != self.hover_part {
            self.hover_column = column;
            self.hover_part = part;
            self.pointer = at;
            cx.notify();
        }
    }

    fn header_click(&mut self, at: gpui_kit::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let x = self.content_x(at.x);
        let Some(col) = self.geometry.column_at(x) else { return };
        self.popover = None;
        if let Some(y) = self.header_line(at.y) {
            // A bar of the profile filters to it (again: clears).
            if let Some(part) = self.part_at(col, x, y) {
                self.toggle_part_filter(col, part, cx);
            }
            return;
        }
        // The funnel at the column's right edge filters; the rest sorts.
        if x > self.geometry.xs[col + 1] - 24. {
            self.edit_filter(col, window, cx);
        } else {
            self.cycle_sort(col, cx);
        }
    }

    /// The part of column `col`'s profile at content x `x`, strip y `y`.
    fn part_at(&self, col: usize, x: f32, y: f32) -> Option<Part> {
        let profile = self.profiles.as_ref()?.get(col)?.as_ref()?;
        let (x0, x1) = (self.geometry.xs[col] + PAD, self.geometry.xs[col + 1] - PAD);
        if x < x0 || x > x1 || x1 <= x0 {
            return None;
        }
        let f = ((x - x0) / (x1 - x0)).clamp(0., 0.9999);
        if profile.nulls > 0 && (PROFILE_MISSING_Y - 4.0..=PROFILE_H).contains(&y) {
            return Some(Part::Missing);
        }
        match &profile.shape {
            Shape::Histogram(h) => Some(Part::Bin((f * h.counts.len() as f32) as usize)),
            Shape::Categories(c) => {
                if mostly_unique(c, profile) {
                    return None;
                }
                let row = ((y - PROFILE_TOP) / PROFILE_ROW).floor();
                (row >= 0. && (row as usize) < c.top.len().min(PROFILE_ROWS)).then_some(Part::Top(row as usize))
            }
            Shape::Bools { yes, no } => {
                let share = *yes as f32 / (*yes + *no).max(1) as f32;
                Some(if f < share { Part::Yes } else { Part::No })
            }
            Shape::Empty => None,
        }
    }

    /// The filter a profile part stands for, as typed in a filter.
    fn part_filter(&self, col: usize, part: Part) -> Option<String> {
        let profile = self.profiles.as_ref()?.get(col)?.as_ref()?;
        Some(match (part, &profile.shape) {
            (Part::Missing, _) => "null".into(),
            (Part::Yes, _) => "=true".into(),
            (Part::No, _) => "=false".into(),
            (Part::Top(i), Shape::Categories(c)) => format!("={}", c.top.get(i)?.0),
            (Part::Bin(i), Shape::Histogram(h)) => {
                let (a, b) = (*h.edges.get(i)?, *h.edges.get(i + 1)?);
                match h.time {
                    Some(kind) => format!("{}..<{}", herse_data::format_time(a, kind), herse_data::format_time(b, kind)),
                    None if h.integral && b - a == 1. => format!("={}", a as i64),
                    None => {
                        let decimals = (-(b - a).log10().floor()).max(0.) as usize;
                        format!("{a:.decimals$}..<{b:.decimals$}")
                    }
                }
            }
            _ => return None,
        })
    }

    /// Filter to a profile part, or clear that filter if it's the one in force.
    fn toggle_part_filter(&mut self, col: usize, part: Part, cx: &mut Context<Self>) {
        let Some(text) = self.part_filter(col, part) else { return };
        let same = self.spec.filters.iter().any(|f| f.column == col && f.text == text);
        let before = self.view_state();
        self.spec.filters.retain(|f| f.column != col);
        if !same && let Ok(f) = Filter::parse(col, &text) {
            self.spec.filters.push(f);
        }
        self.commit(before, "the filter");
        if self.filter.as_ref().is_some_and(|e| e.column == col) {
            self.filter = None;
        }
        self.clear_selection();
        self.reorder(cx);
    }

    // ------------------------------------------------- notebooks and kernels

    fn new_notebook(&mut self, library: Library, cx: &mut Context<Self>) {
        let Source::File { path, format, .. } = &self.source else { return };
        let Some(dir) = path.parent() else { return };
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let sniffed = self.loaded.as_ref().map(|l| l.sniffed.clone()).unwrap_or_default();
        let code = herse_data::loading_code(&file, *format, &sniffed, library);
        let stem = file.split('.').next().filter(|s| !s.is_empty()).unwrap_or("data").to_string();
        let target = unused_path(dir, &stem);
        let mut nb = herse_nb::Notebook::new_empty("python3", "Python 3", "python");
        let mut title = nb.new_cell(herse_nb::CellType::Markdown);
        title.source = format!("# {file}");
        let mut load = nb.new_cell(herse_nb::CellType::Code);
        load.source = code;
        nb.cells = vec![title, load];
        let mut doc = crate::notebook::document::Document::from_notebook(nb);
        doc.path = Some(target.clone());
        if std::env::var_os("HERSE_NO_SAVE").is_none()
            && let Err(e) = doc.save(None, false)
        {
            self.load_problem(format!("Couldn't write {}: {e:#}", target.display()), cx);
            return;
        }
        cx.emit(FrameEvent::OpenNotebook(Rc::new(RefCell::new(Some(doc)))));
    }

    fn load_problem(&mut self, text: String, cx: &mut Context<Self>) {
        self.fetch = Fetch::Failed(text.into());
        cx.notify();
    }

    /// Load All Rows: the kernel writes the whole frame to a file, and it
    /// streams in like any data file.
    fn load_all_rows(&mut self, cx: &mut Context<Self>) {
        let Source::Output(o) = &self.source else { return };
        let notebook = o.notebook.clone();
        let (uid, output) = (o.cell_uid, o.output);
        self.fetch = Fetch::Running(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::notebook::frame_fetch::fetch(notebook, uid, output, cx).await;
            this.update(cx, |this, cx| match result {
                Ok((path, index)) => this.read_fetched(path, index, cx),
                Err(e) => {
                    this.fetch = Fetch::Failed(e.into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Read the file the kernel wrote, in place of the saved rows.
    fn read_fetched(&mut self, path: PathBuf, index: usize, cx: &mut Context<Self>) {
        let Source::Output(o) = &mut self.source else { return };
        o.complete = true;
        o.fetched_index = index;
        self.fetch = Fetch::Idle;
        self.stop.store(true, AtomicOrdering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        self.frame = Arc::new(Frame::default());
        self.order = None;
        self.profiles = None;
        self.view_profiles = None;
        self.clear_selection();
        self.widths.clear();
        self.forget_history();
        let started = Instant::now();
        self.load = Load::Reading { progress: None, started };
        let (tx, rx) = smol::channel::unbounded::<Msg>();
        let spawned = std::thread::Builder::new().name("herse-data".into()).spawn(move || {
            let result = herse_data::read(&path, None, &mut |event| {
                if stop.load(AtomicOrdering::Relaxed) || tx.send_blocking(Msg::Event(event)).is_err() {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            });
            let _ = std::fs::remove_file(&path);
            let _ = tx.send_blocking(Msg::Done(result));
        });
        if let Err(e) = spawned {
            self.load = Load::Failed(format!("Couldn't start reading: {e}").into());
            return;
        }
        self._reader = Some(cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                if this.update(cx, |this, cx| this.apply(batch, started, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn kernel_live(&self, cx: &App) -> bool {
        match &self.source {
            Source::Output(o) => o.notebook.upgrade().is_some_and(|nb| nb.read(cx).frame_kernel_live()),
            Source::File { .. } => false,
        }
    }

    fn switch_table(&mut self, table: String, cx: &mut Context<Self>) {
        if let Source::File { table: current, .. } = &mut self.source {
            *current = Some(table.clone());
            self.spec = ViewSpec { search: self.spec.search.clone(), ..Default::default() };
            self.start_reading(Some(table), cx);
            cx.emit(FrameEvent::Changed);
        }
    }

    // ----------------------------------------------------------- actions

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }
    fn on_clear_search(&mut self, _: &ClearSearch, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.view_state();
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.spec.search.clear();
        self.commit(before, "the search");
        self.focus.focus(window, cx);
        self.reorder(cx);
    }
    fn on_apply_filter(&mut self, _: &ApplyFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_filter(window, cx);
    }
    fn on_cancel_filter(&mut self, _: &CancelFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = None;
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn on_edit_filter(&mut self, _: &EditFilter, window: &mut Window, cx: &mut Context<Self>) {
        let col = self.selected.map(|s| s.1).or(self.hover_column).unwrap_or(0);
        if col < self.frame.columns().len() {
            self.edit_filter(col, window, cx);
        }
    }
    fn on_select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_by(-1, 0, cx);
    }
    fn on_select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_by(1, 0, cx);
    }
    fn on_select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_by(0, -1, cx);
    }
    fn on_select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_by(0, 1, cx);
    }
    /// Every cell in view; the cursor stays at the top left.
    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let (rows, cols) = (self.visible_rows(), self.frame.columns().len());
        if rows == 0 || cols == 0 {
            return;
        }
        self.selected = Some((0, 0));
        self.anchor = Some((rows - 1, cols - 1));
        self.popover = None;
        self.update_summary(cx);
        cx.notify();
    }
    fn on_show_value(&mut self, _: &ShowValue, _: &mut Window, cx: &mut Context<Self>) {
        let Some((ix, col)) = self.selected else { return };
        let Some(r) = self.row_at(ix) else { return };
        self.popover = if self.popover.is_some() { None } else { Some((r, col, self.cell_point(ix, col))) };
        cx.notify();
    }
    /// Escape: the popover, then a range, then whatever's around the table.
    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.popover.take().is_some() {
            cx.notify();
        } else if self.anchor.take().is_some() {
            self.update_summary(cx);
            cx.notify();
        } else {
            cx.propagate();
        }
    }
    fn on_move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, 0, cx);
    }
    fn on_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        // Enter in the search field lands on the first hit.
        if !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
            self.select(0, self.selected.map_or(0, |s| s.1), cx);
            return;
        }
        if self.selected.is_none() {
            self.select(0, 0, cx);
        } else {
            self.move_by(1, 0, cx);
        }
    }
    fn on_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(0, -1, cx);
    }
    fn on_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(0, 1, cx);
    }
    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-self.page(), 0, cx);
    }
    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(self.page(), 0, cx);
    }
    fn on_move_to_top(&mut self, _: &MoveToTop, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.selected.map_or(0, |s| s.1);
        self.select(0, col, cx);
    }
    fn on_move_to_bottom(&mut self, _: &MoveToBottom, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.selected.map_or(0, |s| s.1);
        self.select(usize::MAX, col, cx);
    }

    // ------------------------------------------------------------ render

    fn palette(&self, cx: &App) -> Palette {
        let theme = cx.theme();
        Palette {
            ink: theme.foreground,
            pencil: theme.muted_foreground,
            page: theme.background,
            rule: theme.foreground.opacity(0.07),
            strong_rule: theme.foreground.opacity(0.18),
            accent: theme::accent(cx),
            tint: theme.selection,
            row_tint: theme::accent(cx).opacity(if theme.is_dark() { 0.10 } else { 0.06 }),
            hover: theme.foreground.opacity(if theme.is_dark() { 0.045 } else { 0.03 }),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let pencil = theme.muted_foreground;
        let title: SharedString = match &self.source {
            Source::File { path, .. } => {
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()).into()
            }
            Source::Output(o) => o.name.clone().into(),
        };
        let subtitle: Option<SharedString> = match &self.source {
            Source::Output(o) => Some(format!("{} · cell {}", o.notebook_title, o.cell_number).into()),
            Source::File { format, .. } => Some(format.label().into()),
        };
        let meta = self.meta_line();
        let is_file = matches!(self.source, Source::File { .. });
        let tables = self.loaded.as_ref().map(|l| l.tables.clone()).unwrap_or_default();
        let current_table = match &self.source {
            Source::File { table, .. } => table.clone(),
            Source::Output(_) => None,
        };
        let entity = cx.entity().downgrade();
        let copy_menu = {
            let entity = entity.clone();
            crate::ui::MenuTrigger::new("frame-copy")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .h(crate::ui::CONTROL)
                .px(px(7.))
                .text_size(px(12.))
                .text_color(pencil)
                .child(svg().path("icons/copy.svg").size(crate::ui::ICON).text_color(pencil))
                .child("Copy")
                .child(svg().path("icons/chevron-down.svg").size(px(11.)).text_color(pencil))
                .dropdown_menu(move |menu, _, _| {
                    let (a, b, c) = (entity.clone(), entity.clone(), entity.clone());
                    menu.min_w(px(200.))
                        .item(PopupMenuItem::new("Copy as CSV").on_click(move |_, _, cx| {
                            a.update(cx, |this, cx| this.copy_as(CopyAs::Csv, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Copy as Markdown").on_click(move |_, _, cx| {
                            b.update(cx, |this, cx| this.copy_as(CopyAs::Markdown, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Copy as TSV (for spreadsheets)").on_click(move |_, _, cx| {
                            c.update(cx, |this, cx| this.copy_as(CopyAs::Tsv, cx)).ok();
                        }))
                })
        };
        let notebook_menu = is_file.then(|| {
            let entity = entity.clone();
            crate::ui::MenuTrigger::new("frame-new-notebook")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .h(crate::ui::CONTROL)
                .px(px(7.))
                .text_size(px(12.))
                .text_color(pencil)
                .child(svg().path("icons/notebook.svg").size(crate::ui::ICON).text_color(pencil))
                .child("New Notebook")
                .child(svg().path("icons/chevron-down.svg").size(px(11.)).text_color(pencil))
                .dropdown_menu(move |menu, _, _| {
                    let (a, b) = (entity.clone(), entity.clone());
                    menu.min_w(px(220.))
                        .label("Load this file with")
                        .item(PopupMenuItem::new("pandas").on_click(move |_, _, cx| {
                            a.update(cx, |this, cx| this.new_notebook(Library::Pandas, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("polars").on_click(move |_, _, cx| {
                            b.update(cx, |this, cx| this.new_notebook(Library::Polars, cx)).ok();
                        }))
                })
        });
        let table_menu = (tables.len() > 1).then(|| {
            let entity = entity.clone();
            let label: SharedString = current_table.clone().unwrap_or_default().into();
            crate::ui::MenuTrigger::new("frame-tables")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .h(crate::ui::CONTROL)
                .px(px(7.))
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(svg().path("icons/table-2.svg").size(crate::ui::ICON).text_color(pencil))
                .child(div().max_w(px(180.)).truncate().child(label))
                .child(svg().path("icons/chevron-down.svg").size(px(11.)).text_color(pencil))
                .dropdown_menu(move |mut menu, _, _| {
                    menu = menu.min_w(px(200.));
                    for t in tables.clone() {
                        let e = entity.clone();
                        let on = current_table.as_deref() == Some(t.as_str());
                        menu = menu.item(PopupMenuItem::new(t.clone()).checked(on).on_click(move |_, _, cx| {
                            e.update(cx, |this, cx| this.switch_table(t.clone(), cx)).ok();
                        }));
                    }
                    menu
                })
        });
        div()
            .flex_none()
            .h(px(52.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(8.))
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .when_some(subtitle, |d, s| {
                                d.child(div().flex_none().text_size(px(11.5)).text_color(pencil).child(s))
                            }),
                    )
                    .child(div().truncate().text_size(px(11.5)).text_color(pencil).child(meta)),
            )
            .children(table_menu)
            .child(
                div()
                    .key_context(FRAME_SEARCH)
                    .flex_none()
                    .w(px(240.))
                    .h(px(28.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(8.))
                    .rounded(crate::ui::CONTROL_RADIUS)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(svg().path("icons/search.svg").flex_none().size(px(13.)).text_color(pencil))
                    .child(div().flex_1().min_w_0().text_size(px(12.5)).child(Input::new(&self.search).appearance(false))),
            )
            .child(copy_menu)
            .children(notebook_menu)
    }

    /// "1,100,000 rows × 12 columns · 141.0 KB", or how far along. How long
    /// reading or sorting took isn't the reader's business; that it's still
    /// going is.
    fn meta_line(&self) -> SharedString {
        if let Some((m, _)) = &self.message {
            return m.clone();
        }
        let frame = &self.frame;
        let rows = frame.rows();
        let cols = frame.columns().len();
        if matches!(self.load, Load::Failed(_)) && cols == 0 {
            return "Couldn't be read".into();
        }
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("{} × {}", plural(rows, "row"), plural(cols, "column")));
        match &self.load {
            Load::Reading { progress, started } => {
                let pct = progress.map(|p| format!(" {:.0}%", p * 100.)).unwrap_or_default();
                parts.push(format!("reading{pct} · {:.1} s", started.elapsed().as_secs_f64()));
            }
            Load::Done { ms } if *ms > 0. => {
                if let Source::File { path, .. } = &self.source
                    && let Ok(md) = std::fs::metadata(path)
                {
                    parts.push(crate::workspace::human_bytes(md.len() as usize));
                }
            }
            _ => {}
        }
        if let Some(started) = self.sorting
            && started.elapsed() > Duration::from_millis(80)
        {
            parts.push(if self.spec.sort.is_some() { "sorting…".into() } else { "filtering…".into() });
        }
        parts.join(" · ").into()
    }

    /// Under the toolbar: what a notebook's output left out, problems,
    /// warnings, and the filters in force.
    fn render_notices(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui_kit::AnyElement> {
        let theme = cx.theme().clone();
        let pencil = theme.muted_foreground;
        let accent = theme::accent(cx);
        let mut out = Vec::new();
        if let Load::Failed(e) = &self.load
            && !self.error_dismissed
        {
            let entity = cx.entity().downgrade();
            out.push(
                div()
                    .px(px(16.))
                    .pt(px(12.))
                    .child(crate::ui::problem_banner(
                        "frame-error",
                        "This file couldn't be read".into(),
                        e.clone(),
                        Vec::new(),
                        |_, _, _| {},
                        move |_, _, cx| {
                            entity
                                .update(cx, |this, cx| {
                                    this.error_dismissed = true;
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        if let Fetch::Failed(e) = &self.fetch {
            let entity = cx.entity().downgrade();
            out.push(
                div()
                    .px(px(16.))
                    .pt(px(12.))
                    .child(crate::ui::problem_banner(
                        "frame-fetch-error",
                        "Couldn't load all rows".into(),
                        e.clone(),
                        Vec::new(),
                        |_, _, _| {},
                        move |_, _, cx| {
                            entity
                                .update(cx, |this, cx| {
                                    this.fetch = Fetch::Idle;
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        if let Source::Output(o) = &self.source
            && !o.complete
            && o.truncated
        {
            let saved = plural(o.saved_rows, "row");
            let text = match o.total_rows {
                Some(total) if total > o.saved_rows => {
                    format!("Showing the {saved} the notebook saved, of {}.", plural(total, "row"))
                }
                _ => format!("Showing the {saved} the notebook saved; pandas left the rest out."),
            };
            let live = self.kernel_live(cx);
            let running = matches!(self.fetch, Fetch::Running(_));
            let entity = cx.entity().downgrade();
            out.push(
                div()
                    .flex_none()
                    .mx(px(16.))
                    .mt(px(12.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(theme.foreground.opacity(0.035))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(svg().path("icons/table-2.svg").flex_none().size(crate::ui::ICON).text_color(pencil))
                    .child(div().flex_1().min_w_0().text_size(px(12.5)).text_color(theme.foreground).child(text))
                    .when(live && !running, |d| {
                        d.child(crate::ui::primary_button("frame-load-all", "Load All Rows", true, cx).on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.load_all_rows(cx)).ok();
                        }))
                    })
                    .when(running, |d| {
                        let secs = match self.fetch {
                            Fetch::Running(t) => t.elapsed().as_secs_f32(),
                            _ => 0.,
                        };
                        d.child(div().text_size(px(12.)).text_color(pencil).child(format!("Loading from the kernel… {secs:.0} s")))
                    })
                    .when(!live, |d| {
                        d.child(div().flex_none().text_size(px(12.)).text_color(pencil).child("Run the notebook to load every row"))
                    })
                    .into_any_element(),
            );
        }
        if let Some(loaded) = &self.loaded
            && !loaded.warnings.is_empty()
        {
            let text: SharedString = loaded.warnings.join("\n").into();
            out.push(
                div()
                    .flex_none()
                    .mx(px(16.))
                    .mt(px(10.))
                    .text_size(px(12.))
                    .text_color(theme::changed_color(theme.is_dark()))
                    .child(gpui_kit::base::SelectableText::new("frame-warnings", text))
                    .into_any_element(),
            );
        }
        // Filters: chips for those in force, and the one being edited.
        let columns = self.frame.columns();
        let editing = self.filter.as_ref();
        if !self.spec.filters.is_empty() || editing.is_some() {
            let mut row = div().flex_none().mx(px(16.)).mt(px(10.)).flex().flex_row().flex_wrap().items_center().gap(px(6.));
            row = row.child(svg().path("icons/funnel.svg").flex_none().size(px(13.)).text_color(accent));
            for f in &self.spec.filters {
                if editing.is_some_and(|e| e.column == f.column) {
                    continue;
                }
                let name = columns.get(f.column).map_or("?", |c| c.name.as_str());
                let col = f.column;
                row = row.child(
                    div()
                        .id(("filter-chip", col))
                        .h(crate::ui::CONTROL)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(8.))
                        .pr(px(2.))
                        .rounded(crate::ui::CONTROL_RADIUS)
                        .bg(theme.selection)
                        .text_size(px(12.))
                        .text_color(theme.foreground)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| this.edit_filter(col, window, cx)))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
                        .child(f.text.clone())
                        .child(
                            crate::ui::icon_button(("filter-remove", col), "icons/close.svg", false, cx)
                                .size(px(18.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.remove_filter(col, cx)
                                })),
                        ),
                );
            }
            if let Some(edit) = editing {
                let name = columns.get(edit.column).map_or("?", |c| c.name.as_str());
                let col = edit.column;
                row = row.child(
                    div()
                        .key_context(FRAME_FILTER)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
                        .child(
                            div()
                                .w(px(220.))
                                .h(px(26.))
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .rounded(crate::ui::CONTROL_RADIUS)
                                .border_1()
                                .border_color(accent)
                                .text_size(px(12.5))
                                .child(Input::new(&edit.input).appearance(false)),
                        )
                        .child(crate::ui::button("filter-apply", "Filter", cx).on_click(cx.listener(|this, _, window, cx| this.apply_filter(window, cx))))
                        .when(self.spec.filters.iter().any(|f| f.column == col), |d| {
                            d.child(crate::ui::ghost_button("filter-clear", "Remove", cx).on_click(cx.listener(move |this, _, _, cx| this.remove_filter(col, cx))))
                        })
                        .child(div().text_size(px(11.5)).text_color(pencil).child("↩ to filter, esc to cancel")),
                );
                if let Some(e) = &edit.error {
                    row = row.child(
                        div()
                            .w_full()
                            .text_size(px(12.))
                            .text_color(theme.danger)
                            .child(gpui_kit::base::SelectableText::new("frame-filter-error", e.clone())),
                    );
                }
            }
            out.push(row.into_any_element());
        }
        let _ = window;
        out
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette(cx);
        let z = theme::zoom();
        let geometry = self.geometry.clone();
        let frame = self.frame.clone();
        let order = self.order.clone();
        let count = self.visible_rows();
        let scroll = self.scroll.clone();
        let selected = self.selected;
        let span = self.range().filter(|(r, c)| r.len() * c.len() > 1);
        let hover_tint = palette.hover;
        let needle = self.spec.search.trim().to_lowercase();
        let entity = cx.entity().downgrade();
        let row_width = px(geometry.width * z);
        let looks: Arc<[Look]> = self
            .frame
            .columns()
            .iter()
            .enumerate()
            .map(|(c, col)| Look { numeric: self.frame.kind(c).is_numeric(), typed: col.dtype.is_some(), index: col.index })
            .collect();
        let list = {
            let geometry = geometry.clone();
            let scroll = scroll.clone();
            let palette = palette.clone();
            uniform_list("frame-rows", count, move |range, _window, _cx| {
                range
                    .map(|ix| {
                        let row = match &order {
                            Some(o) => o.get(ix).map(|&r| r as usize),
                            None => Some(ix),
                        };
                        let frame = frame.clone();
                        let geometry = geometry.clone();
                        let palette = palette.clone();
                        let scroll = scroll.clone();
                        let needle = needle.clone();
                        let entity = entity.clone();
                        let looks = looks.clone();
                        let marks = RowMarks {
                            head: selected.filter(|s| s.0 == ix).map(|s| s.1),
                            cols: span.as_ref().filter(|(rows, _)| rows.contains(&ix)).map(|(_, cols)| cols.clone()),
                        };
                        let (down, drag, up) = (entity.clone(), entity.clone(), entity.clone());
                        div()
                            .id(("frame-row", ix))
                            .h(px(ROW_H * z))
                            .w(row_width)
                            // Rows answer the pointer with a faint tint.
                            .hover(|s| s.bg(hover_tint))
                            .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
                                down.update(cx, |this, cx| this.row_mouse_down(ix, ev, window, cx)).ok();
                            })
                            .on_mouse_move(move |ev: &MouseMoveEvent, _, cx| {
                                if ev.pressed_button == Some(MouseButton::Left) {
                                    drag.update(cx, |this, cx| this.row_drag(ix, ev.position.x, cx)).ok();
                                }
                            })
                            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                                up.update(cx, |this, _| this.dragging = false).ok();
                            })
                            .child(
                                canvas(
                                    |_, _, _| (),
                                    move |bounds, _, window, cx| {
                                        if let Some(row) = row {
                                            paint_row(&frame, &looks, row, &marks, &needle, &geometry, &palette, &scroll, z, bounds, window, cx);
                                        }
                                    },
                                )
                                .size_full(),
                            )
                            .into_any_element()
                    })
                    .collect()
            })
            .with_horizontal_sizing_behavior(gpui_kit::ListHorizontalSizingBehavior::Unconstrained)
            .track_scroll(&self.scroll)
            .size_full()
        };
        let header = {
            let columns: Arc<[HeaderColumn]> = self
                .frame
                .columns()
                .iter()
                .enumerate()
                .map(|(c, col)| {
                    let kind = self.frame.kind(c);
                    HeaderColumn {
                        name: col.name.clone().into(),
                        dtype: col.dtype.clone().unwrap_or_else(|| kind.label().to_string()).into(),
                        numeric: kind.is_numeric(),
                        sort: self.spec.sort.filter(|s| s.0 == c).map(|s| s.1),
                        filtered: self.spec.filters.iter().any(|f| f.column == c),
                    }
                })
                .collect();
            let hover = self.hover_column;
            let geometry = geometry.clone();
            let scroll = scroll.clone();
            let palette = palette.clone();
            let resize = self.resize.clone();
            let weak = cx.entity().downgrade();
            let profiles = Profiles {
                base: self.profiles.clone(),
                view: self.view_profiles.clone(),
                hover: self.hover_part,
            };
            let header_top = self.header_top.clone();
            canvas(
                {
                    let geometry = geometry.clone();
                    let scroll = scroll.clone();
                    move |bounds, window, _| {
                        header_top.set(bounds.top());
                        header_hitboxes(&geometry, &scroll, z, bounds, window)
                    }
                },
                move |bounds, hitboxes, window, cx| {
                    paint_header(&columns, hover, &profiles, &geometry, &palette, &scroll, z, bounds, window, cx);
                    for (_, hitbox) in &hitboxes {
                        window.set_cursor_style(CursorStyle::ResizeLeftRight, hitbox);
                    }
                    // Dragging a column's right edge resizes it.
                    let starts = hitboxes.clone();
                    let (resize_down, weak_down) = (resize.clone(), weak.clone());
                    window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                        if phase != gpui_kit::DispatchPhase::Bubble || ev.button != MouseButton::Left {
                            return;
                        }
                        let Some((col, _)) = starts.iter().find(|(_, h)| h.is_hovered(window)) else { return };
                        let col = *col;
                        let width = weak_down
                            .update(cx, |this, _| {
                                this.width_before = Some(this.view_state());
                                this.geometry.xs[col + 1] - this.geometry.xs[col]
                            })
                            .unwrap_or(0.);
                        resize_down.set(Some(Resize { column: col, start_x: ev.position.x, start_width: width }));
                        cx.stop_propagation();
                    });
                    let (resize_move, weak_move) = (resize.clone(), weak.clone());
                    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                        if phase != gpui_kit::DispatchPhase::Bubble {
                            return;
                        }
                        let Some(r) = resize_move.get() else { return };
                        if !ev.dragging() {
                            resize_move.set(None);
                            return;
                        }
                        let width = (r.start_width + (ev.position.x - r.start_x).as_f32() / z).max(MIN_COLUMN);
                        weak_move
                            .update(cx, |this, cx| {
                                if r.column < this.widths.len() {
                                    this.widths[r.column] = Some(width);
                                    this.layout_columns();
                                    cx.notify();
                                }
                            })
                            .ok();
                    });
                    let (resize_up, weak_up) = (resize.clone(), weak.clone());
                    window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                        resize_up.set(None);
                        weak_up
                            .update(cx, |this, _| {
                                if let Some(before) = this.width_before.take() {
                                    this.commit(before, "the column width");
                                }
                            })
                            .ok();
                    });
                },
            )
            .size_full()
        };
        let resize = self.resize.clone();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .mt(px(8.))
            .child(
                div()
                    .id("frame-header")
                    .flex_none()
                    .h(px(self.header_height() * z))
                    .w_full()
                    .cursor_pointer()
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| this.header_hover(ev.position, cx)))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered && (this.hover_column.is_some() || this.hover_part.is_some()) {
                            this.hover_column = None;
                            this.hover_part = None;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                        if resize.get().is_none() {
                            this.header_click(ev.position(), window, cx)
                        }
                    }))
                    .child(header),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(list)
                    .child(div().absolute().inset_0().child(gpui_kit::component::scroll::Scrollbar::new(&self.scroll)))
                    .when(count == 0 && !matches!(self.load, Load::Reading { .. }) && self.sorting.is_none(), |d| {
                        let text = if self.frame.rows() == 0 { "No rows" } else { "No rows match" };
                        d.child(
                            div()
                                .absolute()
                                .top(px(24.))
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .text_size(px(13.))
                                .text_color(palette.pencil)
                                .child(text),
                        )
                    }),
            )
    }

    /// Hovering a column's name: its numbers.
    /// Under the table: how many rows are in view (with Clear when a filter
    /// or search hides some), and what the selected cells add up to.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (ink, pencil) = (theme.foreground, theme.muted_foreground);
        let rows = self.frame.rows();
        let narrowed = !self.spec.filters.is_empty() || !self.spec.search.trim().is_empty();
        let tnum = gpui_kit::FontFeatures(Arc::new(vec![("tnum".into(), 1)]));
        let mut left = div().flex().flex_row().items_center().gap(px(6.)).min_w_0();
        if narrowed && self.sorting.is_some_and(|t| t.elapsed() > Duration::from_millis(80)) {
            left = left.child("Filtering…");
        } else if narrowed {
            left = left
                .child(div().text_color(ink).font_weight(FontWeight::MEDIUM).child(group(self.visible_rows())))
                .child(format!("of {}", plural(rows, "row")))
                .child(crate::ui::ghost_button("frame-clear-view", "Clear", cx).h(px(20.)).px(px(7.)).on_click(cx.listener(
                    |this, _, window, cx| {
                        let before = this.view_state();
                        this.search.update(cx, |s, cx| s.set_value("", window, cx));
                        this.spec.search.clear();
                        this.spec.filters.clear();
                        this.commit(before, "Clear");
                        this.filter = None;
                        this.clear_selection();
                        this.focus.focus(window, cx);
                        this.reorder(cx);
                    },
                )));
        } else {
            left = left.child(plural(rows, "row"));
        }
        let mut right = div().flex().flex_row().items_center().gap(px(12.)).flex_none();
        match (&self.summary, self.range()) {
            (Some(s), _) => {
                let cells = if s.numbers > 0 && s.numbers < s.cells {
                    format!("{} · {} numbers", plural(s.cells, "cell"), group(s.numbers))
                } else {
                    plural(s.cells, "cell")
                };
                right = right.child(cells);
                if s.numbers > 0 {
                    let mean = s.sum / s.numbers as f64;
                    for (i, (label, x)) in [("Sum", s.sum), ("Mean", mean), ("Min", s.min), ("Max", s.max)].into_iter().enumerate() {
                        let text = number(x);
                        let copy = text.clone();
                        right = right.child(
                            div()
                                .id(("frame-sum", i))
                                .flex()
                                .flex_row()
                                .gap(px(4.))
                                .cursor_pointer()
                                .child(label)
                                .child(div().text_color(ink).font_features(tnum.clone()).child(text))
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(format!("Copy the {}", label.to_lowercase())).build(window, cx)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.replace(',', "")));
                                    this.flash(format!("Copied the {}", label.to_lowercase()), cx);
                                })),
                        );
                    }
                }
            }
            (None, Some((r, c))) if r.len() * c.len() > 1 => {
                right = right.child(format!("Adding up {}…", plural(r.len() * c.len(), "cell")));
            }
            (None, Some(_)) => {
                right = right.child(div().text_color(pencil.opacity(0.75)).child("Shift-click or drag to add up cells"));
            }
            _ => {}
        }
        div()
            .flex_none()
            .h(px(28.))
            .px(px(16.))
            .border_t_1()
            .border_color(theme.foreground.opacity(0.07))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .text_size(px(12.))
            .text_color(pencil)
            .child(left)
            .child(right)
    }

    /// The card over a profile's bar: what it holds, how many, and that a
    /// click filters to it.
    fn render_part_card(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let (col, part) = self.hover_part?;
        let profile = self.profiles.as_ref()?.get(col)?.as_ref()?;
        let theme = cx.theme();
        let count_of = |p: &Profile| -> Option<usize> {
            Some(match (part, &p.shape) {
                (Part::Missing, _) => p.nulls,
                (Part::Yes, Shape::Bools { yes, .. }) => *yes,
                (Part::No, Shape::Bools { no, .. }) => *no,
                (Part::Top(i), Shape::Categories(c)) => c.top.get(i)?.1,
                (Part::Bin(i), Shape::Histogram(h)) => *h.counts.get(i)?,
                _ => return None,
            })
        };
        let count = count_of(profile)?;
        let label: String = match (part, &profile.shape) {
            (Part::Missing, _) => "Missing".into(),
            (Part::Yes, _) => "true".into(),
            (Part::No, _) => "false".into(),
            (Part::Top(i), Shape::Categories(c)) => {
                let v = &c.top.get(i)?.0;
                if v.is_empty() { "(empty)".into() } else { v.chars().take(80).collect() }
            }
            (Part::Bin(i), Shape::Histogram(h)) => {
                let (a, b) = (*h.edges.get(i)?, *h.edges.get(i + 1)?);
                match h.time {
                    Some(kind) => format!("{} to {}", herse_data::format_time(a, kind), herse_data::format_time(b, kind)),
                    None if h.integral && b - a == 1. => number(a),
                    None => format!("{} to {}", number(a), number(b)),
                }
            }
            _ => return None,
        };
        let share = if profile.count > 0 { count as f64 * 100. / profile.count as f64 } else { 0. };
        let mut lines = vec![format!("{} · {}", plural(count, "row"), percent(share))];
        // With a filter or search on, how many of them are in view.
        if let Some(view) = self.view_profiles.as_ref().and_then(|v| v.get(col)?.as_ref())
            && let Some(n) = count_of(view)
        {
            lines.push(format!("{} in view", group(n)));
        }
        let active = self.part_filter(col, part).is_some_and(|t| self.spec.filters.iter().any(|f| f.column == col && f.text == t));
        let hint = if active { "Click to clear the filter" } else { "Click to filter to these rows" };
        let z = theme::zoom();
        let at = point(self.pointer.x + px(12.), self.pointer.y + px(18. * z));
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .max_w(px(300.))
                        .px(px(10.))
                        .py(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_md()
                        .text_size(px(12.))
                        .text_color(theme.foreground)
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                        .children(lines)
                        .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(hint)),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// A cell's whole value, on a double-click or space.
    fn render_popover(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let (row, col, at) = self.popover?;
        let column = self.frame.columns().get(col)?;
        if row >= self.frame.rows() {
            return None;
        }
        let theme = cx.theme();
        let null = self.frame.is_null(row, col);
        let text = self.frame.text(row, col).to_string();
        // A huge value is laid out only in part; Copy still copies it all.
        const SHOWN: usize = 20_000;
        let chars = text.chars().count();
        let shown: SharedString = if null {
            if text.is_empty() { "Missing".into() } else { format!("Missing ({text})").into() }
        } else if text.is_empty() {
            "(empty)".into()
        } else if chars > SHOWN {
            let cut = text.char_indices().nth(SHOWN).map_or(text.len(), |(i, _)| i);
            format!("{}…", &text[..cut]).into()
        } else {
            text.clone().into()
        };
        let facts = match chars {
            0 | 1 => format!("row {}", group(row + 1)),
            n if n > SHOWN => format!("row {} · first {} of {} characters", group(row + 1), group(SHOWN), group(n)),
            n => format!("row {} · {} characters", group(row + 1), group(n)),
        };
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("frame-value")
                        .occlude()
                        .w(px(380.))
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_md()
                        .text_size(px(12.5))
                        .text_color(theme.foreground)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            if this.popover.take().is_some() {
                                cx.notify();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap(px(6.))
                                .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(column.name.clone()))
                                .child(div().flex_none().text_size(px(11.)).text_color(theme.muted_foreground).child(facts)),
                        )
                        .child(
                            div()
                                .id("frame-value-text")
                                .max_h(px(280.))
                                .overflow_y_scroll()
                                .when(null, |d| d.text_color(theme.muted_foreground))
                                .child(gpui_kit::base::SelectableText::new("frame-value-selectable", shown)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap(px(6.))
                                .child(crate::ui::ghost_button("frame-value-close", "Close", cx).on_click(cx.listener(|this, _, _, cx| {
                                    this.popover = None;
                                    cx.notify();
                                })))
                                .child(crate::ui::button("frame-value-copy", "Copy", cx).on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                    this.popover = None;
                                    this.flash("Copied the value", cx);
                                }))),
                        ),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn render_stats(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let col = self.hover_column?;
        if self.resize.get().is_some() {
            return None;
        }
        let column = self.frame.columns().get(col)?;
        let theme = cx.theme();
        let s = self.frame.stats(col);
        let kind = s.kind();
        let mut rows: Vec<(&'static str, String)> = vec![
            ("Values", group(s.values())),
            ("Missing", if s.count == 0 { "0".into() } else { format!("{} ({:.1}%)", group(s.nulls), s.nulls as f64 * 100. / s.count as f64) }),
        ];
        if kind.is_numeric() {
            rows.push(("Min", number(s.min)));
            rows.push(("Max", number(s.max)));
            if let Some(mean) = s.mean() {
                rows.push(("Mean", number(mean)));
            }
        }
        let state = self.scroll.0.borrow();
        let left = state.base_handle.bounds().left();
        let origin_x = state.base_handle.bounds().origin.x;
        let scroll_x = state.base_handle.offset().x;
        drop(state);
        let z = theme::zoom();
        let sticky = col < self.geometry.sticky;
        let x = px(self.geometry.xs[col] * z) + if sticky { px(0.) } else { scroll_x };
        let _ = (left, origin_x);
        let x = x.max(px(8.));
        let note = match &self.source {
            Source::Output(o) if !o.complete && o.truncated => Some("Of the rows shown"),
            _ if matches!(self.load, Load::Reading { .. }) => Some("Of the rows read so far"),
            _ => None,
        };
        Some(
            div()
                .absolute()
                .top(px(self.header_height() * z + 2.))
                .left(x)
                .w(px(220.))
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .text_size(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(column.name.clone()))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(column.dtype.clone().unwrap_or_else(|| kind.label().to_string())),
                        ),
                )
                .children(rows.into_iter().map(|(label, value)| {
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .child(div().text_color(theme.muted_foreground).child(label))
                        .child(div().child(value))
                }))
                .when_some(note, |d, n| d.child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(n)))
                .into_any_element(),
        )
    }
}

impl Render for FrameView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep "reading… 2.3 s" and "Loading from the kernel…" ticking.
        if self.busy() && self._ticker.is_none() {
            self._ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                    let busy = this
                        .update(cx, |this, cx| {
                            cx.notify();
                            let busy = this.busy();
                            if !busy {
                                this._ticker = None;
                            }
                            busy
                        })
                        .unwrap_or(false);
                    if !busy {
                        break;
                    }
                }
            }));
        }
        let theme = cx.theme().clone();
        let progress = match &self.load {
            Load::Reading { progress: Some(p), .. } => Some(*p as f32),
            Load::Reading { progress: None, .. } => Some(0.),
            _ => None,
        };
        let accent = theme::accent(cx);
        let toolbar = self.render_toolbar(cx);
        let notices = self.render_notices(window, cx);
        let grid = self.render_grid(cx);
        let stats = self.render_stats(cx);
        let part_card = self.render_part_card(cx);
        let popover = self.render_popover(cx);
        let footer = self.render_footer(cx);
        div()
            .id("frame-view")
            .key_context(FRAME)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_copy_selection))
            .on_action(cx.listener(Self::on_clear_search))
            .on_action(cx.listener(Self::on_apply_filter))
            .on_action(cx.listener(Self::on_cancel_filter))
            .on_action(cx.listener(Self::on_edit_filter))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_move_to_top))
            .on_action(cx.listener(Self::on_move_to_bottom))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_show_value))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_undo_view))
            .on_action(cx.listener(Self::on_redo_view))
            .child(toolbar)
            // Reading: a hairline of dew that fills as the file is read.
            .child(div().flex_none().h(px(2.)).w_full().when_some(progress, |d, p| {
                d.child(div().h_full().w(gpui_kit::relative(p.clamp(0.02, 1.))).bg(accent))
            }))
            .children(notices)
            // A file that couldn't be read has nothing to draw under its error.
            .when(!(matches!(self.load, Load::Failed(_)) && self.frame.columns().is_empty()), |d| {
                d.child(div().flex_1().min_h_0().relative().flex().flex_col().child(grid).children(stats))
                    .child(footer)
                    .children(part_card)
                    .children(popover)
            })
    }
}

/// What a reader thread sends.
enum Msg {
    Event(Event),
    Done(Result<Loaded, herse_data::ReadError>),
}

#[derive(Clone, Copy)]
enum CopyAs {
    Csv,
    Tsv,
    Markdown,
}

impl CopyAs {
    fn label(self) -> &'static str {
        match self {
            CopyAs::Csv => "CSV",
            CopyAs::Tsv => "TSV",
            CopyAs::Markdown => "Markdown",
        }
    }
}

#[derive(Clone)]
struct Palette {
    ink: Hsla,
    pencil: Hsla,
    page: Hsla,
    rule: Hsla,
    strong_rule: Hsla,
    accent: Hsla,
    tint: Hsla,
    row_tint: Hsla,
    /// The row under the pointer.
    hover: Hsla,
}

/// What's selected in one row: the cursor's column, and the range's columns.
struct RowMarks {
    head: Option<usize>,
    cols: Option<std::ops::Range<usize>>,
}

/// How a column's cells are drawn, worked out once per frame.
#[derive(Clone, Copy)]
struct Look {
    numeric: bool,
    /// The source declared types (Parquet, SQLite…): a missing value there
    /// is a real null, and says so.
    typed: bool,
    index: bool,
}

struct HeaderColumn {
    name: SharedString,
    dtype: SharedString,
    numeric: bool,
    sort: Option<Order>,
    filtered: bool,
}

/// One line of text in a cell's box, cut with an ellipsis when too wide.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    text: &str,
    font: &gpui_kit::Font,
    size: Pixels,
    color: Hsla,
    x0: Pixels,
    x1: Pixels,
    y: Pixels,
    line_h: Pixels,
    right: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let line_text = text.split('\n').next().unwrap_or("");
    // Never shape more than fits: a 200 KB cell is cut before shaping.
    let max_chars = ((x1 - x0).as_f32() / 3.).max(8.) as usize;
    let mut cut = line_text.len().min(max_chars * 4);
    while !line_text.is_char_boundary(cut) {
        cut -= 1;
    }
    let shown = &line_text[..cut];
    let more = cut < line_text.len() || line_text.len() < text.len();
    let run = TextRun { len: shown.len(), font: font.clone(), color, background_color: None, underline: None, strikethrough: None };
    let system = window.text_system().clone();
    let mut line = system.shape_line(SharedString::from(shown.to_string()), size, std::slice::from_ref(&run), None);
    let inner = x1 - x0;
    if (line.width() > inner || more) && inner > px(0.) {
        let ellipsis = system.shape_line("…".into(), size, &[TextRun { len: "…".len(), ..run.clone() }], None);
        let keep = if line.width() > inner - ellipsis.width() { line.index_for_x(inner - ellipsis.width()).unwrap_or(0) } else { shown.len() };
        let mut k = keep.min(shown.len());
        while !shown.is_char_boundary(k) {
            k -= 1;
        }
        let short = format!("{}…", shown[..k].trim_end());
        line = system.shape_line(short.clone().into(), size, &[TextRun { len: short.len(), ..run }], None);
    }
    let x = if right { x0 + (inner - line.width()).max(px(0.)) } else { x0 };
    let clip = Bounds::new(point(x0, y), size_of(inner.max(px(0.)), line_h));
    window.with_content_mask(Some(gpui_kit::ContentMask { bounds: clip }), |window| {
        let _ = line.paint(point(x, y), line_h, TextAlign::Left, None, window, cx);
    });
}

fn size_of(w: Pixels, h: Pixels) -> gpui_kit::Size<Pixels> {
    size(w, h)
}

/// Where the sticky columns start, and the scroll offset, at paint time.
fn scroll_state(scroll: &UniformListScrollHandle) -> (Pixels, Pixels) {
    let s = scroll.0.borrow();
    (s.base_handle.bounds().left(), s.base_handle.offset().x)
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    frame: &Frame,
    looks: &[Look],
    row: usize,
    marks: &RowMarks,
    needle: &str,
    g: &Geometry,
    p: &Palette,
    scroll: &UniformListScrollHandle,
    z: f32,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let visible = window.content_mask().bounds;
    let (list_left, _) = scroll_state(scroll);
    let row_h = bounds.size.height;
    let y = bounds.origin.y;
    let pad = px(PAD * z);
    let font_size = px(FONT_SIZE * z);
    let regular = font(theme::UI_FONT);
    let numeric_font = gpui_kit::Font {
        features: gpui_kit::FontFeatures(Arc::new(vec![("tnum".into(), 1)])),
        ..regular.clone()
    };
    let bold = gpui_kit::Font { weight: FontWeight::SEMIBOLD, ..regular.clone() };
    let left = visible.left().max(list_left);
    let full = Bounds::new(point(left, y), size(visible.size.width, row_h));
    // The row's own element tints it under the pointer; the sticky band
    // paints over that, so it tints itself.
    let mouse = window.mouse_position();
    let hovered = full.contains(&mouse) && visible.contains(&mouse);
    if marks.head.is_some() {
        window.paint_quad(fill(full, p.row_tint));
    }
    let sticky_w = px(g.sticky_width() * z);
    let n = frame.columns().len().min(looks.len());
    let paint_cell = |c: usize, x0: Pixels, x1: Pixels, window: &mut Window, cx: &mut App| {
        let cell = Bounds::new(point(x0, y), size(x1 - x0, row_h - px(1.)));
        let in_range = marks.cols.as_ref().is_some_and(|r| r.contains(&c));
        let head = marks.head == Some(c);
        if in_range || head {
            window.paint_quad(fill(cell, p.tint));
        }
        if head {
            window.paint_quad(gpui_kit::outline(cell, p.accent, gpui_kit::BorderStyle::Solid));
        }
        let null = frame.is_null(row, c);
        let text = frame.text(row, c);
        let number = !null && frame.number(row, c).is_some();
        if !needle.is_empty() && !null && !in_range && !head && herse_data_contains(text, needle) {
            window.paint_quad(fill(cell, p.tint));
        }
        let look = looks[c];
        // A missing value: what the source wrote (NaN, NA) in pencil; an
        // empty CSV field stays empty; a typed source's null says "null".
        let (shown, color, f) = if null {
            (if text.is_empty() && look.typed { "null" } else { text }, p.pencil, &regular)
        } else if look.index {
            (text, p.ink, &bold)
        } else if number {
            (text, p.ink, &numeric_font)
        } else {
            (text, p.ink, &regular)
        };
        if !shown.is_empty() {
            let right = (number || (null && look.numeric)) && !look.index;
            paint_text(shown, f, font_size, color, x0 + pad, x1 - pad, y, row_h, right, window, cx);
        }
    };
    // Scrolling columns first, then the sticky ones over them.
    for c in g.sticky..n {
        let x0 = bounds.origin.x + px(g.xs[c] * z);
        let x1 = bounds.origin.x + px(g.xs[c + 1] * z);
        if x1 <= left + sticky_w || x0 >= visible.right() {
            continue;
        }
        paint_cell(c, x0, x1, window, cx);
    }
    if sticky_w > px(0.) {
        let band = Bounds::new(point(left, y), size(sticky_w, row_h));
        window.paint_quad(fill(band, p.page));
        if hovered {
            window.paint_quad(fill(band, p.hover));
        }
        if marks.head.is_some() {
            window.paint_quad(fill(band, p.row_tint));
        }
        if g.gutter > 0. {
            let label = group(row + 1);
            paint_text(&label, &numeric_font, px(FONT_SIZE * 0.92 * z), p.pencil, left + pad, left + px(g.gutter * z) - pad, y, row_h, true, window, cx);
        }
        for c in 0..g.sticky {
            paint_cell(c, left + px(g.xs[c] * z), left + px(g.xs[c + 1] * z), window, cx);
        }
        // A rule at the band's edge once the table has scrolled under it.
        if bounds.origin.x < list_left - px(0.5) {
            window.paint_quad(fill(Bounds::new(point(left + sticky_w - px(1.), y), size(px(1.), row_h)), p.rule));
        }
    }
    window.paint_quad(fill(Bounds::new(point(left, y + row_h - px(1.)), size(visible.size.width, px(1.))), p.rule));
}

fn herse_data_contains(text: &str, needle: &str) -> bool {
    if needle.is_ascii() {
        let n = needle.as_bytes();
        text.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        text.to_lowercase().contains(needle)
    }
}

/// Invisible handles at each visible column's right edge, for resizing.
fn header_hitboxes(g: &Geometry, scroll: &UniformListScrollHandle, z: f32, bounds: Bounds<Pixels>, window: &mut Window) -> Vec<(usize, Hitbox)> {
    let (list_left, scroll_x) = scroll_state(scroll);
    let left = bounds.left().max(list_left);
    let sticky_w = px(g.sticky_width() * z);
    let mut out = Vec::new();
    for c in 0..g.xs.len().saturating_sub(1) {
        let x = if c < g.sticky { left + px(g.xs[c + 1] * z) } else { left + scroll_x + px(g.xs[c + 1] * z) };
        if c >= g.sticky && x <= left + sticky_w {
            continue;
        }
        if x > bounds.right() {
            break;
        }
        // On the names' line only: the profiles below take clicks.
        let hit = Bounds::new(point(x - px(4.), bounds.top()), size(px(8.), px(HEADER_H * z)));
        out.push((c, window.insert_hitbox(hit, HitboxBehavior::Normal)));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn paint_header(
    columns: &[HeaderColumn],
    hover: Option<usize>,
    profiles: &Profiles,
    g: &Geometry,
    p: &Palette,
    scroll: &UniformListScrollHandle,
    z: f32,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let (list_left, scroll_x) = scroll_state(scroll);
    let left = bounds.left().max(list_left);
    let pad = px(PAD * z);
    let regular = font(theme::UI_FONT);
    let strong = gpui_kit::Font { weight: FontWeight::SEMIBOLD, ..regular.clone() };
    let name_size = px(FONT_SIZE * z);
    let type_size = px(11. * z);
    let top = bounds.top();
    let line_h = px(18. * z);
    let sticky_w = px(g.sticky_width() * z);
    let paint_column = |c: usize, x0: Pixels, x1: Pixels, window: &mut Window, cx: &mut App| {
        let col = &columns[c];
        let hovered = hover == Some(c);
        if hovered {
            window.paint_quad(fill(Bounds::new(point(x0, top), size(x1 - x0, px(HEADER_H * z))), p.ink.opacity(0.03)));
        }
        if let Some(base) = profiles.base.as_ref().and_then(|b| b.get(c)).and_then(Option::as_ref) {
            let view = profiles.view.as_ref().and_then(|v| v.get(c)).and_then(Option::as_ref);
            let hover = profiles.hover.filter(|h| h.0 == c).map(|h| h.1);
            let strip = Bounds::new(point(x0 + pad, top + px(HEADER_H * z)), size((x1 - x0 - pad * 2.).max(px(0.)), px(PROFILE_H * z)));
            paint_profile(base, view, hover, strip, z, p, window, cx);
        }
        let marks = px(MARKS * z);
        let right = col.numeric;
        let (tx0, tx1) = (x0 + pad, x1 - pad - marks);
        paint_text(&col.name, &strong, name_size, p.ink, tx0, tx1, top + px(4. * z), line_h, right, window, cx);
        paint_text(&col.dtype, &regular, type_size, p.pencil, tx0, tx1, top + px(21. * z), line_h, right, window, cx);
        let icon = px(12. * z);
        let mid = top + px(6. * z) + (line_h - icon) / 2.;
        if let Some(order) = col.sort {
            let path = if order == Order::Ascending { "icons/arrow-up.svg" } else { "icons/arrow-down.svg" };
            let at = Bounds::new(point(x1 - pad - marks + px(4. * z), mid), size(icon, icon));
            let _ = window.paint_svg(at, path.into(), None, gpui_kit::TransformationMatrix::unit(), p.accent, cx);
        }
        if col.filtered || hovered {
            let at = Bounds::new(point(x1 - pad - icon, mid), size(icon, icon));
            let color = if col.filtered { p.accent } else { p.pencil };
            let _ = window.paint_svg(at, "icons/funnel.svg".into(), None, gpui_kit::TransformationMatrix::unit(), color, cx);
        }
    };
    let n = columns.len();
    for c in g.sticky..n {
        let x0 = left + scroll_x + px(g.xs[c] * z);
        let x1 = left + scroll_x + px(g.xs[c + 1] * z);
        if x1 <= left + sticky_w || x0 >= bounds.right() {
            continue;
        }
        paint_column(c, x0, x1, window, cx);
    }
    if sticky_w > px(0.) {
        window.paint_quad(fill(Bounds::new(point(left, top), size(sticky_w, bounds.size.height)), p.page));
        if g.gutter > 0. {
            paint_text("#", &regular, type_size, p.pencil, left + pad, left + px(g.gutter * z) - pad, top + px(4. * z), line_h, true, window, cx);
        }
        for c in 0..g.sticky.min(n) {
            paint_column(c, left + px(g.xs[c] * z), left + px(g.xs[c + 1] * z), window, cx);
        }
        if scroll_x < px(-0.5) {
            window.paint_quad(fill(Bounds::new(point(left + sticky_w - px(1.), top), size(px(1.), bounds.size.height)), p.rule));
        }
    }
    // The header's rule is the strong one.
    window.paint_quad(fill(
        Bounds::new(point(bounds.left(), bounds.bottom() - px(1.)), size(bounds.size.width, px(1.))),
        p.strong_rule,
    ));
}

/// What the header draws under each name.
struct Profiles {
    base: Option<Arc<Vec<Option<Profile>>>>,
    view: Option<Arc<Vec<Option<Profile>>>>,
    hover: Option<(usize, Part)>,
}

/// A text column with (nearly) a value per row: its top values say little.
fn mostly_unique(c: &herse_data::Categories, p: &Profile) -> bool {
    c.top.first().is_none_or(|t| t.1 <= 1) || (c.distinct > 50 && c.distinct as f64 >= p.values() as f64 * 0.9)
}

/// A column's profile in `strip`: a histogram for numbers and times, its
/// commonest values for text, a split for booleans; the share missing as a
/// track along the bottom. With some rows hidden, the rows in view are
/// drawn in dew over the whole column's shape in faint ink.
#[allow(clippy::too_many_arguments)]
fn paint_profile(
    base: &Profile,
    view: Option<&Profile>,
    hover: Option<Part>,
    strip: Bounds<Pixels>,
    z: f32,
    p: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let (x0, w) = (strip.left(), strip.size.width);
    if w < px(8.) {
        return;
    }
    let top = strip.top();
    let regular = font(theme::UI_FONT);
    let numeric = gpui_kit::Font { features: gpui_kit::FontFeatures(Arc::new(vec![("tnum".into(), 1)])), ..regular.clone() };
    let small = px(10. * z);
    let ink = |a: f32| p.ink.opacity(a);
    let (base_a, hover_a) = if view.is_some() { (0.09, 0.2) } else { (0.2, 0.42) };
    let chart_h = px(PROFILE_CHART_H * z);
    let chart_bottom = top + px((PROFILE_TOP + PROFILE_CHART_H) * z);
    let label_y = chart_bottom + px(1. * z);
    let label_h = px(12. * z);
    match &base.shape {
        Shape::Histogram(h) => {
            let n = h.counts.len().max(1);
            let max = h.counts.iter().copied().max().unwrap_or(0).max(1) as f32;
            let slot = w / n as f32;
            let gap = if slot > px(4.) { px(1.) } else { px(0.) };
            let view_counts = match view.map(|v| &v.shape) {
                Some(Shape::Histogram(v)) => Some(&v.counts),
                _ => None,
            };
            for (i, &count) in h.counts.iter().enumerate() {
                let hovered = hover == Some(Part::Bin(i));
                let bx = x0 + slot * i as f32;
                let bar = |count: usize| {
                    let hgt = if count == 0 { px(0.) } else { (chart_h * (count as f32 / max)).max(px(1.)) };
                    Bounds::new(point(bx, chart_bottom - hgt), size((slot - gap).max(px(1.)), hgt))
                };
                if hovered {
                    window.paint_quad(fill(Bounds::new(point(bx, chart_bottom - chart_h), size((slot - gap).max(px(1.)), chart_h)), ink(0.04)));
                }
                window.paint_quad(fill(bar(count), ink(if hovered { hover_a } else { base_a })));
                if let Some(v) = view_counts.and_then(|v| v.get(i)) {
                    window.paint_quad(fill(bar(*v), p.accent.opacity(if hovered { 1. } else { 0.8 })));
                }
            }
            // The smallest value at the left, the largest at the right.
            let pairs = match h.time {
                Some(kind) => time_labels(h.lo, h.hi, kind),
                None => vec![(number(h.lo), number(h.hi))],
            };
            paint_ends(&pairs, &numeric, small, p.pencil, x0, w, label_y, label_h, window, cx);
        }
        Shape::Categories(c) => {
            let values = base.values().max(1) as f32;
            if mostly_unique(c, base) {
                let text = if c.capped { format!("{}+ distinct", group(c.distinct)) } else { format!("{} distinct", group(c.distinct)) };
                paint_text(&text, &regular, px(11. * z), p.pencil, x0, x0 + w, top + px(10. * z), px(16. * z), false, window, cx);
                let sub = if c.distinct as f64 >= base.values() as f64 { "all unique" } else { "mostly unique" };
                paint_text(sub, &regular, small, p.pencil, x0, x0 + w, top + px(25. * z), label_h, false, window, cx);
            } else {
                let view_top = match view.map(|v| &v.shape) {
                    Some(Shape::Categories(v)) => Some(&v.top),
                    _ => None,
                };
                for (i, (value, count)) in c.top.iter().take(PROFILE_ROWS).enumerate() {
                    let hovered = hover == Some(Part::Top(i));
                    let y = top + px((PROFILE_TOP + i as f32 * PROFILE_ROW) * z);
                    let row_h = px((PROFILE_ROW - 2.) * z);
                    window.paint_quad(fill(Bounds::new(point(x0, y), size(w, row_h)), ink(if hovered { 0.06 } else { 0.03 })));
                    let share = *count as f32 / values;
                    window.paint_quad(fill(Bounds::new(point(x0, y), size((w * share).max(px(1.)), row_h)), ink(if hovered { hover_a } else { base_a * 0.8 })));
                    if let Some(v) = view_top.and_then(|v| v.get(i)) {
                        let vs = v.1 as f32 / values;
                        if v.1 > 0 {
                            window.paint_quad(fill(Bounds::new(point(x0, y), size((w * vs).max(px(1.)), row_h)), p.accent.opacity(if hovered { 0.75 } else { 0.55 })));
                        }
                    }
                    let pct = format!("{:.0}%", share * 100.);
                    let label = if value.is_empty() { "(empty)" } else { value.as_str() };
                    paint_text(&pct, &numeric, small, p.pencil, x0 + w - px(34. * z), x0 + w - px(3.), y - px(0.5), row_h, true, window, cx);
                    paint_text(label, &regular, small, p.ink, x0 + px(4.), x0 + w - px(36. * z), y - px(0.5), row_h, false, window, cx);
                }
                if c.top.len() > PROFILE_ROWS || c.other > 0 {
                    let more = c.distinct.saturating_sub(PROFILE_ROWS);
                    if more > 0 {
                        let text = format!("+ {} more", group(more));
                        let y = top + px((PROFILE_TOP + PROFILE_ROWS as f32 * PROFILE_ROW) * z);
                        paint_text(&text, &regular, px(9.5 * z), p.pencil, x0 + px(4.), x0 + w, y - px(1.), px(10. * z), false, window, cx);
                    }
                }
            }
        }
        Shape::Bools { yes, no } => {
            let total = (*yes + *no).max(1) as f32;
            let y = top + px(8. * z);
            let bar_h = px(12. * z);
            let yes_w = w * (*yes as f32 / total);
            let (hy, hn) = (hover == Some(Part::Yes), hover == Some(Part::No));
            window.paint_quad(fill(Bounds::new(point(x0, y), size(yes_w, bar_h)), ink(if hy { hover_a + 0.1 } else { base_a + 0.08 })));
            window.paint_quad(fill(Bounds::new(point(x0 + yes_w, y), size(w - yes_w, bar_h)), ink(if hn { hover_a } else { base_a * 0.5 })));
            if let Some(Shape::Bools { yes: vy, no: vn }) = view.map(|v| &v.shape) {
                window.paint_quad(fill(Bounds::new(point(x0, y), size(w * (*vy as f32 / total), bar_h)), p.accent.opacity(0.8)));
                window.paint_quad(fill(Bounds::new(point(x0 + yes_w, y), size(w * (*vn as f32 / total), bar_h)), p.accent.opacity(0.5)));
            }
            let ly = y + bar_h + px(2. * z);
            let (ty, tn) = (percent(*yes as f64 * 100. / total as f64), percent(*no as f64 * 100. / total as f64));
            let pairs = [(format!("true {ty}"), format!("false {tn}")), (ty, tn)];
            paint_ends(&pairs, &regular, small, p.pencil, x0, w, ly, label_h, window, cx);
        }
        Shape::Empty => {
            paint_text("all missing", &regular, small, p.pencil, x0, x0 + w, top + px(12. * z), label_h, false, window, cx);
        }
    }
    // Missing values: a track along the bottom, the missing share at its right.
    if base.nulls > 0 && base.count > 0 {
        let y = top + px(PROFILE_MISSING_Y * z);
        let track_h = px(3. * z);
        let hovered = hover == Some(Part::Missing);
        window.paint_quad(fill(Bounds::new(point(x0, y), size(w, track_h)), ink(0.06)));
        let miss = (w * (base.nulls as f32 / base.count as f32)).max(px(2.));
        window.paint_quad(fill(Bounds::new(point(x0 + w - miss, y), size(miss, track_h)), p.pencil.opacity(if hovered { 0.95 } else { 0.55 })));
    }
}

/// Two labels at a strip's ends, the first pair that fits whole (else the
/// last, cut to halves).
#[allow(clippy::too_many_arguments)]
fn paint_ends(
    pairs: &[(String, String)],
    font: &gpui_kit::Font,
    size: Pixels,
    color: Hsla,
    x0: Pixels,
    w: Pixels,
    y: Pixels,
    line_h: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let system = window.text_system().clone();
    let width = |text: &str| {
        let run = TextRun { len: text.len(), font: font.clone(), color, background_color: None, underline: None, strikethrough: None };
        system.shape_line(SharedString::from(text.to_string()), size, &[run], None).width()
    };
    let gap = px(6.);
    let fit = pairs.iter().find(|(a, b)| width(a) + width(b) + gap <= w);
    let (lo, hi, split) = match fit {
        // A pixel of slack: the measured width can round under the painted one.
        Some((a, b)) => (a, b, x0 + width(a) + px(1.)),
        None => match pairs.last() {
            Some((a, b)) => (a, b, x0 + w / 2. - gap / 2.),
            None => return,
        },
    };
    paint_text(lo, font, size, color, x0, split, y, line_h, false, window, cx);
    paint_text(hi, font, size, color, split + gap, x0 + w, y, line_h, true, window, cx);
}

/// The ends of a time histogram, most precise first.
fn time_labels(lo: f64, hi: f64, kind: herse_data::TimeKind) -> Vec<(String, String)> {
    use herse_data::TimeKind;
    let (a, b) = (herse_data::format_time(lo, TimeKind::DateTime), herse_data::format_time(hi, TimeKind::DateTime));
    let day = |s: &str| s[..10.min(s.len())].to_string();
    let clock = |s: &str| s.get(11..).unwrap_or("").to_string();
    let month = |s: &str| {
        const NAMES: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let m: usize = s.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(1);
        let d: u32 = s.get(8..10).and_then(|d| d.parse().ok()).unwrap_or(1);
        format!("{} {d}", NAMES[(m.clamp(1, 12)) - 1])
    };
    let year = |s: &str| s[..4.min(s.len())].to_string();
    let mut out = Vec::new();
    if kind == TimeKind::DateTime && day(&a) == day(&b) {
        // One day: the clock, then the day.
        out.push((a.clone(), clock(&b)));
        out.push((clock(&a), clock(&b)));
        return out;
    }
    if kind == TimeKind::DateTime && hi - lo < 3. * 86_400. {
        out.push((a.clone(), b.clone()));
        out.push((format!("{} {}", month(&a), clock(&a)), format!("{} {}", month(&b), clock(&b))));
    }
    out.push((day(&a), day(&b)));
    if year(&a) == year(&b) {
        out.push((month(&a), month(&b)));
    } else {
        out.push((a.get(..7).unwrap_or("").to_string(), b.get(..7).unwrap_or("").to_string()));
        out.push((year(&a), year(&b)));
    }
    out
}

/// `name.ipynb` beside the data, or `name 2.ipynb`, … if that's taken.
fn unused_path(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.ipynb"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} {n}.ipynb")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// `1234567` → `1,234,567`.
pub fn group(n: usize) -> String {
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

/// A share of rows: "12%", "4.5%", "95.5%", "<0.1%".
fn percent(p: f64) -> String {
    if p > 0. && p < 0.1 {
        "<0.1%".into()
    } else if (p < 10. || p > 90.) && (p * 10.).round() % 10. != 0. {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{} {what}s", group(n)) }
}

/// A statistic, to about six significant digits.
fn number(x: f64) -> String {
    if !x.is_finite() {
        return format!("{x}");
    }
    if x.fract() == 0. && x.abs() < 1e15 {
        return group_signed(x as i64);
    }
    let mag = x.abs();
    if !(1e-4..1e15).contains(&mag) {
        return format!("{x:.4e}");
    }
    let decimals = (5 - mag.log10().floor() as i32).clamp(0, 6) as usize;
    let s = format!("{x:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    match s.split_once('.') {
        Some((int, frac)) => {
            let n: i64 = int.parse().unwrap_or(0);
            let sign = if x < 0. && n == 0 { "-" } else { "" };
            format!("{sign}{}.{frac}", group_signed(n))
        }
        None => s,
    }
}

fn group_signed(n: i64) -> String {
    let g = group(n.unsigned_abs() as usize);
    if n < 0 { format!("-{g}") } else { g }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_steps_compare_what_the_reader_sees() {
        let base = ViewState { sort: None, filters: Vec::new(), search: String::new(), widths: Vec::new() };
        // Unset widths, however many, are the same view; so is a padded search.
        assert!(base.same(&ViewState { widths: vec![None; 4], search: " ".into(), ..base.clone() }));
        let wider = ViewState { widths: vec![None, Some(120.)], ..base.clone() };
        assert!(!base.same(&wider));
        assert!(base.same_rows(&wider));
        let sorted = ViewState { sort: Some((1, Order::Descending)), ..base.clone() };
        assert!(!base.same_rows(&sorted));
        let filtered = ViewState { filters: vec![Filter::parse(0, "> 3").unwrap()], ..base.clone() };
        assert!(!base.same(&filtered));
        assert!(filtered.same(&ViewState { filters: vec![Filter::parse(0, "> 3").unwrap()], ..base.clone() }));
    }

    #[test]
    fn shares_and_time_ends_read_well() {
        assert_eq!(percent(0.04), "<0.1%");
        assert_eq!(percent(4.5), "4.5%");
        assert_eq!(percent(95.5), "95.5%");
        assert_eq!(percent(17.4), "17%");
        assert_eq!(percent(100.), "100%");
        let day = 86_400.;
        let jan1 = 1_704_067_200.; // 2024-01-01 00:00 UTC
        let ends = time_labels(jan1, jan1 + 51. * day, herse_data::TimeKind::Date);
        assert_eq!(ends[0], ("2024-01-01".to_string(), "2024-02-21".to_string()));
        assert_eq!(ends[1], ("Jan 1".to_string(), "Feb 21".to_string()));
        let hours = time_labels(jan1 + 3600., jan1 + 5. * 3600., herse_data::TimeKind::DateTime);
        assert_eq!(hours[1], ("01:00".to_string(), "05:00".to_string()));
        let years = time_labels(jan1, jan1 + 800. * day, herse_data::TimeKind::Date);
        assert_eq!(years.last().unwrap(), &("2024".to_string(), "2026".to_string()));
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(group(12), "12");
        assert_eq!(number(1234.5678), "1,234.57");
        assert_eq!(number(-0.5), "-0.5");
        assert_eq!(number(3.0), "3");
        assert_eq!(number(0.000123456), "0.000123");
        assert_eq!(number(-1234567.0), "-1,234,567");
        assert_eq!(digits(0), 1);
        assert_eq!(digits(1_100_000), 7);
    }

    #[test]
    fn geometry_finds_columns() {
        let g = Geometry { xs: vec![40., 100., 200.].into(), gutter: 40., sticky: 0, width: 224., digits: 2 };
        assert_eq!(g.column_at(10.), None);
        assert_eq!(g.column_at(50.), Some(0));
        assert_eq!(g.column_at(150.), Some(1));
        assert_eq!(g.column_at(250.), None);
    }
}
