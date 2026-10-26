//! The window's root view: sidebar, tabs, the active notebook, status bar,
//! and the overlays (file finder, command palette).

use std::path::{Path, PathBuf};

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};

use gpui_kit::StyledImage as _;

use crate::{
    actions::*,
    finder::{FileFinder, KernelPicker, OutlinePicker, WorkspaceSearch},
    picker::Picker,
    frame_view::{FrameEvent, FrameView},
    notebook::{NotebookEvent, NotebookView, document::Document},
    sidebar::{Sidebar, SidebarEvent},
    theme,
};

/// A notebook that wouldn't open (see [`Workspace::open_problem`]).
struct OpenProblem {
    title: SharedString,
    detail: SharedString,
    /// The file it's about, for Show in Finder.
    path: Option<PathBuf>,
    /// Ways to get the notebook back (see [`crate::broken`]).
    fixes: Vec<crate::broken::Fix>,
}

pub struct Workspace {
    pub root: Option<PathBuf>,
    tabs: Vec<Tab>,
    /// The tab `add_tab` makes next is a preview (see [`Tab::preview`]).
    next_tab_is_preview: bool,
    /// What the window last told macOS: title, unsaved edits, file. (The
    /// title bar is hidden, but Mission Control, the Window menu and
    /// VoiceOver read them.)
    window_identity: Option<(String, bool, Option<PathBuf>)>,
    active: Option<usize>,
    sidebar: Entity<Sidebar>,
    sidebar_open: bool,
    focus_handle: FocusHandle,
    overlay: Option<Overlay>,
    /// Notebooks opened this session, most recent first (feeds Go to File).
    recent: Vec<PathBuf>,
    /// Closed tabs' files, the latest last, for ⌘⇧T.
    closed: Vec<PathBuf>,
    /// A figure shown large, over everything.
    lightbox: Option<(std::sync::Arc<gpui_kit::Image>, f32, f32)>,
    /// A notebook that wouldn't open: shown above the content, selectable
    /// and copyable, until dismissed or another notebook opens.
    open_problem: Option<OpenProblem>,
    /// A conversation with a coding agent about a notebook: texts in the
    /// bottom right corner.
    agent: Option<(Entity<crate::agent::Conversation>, Subscription)>,
    lightbox_focus: FocusHandle,
    /// A mouse-down on the tab strip that may turn into a window drag.
    drag_pending: std::rc::Rc<std::cell::Cell<bool>>,
    _watcher: Option<herse_workspace::Watcher>,
    _watch_task: Option<gpui_kit::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

struct Overlay {
    view: gpui_kit::AnyView,
    /// Which opening this is: each one animates in afresh.
    seq: usize,
    return_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

struct Tab {
    page: Page,
    /// A preview (a single click in the sidebar): the next preview replaces
    /// it. Editing it or double-clicking it keeps it.
    preview: bool,
    _subscription: Subscription,
}

/// What a tab shows: a notebook, or a table (a data file, or a notebook's
/// DataFrame output at full size).
#[derive(Clone, PartialEq)]
enum Page {
    Notebook(Entity<NotebookView>),
    Frame(Entity<FrameView>),
}

impl Tab {
    fn notebook(&self) -> Option<&Entity<NotebookView>> {
        match &self.page {
            Page::Notebook(view) => Some(view),
            Page::Frame(_) => None,
        }
    }

    fn path<'a>(&'a self, cx: &'a gpui_kit::App) -> Option<&'a PathBuf> {
        match &self.page {
            Page::Notebook(view) => view.read(cx).path(),
            Page::Frame(view) => view.read(cx).path(),
        }
    }

    /// Unsaved edits (only notebooks have any).
    fn dirty(&self, cx: &gpui_kit::App) -> bool {
        self.notebook().is_some_and(|view| view.read(cx).doc.dirty)
    }

    fn title(&self, cx: &gpui_kit::App) -> String {
        match &self.page {
            Page::Notebook(view) => view.read(cx).title(),
            Page::Frame(view) => view.read(cx).title(),
        }
    }

    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        match &self.page {
            Page::Notebook(view) => view.read(cx).focus_handle(cx),
            Page::Frame(view) => view.read(cx).focus_handle(cx),
        }
    }

    fn element(&self) -> gpui_kit::AnyElement {
        match &self.page {
            Page::Notebook(view) => view.clone().into_any_element(),
            Page::Frame(view) => view.clone().into_any_element(),
        }
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let sub = cx.subscribe_in(&sidebar, window, |this, _, event: &SidebarEvent, window, cx| match event {
            SidebarEvent::Open(path) => this.open_path(path, window, cx),
            // A preview leaves the keyboard in the tree, to keep browsing.
            SidebarEvent::Preview(path) => {
                this.open_path_as(path, true, window, cx);
                this.sidebar.read(cx).focus_handle(cx).focus(window, cx);
            }
            SidebarEvent::Trashed(path) => {
                this.forget_path(path, window, cx);
                // Keep browsing (and trashing) from the tree.
                this.sidebar.read(cx).focus_handle(cx).focus(window, cx);
            }
            SidebarEvent::Renamed { from, to } => this.follow_rename(from, to, cx),
            SidebarEvent::Ask { title, detail, choices, answer } => {
                this.show_dialog(None, title.clone(), Some(detail.clone()), choices.clone(), answer.clone(), window, cx);
            }
            SidebarEvent::UndoFailed(why) => {
                this.open_problem = Some(OpenProblem {
                    title: "Couldn't do that to the file".into(),
                    detail: why.clone().into(),
                    path: None,
                    fixes: Vec::new(),
                });
                cx.notify();
            }
            SidebarEvent::Blur => {
                if let Some(view) = this.active_view() {
                    view.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            SidebarEvent::JumpToHeading { cell, heading } => {
                if let Some(view) = this.active_view().cloned() {
                    view.update(cx, |nb, cx| nb.jump_to_heading(*cell, *heading, window, cx));
                }
                this.push_outline(cx);
            }
            SidebarEvent::OutlineShown => this.push_outline(cx),
        });
        // With no tab open the workspace itself takes focus, so its key
        // bindings (Cmd-O, Cmd-P, ...) work from the welcome screen.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let mut subscriptions = vec![sub];
        // An extension asking whether it may run shows above the page; one
        // allowed from an output that then won't start says why there.
        if let Some(host) = crate::extensions::host(cx) {
            subscriptions.push(cx.observe(&host, |_, _, cx| cx.notify()));
            subscriptions.push(cx.subscribe(&host, |this, _, problem: &crate::extensions::HostProblem, cx| {
                this.open_problem = Some(OpenProblem {
                    title: problem.title.clone().into(),
                    detail: problem.detail.clone().into(),
                    path: crate::extensions::folder(),
                    fixes: Vec::new(),
                });
                cx.notify();
            }));
        }
        Self {
            root: None,
            tabs: Vec::new(),
            closed: Vec::new(),
            next_tab_is_preview: false,
            window_identity: None,
            active: None,
            sidebar,
            sidebar_open: true,
            focus_handle,
            overlay: None,
            recent: Vec::new(),
            lightbox: None,
            open_problem: None,
            agent: None,
            lightbox_focus: cx.focus_handle(),
            drag_pending: Default::default(),
            _watcher: None,
            _watch_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path_as(path, false, window, cx);
    }

    /// Open `path`; as a `preview`, it takes the place of the current preview
    /// tab (one without unsaved edits) instead of adding a tab.
    fn open_path_as(&mut self, path: &Path, preview: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if path.is_dir() {
            self.set_root(path, window, cx);
            return;
        }
        if self.root.is_none()
            && let Some(parent) = path.parent()
        {
            self.set_root(parent.to_path_buf(), window, cx);
        }
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| t.path(cx) == Some(&path))
        {
            if !preview {
                self.tabs[ix].preview = false;
            }
            self.activate(ix, window, cx);
            return;
        }
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path.clone());
        // Data files open as a table, read in the background.
        if herse_data::Format::is_data_path(&path) {
            let store = crate::recents::get(cx);
            store.recents.open_notebook(&path);
            store.save();
            cx.defer(crate::set_menus);
            let view = cx.new(|cx| FrameView::open_file(path.clone(), window, cx));
            self.next_tab_is_preview = preview;
            self.add_frame_tab(view, window, cx);
            self.open_problem = None;
            return;
        }
        match Document::open(&path) {
            Ok(doc) => {
                let store = crate::recents::get(cx);
                store.recents.open_notebook(&path);
                store.save();
                cx.defer(crate::set_menus);
                let view = cx.new(|cx| NotebookView::new(doc, window, cx));
                self.next_tab_is_preview = preview;
                if let Some(cell) = crate::settings::position(&path) {
                    view.update(cx, |nb, cx| {
                        if cell > 0 && cell < nb.cell_count() {
                            nb.jump_to_cell(cell, window, cx);
                        }
                    });
                }
                self.add_tab(view, window, cx);
                self.open_problem = None;
            }
            Err(err) => {
                eprintln!("Couldn't open {}: {err:#}", path.display());
                // What's wrong in a sentence, the file's text, and any fix.
                let d = crate::broken::diagnose(&path, &err);
                self.open_problem =
                    Some(OpenProblem { title: d.title.into(), detail: d.detail.into(), path: Some(path), fixes: d.fixes });
                cx.notify();
            }
        }
    }

    fn set_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.as_ref() == Some(&root) {
            return;
        }
        self.root = Some(root.clone());
        self.sidebar.update(cx, |s, cx| s.set_root(root.clone(), window, cx));
        let store = crate::recents::get(cx);
        store.recents.open_folder(&root);
        store.save();
        self.watch(root, cx);
        cx.notify();
    }

