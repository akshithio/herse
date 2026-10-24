//! The workspace file tree: folders and notebooks, each notebook annotated
//! with its cell count and kernel. Virtualized, so a 10k-file tree costs the
//! same to draw as a 10-file one.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, UniformListScrollHandle, Window, actions, div, prelude::FluentBuilder as _, px, uniform_list,
};

use crate::theme;

actions!(
    sidebar,
    [
        TreeUp,
        TreeDown,
        TreeCollapse,
        TreeExpand,
        TreePreview,
        TreeOpen,
        TreeRename,
        TreeCancelRename,
        TreeTrash,
        TreeDuplicate,
        TreeCopyPath,
        TreeBlur,
        TreeUndo,
        TreeRedo
    ]
);

/// The file tree's keys, as in Finder (and VS Code's explorer on the Mac):
/// arrows move and fold, Space previews, ⌘↓ opens, Return renames, ⌘⌫ moves
/// to the Trash, ⌘D duplicates, ⌥⌘C copies the path, ⌘Z / ⌘⇧Z undo and redo
/// those.
pub fn bind_keys(cx: &mut App) {
    // Not while a name is being typed: Space and the arrows are text there.
    let tree = Some("Sidebar && !renaming");
    cx.bind_keys([
        KeyBinding::new("up", TreeUp, tree),
        KeyBinding::new("down", TreeDown, tree),
        KeyBinding::new("k", TreeUp, tree),
        KeyBinding::new("j", TreeDown, tree),
        KeyBinding::new("left", TreeCollapse, tree),
        KeyBinding::new("right", TreeExpand, tree),
        KeyBinding::new("space", TreePreview, tree),
        KeyBinding::new("cmd-down", TreeOpen, tree),
        KeyBinding::new("cmd-o", TreeOpen, tree),
        KeyBinding::new("enter", TreeRename, tree),
        KeyBinding::new("cmd-backspace", TreeTrash, tree),
        KeyBinding::new("cmd-d", TreeDuplicate, tree),
        KeyBinding::new("cmd-alt-c", TreeCopyPath, tree),
        KeyBinding::new("escape", TreeBlur, tree),
        KeyBinding::new("cmd-z", TreeUndo, tree),
        KeyBinding::new("cmd-shift-z", TreeRedo, tree),
        // Beats the field's own Escape: cancel the rename.
        KeyBinding::new("escape", TreeCancelRename, Some("Sidebar > Input")),
    ]);
}

pub enum SidebarEvent {
    Open(PathBuf),
    /// A single click: open as a preview tab, which the next one replaces.
    Preview(PathBuf),
    /// Outline row clicked: go to that heading (its cell, and its place in
    /// the outline) in the active notebook.
    JumpToHeading { cell: usize, heading: usize },
    /// The outline tab became visible and needs the active notebook's headings.
    OutlineShown,
    /// A file or folder went to the Trash: its tabs close, unsaved.
    Trashed(PathBuf),
    /// Renamed on disk: open tabs follow it.
    Renamed { from: PathBuf, to: PathBuf },
    /// Escape: hand the keyboard back to the notebook.
    Blur,
    /// An undo or redo of a file operation that couldn't happen, to show.
    UndoFailed(String),
    /// A question for the reader, in Herse's dialog; the index of the
    /// choice goes back on `answer`.
    Ask {
        title: SharedString,
        detail: SharedString,
        choices: Vec<(SharedString, crate::dialog::Role)>,
        answer: smol::channel::Sender<usize>,
    },
}

/// What takes a file operation back. Doing one gives the step that takes
/// *it* back, so the same steps serve undo and redo.
#[derive(Clone, Debug, PartialEq)]
enum FileStep {
    /// Move `from` to `to`: a rename, or out of the Trash.
    Move { from: PathBuf, to: PathBuf },
    /// Move to the Trash (undoing a duplicate).
    Trash(PathBuf),
}