    /// Follow the folder on disk: the tree refreshes on structural changes,
    /// and open notebooks edited elsewhere reload when they have no unsaved edits.
    fn watch(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = herse_kernel::async_channel::unbounded();
        let watcher = herse_workspace::watch(&root, move |event| {
            let _ = tx.try_send(event);
        });
        let Ok(watcher) = watcher else {
            self._watcher = None;
            return;
        };
        self._watcher = Some(watcher);
        self._watch_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut events = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    events.push(ev);
                }
                if this.update(cx, |ws, cx| ws.on_disk_events(events, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn on_disk_events(&mut self, events: Vec<herse_workspace::WatchEvent>, cx: &mut Context<Self>) {
        use herse_workspace::WatchEventKind as K;
        let mut rescan = false;
        let mut touched: Vec<PathBuf> = Vec::new();
        for event in events {
            let notebook_paths = event.paths.iter().filter(|p| herse_workspace::is_notebook_path(p));
            match event.kind {
                K::Modified => touched.extend(notebook_paths.cloned()),
                K::Created | K::Renamed => {
                    rescan = true;
                    // Atomic saves (write temp, rename over) arrive as creates.
                    touched.extend(notebook_paths.cloned());
                }
                K::Removed | K::Rescan => rescan = true,
            }
        }
        if rescan {
            self.sidebar.update(cx, |s, cx| s.rescan(cx));
        }
        touched.sort();
        touched.dedup();
        for path in touched {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if let Some(tab) = self.tabs.iter().find(|t| t.path(cx) == Some(&path)) {
                match &tab.page {
                    Page::Notebook(view) => view.update(cx, |v, cx| v.disk_changed(cx)),
                    Page::Frame(view) => view.update(cx, |v, cx| v.reload(cx)),
                }
            }
        }
    }

    /// Reopen the last folder and its tabs.
    pub fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = crate::recents::get(cx);
        let Some(folder) = store.recents.last_folder.clone().filter(|f| f.is_dir()) else { return };
        let (tabs, active) = store.folder_tabs(&folder);
        self.set_root(folder, window, cx);
        for tab in tabs.iter().filter(|t| t.is_file()) {
            self.open_path(tab, window, cx);
        }
        if let Some(active) = active
            && let Some(ix) = self.tabs.iter().position(|t| t.path(cx) == Some(&active))
        {
            self.activate(ix, window, cx);
        }
    }

    fn remember_tabs(&self, cx: &mut Context<Self>) {
        let positions: Vec<(PathBuf, usize)> = self
            .tabs
            .iter()
            .filter_map(|t| {
                let v = t.notebook()?.read(cx);
                v.path().map(|p| (p.clone(), v.selected_index()))
            })
            .collect();
        crate::settings::set_positions(positions);
        let Some(root) = self.root.clone() else { return };
        let tabs: Vec<PathBuf> = self.tabs.iter().filter_map(|t| t.path(cx).cloned()).collect();
        let active = self.active_tab().and_then(|t| t.path(cx).cloned());
        let store = crate::recents::get(cx);
        store.recents.set_tabs(&root, tabs, active);
        store.save();
    }

    /// Before quitting: write pending edits, remember the session.
    pub fn prepare_quit(&mut self, cx: &mut Context<Self>) {
        for view in self.tabs.iter().filter_map(Tab::notebook) {
            view.update(cx, |v, cx| {
                if v.doc.dirty && v.doc.path.is_some() {
                    v.save_now(false, cx);
                }
            });
        }
        self.remember_tabs(cx);
    }

    /// Keep the outline panel in step with the active notebook.
    fn push_outline(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.read(cx).mode() != crate::sidebar::Mode::Outline {
            return;
        }
        let (outline, current) = match self.active_view() {
            Some(view) => view.read(cx).outline(),
            None => (Default::default(), None),
        };
        self.sidebar.update(cx, |s, cx| s.set_outline(outline, current, cx));
    }

    fn add_tab(&mut self, view: Entity<NotebookView>, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.subscribe_in(&view, window, |this, from, event: &NotebookEvent, window, cx| {
            match event {
                NotebookEvent::OpenLink(link) => this.open_web_notebook(link.clone(), from.clone(), window, cx),
                NotebookEvent::OpenNotebook(path, anchor) => {
                    this.open_path(path, window, cx);
                    if let (Some(anchor), Some(view)) = (anchor, this.active_view().cloned()) {
                        view.update(cx, |nb, cx| nb.jump_to_anchor(anchor, window, cx));
                    }
                }
                NotebookEvent::ShowImage(image, w, h) => {
                    this.lightbox = Some((image.clone(), *w, *h));
                    this.lightbox_focus.focus(window, cx);
                }
                NotebookEvent::Changed => {
                    if let Some(tab) = this.tabs.iter_mut().find(|t| t.preview && t.dirty(cx)) {
                        tab.preview = false;
                    }
                }
                NotebookEvent::OpenFrame(open) => this.open_output_frame(from.clone(), open, window, cx),
            }
            this.push_outline(cx);
            cx.notify();
        });
        let described = crate::extensions::describe(view.read(cx));
        crate::extensions::notify("notebook.opened", described, serde_json::Value::Null, cx);
        self.push_tab(Page::Notebook(view), sub, window, cx);
    }

    /// A table in a tab: a data file, or a DataFrame output.
    fn add_frame_tab(&mut self, view: Entity<FrameView>, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.subscribe_in(&view, window, |this, _, event: &FrameEvent, window, cx| {
            match event {
                FrameEvent::Changed => this.remember_tabs(cx),
                // New Notebook from This File: the notebook it wrote.
                FrameEvent::OpenNotebook(doc) => {
                    if let Some(doc) = doc.borrow_mut().take() {
                        let view = cx.new(|cx| NotebookView::new(doc, window, cx));
                        this.add_tab(view, window, cx);
                        this.sidebar.update(cx, |s, cx| s.rescan(cx));
                    }
                }
            }
            cx.notify();
        });
        self.push_tab(Page::Frame(view), sub, window, cx);
    }

    /// A notebook's DataFrame output, full size, in a tab of its own.
    fn open_output_frame(&mut self, nb: Entity<NotebookView>, open: &crate::notebook::OpenFrame, window: &mut Window, cx: &mut Context<Self>) {
        let (cell_number, name) = nb.read(cx).frame_name(open.cell_uid, open.output);
        let source = crate::frame_view::OutputSource {
            notebook: nb.downgrade(),
            notebook_title: nb.read(cx).title(),
            cell_uid: open.cell_uid,
            output: open.output,
            cell_number,
            name,
            saved_rows: 0,
            total_rows: None,
            total_columns: None,
            truncated: false,
            complete: false,
            fetched_index: 0,
        };
        let table = open.table.clone();
        let view = cx.new(|cx| FrameView::from_output(&table, source, window, cx));
        self.next_tab_is_preview = false;
        self.add_frame_tab(view, window, cx);
    }

    fn push_tab(&mut self, page: Page, sub: Subscription, window: &mut Window, cx: &mut Context<Self>) {
        let preview = std::mem::take(&mut self.next_tab_is_preview);
        let tab = Tab {
            page,
            preview,
            _subscription: sub,
        };
        // A new preview takes the old one's place.
        let replace = if preview {
            self.tabs.iter().position(|t| t.preview && !t.dirty(cx))
        } else {
            None
        };
        let ix = match replace {
            Some(ix) => {
                self.retire_tab(ix, cx);
                self.tabs[ix] = tab;
                ix
            }
            None => {
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        self.activate(ix, window, cx);
        self.remember_tabs(cx);
    }

    /// What closing a tab keeps: its place in the notebook, and its edits.
    fn retire_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(view) = self.tabs[ix].notebook().cloned() else { return };
        if let Some(path) = view.read(cx).path().cloned() {
            let cell = view.read(cx).selected_index();
            crate::settings::set_positions(vec![(path, cell)]);
        }
        // Closing writes pending edits, like the auto-save would have.
        view.update(cx, |v, cx| {
            if v.doc.dirty && v.doc.path.is_some() {
                v.save_now(false, cx);
            }
        });
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = Some(ix);
        let tab = &self.tabs[ix];
        if let Some(view) = tab.notebook().cloned() {
            crate::extensions::set_target(window.window_handle(), &view, cx);
        }
        let path = tab.path(cx).cloned();
        let focus = tab.focus_handle(cx);
        self.sidebar.update(cx, |s, cx| s.set_active(path, cx));
        focus.focus(window, cx);
        self.remember_tabs(cx);
        self.push_outline(cx);
        cx.notify();
    }

    /// The active tab's notebook (None when it shows a table).
    fn active_view(&self) -> Option<&Entity<NotebookView>> {
        self.active_tab().and_then(Tab::notebook)
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|ix| self.tabs.get(ix))
    }

    /// Close every tab but `keep` (the highest first, so indices hold).
    fn close_other_tabs(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kept) = self.tabs.get(keep).map(|t| t.page.clone()) else { return };
        for ix in (0..self.tabs.len()).rev().filter(|&ix| ix != keep) {
            self.close_tab(ix, window, cx);
        }
        // Closing activates a neighbour; end on the tab that was kept (an
        // untitled tab asking to be saved may still be open beside it).
        if let Some(ix) = self.tabs.iter().position(|t| t.page == kept) {
            self.activate(ix, window, cx);
        }
    }

    /// Close every tab (those with unsaved edits ask first).
    fn close_all_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for ix in (0..self.tabs.len()).rev() {
            self.close_tab(ix, window, cx);
        }
    }