/// Move `path` to the Trash; where it went there (Finder's Put Back works
/// on it too).
fn move_to_trash(path: &Path) -> Result<PathBuf, String> {
    let out = std::process::Command::new("/usr/bin/trash")
        .arg("-v")
        .arg(path)
        .output()
        .map_err(|e| format!("Couldn't run /usr/bin/trash: {e}"))?;
    // `# Moved "/a/b.txt" to "/Users/me/.Trash/b.txt"`
    let said = String::from_utf8_lossy(&out.stdout);
    match said.lines().find_map(|l| l.rsplit_once("\" to \"")).map(|(_, to)| to.trim_end().trim_end_matches('"')) {
        Some(to) if out.status.success() => Ok(PathBuf::from(to)),
        _ => Err(format!(
            "Couldn't move {} to the Trash. {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Do `step`; the step that undoes it.
fn perform(step: &FileStep) -> Result<FileStep, String> {
    match step {
        FileStep::Move { from, to } => {
            if to.exists() {
                return Err(format!("Couldn't put back {}: something else is there now.", to.display()));
            }
            if !from.exists() {
                return Err(format!("Couldn't find {} any more.", from.display()));
            }
            std::fs::rename(from, to).map_err(|e| format!("Couldn't move {} back: {e}", from.display()))?;
            Ok(FileStep::Move { from: to.clone(), to: from.clone() })
        }
        FileStep::Trash(path) => Ok(FileStep::Move { from: move_to_trash(path)?, to: path.clone() }),
    }
}

/// A name being typed in place of a row's label.
struct Rename {
    path: PathBuf,
    input: Entity<InputState>,
    _subscription: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Files,
    Outline,
}

/// (cell index, heading level, text)
pub type Outline = std::rc::Rc<Vec<(usize, usize, SharedString)>>;

impl EventEmitter<SidebarEvent> for Sidebar {}

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: SharedString,
    pub depth: usize,
    pub is_dir: bool,
    /// A data file's icon (`None` for folders and notebooks).
    pub icon: Option<&'static str>,
    /// Bytes, for files.
    pub size: u64,
}

pub struct Sidebar {
    root: Option<PathBuf>,
    entries: Vec<Entry>,
    /// Indices into `entries` that are currently visible (ancestors expanded).
    visible: Vec<usize>,
    collapsed: HashSet<PathBuf>,
    summaries: HashMap<PathBuf, crate::workspace_scan::Summary>,
    active: Option<PathBuf>,
    scroll: UniformListScrollHandle,
    mode: Mode,
    outline: Outline,
    /// Index into `outline` of the section containing the selected cell.
    outline_current: Option<usize>,
    outline_scroll: UniformListScrollHandle,
    /// A mouse-down on the header strip that may turn into a window drag.
    drag_pending: std::rc::Rc<std::cell::Cell<bool>>,
    focus_handle: FocusHandle,
    /// The row the keyboard is on (by path, so it survives rescans).
    cursor: Option<PathBuf>,
    rename: Option<Rename>,
    /// File operations to undo (the last one last), and undone ones to redo.
    undo: Vec<FileStep>,
    redo: Vec<FileStep>,
    _scan: Option<Task<()>>,
}

impl Focusable for Sidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Sidebar {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            root: None,
            entries: Vec::new(),
            visible: Vec::new(),
            collapsed: HashSet::new(),
            summaries: HashMap::new(),
            active: None,
            scroll: UniformListScrollHandle::new(),
            mode: Mode::Files,
            outline: Default::default(),
            outline_current: None,
            outline_scroll: UniformListScrollHandle::new(),
            drag_pending: Default::default(),
            focus_handle: cx.focus_handle(),
            cursor: None,
            rename: None,
            undo: Vec::new(),
            redo: Vec::new(),
            _scan: None,
        }
    }

    // ------------------------------------------------------------ keyboard

    /// Where the cursor is in the visible rows: the cursor's row, else the
    /// open notebook's.
    fn cursor_pos(&self) -> Option<usize> {
        let at = |path: &PathBuf| self.visible.iter().position(|&ix| &self.entries[ix].path == path);
        self.cursor.as_ref().and_then(at).or_else(|| self.active.as_ref().and_then(at))
    }

    fn cursor_entry(&self) -> Option<&Entry> {
        self.cursor_pos().map(|pos| &self.entries[self.visible[pos]])
    }

    fn put_cursor(&mut self, pos: usize, cx: &mut Context<Self>) {
        let Some(&ix) = self.visible.get(pos) else { return };
        self.cursor = Some(self.entries[ix].path.clone());
        self.scroll.scroll_to_item(pos, gpui_kit::ScrollStrategy::Nearest);
        cx.notify();
    }

    fn on_up(&mut self, _: &TreeUp, _: &mut Window, cx: &mut Context<Self>) {
        let pos = self.cursor_pos().map_or(0, |p| p.saturating_sub(1));
        self.put_cursor(pos, cx);
    }

    fn on_down(&mut self, _: &TreeDown, _: &mut Window, cx: &mut Context<Self>) {
        let last = self.visible.len().saturating_sub(1);
        let pos = self.cursor_pos().map_or(0, |p| (p + 1).min(last));
        self.put_cursor(pos, cx);
    }

    /// ←: fold an open folder, else go up to the parent folder.
    fn on_collapse(&mut self, _: &TreeCollapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        if entry.is_dir && !self.collapsed.contains(&entry.path) {
            self.toggle(&entry.path, cx);
        } else if let Some(parent) = entry.path.parent()
            && let Some(pos) = self.visible.iter().position(|&ix| self.entries[ix].path == parent)
        {
            self.put_cursor(pos, cx);
        }
    }

    /// →: unfold a folder, else step into it.
    fn on_expand(&mut self, _: &TreeExpand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        if !entry.is_dir {
            return;
        }
        if self.collapsed.contains(&entry.path) {
            self.toggle(&entry.path, cx);
        } else if let Some(pos) = self.cursor_pos() {
            self.put_cursor(pos + 1, cx);
        }
    }

    fn on_preview(&mut self, _: &TreePreview, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        if entry.is_dir {
            self.toggle(&entry.path, cx);
        } else {
            cx.emit(SidebarEvent::Preview(entry.path));
        }
    }

    fn on_open(&mut self, _: &TreeOpen, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        if entry.is_dir {
            self.toggle(&entry.path, cx);
        } else {
            cx.emit(SidebarEvent::Open(entry.path));
        }
    }

    fn on_blur(&mut self, _: &TreeBlur, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::Blur);
    }

    fn on_copy_path(&mut self, _: &TreeCopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.cursor_entry() {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(entry.path.display().to_string()));
        }
    }

    /// ⌘⌫: move the row's file or folder to the Trash (Finder's Put Back
    /// brings it back). A folder asks first.
    fn on_trash(&mut self, _: &TreeTrash, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        self.trash(entry, cx);
    }

    fn trash(&mut self, entry: Entry, cx: &mut Context<Self>) {
        use crate::dialog::Role;
        let answer = entry.is_dir.then(|| {
            let (tx, rx) = smol::channel::bounded(1);
            cx.emit(SidebarEvent::Ask {
                title: format!("Move the folder “{}” to the Trash?", entry.name).into(),
                detail: "Everything in it goes too. ⌘Z puts it back, as does Finder's Put Back.".into(),
                choices: vec![("Cancel".into(), Role::Cancel), ("Move to Trash".into(), Role::Destructive)],
                answer: tx,
            });
            rx
        });
        let next = self.cursor_pos().and_then(|pos| self.visible.get(pos + 1).or(pos.checked_sub(1).and_then(|p| self.visible.get(p))).copied());
        let next = next.map(|ix| self.entries[ix].path.clone());
        cx.spawn(async move |this, cx| {
            if let Some(answer) = answer
                && answer.recv().await.ok() != Some(1)
            {
                return;
            }
            let path = entry.path.clone();
            let moved = cx.background_spawn(async move { move_to_trash(&path) }).await;
            this.update(cx, |this, cx| match moved {
                Ok(in_trash) => {
                    this.cursor = next;
                    this.remember(FileStep::Move { from: in_trash, to: entry.path.clone() });
                    cx.emit(SidebarEvent::Trashed(entry.path.clone()));
                    this.rescan(cx);
                }
                Err(why) => cx.emit(SidebarEvent::UndoFailed(why)),
            })
            .ok();
        })
        .detach();
    }

    /// ⌘D: a copy beside the file, named the way Finder names copies.
    fn on_duplicate(&mut self, _: &TreeDuplicate, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        if entry.is_dir {
            return;
        }
        let Some(copy) = copy_name(&entry.path) else { return };
        if std::fs::copy(&entry.path, &copy).is_ok() {
            self.remember(FileStep::Trash(copy.clone()));
            self.cursor = Some(copy);
            self.rescan(cx);
        }
    }

    /// A file operation done: undoable, and the end of what redo could bring back.
    fn remember(&mut self, undo: FileStep) {
        self.undo.push(undo);
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn on_undo(&mut self, _: &TreeUndo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_file_op(cx);
    }

    fn on_redo(&mut self, _: &TreeRedo, _: &mut Window, cx: &mut Context<Self>) {
        self.redo_file_op(cx);
    }

    /// ⌘Z for files: take back the last rename, duplicate or move to the
    /// Trash. Whether there was one.
    pub fn undo_file_op(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(step) = self.undo.pop() else { return false };
        self.replay(step, true, cx);
        true
    }

    /// ⌘⇧Z for files. Whether there was something to redo.
    pub fn redo_file_op(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(step) = self.redo.pop() else { return false };
        self.replay(step, false, cx);
        true
    }

    /// Do an undo (or redo) step off the main thread (the Trash can be
    /// slow), then keep its inverse for the other direction.
    fn replay(&mut self, step: FileStep, undoing: bool, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let todo = step.clone();
            let done = cx.background_spawn(async move { perform(&todo) }).await;
            this.update(cx, |this, cx| {
                match done {
                    Ok(inverse) => {
                        match &step {
                            FileStep::Move { from, to } => {
                                this.cursor = Some(to.clone());
                                cx.emit(SidebarEvent::Renamed { from: from.clone(), to: to.clone() });
                            }
                            FileStep::Trash(path) => cx.emit(SidebarEvent::Trashed(path.clone())),
                        }
                        if undoing { this.redo.push(inverse) } else { this.undo.push(inverse) }
                    }
                    Err(why) => cx.emit(SidebarEvent::UndoFailed(why)),
                }
                this.rescan(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Return: type a new name in place of the row's label (the name
    /// without its extension selected, as in Finder).
    fn on_rename(&mut self, _: &TreeRename, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        self.start_rename(entry, window, cx);
    }

    fn start_rename(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        let name = entry.name.to_string();
        let stem = if entry.is_dir { name.len() } else { name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len()) };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name.clone()));
        input.update(cx, |input, cx| input.set_selected_range(0..stem, cx));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
            InputEvent::Blur => this.commit_rename(window, cx),
            _ => {}
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.rename = Some(Rename { path: entry.path, input, _subscription: subscription });
        cx.notify();
    }

    fn on_cancel_rename(&mut self, _: &TreeCancelRename, window: &mut Window, cx: &mut Context<Self>) {
        self.rename = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else { return };
        let name = rename.input.read(cx).value().trim().to_string();
        let from = rename.path;
        self.focus_handle.focus(window, cx);
        cx.notify();
        let Some(parent) = from.parent() else { return };
        let to = parent.join(&name);
        if name.is_empty() || name.contains('/') || to == from || to.exists() {
            return;
        }
        if std::fs::rename(&from, &to).is_ok() {
            self.remember(FileStep::Move { from: to.clone(), to: from.clone() });
            self.cursor = Some(to.clone());
            cx.emit(SidebarEvent::Renamed { from, to });
            self.rescan(cx);
        }
    }

    pub fn set_root(&mut self, root: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        if self.root.as_ref() == Some(&root) {
            return;
        }
        self.root = Some(root.clone());
        self.entries.clear();
        self.visible.clear();
        self.summaries.clear();
        self.rescan(cx);
    }

    pub fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        self._scan = Some(cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn({
                    let root = root.clone();
                    async move { crate::workspace_scan::scan(&root) }
                })
                .await;
            // Data files have their size instead of a cell count.
            let notebooks: Vec<PathBuf> = entries
                .iter()
                .filter(|e| !e.is_dir && e.icon.is_none())
                .map(|e| e.path.clone())
                .collect();
            this.update(cx, |this, cx| {
                this.entries = entries;
                this.recompute_visible();
                cx.notify();
            })
            .ok();
            // Annotations arrive after the tree is already on screen.
            let summaries = cx
                .background_spawn(async move { crate::workspace_scan::summarize(&notebooks) })
                .await;
            this.update(cx, |this, cx| {
                this.summaries = summaries;
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if mode == Mode::Outline {
            cx.emit(SidebarEvent::OutlineShown);
        }
        cx.notify();
    }

    /// New outline for the active notebook. Cheap to call often: an unchanged
    /// list and position is a no-op.
    pub fn set_outline(&mut self, outline: Outline, current: Option<usize>, cx: &mut Context<Self>) {
        let same = std::rc::Rc::ptr_eq(&self.outline, &outline);
        if same && self.outline_current == current {
            return;
        }
        self.outline = outline;
        if self.outline_current != current {
            self.outline_current = current;
            if let Some(c) = current {
                self.outline_scroll.scroll_to_item(c, gpui_kit::ScrollStrategy::Nearest);
            }
        }
        cx.notify();
    }

    fn render_outline_rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<gpui_kit::AnyElement> {
        let theme = cx.theme();
        range
            .filter_map(|ix| self.outline.get(ix).cloned().map(|h| (ix, h)))
            .map(|(ix, (cell, level, text))| {
                let current = self.outline_current == Some(ix);
                row_frame(("outline", ix))
                    .child(
                        div()
                            .id(("outline-bg", ix))
                            .h_full()
                            .w_full()
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .pl(px(8. + (level.saturating_sub(1)) as f32 * 12.))
                            .pr(px(8.))
                            .text_size(px(13.))
                            .text_color(if level <= 2 { theme.foreground } else { theme.muted_foreground })
                            .when(level <= 1, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                            .when(current, |d| d.bg(theme.foreground.opacity(0.07)).text_color(theme.foreground))
                            .when(!current, |d| d.hover(|s| s.bg(theme.foreground.opacity(0.04))))
                            .child(div().truncate().child(text)),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::JumpToHeading { cell, heading: ix })))
                    .into_any_element()
            })
            .collect()
    }

    pub fn notebook_paths(&self) -> Vec<PathBuf> {
        self.entries.iter().filter(|e| !e.is_dir).map(|e| e.path.clone()).collect()
    }

    pub fn set_active(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.active = path;
        if let Some(active) = self.active.clone() {
            // Reveal: expand every ancestor folder.
            let mut changed = false;
            for ancestor in active.ancestors().skip(1) {
                changed |= self.collapsed.remove(ancestor);
            }
            if changed {
                self.recompute_visible();
            }
            if let Some(pos) = self.visible.iter().position(|&ix| self.entries[ix].path == active) {
                self.scroll.scroll_to_item(pos, gpui_kit::ScrollStrategy::Center);
            }
        }
        cx.notify();
    }

    fn recompute_visible(&mut self) {
        self.visible.clear();
        let mut hidden_below: Option<usize> = None;
        for (ix, entry) in self.entries.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if entry.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            self.visible.push(ix);
            if entry.is_dir && self.collapsed.contains(&entry.path) {
                hidden_below = Some(entry.depth);
            }
        }
    }

    /// Fold (or unfold) every folder in the tree.
    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.collapsed = if collapsed {
            self.entries.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect()
        } else {
            HashSet::new()
        };
        self.recompute_visible();
        cx.notify();
    }

    fn toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_path_buf());
        }
        self.recompute_visible();
        cx.notify();
    }

    fn render_rows(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui_kit::AnyElement> {
        let focused = self.focus_handle.contains_focused(window, cx);
        let accent = theme::accent(cx);
        let notebook_ink = theme::notebook_icon(cx);
        let theme = cx.theme();
        range
            .filter_map(|pos| self.visible.get(pos).map(|&ix| (pos, &self.entries[ix])))
            .map(|(pos, entry)| {
                let active = self.active.as_ref() == Some(&entry.path);
                let at_cursor = focused && self.cursor.as_ref() == Some(&entry.path);
                let renaming = self.rename.as_ref().filter(|r| r.path == entry.path).map(|r| r.input.clone());
                let collapsed = self.collapsed.contains(&entry.path);
                let path = entry.path.clone();
                let menu_path = path.clone();
                let is_dir = entry.is_dir;
                let entry_for_menu = entry.clone();
                let summary = match entry.icon {
                    Some(_) => {
                        let size: SharedString = crate::workspace::human_bytes(entry.size as usize).into();
                        Some((size.clone(), size))
                    }
                    None => self.summaries.get(&entry.path).map(|s| (s.short.clone(), s.long.clone())),
                };
                let data_icon = entry.icon;
                let chevron = if collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" };
                row_frame(("tree", pos))
                    .when_some(summary.as_ref().map(|(_, long)| long.clone()), |d, long| {
                        d.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(long.clone()).build(window, cx))
                    })
                    .child(
                        // An id, so a hover redraws the (cached) sidebar the
                        // moment it changes: GPUI tracks hover per element id.
                        div()
                            .id(("tree-row-bg", pos))
                            .h_full()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .rounded(px(6.))
                            .pl(px(4. + entry.depth as f32 * 14.))
                            .pr(px(8.))
                            .text_size(px(13.))
                            .when(active && !at_cursor, |d| d.bg(theme.foreground.opacity(0.07)).text_color(theme.foreground))
                            // The keyboard's row, while the tree has focus: dew, as a selection.
                            .when(at_cursor, |d| d.bg(accent.opacity(0.16)).text_color(theme.foreground))
                            .when(!active && !at_cursor, |d| d.hover(|s| s.bg(theme.foreground.opacity(0.04))))
                            .child(
                                div().size(px(14.)).flex_none().flex().items_center().justify_center().when(is_dir, |d| {
                                    d.child(gpui_kit::svg().path(chevron).size(px(12.)).text_color(theme.muted_foreground))
                                }),
                            )
                            // The file's icon: a folder that opens with the
                            // row, or a notebook (a page holding Herse's
                            // brackets).
                            .child(
                                gpui_kit::svg()
                                    .path(match (is_dir, collapsed) {
                                        (true, true) => "icons/folder.svg",
                                        (true, false) => "icons/folder-open.svg",
                                        (false, _) => data_icon.unwrap_or("icons/herse-notebook.svg"),
                                    })
                                    .size(px(14.))
                                    .flex_none()
                                    .mr(px(2.))
                                    .text_color(if is_dir || data_icon.is_some() { theme.muted_foreground.opacity(0.8) } else { notebook_ink }),
                            )
                            .child(match renaming {
                                Some(input) => div()
                                    .flex_1()
                                    .min_w_0()
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(accent)
                                    .bg(theme.background)
                                    .text_size(px(13.))
                                    .child(Input::new(&input).appearance(false))
                                    .into_any_element(),
                                None => div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(is_dir, |d| d.text_color(theme.muted_foreground))
                                    .when(active, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                                    // The whole name, extension included, the
                                    // extension a shade quieter than the name.
                                    .child({
                                        let quiet = (!is_dir)
                                            .then(|| entry.name.rfind('.').filter(|&dot| dot > 0))
                                            .flatten()
                                            .map(|dot| {
                                                let style = gpui_kit::HighlightStyle {
                                                    color: Some(theme.muted_foreground),
                                                    ..Default::default()
                                                };
                                                (dot..entry.name.len(), style)
                                            });
                                        gpui_kit::StyledText::new(entry.name.clone()).with_highlights(quiet)
                                    })
                                    .into_any_element(),
                            })
                            .when_some(summary.map(|(short, _)| short), |d, short| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(theme.muted_foreground.opacity(0.75))
                                        .child(short),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                        this.cursor = Some(path.clone());
                        this.focus_handle.focus(window, cx);
                        if is_dir {
                            this.toggle(&path, cx);
                        } else if ev.click_count() >= 2 {
                            cx.emit(SidebarEvent::Open(path.clone()));
                        } else {
                            cx.emit(SidebarEvent::Preview(path.clone()));
                        }
                    }))
                    .context_menu({
                        let sidebar = cx.entity().downgrade();
                        let path = menu_path;
                        let relative = self
                            .root
                            .as_ref()
                            .and_then(|r| path.strip_prefix(r).ok())
                            .map(|p| p.display().to_string());
                        move |menu, _, _| {
                            use gpui_kit::component::menu::PopupMenuItem;
                            let (open, reveal, full) = (path.clone(), path.clone(), path.display().to_string());
                            let (rename_tree, trash_tree, dup_tree) = (sidebar.clone(), sidebar.clone(), sidebar.clone());
                            let sidebar = sidebar.clone();
                            let menu = if is_dir {
                                menu
                            } else {
                                menu.item(PopupMenuItem::new("Open").on_click(move |_, _, cx| {
                                    sidebar.update(cx, |_, cx| cx.emit(SidebarEvent::Open(open.clone()))).ok();
                                }))
                                .separator()
                            };
                            let (rename_entry, trash_entry) = (entry_for_menu.clone(), entry_for_menu.clone());
                            let dup_path = path.clone();
                            let menu = menu
                                .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                                    rename_tree.update(cx, |this, cx| this.start_rename(rename_entry.clone(), window, cx)).ok();
                                }))
                                .when(!is_dir, |menu| {
                                    menu.item(PopupMenuItem::new("Duplicate").on_click(move |_, window, cx| {
                                        dup_tree
                                            .update(cx, |this, cx| {
                                                this.cursor = Some(dup_path.clone());
                                                this.on_duplicate(&TreeDuplicate, window, cx);
                                            })
                                            .ok();
                                    }))
                                })
                                .item(PopupMenuItem::new("Move to Trash").on_click(move |_, _, cx| {
                                    trash_tree.update(cx, |this, cx| this.trash(trash_entry.clone(), cx)).ok();
                                }))
                                .separator()
                                .item(PopupMenuItem::new("Reveal in Finder").on_click(move |_, _, cx| cx.reveal_path(&reveal)))
                                .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(full.clone()));
                                }));
                            match relative.clone() {
                                Some(rel) => menu.item(PopupMenuItem::new("Copy Relative Path").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(rel.clone()));
                                })),
                                None => menu,
                            }
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }
}