    /// Close the tabs after `ix`.
    fn close_tabs_to_the_right(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        for later in (ix + 1..self.tabs.len()).rev() {
            self.close_tab(later, window, cx);
        }
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        // Edits with nowhere to auto-save (an untitled notebook, or one whose
        // auto-save is paused: changed on disk, or recovered): ask.
        let Some(view) = self.tabs[ix].notebook().cloned() else {
            self.discard_tab(ix, window, cx);
            return;
        };
        let (untitled_dirty, title, has_path) = {
            let v = view.read(cx);
            (v.doc.dirty && (v.doc.path.is_none() || v.doc.autosave_blocked), v.title(), v.doc.path.is_some())
        };
        if untitled_dirty {
            use crate::dialog::Role;
            let answer = self.ask(
                Some(crate::dialog::Icon::Svg("icons/notebook.svg")),
                format!("Save changes to “{title}”?"),
                Some(
                    if has_path {
                        "Its changes haven't been written to the file; closing without saving discards them."
                    } else {
                        "It hasn't been saved yet; closing without saving discards it."
                    }
                    .into(),
                ),
                vec![
                    ("Don't Save".into(), Role::Destructive),
                    ("Cancel".into(), Role::Cancel),
                    ((if has_path { "Save" } else { "Save…" }).into(), Role::Default),
                ],
                window,
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Ok(choice) = answer.recv().await else { return };
                this.update_in(cx, |this, window, cx| match choice {
                    2 if has_path => {
                        view.update(cx, |v, cx| v.save_now(true, cx));
                        if !view.read(cx).doc.dirty
                            && let Some(ix) = this.tabs.iter().position(|t| t.notebook() == Some(&view))
                        {
                            this.discard_tab(ix, window, cx);
                        }
                    }
                    2 => this.save_as(view.clone(), window, cx),
                    0 => {
                        if let Some(ix) = this.tabs.iter().position(|t| t.notebook() == Some(&view)) {
                            this.discard_tab(ix, window, cx);
                        }
                    }
                    _ => {}
                })
                .ok();
            })
            .detach();
            return;
        }
        self.discard_tab(ix, window, cx);
    }

    /// A file (or folder) went to the Trash: close its tabs without writing
    /// them back, which would bring the file back.
    fn forget_path(&mut self, gone: &Path, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(ix) = self.tabs.iter().position(|t| t.path(cx).is_some_and(|p| p.starts_with(gone))) {
            self.tabs.remove(ix);
            self.closed.retain(|p| !p.starts_with(gone));
            if self.tabs.is_empty() {
                self.active = None;
                self.focus_handle.focus(window, cx);
            } else {
                let next = self.active.unwrap_or(0).min(self.tabs.len() - 1);
                self.activate(next, window, cx);
            }
        }
        let store = crate::recents::get(cx);
        store.recents.forget_notebook(gone);
        store.save();
        cx.defer(crate::set_menus);
        self.remember_tabs(cx);
        cx.notify();
    }

    /// Renamed in the tree: open tabs of it (or of files inside a renamed
    /// folder) take the new path.
    fn follow_rename(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            match &tab.page {
                Page::Notebook(view) => view.update(cx, |view, cx| {
                    if let Some(path) = view.doc.path.clone()
                        && let Ok(rest) = path.strip_prefix(from)
                    {
                        view.doc.path = Some(if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) });
                        cx.emit(NotebookEvent::Changed);
                        cx.notify();
                    }
                }),
                Page::Frame(view) => view.update(cx, |view, cx| {
                    if let Some(path) = view.path().cloned()
                        && let Ok(rest) = path.strip_prefix(from)
                    {
                        view.set_path(if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) }, cx);
                    }
                }),
            }
        }
        let active = self.active_tab().and_then(|t| t.path(cx).cloned());
        self.sidebar.update(cx, |s, cx| s.set_active(active, cx));
        let store = crate::recents::get(cx);
        store.recents.rename_notebook(from, to);
        store.save();
        cx.defer(crate::set_menus);
        self.remember_tabs(cx);
        cx.notify();
    }

    fn discard_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.retire_tab(ix, cx);
        if let Some(path) = self.tabs[ix].path(cx).cloned() {
            self.closed.retain(|p| p != &path);
            self.closed.push(path);
        }
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            self.active = None;
            self.focus_handle.focus(window, cx);
            self.sidebar.update(cx, |s, cx| s.set_active(None, cx));
        } else {
            let next = ix.min(self.tabs.len() - 1);
            self.activate(next, window, cx);
        }
        self.remember_tabs(cx);
        cx.notify();
    }

    // ---------------------------------------------------------------- overlays

    /// Any sheet that closes itself with `DismissEvent`, over the workspace.
    /// Ask in Herse's own dialog, never the Mac's alert: the index of the
    /// choice taken (an error when it's closed unanswered, by a click
    /// outside).
    pub fn ask(
        &mut self,
        icon: Option<crate::dialog::Icon>,
        title: impl Into<SharedString>,
        detail: Option<SharedString>,
        choices: Vec<(SharedString, crate::dialog::Role)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> smol::channel::Receiver<usize> {
        let (tx, rx) = smol::channel::bounded(1);
        self.show_dialog(icon, title.into(), detail, choices, tx, window, cx);
        rx
    }

    #[allow(clippy::too_many_arguments)]
    fn show_dialog(
        &mut self,
        icon: Option<crate::dialog::Icon>,
        title: SharedString,
        detail: Option<SharedString>,
        choices: Vec<(SharedString, crate::dialog::Role)>,
        answer: smol::channel::Sender<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = window.focused(cx);
        let dialog = cx.new(|cx| crate::dialog::Dialog::new(icon, title, detail, choices, answer, window, cx));
        self.show_sheet(dialog, return_focus, window, cx);
    }

    fn show_sheet<V: gpui_kit::Render + gpui_kit::EventEmitter<gpui_kit::DismissEvent>>(&mut self, sheet: Entity<V>, return_focus: Option<FocusHandle>, window: &mut Window, cx: &mut Context<Self>) {
        let subscription = cx.subscribe_in(&sheet, window, |this, _, _: &gpui_kit::DismissEvent, window, cx| {
            this.close_overlay(window, cx);
        });
        self.overlay = Some(Overlay { view: sheet.into(), seq: next_overlay_seq(), return_focus, _subscription: subscription });
        cx.notify();
    }

    /// Open Link: the sheet for a notebook's web link or a live session's
    /// link (filled in when a `herse://` link was clicked).
    pub fn show_link_sheet(
        &mut self,
        link: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::link_sheet::LinkSheet> {
        let return_focus = window.focused(cx);
        let ws = cx.entity().downgrade();
        let sheet = cx.new(|cx| crate::link_sheet::LinkSheet::new(ws, link, window, cx));
        self.show_sheet(sheet.clone(), return_focus, window, cx);
        sheet
    }

    /// A link pasted into ⌘K: open it straight away, in the sheet, which
    /// shows the download or the join and anything that goes wrong.
    fn on_open_link_text(&mut self, action: &OpenLinkText, window: &mut Window, cx: &mut Context<Self>) {
        let sheet = self.show_link_sheet(Some(action.0.clone()), window, cx);
        sheet.update(cx, |sheet, cx| sheet.open(window, cx));
    }

    /// ⌘K ▸ an extension's command: what it says back goes to the status
    /// line; a failure stays above the page, to read and copy. One not
    /// allowed to run asks first, and runs the command once allowed.
    fn on_run_extension_command(&mut self, action: &crate::extensions::RunExtensionCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = crate::extensions::host(cx) else { return };
        if !host.update(cx, |h, cx| h.ready(&action.extension, cx)) {
            host.update(cx, |h, cx| h.ask(&action.extension, Some(action.command.clone()), cx));
            return;
        }
        self.run_extension_command(&action.extension, &action.command, window, cx);
    }

    fn run_extension_command(&mut self, extension: &str, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = crate::extensions::host(cx) else { return };
        if let Some(view) = self.active_view().cloned() {
            crate::extensions::set_target(window.window_handle(), &view, cx);
        }
        let task = host.update(cx, |h, cx| h.run_command(extension, command, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Some(message)) => {
                        if let Some(view) = this.active_view().cloned() {
                            view.update(cx, |nb, cx| nb.flash(message, cx));
                        }
                    }
                    Ok(None) => {}
                    Err(why) => {
                        this.open_problem = Some(OpenProblem {
                            title: "An extension's command failed".into(),
                            detail: why.into(),
                            path: crate::extensions::folder(),
                            fixes: Vec::new(),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Herse ▸ Install Command Line Tool: `herse` in the Terminal. Done, it
    /// says where (an alert, since it's a one-time setup); if it can't, why,
    /// above the page, with the command to do it by hand.
    fn on_install_command_line_tool(&mut self, _: &InstallCommandLineTool, window: &mut Window, cx: &mut Context<Self>) {
        let result = match crate::cli_install::bundled_tool() {
            None => Err("This copy of Herse doesn't include the command-line tool (it's a development build). \
                         Build the app with packaging/macos/bundle.sh, or run target/release/herse-cli."
                .to_string()),
            Some(tool) => {
                let path = herse_kernel::shell_env::get()
                    .and_then(|env| env.get("PATH").cloned())
                    .or_else(|| std::env::var("PATH").ok())
                    .unwrap_or_default();
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                crate::cli_install::install(&tool, &path, &home)
            }
        };
        match result {
            Ok(done) => {
                let what = if done.already { "The herse command was already installed" } else { "Installed the herse command" };
                let detail = format!(
                    "{} links to this Herse. In a new Terminal window: herse . opens the folder you're in, herse --help says the rest.",
                    done.link.display()
                );
                // The answer (OK) changes nothing.
                drop(self.ask(
                    Some(crate::dialog::Icon::Svg("icons/command.svg")),
                    what,
                    Some(detail.into()),
                    vec![("OK".into(), crate::dialog::Role::Default)],
                    window,
                    cx,
                ));
            }
            Err(why) => {
                self.open_problem = Some(OpenProblem {
                    title: "Couldn't install the herse command".into(),
                    detail: why.into(),
                    path: None,
                    fixes: Vec::new(),
                });
                cx.notify();
            }
        }
    }

    fn on_open_extensions_folder(&mut self, _: &OpenExtensionsFolder, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = crate::extensions::folder() {
            let _ = std::fs::create_dir_all(&dir);
            cx.reveal_path(&dir);
        }
    }

    /// The answer to the question above the page. Allowed, the command
    /// that was waiting on it runs.
    fn answer_extension_ask(&mut self, extension: &str, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        let waiting = crate::extensions::current_ask(cx).filter(|a| a.extension == extension).and_then(|a| a.command);
        crate::extensions::answer(extension, allow, cx);
        if allow && let Some(command) = waiting {
            self.run_extension_command(extension, &command, window, cx);
        }
        cx.notify();
    }

    /// Settings ▸ Extensions…: each extension, what it runs and whether it
    /// may; choosing one allows or revokes it.
    fn on_show_extensions(&mut self, _: &ShowExtensions, window: &mut Window, cx: &mut Context<Self>) {
        use crate::finder::SettingRow;
        let menu = crate::finder::SettingsMenu::new(|cx| {
            let mut rows: Vec<SettingRow> = crate::extensions::list(cx)
                .into_iter()
                .map(|ext| {
                    let allowed = ext.trust == crate::extensions::Trust::Allowed;
                    SettingRow {
                        label: format!("{} {} · {}", ext.name, ext.version, ext.command_line).into(),
                        // (A no-break space: values are drawn as keycaps, one per word.)
                        value: Some(if allowed { "Allowed" } else { "Not\u{a0}allowed" }.into()),
                        apply: Box::new(move |_, cx| crate::extensions::answer(&ext.id, !allowed, cx)),
                    }
                })
                .collect();
            rows.push(SettingRow {
                label: "Open Extensions Folder".into(),
                value: None,
                apply: Box::new(|window, cx| window.dispatch_action(Box::new(OpenExtensionsFolder), cx)),
            });
            rows.push(SettingRow {
                label: "Reload Extensions".into(),
                value: Some(format!("{} loaded", crate::extensions::loaded(cx).len()).into()),
                apply: Box::new(|window, cx| window.dispatch_action(Box::new(ReloadExtensions), cx)),
            });
            rows
        })
        .titled("Extensions");
        self.show_picker(menu, window, cx);
    }

    fn on_reload_extensions(&mut self, _: &ReloadExtensions, _: &mut Window, cx: &mut Context<Self>) {
        crate::extensions::init(cx);
        self.show_extension_problems(cx);
        let n = crate::extensions::loaded(cx).len();
        if let Some(view) = self.active_view().cloned() {
            view.update(cx, |nb, cx| nb.flash(format!("{n} extension{} loaded", if n == 1 { "" } else { "s" }), cx));
        }
    }

    /// Extensions that couldn't load say why, above the page.
    pub(crate) fn show_extension_problems(&mut self, cx: &mut Context<Self>) {
        let problems = crate::extensions::host(cx).map(|h| h.read(cx).problems.clone()).unwrap_or_default();
        if !problems.is_empty() {
            self.open_problem = Some(OpenProblem {
                title: format!("{} extension{} couldn't load", problems.len(), if problems.len() == 1 { "" } else { "s" }).into(),
                detail: problems.join("\n").into(),
                path: crate::extensions::folder(),
                fixes: Vec::new(),
            });
            cx.notify();
        }
    }

    /// What ⌘I asks about, and its name: the notebook or table in front, else
    /// the folder in the sidebar, else home.
    fn agent_subject(&self, cx: &gpui_kit::App) -> (crate::agent::Subject, SharedString) {
        use crate::agent::Subject;
        match self.active_tab().map(|t| &t.page) {
            Some(Page::Notebook(view)) => (Subject::Notebook(view.downgrade()), view.read(cx).title().into()),
            Some(Page::Frame(view)) => (Subject::Table(view.downgrade()), view.read(cx).title().into()),
            None => {
                let folder = self.root.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_default();
                let name = folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
                (Subject::Folder(folder), name.into())
            }
        }
    }

    /// ⌘I: the Ask box, over whatever is in front.
    fn on_ask_agent(&mut self, _: &AskAgent, window: &mut Window, cx: &mut Context<Self>) {
        let (subject, about) = self.agent_subject(cx);
        let (current, follow_up) = match &self.agent {
            Some((conv, _)) => {
                let conv = conv.read(cx);
                (Some(conv.provider), conv.is_about(&subject) && !conv.is_working())
            }
            None => (None, false),
        };
        let return_focus = window.focused(cx);
        let ws = cx.entity().downgrade();
        let sheet = cx.new(|cx| crate::agent::ask::AskBox::new(ws, about, subject.sees(), current, follow_up, window, cx));
        self.show_sheet(sheet, return_focus, window, cx);
    }

    /// ⌘K with a question typed in: ask the preferred assistant.
    fn on_ask_agent_text(&mut self, action: &AskAgentText, window: &mut Window, cx: &mut Context<Self>) {
        let preferred = crate::settings::assistant().and_then(|id| crate::agent::Provider::from_id(&id));
        match crate::agent::Provider::installed(preferred).first() {
            Some(&provider) => self.ask_agent(provider, action.0.clone(), window, cx),
            None => self.on_ask_agent(&AskAgent, window, cx),
        }
    }

    /// Ask `provider` about what's in front: a follow-up when the
    /// conversation in the corner is about it, else a new conversation.
    pub fn ask_agent(&mut self, provider: crate::agent::Provider, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (subject, title) = self.agent_subject(cx);
        let reuse = self.agent.as_ref().is_some_and(|(conv, _)| {
            let conv = conv.read(cx);
            conv.is_about(&subject) && conv.provider == provider && !conv.is_working()
        });
        if !reuse {
            let conv = cx.new(|cx| crate::agent::Conversation::new(subject, title, provider, window, cx));
            let subscription = cx.subscribe(&conv, |this, _, _: &crate::agent::Closed, cx| {
                this.agent = None;
                cx.notify();
            });
            self.agent = Some((conv, subscription));
        }
        if let Some((conv, _)) = &self.agent {
            conv.update(cx, |conv, cx| conv.ask(text, window, cx));
        }
        cx.notify();
    }

    fn on_open_link(&mut self, _: &OpenLink, window: &mut Window, cx: &mut Context<Self>) {
        self.show_link_sheet(None, window, cx);
    }

    fn on_join_live(&mut self, _: &JoinLive, window: &mut Window, cx: &mut Context<Self>) {
        self.show_link_sheet(None, window, cx);
    }

    /// A notebook downloaded from a link, in a new tab named after the file.
    pub fn open_fetched(&mut self, fetched: crate::remote::Fetched, window: &mut Window, cx: &mut Context<Self>) {
        let doc = Document::from_notebook(fetched.notebook);
        let view = cx.new(|cx| NotebookView::new(doc, window, cx));
        view.update(cx, |nb, cx| nb.set_origin(fetched.name, fetched.url, cx));
        self.next_tab_is_preview = false;
        self.add_tab(view, window, cx);
    }

    /// A link in `from` points at a notebook on the web: go to its tab if
    /// one has it open, or download it into a new one. Failures stay above
    /// `from`, where the link was.
    fn open_web_notebook(&mut self, link: String, from: Entity<NotebookView>, window: &mut Window, cx: &mut Context<Self>) {
        let (link, anchor) = match link.split_once('#') {
            Some((link, anchor)) => (link.to_string(), Some(anchor.to_string())),
            None => (link, None),
        };
        let raw = crate::remote::raw_url(&link).ok();
        let open = self
            .tabs
            .iter()
            .position(|t| raw.is_some() && t.notebook().is_some_and(|v| v.read(cx).source_url() == raw.as_deref()));
        if let Some(ix) = open {
            self.activate(ix, window, cx);
            if let (Some(anchor), Some(view)) = (anchor, self.active_view().cloned()) {
                view.update(cx, |nb, cx| nb.jump_to_anchor(&anchor, window, cx));
            }
            return;
        }
        let name = raw.as_deref().map(crate::remote::file_name).unwrap_or_default();
        from.update(cx, |nb, cx| nb.flash(format!("Downloading {name}…"), cx));
        cx.spawn_in(window, async move |this, cx| {
            let fetched = cx.background_spawn({
                let link = link.clone();
                async move { crate::remote::fetch(&link) }
            })
            .await;
            this.update_in(cx, |this, window, cx| match fetched {
                Ok(fetched) => {
                    this.open_fetched(fetched, window, cx);
                    if let (Some(anchor), Some(view)) = (anchor, this.active_view().cloned()) {
                        view.update(cx, |nb, cx| nb.jump_to_anchor(&anchor, window, cx));
                    }
                }
                Err(err) => from.update(cx, |nb, cx| nb.report_link_error(format!("Couldn't open {link}"), err, cx)),
            })
            .ok();
        })
        .detach();
    }

    /// A joined session's notebook, in a new tab of its own.
    pub fn open_live(&mut self, joined: herse_collab::Joined, window: &mut Window, cx: &mut Context<Self>) {
        let doc = Document::from_notebook(joined.notebook);
        let view = cx.new(|cx| NotebookView::new(doc, window, cx));
        view.update(cx, |nb, cx| nb.attach_live(joined.session, window, cx));
        self.next_tab_is_preview = false;
        self.add_tab(view, window, cx);
    }

    /// Share: publish the active notebook to a link, and its comments.
    fn on_share(&mut self, _: &Share, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else { return };
        let return_focus = window.focused(cx);
        let sheet = cx.new(|cx| crate::share_sheet::ShareSheet::new(view, window, cx));
        self.show_sheet(sheet, return_focus, window, cx);
    }

    fn show_picker<D: crate::picker::PickerDelegate>(&mut self, delegate: D, window: &mut Window, cx: &mut Context<Self>) {
        self.show_picker_with(delegate, None, window, cx);
    }

    fn show_picker_with<D: crate::picker::PickerDelegate>(
        &mut self,
        delegate: D,
        query: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A list opened from another (Settings ▸ Extensions…) gives focus back
        // to where the first was opened from.
        let return_focus = self.overlay.take().and_then(|o| o.return_focus).or_else(|| window.focused(cx));
        let picker = cx.new(|cx| Picker::new(delegate, query, window, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, _: &gpui_kit::DismissEvent, window, cx| {
            this.close_overlay(window, cx);
        });
        self.overlay = Some(Overlay {
            view: picker.into(),
            seq: next_overlay_seq(),
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = self.overlay.take() {
            // A command may already have moved focus somewhere meaningful.
            let picker_had_focus = window.focused(cx).is_none_or(|f| overlay.return_focus.as_ref() != Some(&f));
            if picker_had_focus && let Some(focus) = overlay.return_focus {
                focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    fn on_go_to_file(&mut self, _: &GoToFile, window: &mut Window, cx: &mut Context<Self>) {
        let mut paths = self.recent.clone();
        for p in self.sidebar.read(cx).notebook_paths() {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        let this = cx.entity().downgrade();
        let recent = self.recent.clone();
        let finder = FileFinder::new(self.root.clone(), paths, &recent, move |path, window, cx| {
            this.update(cx, |ws, cx| ws.open_path(&path, window, cx)).ok();
        });
        self.show_picker(finder, window, cx);
    }

    fn on_find_in_workspace(&mut self, _: &FindInWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let search = WorkspaceSearch::new(self.root.clone(), move |path, cell, query, window, cx| {
            this.update(cx, |ws, cx| {
                ws.open_path(&path, window, cx);
                if let Some(view) = ws.active_view().cloned() {
                    view.update(cx, |nb, cx| nb.reveal_search(cell, &query, window, cx));
                }
            })
            .ok();
        });
        self.show_picker(search, window, cx);
    }

    fn on_change_kernel(&mut self, _: &ChangeKernel, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else { return };
        let picker = KernelPicker::new(view, cx);
        self.show_picker(picker, window, cx);
    }

    fn on_command_palette(&mut self, _: &crate::actions::CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        let focus = window.focused(cx);
        let palette = crate::finder::CommandPalette::new(focus, window);
        self.show_picker(palette, window, cx);
    }

    fn on_go_to_heading(&mut self, _: &GoToHeading, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else { return };
        let outline = OutlinePicker::new(view, cx);
        self.show_picker(outline, window, cx);
    }

    fn set_zoom(&mut self, z: f32, window: &mut Window, cx: &mut Context<Self>) {
        crate::theme::set_zoom(z);
        crate::settings::set_zoom(crate::theme::zoom());
        for tab in &self.tabs {
            match &tab.page {
                Page::Notebook(view) => view.update(cx, |v, cx| v.zoom_changed(cx)),
                Page::Frame(view) => view.update(cx, |v, cx| v.zoom_changed(cx)),
            }
        }
        window.refresh();
    }

    fn on_zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(crate::theme::zoom() + 0.1, window, cx);
    }

    fn on_zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(crate::theme::zoom() - 0.1, window, cx);
    }

    fn on_zoom_reset(&mut self, _: &ZoomReset, window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(1.0, window, cx);
    }

    fn on_toggle_outline(&mut self, _: &ToggleOutline, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        let next = match self.sidebar.read(cx).mode() {
            crate::sidebar::Mode::Files => crate::sidebar::Mode::Outline,
            crate::sidebar::Mode::Outline => crate::sidebar::Mode::Files,
        };
        self.sidebar.update(cx, |s, cx| s.set_mode(next, cx));
        cx.notify();
    }

    fn on_reveal(&mut self, _: &RevealInFinder, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.active_tab().and_then(|t| t.path(cx).cloned()) {
            cx.reveal_path(&path);
        }
    }

    // ---------------------------------------------------------------- actions

    fn on_open(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open(true, window, cx);
    }

    fn on_open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open(false, window, cx);
    }

    fn prompt_open(&mut self, files: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files,
            directories: true,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |this, window, cx| {
                    for path in paths {
                        this.open_path(&path, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn on_new(&mut self, _: &NewNotebook, window: &mut Window, cx: &mut Context<Self>) {
        let doc = Document::untitled("python3", "Python 3", "python");
        let view = cx.new(|cx| NotebookView::new(doc, window, cx));
        self.add_tab(view, window, cx);
    }

    fn on_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else { return };
        if view.read(cx).path().is_none() {
            self.save_as(view, window, cx);
        } else {
            view.update(cx, |v, cx| v.save_now(true, cx));
        }
    }

    fn on_save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_view().cloned() {
            self.save_as(view, window, cx);
        }
    }

    fn save_as(&mut self, view: Entity<NotebookView>, window: &mut Window, cx: &mut Context<Self>) {
        let dir = view
            .read(cx)
            .doc
            .directory()
            .map(Path::to_path_buf)
            .or_else(|| self.root.clone())
            .or_else(dirs_home)
            .unwrap_or_else(|| PathBuf::from("."));
        let name = view.read(cx).title();
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut path))) = rx.await {
                if path.extension().is_none() {
                    path.set_extension("ipynb");
                }
                view.update(cx, |v, cx| v.save_as(path, cx));
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        })
        .detach();
    }

    fn on_close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_tabs(window, cx);
    }

    fn on_close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.close_other_tabs(ix, window, cx);
        }
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.lightbox.is_some() {
            self.close_lightbox(window, cx);
            return;
        }
        match self.active {
            Some(ix) => self.close_tab(ix, window, cx),
            // The Mac convention: Cmd-W with nothing left to close closes the window.
            None => window.remove_window(),
        }
    }

    fn close_lightbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lightbox = None;
        if let Some(view) = self.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    // ------------------------------------------------------------ menus

    /// The Edit menu's verbs go to whatever has the keyboard: a text field
    /// gets the text action; a notebook in command mode, its cell action.
    fn edit_verb(&mut self, text: Box<dyn gpui_kit::Action>, cells: Option<Box<dyn gpui_kit::Action>>, window: &mut Window, cx: &mut Context<Self>) {
        let stack = window.context_stack();
        if stack.iter().any(|c| c.contains("Input")) {
            window.dispatch_action(text, cx);
        } else if let Some(cells) = cells
            && stack.iter().any(|c| c.contains(NOTEBOOK_COMMAND))
        {
            window.dispatch_action(cells, cx);
        } else {
            cx.propagate();
        }
    }

    /// Edit ▸ Undo (and ⌘Z wherever nothing nearer claims it): whatever is
    /// in front takes back its own last change. A text field its typing; a
    /// cell's editor its typing, then the notebook's history; the notebook,
    /// a table's view, the file tree each their own; with nothing in front,
    /// the last file operation.
    fn on_edit_undo(&mut self, _: &EditUndo, window: &mut Window, cx: &mut Context<Self>) {
        self.undo_or_redo(true, window, cx);
    }
    fn on_edit_redo(&mut self, _: &EditRedo, window: &mut Window, cx: &mut Context<Self>) {
        self.undo_or_redo(false, window, cx);
    }

    fn undo_or_redo(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let stack = window.context_stack();
        let has = |key: &str| stack.iter().any(|c| c.contains(key));
        let action: Option<Box<dyn gpui_kit::Action>> = if has("NotebookEdit") {
            Some(if undo { Box::new(EditorUndo) } else { Box::new(EditorRedo) })
        } else if has("Input") {
            Some(if undo { Box::new(gpui_kit::component::input::Undo) } else { Box::new(gpui_kit::component::input::Redo) })
        } else if has(NOTEBOOK_COMMAND) {
            Some(if undo { Box::new(UndoCellOp) } else { Box::new(RedoCellOp) })
        } else if has(crate::frame_view::FRAME) {
            // The table's view (sort, filters, search, widths), by name; never
            // on to the files behind it.
            if let Ok(action) = cx.build_action(if undo { "frame::UndoView" } else { "frame::RedoView" }, None) {
                window.dispatch_action(action, cx);
            }
            return;
        } else {
            None
        };
        if let Some(action) = action {
            window.dispatch_action(action, cx);
            return;
        }
        // The file tree, or nothing in front: files renamed, duplicated or
        // trashed from the sidebar.
        self.sidebar.update(cx, |s, cx| if undo { s.undo_file_op(cx) } else { s.redo_file_op(cx) });
    }
    fn on_edit_cut(&mut self, _: &EditCut, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_verb(Box::new(gpui_kit::component::input::Cut), Some(Box::new(CommandCut)), window, cx);
    }
    fn on_edit_copy(&mut self, _: &EditCopy, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_verb(Box::new(gpui_kit::component::input::Copy), Some(Box::new(CommandCopy)), window, cx);
    }
    fn on_edit_paste(&mut self, _: &EditPaste, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_verb(Box::new(gpui_kit::component::input::Paste), Some(Box::new(CommandPaste)), window, cx);
    }
    fn on_edit_select_all(&mut self, _: &EditSelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_verb(Box::new(gpui_kit::component::input::SelectAll), Some(Box::new(SelectAllCells)), window, cx);
    }

    fn on_show_emoji(&mut self, _: &ShowEmoji, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    fn on_toggle_full_screen(&mut self, _: &ToggleFullScreen, window: &mut Window, _: &mut Context<Self>) {
        window.toggle_fullscreen();
    }

    fn on_show_about(&mut self, _: &ShowAbout, window: &mut Window, cx: &mut Context<Self>) {
        drop(self.ask(
            Some(crate::dialog::Icon::Mark),
            "Herse",
            Some(format!("Version {}\nA fast, native home for Jupyter notebooks.", env!("CARGO_PKG_VERSION")).into()),
            vec![("OK".into(), crate::dialog::Role::Default)],
            window,
            cx,
        ));
    }

    fn on_open_recent(&mut self, action: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        let path = action.0.clone();
        if path.is_dir() {
            self.set_root(path, window, cx);
        } else {
            self.open_path(&path, window, cx);
        }
    }

    fn on_clear_recent(&mut self, _: &ClearRecent, _: &mut Window, cx: &mut Context<Self>) {
        let store = crate::recents::get(cx);
        store.recents.folders.clear();
        store.recents.notebooks.clear();
        store.save();
        cx.defer(crate::set_menus);
    }

    /// ⌘K: commands, notebooks and headings in one list.
    fn on_command_menu(&mut self, _: &CommandMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.show_command_menu(crate::finder::MenuScope::All, window, cx);
    }

    /// ?: every command and its keys, filterable.
    fn on_show_shortcuts(&mut self, _: &ShowShortcuts, window: &mut Window, cx: &mut Context<Self>) {
        self.show_command_menu(crate::finder::MenuScope::Commands, window, cx);
    }

    fn show_command_menu(&mut self, scope: crate::finder::MenuScope, window: &mut Window, cx: &mut Context<Self>) {
        let focus = window.focused(cx);
        let tabs: Vec<PathBuf> = self.tabs.iter().filter_map(|t| t.path(cx).cloned()).collect();
        let mut recent = self.recent.clone();
        recent.extend(crate::recents::get(cx).recents.notebooks.iter().cloned());
        let files = self.sidebar.read(cx).notebook_paths();
        let this = cx.entity().downgrade();
        let menu = crate::finder::CommandMenu::new(
            scope,
            &tabs,
            &recent,
            &files,
            self.root.as_deref(),
            self.active_view().cloned(),
            focus,
            window,
            cx,
            move |path, window, cx| {
                this.update(cx, |ws, cx| ws.open_path(&path, window, cx)).ok();
            },
        );
        self.show_picker(menu, window, cx);
    }

    /// ⌘,: the preferences, as a list you choose from.
    fn on_show_settings(&mut self, _: &ShowSettings, window: &mut Window, cx: &mut Context<Self>) {
        use crate::finder::SettingRow;
        let ws = cx.entity().downgrade();
        let menu = crate::finder::SettingsMenu::new(move |cx| {
            let check = |on: bool| on.then(|| SharedString::from("✓"));
            let theme = crate::settings::dark_theme();
            let zoom = (crate::theme::zoom() * 100.).round() as i32;
            let lines = crate::settings::line_numbers();
            let (w1, w2, w3, w4, w5, w6) = (ws.clone(), ws.clone(), ws.clone(), ws.clone(), ws.clone(), ws.clone());
            let blend = crate::settings::blend_charts();
            let auto_install = crate::settings::auto_install();
            let _ = cx;
            vec![
                SettingRow {
                    label: "Appearance: Follow System".into(),
                    value: check(theme.is_none()),
                    apply: Box::new(|window, cx| {
                        crate::settings::follow_system_theme();
                        Theme::sync_system_appearance(Some(window), cx);
                    }),
                },
                SettingRow {
                    label: "Appearance: Light".into(),
                    value: check(theme == Some(false)),
                    apply: Box::new(|window, cx| {
                        crate::settings::set_dark_theme(false);
                        Theme::change(ThemeMode::Light, Some(window), cx);
                    }),
                },
                SettingRow {
                    label: "Appearance: Dark".into(),
                    value: check(theme == Some(true)),
                    apply: Box::new(|window, cx| {
                        crate::settings::set_dark_theme(true);
                        Theme::change(ThemeMode::Dark, Some(window), cx);
                    }),
                },
                SettingRow {
                    label: "Line Numbers in Code Cells".into(),
                    value: Some(if lines { "On" } else { "Off" }.into()),
                    apply: Box::new(move |window, cx| {
                        w1.update(cx, |ws, cx| {
                            if let Some(view) = ws.active_view().cloned() {
                                view.update(cx, |nb, cx| nb.on_toggle_all_line_numbers(&ToggleAllLineNumbers, window, cx));
                            } else {
                                crate::settings::set_line_numbers(!crate::settings::line_numbers());
                            }
                        })
                        .ok();
                    }),
                },
                SettingRow {
                    label: "Text Size: Larger".into(),
                    value: Some(format!("{zoom}%").into()),
                    apply: Box::new(move |window, cx| {
                        w2.update(cx, |ws, cx| ws.set_zoom(crate::theme::zoom() + 0.1, window, cx)).ok();
                    }),
                },
                SettingRow {
                    label: "Text Size: Smaller".into(),
                    value: None,
                    apply: Box::new(move |window, cx| {
                        w3.update(cx, |ws, cx| ws.set_zoom(crate::theme::zoom() - 0.1, window, cx)).ok();
                    }),
                },
                SettingRow {
                    label: "Text Size: Actual Size".into(),
                    value: check(zoom == 100),
                    apply: Box::new(move |window, cx| {
                        w4.update(cx, |ws, cx| ws.set_zoom(1.0, window, cx)).ok();
                    }),
                },
                SettingRow {
                    label: "Charts: Blend with the Page".into(),
                    value: Some(if blend { "On" } else { "Off" }.into()),
                    apply: Box::new(move |_, cx| {
                        crate::settings::set_blend_charts(!crate::settings::blend_charts());
                        w6.update(cx, |ws, cx| {
                            for view in ws.tabs.iter().filter_map(Tab::notebook) {
                                view.update(cx, |nb, cx| nb.restyle_outputs(cx));
                            }
                        })
                        .ok();
                    }),
                },
                SettingRow {
                    label: "Extensions…".into(),
                    value: Some(format!("{} loaded", crate::extensions::loaded(cx).len()).into()),
                    apply: Box::new(|window, cx| window.dispatch_action(Box::new(ShowExtensions), cx)),
                },
                SettingRow {
                    label: "Code Completion".into(),
                    value: Some(if crate::settings::complete_as_you_type() { "As You Type" } else { "On Tab" }.into()),
                    apply: Box::new(|_, _| crate::settings::set_complete_as_you_type(!crate::settings::complete_as_you_type())),
                },
                SettingRow {
                    // ⌘I asks this one; choosing the row moves to the next installed.
                    label: "Assistant (⌘I)".into(),
                    value: Some(
                        crate::agent::Provider::installed(
                            crate::settings::assistant().and_then(|id| crate::agent::Provider::from_id(&id)),
                        )
                        .first()
                        .map_or("None installed", |p| p.name())
                        .into(),
                    ),
                    apply: Box::new(|_, _| {
                        let preferred = crate::settings::assistant().and_then(|id| crate::agent::Provider::from_id(&id));
                        let installed = crate::agent::Provider::installed(preferred);
                        if let Some(next) = installed.get(1).or(installed.first()) {
                            crate::settings::set_assistant(next.id());
                        }
                    }),
                },
                SettingRow {
                    label: "Install Missing Packages When Running".into(),
                    value: Some(if auto_install { "On" } else { "Off" }.into()),
                    apply: Box::new(|_, _| crate::settings::set_auto_install(!crate::settings::auto_install())),
                },
                SettingRow {
                    label: "Keyboard Shortcuts…".into(),
                    value: Some("?".into()),
                    apply: Box::new(move |window, cx| {
                        w5.update(cx, |ws, cx| ws.show_command_menu(crate::finder::MenuScope::Commands, window, cx)).ok();
                    }),
                },
            ]
        });
        self.show_picker(menu, window, cx);
    }

    fn on_activate_tab(&mut self, action: &ActivateTab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.tabs.len().checked_sub(1) else { return };
        // ⌘9 is the last tab; ⌘1–8 past the end do nothing.
        let ix = if action.0 == usize::MAX { last } else { action.0 };
        self.activate(ix, window, cx);
    }

    /// ⌘⇧T: open the most recently closed tab's notebook again.
    fn on_reopen_closed_tab(&mut self, _: &ReopenClosedTab, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(path) = self.closed.pop() {
            if path.exists() {
                self.open_path(&path, window, cx);
                return;
            }
        }
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active
            && !self.tabs.is_empty()
        {
            self.activate((ix + 1) % self.tabs.len(), window, cx);
        }
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active
            && !self.tabs.is_empty()
        {
            self.activate((ix + self.tabs.len() - 1) % self.tabs.len(), window, cx);
        }
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn on_light_mode(&mut self, _: &LightMode, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::set_dark_theme(false);
        Theme::change(ThemeMode::Light, Some(window), cx);
        window.refresh();
    }

    fn on_dark_mode(&mut self, _: &DarkMode, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::set_dark_theme(true);
        Theme::change(ThemeMode::Dark, Some(window), cx);
        window.refresh();
    }

    fn on_system_appearance(&mut self, _: &SystemAppearance, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::follow_system_theme();
        Theme::sync_system_appearance(Some(window), cx);
        window.refresh();
    }

    fn on_toggle_theme(&mut self, _: &ToggleTheme, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        crate::settings::set_dark_theme(matches!(mode, ThemeMode::Dark));
        Theme::change(mode, Some(window), cx);
        window.refresh();
    }

    // ---------------------------------------------------------------- rendering

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let mut row = div().flex().flex_row().items_center().h_full().gap(px(2.)).min_w_0().overflow_hidden();
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = self.active == Some(ix);
            let dirty = tab.dirty(cx);
            let title: SharedString = tab.title(cx).into();
            let preview = tab.preview;
            let path = tab.path(cx).cloned();
            let group: SharedString = format!("tab-{ix}").into();
            let ws = cx.entity().downgrade();
            let others = self.tabs.len() > 1;
            let has_right = ix + 1 < self.tabs.len();
            let menu = move |menu: gpui_kit::component::menu::PopupMenu, _: &mut Window, _: &mut gpui_kit::Context<gpui_kit::component::menu::PopupMenu>| {
                use gpui_kit::component::menu::PopupMenuItem;
                let close = ws.clone();
                let close_others = ws.clone();
                let close_right = ws.clone();
                let close_all = ws.clone();
                let menu = menu
                    .item(PopupMenuItem::new("Close Tab").on_click(move |_, window, cx| {
                        close.update(cx, |this, cx| this.close_tab(ix, window, cx)).ok();
                    }))
                    .item(PopupMenuItem::new("Close Other Tabs").disabled(!others).on_click(move |_, window, cx| {
                        close_others.update(cx, |this, cx| this.close_other_tabs(ix, window, cx)).ok();
                    }))
                    .item(PopupMenuItem::new("Close Tabs to the Right").disabled(!has_right).on_click(move |_, window, cx| {
                        close_right.update(cx, |this, cx| this.close_tabs_to_the_right(ix, window, cx)).ok();
                    }))
                    .item(PopupMenuItem::new("Close All Tabs").on_click(move |_, window, cx| {
                        close_all.update(cx, |this, cx| this.close_all_tabs(window, cx)).ok();
                    }));
                match path.clone() {
                    Some(path) => {
                        let reveal = path.clone();
                        menu.separator()
                            .item(PopupMenuItem::new("Reveal in Finder").on_click(move |_, _, cx| cx.reveal_path(&reveal)))
                            .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path.display().to_string()));
                            }))
                    }
                    None => menu,
                }
            };
            // The active tab is the same white as the page card below it.
            row = row.child(
                div()
                    .id(("tab", ix))
                    .group(group.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .h(theme::TAB_HEIGHT)
                    .pl(px(12.))
                    .pr(px(5.))
                    .rounded(px(7.))
                    .border_1()
                    .text_size(px(12.5))
                    .cursor_pointer()
                    // A click here is a tab click, never the start of a window drag.
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when(active, |d| {
                        d.bg(theme.background).border_color(theme.border).text_color(theme.foreground).shadow_xs()
                    })
                    .when(!active, |d| {
                        d.border_color(gpui_kit::transparent_black())
                            .text_color(theme.muted_foreground)
                            .hover(|s| s.bg(theme.foreground.opacity(0.04)).text_color(theme.foreground))
                    })
                    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                        if ev.click_count() >= 2
                            && let Some(tab) = this.tabs.get_mut(ix)
                        {
                            tab.preview = false;
                        }
                        this.activate(ix, window, cx)
                    }))
                    .context_menu(menu)
                    .child(div().max_w(px(200.)).truncate().when(preview, |d| d.italic()).child(title))
                    .child(
                        div()
                            .id(("tab-close", ix))
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .text_color(theme.muted_foreground)
                            // Quiet until needed: on the active tab, on hover, or as
                            // the unsaved dot.
                            .when(!active && !dirty, |d| d.opacity(0.).group_hover(group, |s| s.opacity(1.)))
                            .hover(|s| s.bg(theme.foreground.opacity(0.07)).text_color(theme.foreground))
                            .child(if dirty {
                                div().size(px(6.)).rounded_full().bg(theme.muted_foreground).into_any_element()
                            } else {
                                gpui_kit::svg().path("icons/close.svg").size(px(12.)).text_color(theme.muted_foreground).into_any_element()
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx)
                            })),
                    ),
            );
        }
        row
    }

    /// Run All, Interrupt (while something runs) and Restart, at the right
    /// of the tab strip: the notebook's commands within reach of the mouse.
    fn render_notebook_actions(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let Some(view) = self.active_view().cloned() else {
            // A table, or nothing open: the assistant answers about that too.
            return Some(
                div()
                    .ml_auto()
                    .pl(px(8.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .child(
                        crate::ui::icon_button("ask", "icons/sparkles.svg", false, cx)
                            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Ask Claude or Codex  ⌘I").build(window, cx))
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(AskAgent), cx)),
                    )
                    .into_any_element(),
            );
        };
        let status = view.read(cx).kernel_state().and_then(|(_, status)| status);
        let busy = matches!(status, Some(herse_kernel::KernelStatus::Busy));
        let button = |id: &'static str, icon: &'static str, tip: &'static str, action: Box<dyn gpui_kit::Action>, cx: &mut Context<Self>| {
            let view = view.clone();
            crate::ui::icon_button(id, icon, false, cx)
                // A press here is a click, never the start of a window drag.
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .on_click(move |_, window, cx| {
                    // The notebook handles its own commands: send it there.
                    view.read(cx).focus_handle(cx).focus(window, cx);
                    window.dispatch_action(action.boxed_clone(), cx);
                })
        };
        Some(
            div()
                .ml_auto()
                .pl(px(8.))
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap(px(2.))
                // Live: who's here, as initials in their colors.
                .children(view.read(cx).live().map(|live| {
                    let peers: Vec<(String, gpui_kit::Hsla)> = live
                        .peers
                        .iter()
                        .map(|p| (p.name.chars().next().unwrap_or('?').to_uppercase().collect(), crate::notebook::live::peer_color(&p.color)))
                        .collect();
                    let names: SharedString = format!("Live · {}", live.people()).into();
                    let accent = theme::accent(cx);
                    div()
                        .id("nb-live")
                        .h(crate::ui::CONTROL)
                        .px(px(5.))
                        .mr(px(2.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .rounded(crate::ui::CONTROL_RADIUS)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().foreground.opacity(0.06)))
                        .child(div().size(px(6.)).rounded_full().bg(accent))
                        .children(peers.into_iter().take(4).map(|(initial, color)| {
                            div()
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(color)
                                .text_size(px(10.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(gpui_kit::white())
                                .child(initial)
                        }))
                        .when(live.peers.len() > 4, |d| {
                            d.child(div().text_size(px(11.)).text_color(cx.theme().muted_foreground).child(format!("+{}", live.peers.len() - 4)))
                        })
                        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(names.clone()).build(window, cx))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(Share), cx))
                }))
                // The assistant, where it can be found: ⌘I without the shortcut.
                .child(button("nb-ask", "icons/sparkles.svg", "Ask Claude or Codex  ⌘I", Box::new(AskAgent), cx))
                .child({
                    let comments = view.read(cx).open_comment_count();
                    let view = view.clone();
                    div()
                        .id("nb-comments")
                        .h(crate::ui::CONTROL)
                        .px(px(5.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.))
                        .rounded(crate::ui::CONTROL_RADIUS)
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().foreground.opacity(0.06)))
                        .child(gpui_kit::svg().path("icons/message-square.svg").size(crate::ui::ICON).text_color(cx.theme().muted_foreground))
                        .when(comments > 0, |d| d.child(comments.to_string()))
                        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Comments  ⌥⌘M to add").build(window, cx))
                        .on_click(move |_, window, cx| {
                            view.read(cx).focus_handle(cx).focus(window, cx);
                            window.dispatch_action(Box::new(ToggleComments), cx);
                        })
                })
                .child(
                    crate::ui::icon_button("nb-share", "icons/share-2.svg", false, cx)
                        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Share…").build(window, cx))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(Share), cx)),
                )
                .child(button("nb-run-all", "icons/fast-forward.svg", "Run All", Box::new(RunAll), cx))
                .when(busy, |d| {
                    d.child(button("nb-interrupt", "icons/square.svg", "Interrupt  I I", Box::new(Interrupt), cx))
                })
                .when(status.is_some(), |d| {
                    d.child(button("nb-restart", "icons/rotate-ccw.svg", "Restart Kernel  0 0", Box::new(Restart), cx))
                })
                .into_any_element(),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let accent = theme::accent(cx);
        let mut left = div().flex().flex_none().flex_row().items_center().gap(px(12.));
        let mut right = div().flex().flex_1().min_w_0().flex_row().items_center().justify_end().gap(px(14.));
        if let Some(view) = self.active_view() {
            let v = view.read(cx);
            let n = v.cell_count();
            if v.is_editing() {
                // Live: editing wears the accent, like the cell's outline.
                left = left.child(
                    div()
                        .px(px(6.))
                        .rounded(px(4.))
                        .bg(accent.opacity(0.12))
                        .text_color(accent)
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child("Edit"),
                );
            }
            // How big and how fast: one hover away, not always on screen.
            let detail: Option<SharedString> = v.doc.load_stats.map(|stats| {
                format!("{} · opened in {:.1} ms", human_bytes(stats.bytes), stats.read_ms + stats.parse_ms).into()
            });
            left = left.child(
                div()
                    .id("cell-position")
                    .child(if n == 0 {
                        "No cells".to_string()
                    } else {
                        format!("Cell {} of {}", (v.selected_index() + 1).min(n), n)
                    })
                    .when_some(detail, |d, detail| {
                        d.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(detail.clone()).build(window, cx))
                    }),
            );
            // Opened from a link: where from, and a way back to its page.
            if let Some(url) = v.source_url() {
                let page = crate::remote::page_url(url);
                let tip: SharedString = format!("Opened from {page}. Save keeps a copy on this Mac.").into();
                left = left.child(
                    div()
                        .id("source-url")
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.))
                        .px(px(6.))
                        .h(px(20.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground).bg(theme.foreground.opacity(0.05)))
                        .child(gpui_kit::svg().path("icons/link.svg").size(px(11.)).flex_none().text_color(theme.muted_foreground))
                        .child(div().min_w_0().truncate().child(crate::remote::origin_label(url)))
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .on_click(move |_, _, cx| cx.open_url(&page)),
                );
            }
            // In a git repository: how many cells changed since the last
            // commit (nothing when none); a click goes to the next one.
            if let Some((n, commit)) = v.change_summary().filter(|(n, _)| *n > 0) {
                let tip: SharedString =
                    format!("Cells changed since commit {commit}. F7 goes to the next change, Shift-F7 to the previous.").into();
                left = left.child(
                    div()
                        .id("changes")
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.))
                        .px(px(6.))
                        .h(px(20.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground).bg(theme.foreground.opacity(0.05)))
                        .child(gpui_kit::svg().path("icons/git-compare.svg").size(px(11.)).text_color(theme.muted_foreground))
                        .child(if n == 1 { "1 change".to_string() } else { format!("{n} changes") })
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(view) = this.active_view().cloned() {
                                view.update(cx, |nb, cx| nb.step_change(true, window, cx));
                            }
                        })),
                );
            }
            if let Some(status) = &v.status {
                right = right.child(div().min_w_0().truncate().child(status.clone()));
            }
            if let Some((name, status)) = v.kernel_state() {
                // Dew when the kernel is live (solid while it works), grey
                // when there is none, red when it died; always with a word.
                let (label, dot) = match status {
                    None => ("not started", theme.muted_foreground.opacity(0.35)),
                    Some(herse_kernel::KernelStatus::Idle) => ("idle", accent.opacity(0.55)),
                    Some(herse_kernel::KernelStatus::Busy) => ("busy", accent),
                    Some(herse_kernel::KernelStatus::Starting) => ("starting", accent.opacity(0.3)),
                    Some(herse_kernel::KernelStatus::Restarting) => ("restarting", accent.opacity(0.3)),
                    Some(herse_kernel::KernelStatus::Dead) => ("dead", theme::error_color(theme.is_dark())),
                };
                right = right.child(
                    div()
                        .id("kernel-status")
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .px(px(6.))
                        .h(px(20.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground).bg(theme.foreground.opacity(0.05)))
                        .child(div().size(px(6.)).rounded_full().bg(dot))
                        .child(format!("{name} · {label}"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_change_kernel(&ChangeKernel, window, cx);
                        })),
                );
            }
        } else if let Some(root) = &self.root {
            let shown = match std::env::var_os("HOME").and_then(|h| root.strip_prefix(h).ok().map(Path::to_path_buf)) {
                Some(rest) => format!("~/{}", rest.display()),
                None => root.display().to_string(),
            };
            left = left.child(shown);
        }
        div()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .justify_between()
            .h(theme::STATUS_HEIGHT)
            .pl(px(12.))
            .pr(px(10.) + theme::CARD_GAP)
            .text_size(px(11.5))
            .text_color(theme.muted_foreground)
            .child(left)
            .child(right)
    }

    /// The notebook that wouldn't open, as a banner above the page: what and
    /// why as selectable text, Copy, and Show in Finder.
    /// A fix from the open-problem banner: open what could be recovered (for
    /// the file, unsaved), or make an empty file a new notebook.
    fn apply_open_fix(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(problem) = self.open_problem.take() else { return };
        let Some(path) = problem.path.clone() else { return };
        match problem.fixes.into_iter().nth(ix) {
            Some(crate::broken::Fix::Open { notebook, note, .. }) => {
                let mut doc = Document::from_notebook(*notebook);
                doc.path = Some(path.clone());
                doc.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                // Nothing touches the broken file until ⌘S says so.
                doc.dirty = true;
                doc.autosave_blocked = true;
                let view = cx.new(|cx| NotebookView::new(doc, window, cx));
                view.update(cx, |nb, cx| nb.flash(note, cx));
                self.add_tab(view, window, cx);
            }
            Some(crate::broken::Fix::StartFresh) => {
                let mut doc = Document::untitled("python3", "Python 3", "python");
                match doc.save(Some(path.clone()), false) {
                    Ok(_) => self.open_path(&path, window, cx),
                    Err(err) => {
                        self.open_problem = Some(OpenProblem {
                            title: "Couldn't write the new notebook".into(),
                            detail: format!("{err:#}").into(),
                            path: Some(path),
                            fixes: Vec::new(),
                        });
                    }
                }
            }
            None => {}
        }
        cx.notify();
    }

    /// An extension asking whether it may run: what it would run and where,
    /// as selectable text, with Allow, Don't Allow and Show in Finder (to
    /// look at it first). Closed, it asks again next time it's needed.
    fn render_extension_ask(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let ask = crate::extensions::current_ask(cx)?;
        let mut detail = format!("in {}", ask.folder_label);
        if ask.changed {
            detail.push_str("\nIt changed since you last answered.");
        }
        let (allow, deny, close) = (ask.extension.clone(), ask.extension.clone(), ask.extension.clone());
        let folder = ask.folder.clone();
        let actions = vec![
            crate::ui::primary_button("extension-ask-allow", "Allow", true, cx)
                .on_click(cx.listener(move |this, _, window, cx| this.answer_extension_ask(&allow, true, window, cx)))
                .into_any_element(),
            crate::ui::button("extension-ask-deny", "Don't Allow", cx)
                .on_click(cx.listener(move |this, _, window, cx| this.answer_extension_ask(&deny, false, window, cx)))
                .into_any_element(),
            crate::ui::button("extension-ask-reveal", "Show in Finder", cx)
                .on_click(move |_, _, cx| cx.reveal_path(&folder))
                .into_any_element(),
        ];
        let banner = crate::ui::banner(
            crate::ui::Tone::Permission,
            "extension-ask",
            format!("{} wants to run: {}", ask.name, ask.command_line).into(),
            detail.into(),
            actions,
            cx.listener(|this, _, _, cx| {
                if let Some(view) = this.active_view().cloned() {
                    view.update(cx, |nb, cx| nb.flash("Copied", cx));
                }
            }),
            move |_, _, cx| {
                if let Some(host) = crate::extensions::host(cx) {
                    host.update(cx, |h, cx| h.dismiss_ask(&close, cx));
                }
            },
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

    fn render_open_problem(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let problem = self.open_problem.as_ref()?;
        let reveal = problem.path.clone().map(|path| {
            crate::ui::button("open-problem-reveal", "Show in Finder", cx)
                .on_click(move |_, _, cx| cx.reveal_path(&path))
                .into_any_element()
        });
        let mut actions: Vec<gpui_kit::AnyElement> = problem
            .fixes
            .iter()
            .enumerate()
            .map(|(ix, fix)| {
                let label: SharedString = match fix {
                    crate::broken::Fix::Open { label, .. } => label.clone().into(),
                    crate::broken::Fix::StartFresh => "Make It a New Notebook".into(),
                };
                let id = ("open-problem-fix", ix);
                let button = if ix == 0 { crate::ui::primary_button(id, label, true, cx) } else { crate::ui::button(id, label, cx) };
                button.on_click(cx.listener(move |this, _, window, cx| this.apply_open_fix(ix, window, cx))).into_any_element()
            })
            .collect();
        actions.extend(reveal);
        let banner = crate::ui::problem_banner(
            "open-problem",
            problem.title.clone(),
            problem.detail.clone(),
            actions,
            cx.listener(|this, _, _, cx| {
                if let Some(view) = this.active_view().cloned() {
                    view.update(cx, |nb, cx| nb.flash("Copied", cx));
                }
            }),
            cx.listener(|this, _, _, cx| {
                this.open_problem = None;
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

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (folders, notebooks) = {
            let store = crate::recents::get(cx);
            let folders: Vec<PathBuf> = store.recents.folders.iter().filter(|p| p.is_dir()).take(4).cloned().collect();
            let notebooks: Vec<PathBuf> = store.recents.notebooks.iter().filter(|p| p.is_file()).take(5).cloned().collect();
            (folders, notebooks)
        };
        // Owned: the row builders below need `cx` mutably for click handlers.
        let theme = cx.theme().clone();
        const WIDTH: f32 = 340.;
        let action_row = |id: &'static str, label: &'static str, keys: &'static str, action: Box<dyn gpui_kit::Action>, cx: &mut Context<Self>| {
            let theme = cx.theme();
            div()
                .id(id)
                .w(px(WIDTH))
                .h(px(32.))
                .px(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme.foreground.opacity(0.04)))
                .child(div().text_color(theme.foreground.opacity(0.85)).child(label))
                .child(crate::picker::keycaps(keys, cx))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let pretty = |p: &Path| -> String {
            match home.as_ref().and_then(|h| p.strip_prefix(h).ok()) {
                Some(rest) => format!("~/{}", rest.display()),
                None => p.display().to_string(),
            }
        };
        let recent_row = |id: (&'static str, usize), path: PathBuf, cx: &mut Context<Self>| {
            let theme = cx.theme();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parent = path.parent().map(pretty).unwrap_or_default();
            div()
                .id(id)
                .w(px(WIDTH))
                .h(px(30.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme.foreground.opacity(0.04)))
                .child(div().flex_none().max_w(px(180.)).truncate().child(name))
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.muted_foreground).child(parent))
                .on_click(cx.listener(move |this, _, window, cx| this.open_path(&path, window, cx)))
        };
        let section = |label: &'static str| {
            div()
                .w(px(WIDTH))
                .px(px(10.))
                .pt(px(20.))
                .pb(px(4.))
                .text_size(px(11.5))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let mut col = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .child(crate::ui::mark(px(48.)).mb(px(14.)))
            // The wordmark is set in the prose face, like a title page.
            .child(
                div()
                    .font_family(theme::prose_font())
                    .text_size(px(26.))
                    .font_weight(gpui_kit::FontWeight::BOLD)
                    .child("Herse"),
            )
            .child(
                div()
                    .pt(px(4.))
                    .pb(px(16.))
                    .text_color(theme.muted_foreground)
                    .child("Read, search and run Jupyter notebooks"),
            );
        col = col
            .child(action_row("welcome-open", "Open a notebook or folder", "⌘O", Box::new(Open), cx))
            .child(action_row("welcome-find", "Go to a notebook", "⌘P", Box::new(GoToFile), cx))
            .child(action_row("welcome-search", "Search every notebook", "⇧⌘F", Box::new(FindInWorkspace), cx))
            .child(action_row("welcome-new", "New notebook", "⌘N", Box::new(NewNotebook), cx));
        if !folders.is_empty() || !notebooks.is_empty() {
            col = col.child(section("Recent"));
        }
        for (i, f) in folders.into_iter().enumerate() {
            col = col.child(recent_row(("recent-folder", i), f, cx));
        }
        for (i, f) in notebooks.into_iter().enumerate() {
            col = col.child(recent_row(("recent-notebook", i), f, cx));
        }
        col
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let identity = match self.active_tab() {
            Some(tab) => (tab.title(cx), tab.dirty(cx), tab.path(cx).cloned()),
            None => ("Herse".to_string(), false, None),
        };
        if self.window_identity.as_ref() != Some(&identity) {
            _window.set_window_title(&identity.0);
            _window.set_window_edited(identity.1);
            _window.set_document_path(identity.2.as_deref());
            self.window_identity = Some(identity);
        }
        if let Some((image, _, _)) = &self.lightbox {
            crate::images::touch(image, cx);
        }
        let theme = cx.theme().clone();
        let content: gpui_kit::AnyElement = match self.active_tab() {
            Some(tab) => tab.element(),
            None => self.render_welcome(cx).into_any_element(),
        };
        let banners: Vec<gpui_kit::AnyElement> =
            [self.render_extension_ask(cx), self.render_open_problem(cx)].into_iter().flatten().collect();
        let content: gpui_kit::AnyElement = if banners.is_empty() {
            content
        } else {
            // The page below the banners keeps to the room left: the welcome
            // screen, centered, would otherwise spill up over them.
            div()
                .size_full()
                .flex()
                .flex_col()
                .children(banners)
                .child(div().flex_1().min_h_0().overflow_hidden().child(content))
                .into_any_element()
        };
        let tabs = self.render_tabs(cx);
        let status = self.render_status(cx);
        div()
            .id("workspace")
            .key_context(WORKSPACE)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .text_color(theme.foreground)
            .font_family(theme::UI_FONT)
            .text_size(theme::UI_SIZE)
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_open_folder))
            .on_action(cx.listener(Self::on_new))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_save_as))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_close_all_tabs))
            .on_action(cx.listener(Self::on_close_other_tabs))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_activate_tab))
            .on_action(cx.listener(Self::on_edit_undo))
            .on_action(cx.listener(Self::on_edit_redo))
            .on_action(cx.listener(Self::on_edit_cut))
            .on_action(cx.listener(Self::on_edit_copy))
            .on_action(cx.listener(Self::on_edit_paste))
            .on_action(cx.listener(Self::on_edit_select_all))
            .on_action(cx.listener(Self::on_show_emoji))
            .on_action(cx.listener(Self::on_toggle_full_screen))
            .on_action(cx.listener(Self::on_show_about))
            .on_action(cx.listener(Self::on_open_recent))
            .on_action(cx.listener(Self::on_clear_recent))
            .on_action(cx.listener(Self::on_command_menu))
            .on_action(cx.listener(Self::on_show_shortcuts))
            .on_action(cx.listener(Self::on_show_settings))
            .on_action(cx.listener(Self::on_share))
            .on_action(cx.listener(Self::on_join_live))
            .on_action(cx.listener(Self::on_open_link))
            .on_action(cx.listener(Self::on_open_link_text))
            .on_action(cx.listener(Self::on_ask_agent))
            .on_action(cx.listener(Self::on_run_extension_command))
            .on_action(cx.listener(Self::on_open_extensions_folder))
            .on_action(cx.listener(Self::on_install_command_line_tool))
            .on_action(cx.listener(Self::on_reload_extensions))
            .on_action(cx.listener(Self::on_show_extensions))
            .on_action(cx.listener(Self::on_ask_agent_text))
            .on_action(cx.listener(Self::on_reopen_closed_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_theme))
            .on_action(cx.listener(Self::on_light_mode))
            .on_action(cx.listener(Self::on_dark_mode))
            .on_action(cx.listener(Self::on_system_appearance))
            .on_action(cx.listener(Self::on_go_to_file))
            .on_action(cx.listener(Self::on_command_palette))
            .on_action(cx.listener(Self::on_go_to_heading))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_toggle_outline))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_reset))
            .on_action(cx.listener(Self::on_find_in_workspace))
            .on_action(cx.listener(Self::on_change_kernel))
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, window, cx| {
                for path in paths.paths() {
                    this.open_path(path, window, cx);
                }
            }))
            .relative()
            // One quiet frame: the sidebar runs the full height (the traffic
            // lights sit at its top), and the content column holds the tab
            // strip, the page as a rounded card, and the status line.
            .flex_row()
            .bg(theme.sidebar)
            .when(self.sidebar_open, |d| {
                d.child(
                    div()
                        .w(theme::SIDEBAR_WIDTH)
                        .flex_none()
                        .h_full()
                        // Cached: scrolling or editing a notebook re-renders the
                        // workspace, but the tree hasn't changed, so GPUI replays
                        // last frame's sidebar instead of laying it out again.
                        .child(self.sidebar.clone().cached(gpui_kit::StyleRefinement::default().size_full())),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        crate::chrome::drag_strip("tab-strip", &self.drag_pending)
                            .h(theme::CHROME_HEIGHT)
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .pr(theme::CARD_GAP)
                            .pl(if self.sidebar_open || _window.is_fullscreen() {
                                px(0.)
                            } else {
                                theme::TRAFFIC_LIGHTS_WIDTH
                            })
                            .child(tabs)
                            .children(self.render_notebook_actions(cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .mr(theme::CARD_GAP)
                            .when(!self.sidebar_open, |d| d.ml(theme::CARD_GAP))
                            .rounded(theme::CARD_RADIUS)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .shadow_xs()
                            .overflow_hidden()
                            .child(content),
                    )
                    .child(status),
            )
            .when_some(self.agent.as_ref().map(|(conv, _)| conv.clone()), |d, conv| {
                // Above the status line, inside the page's corner.
                d.child(
                    div()
                        .absolute()
                        .right(theme::CARD_GAP + px(16.))
                        .bottom(theme::STATUS_HEIGHT + px(16.))
                        .child(conv),
                )
            })
            .when_some(crate::chrome::fake_traffic_lights(), |d, lights| d.child(lights))
            .when_some(self.lightbox.clone(), |d, (image, w, h)| {
                d.child(
                    div()
                        .id("lightbox")
                        .track_focus(&self.lightbox_focus)
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(32.))
                        .bg(gpui_kit::hsla(0., 0., 0., 0.72))
                        .cursor_pointer()
                        .on_key_down(cx.listener(|this, ev: &gpui_kit::KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" || ev.keystroke.key == "space" {
                                this.close_lightbox(window, cx);
                            }
                        }))
                        .on_click(cx.listener(|this, _, window, cx| this.close_lightbox(window, cx)))
                        .child(
                            // Up to 1.5× its natural size (rasters soften beyond), never past the window.
                            gpui_kit::img(image)
                                .w(px(w * 1.5))
                                .max_w_full()
                                .max_h_full()
                                .aspect_ratio(w / h)
                                .object_fit(gpui_kit::ObjectFit::Contain)
                                .rounded(px(4.)),
                        ),
                )
            })
            .when_some(self.overlay.as_ref().map(|o| (o.view.clone(), o.seq)), |d, (view, seq)| {
                use gpui_kit::AnimationExt as _;
                // The page dims under an overlay, so what's in front reads as
                // in front; both arrive in a quick fade, the panel sliding the
                // last few pixels down into place.
                let scrim = gpui_kit::hsla(0., 0., 0., if theme.is_dark() { 0.45 } else { 0.22 });
                let arrive = || gpui_kit::Animation::new(std::time::Duration::from_millis(170)).with_easing(gpui_kit::ease_out_quint());
                d.child(
                    div()
                        .id(("overlay-backdrop", seq))
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(72.))
                        .px(px(16.))
                        .bg(scrim)
                        .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, window, cx| {
                            this.close_overlay(window, cx)
                        }))
                        .child(
                            div()
                                .id("overlay")
                                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(view)
                                .with_animation(("overlay-panel", seq), arrive(), |el, t| el.mt(px(-10. * (1. - t)))),
                        )
                        .with_animation(("overlay-in", seq), arrive(), |el, t| el.opacity(t)),
                )
            })
    }
}

/// A fresh number for each overlay opened.
fn next_overlay_seq() -> usize {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn human_bytes(n: usize) -> String {
    let n = n as f64;
    if n < 1024. {
        format!("{n} B")
    } else if n < 1024. * 1024. {
        format!("{:.1} KB", n / 1024.)
    } else {
        format!("{:.1} MB", n / 1024. / 1024.)
    }
}

/// A path as people write it: `~/…` under the home folder.
pub fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