/// A sidebar row: full height, inset so its rounded highlight clears the
/// sidebar's edges.
fn row_frame(id: impl Into<gpui_kit::ElementId>) -> gpui_kit::Stateful<gpui_kit::Div> {
    div().id(id).h(theme::TREE_ROW_HEIGHT).w_full().px(px(8.)).py(px(1.)).cursor_pointer()
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title: SharedString = match self.mode {
            Mode::Files => self
                .root
                .as_ref()
                .and_then(|r| r.file_name())
                .map(|n| n.to_string_lossy().into_owned().into())
                .unwrap_or_else(|| "No folder".into()),
            Mode::Outline => "Outline".into(),
        };
        let count = self.visible.len();
        let mode = self.mode;
        let toggle = |id: &'static str, icon: &'static str, tip: &'static str, this_mode: Mode, cx: &mut Context<Self>| {
            crate::ui::icon_button(id, icon, mode == this_mode, cx)
                // A press here is a click, never the start of a window drag.
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(this_mode, cx)))
        };
        let files = toggle("mode-files", "icons/folder.svg", "Files", Mode::Files, cx);
        let outline = toggle("mode-outline", "icons/table-of-contents.svg", "Outline  ⇧⌘E", Mode::Outline, cx);
        let theme = cx.theme();
        let outline_count = self.outline.len();
        let fullscreen = window.is_fullscreen();
        div()
            .key_context(if self.rename.is_some() { "Sidebar renaming" } else { "Sidebar" })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_collapse))
            .on_action(cx.listener(Self::on_expand))
            .on_action(cx.listener(Self::on_preview))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_cancel_rename))
            .on_action(cx.listener(Self::on_trash))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_blur))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .size_full()
            .flex()
            .flex_col()
            .child(
                // The window's title bar: the traffic lights sit at its left.
                crate::chrome::drag_strip("sidebar-strip", &self.drag_pending)
                    .h(theme::CHROME_HEIGHT)
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(2.))
                    .pl(if fullscreen { px(8.) } else { theme::TRAFFIC_LIGHTS_WIDTH })
                    .pr(px(8.))
                    .child(files)
                    .child(outline),
            )
            .child(match (mode, self.root.clone()) {
                // The folder's name opens the folder menu: other folders,
                // Finder, the path, and folding the whole tree.
                (Mode::Files, Some(root)) => {
                    use gpui_kit::component::menu::DropdownMenu as _;
                    let sidebar = cx.entity().downgrade();
                    let muted = theme.muted_foreground;
                    div()
                        .h(px(28.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(10.))
                        .child(
                            crate::ui::MenuTrigger::new("folder-menu")
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.))
                                .h(px(22.))
                                .px(px(6.))
                                .text_size(px(11.5))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(muted)
                                .child(div().truncate().child(title))
                                .child(gpui_kit::svg().path("icons/chevron-down.svg").flex_none().size(px(11.)).text_color(muted))
                                .dropdown_menu(move |menu, _, cx| folder_menu(menu, &root, sidebar.clone(), cx)),
                        )
                        .into_any_element()
                }
                _ => div()
                    .h(px(28.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(16.))
                    .text_size(px(11.5))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(div().truncate().child(title))
                    .into_any_element(),
            })
            .when(mode == Mode::Files && self.root.is_none(), |d| {
                d.child(
                    div()
                        .px(px(16.))
                        .text_size(px(12.5))
                        .text_color(theme.muted_foreground)
                        .child("Open a folder to browse its notebooks."),
                )
                .child(div().flex().px(px(16.)).pt(px(10.)).child(
                    crate::ui::button("sidebar-open-folder", "Open Folder…", cx)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::OpenFolder), cx)),
                ))
            })
            .when(mode == Mode::Files, |d| {
                d.child(
                    uniform_list(
                        "file-tree",
                        count,
                        cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
                    )
                    .track_scroll(&self.scroll)
                    .pb(px(8.))
                    .flex_1(),
                )
            })
            .when(mode == Mode::Outline && outline_count == 0, |d| {
                d.child(
                    div()
                        .px(px(16.))
                        .text_size(px(12.5))
                        .text_color(theme.muted_foreground)
                        .child("Headings in the open notebook appear here."),
                )
            })
            .when(mode == Mode::Outline, |d| {
                d.child(
                    uniform_list(
                        "outline",
                        outline_count,
                        cx.processor(|this, range, _window, cx| this.render_outline_rows(range, cx)),
                    )
                    .track_scroll(&self.outline_scroll)
                    .pb(px(8.))
                    .flex_1(),
                )
            })
    }
}

/// The folder menu, under the folder's name.
fn folder_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    root: &Path,
    sidebar: gpui_kit::WeakEntity<Sidebar>,
    cx: &mut gpui_kit::Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu {
    use gpui_kit::component::menu::PopupMenuItem;
    let recent: Vec<PathBuf> =
        crate::recents::get(cx).recents.folders.iter().filter(|f| f.as_path() != root && f.is_dir()).take(8).cloned().collect();
    let mut menu = menu.min_w(px(220.)).menu("Open Folder…", Box::new(crate::actions::OpenFolder));
    if !recent.is_empty() {
        menu = menu.separator().label("Recent Folders");
        for folder in recent {
            let name = folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
            menu = menu.menu(name, Box::new(crate::actions::OpenRecent(folder)));
        }
    }
    let (reveal, copy) = (root.to_path_buf(), root.display().to_string());
    let (collapse, expand) = (sidebar.clone(), sidebar);
    menu.separator()
        .item(PopupMenuItem::new("Reveal in Finder").on_click(move |_, _, cx| cx.reveal_path(&reveal)))
        .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(copy.clone()));
        }))
        .separator()
        .item(PopupMenuItem::new("Collapse All Folders").on_click(move |_, _, cx| {
            collapse.update(cx, |s, cx| s.set_all_collapsed(true, cx)).ok();
        }))
        .item(PopupMenuItem::new("Expand All Folders").on_click(move |_, _, cx| {
            expand.update(cx, |s, cx| s.set_all_collapsed(false, cx)).ok();
        }))
}

#[allow(dead_code)]
fn noop(_: &mut App) {}

/// Finder's name for a copy: "a copy.ipynb", then "a copy 2.ipynb", …
fn copy_name(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..1000).map(|n| {
        let suffix = if n == 1 { " copy".to_string() } else { format!(" copy {n}") };
        parent.join(format!("{stem}{suffix}{ext}"))
    })
    .find(|p| !p.exists())
}
