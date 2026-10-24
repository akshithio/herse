//! Picker delegates: Go to File, the command palette, and the notebook outline.

use std::path::PathBuf;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui_kit::{Action, App, AppContext as _, Context, Entity, FocusHandle, SharedString, Task, Window};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    actions::*,
    notebook::NotebookView,
    picker::{Match, Picker, PickerDelegate},
};

/// Fuzzy-rank `candidates` against `query`, returning (index, positions) best first.
fn fuzzy(query: &str, candidates: &[SharedString], matcher: &mut Matcher, limit: usize) -> Vec<(usize, Vec<u32>)> {
    if query.trim().is_empty() {
        return (0..candidates.len().min(limit)).map(|i| (i, Vec::new())).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(ix, c)| {
            pattern
                .score(Utf32Str::new(c, &mut buf), matcher)
                .map(|score| (score, ix))
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(_, ix)| {
            let mut positions = Vec::new();
            pattern.indices(Utf32Str::new(&candidates[ix], &mut buf), matcher, &mut positions);
            positions.sort_unstable();
            positions.dedup();
            (ix, positions)
        })
        .collect()
}

/// Convert char positions to byte ranges, merging neighbours.
fn char_positions_to_ranges(text: &str, positions: &[u32]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut want = positions.iter().peekable();
    for (char_ix, (byte_ix, ch)) in text.char_indices().enumerate() {
        let Some(&&p) = want.peek() else { break };
        if p as usize == char_ix {
            want.next();
            let end = byte_ix + ch.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_ix => last.end = end,
                _ => ranges.push(byte_ix..end),
            }
        }
    }
    ranges
}

// ------------------------------------------------------------------ files

type OnOpen = Box<dyn Fn(PathBuf, &mut Window, &mut App)>;
/// Open a hit: notebook, cell, and the query to find there.
type OnOpenHit = Arc<dyn Fn(PathBuf, usize, String, &mut Window, &mut App)>;

pub struct FileFinder {
    root: Option<PathBuf>,
    paths: Vec<PathBuf>,
    index: herse_workspace::FuzzyIndex,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
    on_open: OnOpen,
}

impl FileFinder {
    /// `recent` paths rank first for an empty query.
    pub fn new(
        root: Option<PathBuf>,
        paths: Vec<PathBuf>,
        recent: &[PathBuf],
        on_open: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    ) -> Self {
        let rel = |p: &PathBuf| -> String {
            root.as_ref()
                .and_then(|r| p.strip_prefix(r).ok())
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        };
        let labels: Vec<String> = paths.iter().map(rel).collect();
        let mut index = herse_workspace::FuzzyIndex::new(labels);
        let recents: Vec<String> = recent.iter().map(rel).collect();
        index.set_recents(&recents);
        Self {
            root,
            paths,
            index,
            matches: Vec::new(),
            match_ix: Vec::new(),
            on_open: Box::new(on_open),
        }
    }
}

impl PickerDelegate for FileFinder {
    fn placeholder(&self) -> SharedString {
        match &self.root {
            Some(root) => format!(
                "Go to notebook in {}",
                root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            )
            .into(),
            None => "Go to notebook".into(),
        }
    }

    fn update_matches(&mut self, query: &str, _cx: &mut Context<Picker<Self>>) {
        let hits = self.index.query(query, 200);
        self.match_ix = hits.iter().map(|h| h.index).collect();
        self.matches = hits
            .into_iter()
            .map(|hit| {
                let label = self.index.path(hit.index);
                // File name prominent, folder as detail.
                let (dir, name) = match label.rfind('/') {
                    Some(slash) => (&label[..slash], &label[slash + 1..]),
                    None => ("", label),
                };
                let offset = (label.len() - name.len()) as u32;
                let mut highlights: Vec<std::ops::Range<usize>> = Vec::new();
                for &p in hit.positions.iter().filter(|&&p| p >= offset) {
                    let start = (p - offset) as usize;
                    let len = name[start..].chars().next().map_or(1, char::len_utf8);
                    match highlights.last_mut() {
                        Some(last) if last.end == start => last.end = start + len,
                        _ => highlights.push(start..start + len),
                    }
                }
                Match {
                    label: SharedString::from(name.to_string()),
                    highlights,
                    detail: (!dir.is_empty()).then(|| SharedString::from(dir.to_string())),
                    hint: None,
                    icon: Some("icons/herse-notebook.svg"),
                }
            })
            .collect();
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(&path_ix) = self.match_ix.get(ix) {
            (self.on_open)(self.paths[path_ix].clone(), window, cx);
        }
        true
    }

    fn empty_text(&self) -> SharedString {
        if self.paths.is_empty() {
            "Open a folder to search its notebooks".into()
        } else {
            "No matching notebooks".into()
        }
    }
}

// ------------------------------------------------------------------ commands

pub struct CommandPalette {
    commands: Vec<(SharedString, Box<dyn Action>, Option<SharedString>)>,
    labels: Vec<SharedString>,
    matcher: Matcher,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
    return_focus: Option<FocusHandle>,
}

/// Other words for a command, for ⌘K's search.
fn keywords(label: &str) -> Option<&'static str> {
    Some(match label {
        "Toggle Dark Mode" => "light mode theme appearance switch",
        "Light Mode" => "theme appearance day bright",
        "Dark Mode" => "theme appearance night",
        "Match System Appearance" => "light dark mode theme auto follow",
        "Settings…" => "preferences options config",
        "Close All Tabs" => "close everything",
        "Ask Claude or Codex…" => "ai assistant agent chat gpt help",
        "Open Extensions Folder" => "plugins add-ons",
        "Install Command Line Tool…" => "herse cli terminal shell command path",
        "Reload Extensions" => "plugins add-ons refresh",
        "Extensions…" => "plugins add-ons allow trust revoke permissions",
        "Keyboard Shortcuts" => "keys bindings hotkeys",
        "Export as HTML" => "save web page share",
        "Change Kernel…" => "python environment interpreter venv conda",
        "Restart Kernel" => "reset python",
        "Interrupt Kernel" => "stop cancel halt",
        _ => return None,
    })
}

pub fn command_list() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Go to File", Box::new(GoToFile)),
        ("Go to Heading", Box::new(GoToHeading)),
        ("Open…", Box::new(Open)),
        ("Open Folder…", Box::new(OpenFolder)),
        ("New Notebook", Box::new(NewNotebook)),
        ("Save", Box::new(Save)),
        ("Save As…", Box::new(SaveAs)),
        ("Close Tab", Box::new(CloseTab)),
        ("Toggle Sidebar", Box::new(ToggleSidebar)),
        ("Toggle Outline / Files", Box::new(ToggleOutline)),
        ("Toggle Dark Mode", Box::new(ToggleTheme)),
        ("Light Mode", Box::new(LightMode)),
        ("Dark Mode", Box::new(DarkMode)),
        ("Match System Appearance", Box::new(SystemAppearance)),
        ("Zoom In", Box::new(ZoomIn)),
        ("Zoom Out", Box::new(ZoomOut)),
        ("Reset Zoom", Box::new(ZoomReset)),
        ("Find in Workspace", Box::new(FindInWorkspace)),
        ("Run Cell", Box::new(RunCell)),
        ("Run Cell and Advance", Box::new(RunAndAdvance)),
        ("Run Cell and Insert Below", Box::new(RunAndInsert)),
        ("Run All Cells", Box::new(RunAll)),
        ("Run All Above", Box::new(RunAbove)),
        ("Run Selected and Below", Box::new(RunBelow)),
        ("Interrupt Kernel", Box::new(Interrupt)),
        ("Restart Kernel", Box::new(Restart)),
        ("Restart Kernel and Run All", Box::new(RestartAndRunAll)),
        ("Change Kernel…", Box::new(ChangeKernel)),
        ("Shut Down Kernel", Box::new(ShutdownKernel)),
        ("Insert Code Cell Above", Box::new(InsertCodeAbove)),
        ("Insert Code Cell Below", Box::new(InsertCodeBelow)),
        ("Insert Markdown Cell Below", Box::new(InsertMarkdownBelow)),
        ("Change Cell to Code", Box::new(ToCode)),
        ("Change Cell to Markdown", Box::new(ToMarkdown)),
        ("Change Cell to Raw", Box::new(ToRaw)),
        ("Delete Cell", Box::new(DeleteCell)),
        ("Cut Cells", Box::new(CutCells)),
        ("Copy Cells", Box::new(CopyCells)),
        ("Paste Cells Below", Box::new(PasteCellsBelow)),
        ("Undo Cell Operation", Box::new(UndoCellOp)),
        ("Redo Cell Operation", Box::new(RedoCellOp)),
        ("Select All Cells", Box::new(SelectAllCells)),
        ("Move Cell Up", Box::new(MoveCellUp)),
        ("Move Cell Down", Box::new(MoveCellDown)),
        ("Merge Cell Below", Box::new(MergeBelow)),
        ("Split Cell at Cursor", Box::new(SplitCell)),
        ("Toggle Cell Output", Box::new(ToggleOutput)),
        ("Toggle Cell Input", Box::new(ToggleInput)),
        ("Toggle Line Numbers", Box::new(ToggleLineNumbers)),
        ("Toggle Line Numbers in All Cells", Box::new(ToggleAllLineNumbers)),
        ("Collapse Section", Box::new(CollapseSection)),
        ("Expand Section", Box::new(ExpandSection)),
        ("Collapse All Headings", Box::new(CollapseAllHeadings)),
        ("Expand All Headings", Box::new(ExpandAllHeadings)),
        ("Expand Long Output", Box::new(ToggleOutputScroll)),
        ("Next Error", Box::new(NextError)),
        ("Next Change", Box::new(NextChange)),
        ("Previous Change", Box::new(PrevChange)),
        ("Show or Hide Changes Since Last Commit", Box::new(ToggleChanges)),
        ("Previous Error", Box::new(PrevError)),
        ("Hide All Outputs", Box::new(HideAllOutputs)),
        ("Show All Outputs", Box::new(ShowAllOutputs)),
        ("Hide All Code", Box::new(HideAllCode)),
        ("Show All Code", Box::new(ShowAllCode)),
        ("Clear Cell Output", Box::new(ClearOutputs)),
        ("Clear All Outputs", Box::new(ClearAllOutputs)),
        ("Find in Notebook", Box::new(Find)),
        ("Find and Replace", Box::new(crate::notebook::find::ToggleReplace)),
        ("Reveal in Finder", Box::new(RevealInFinder)),
        ("Reload from Disk", Box::new(ReloadFromDisk)),
        ("Export as HTML", Box::new(ExportHtml)),
        ("Share…", Box::new(Share)),
        ("Open Link…", Box::new(OpenLink)),
        ("Ask Claude or Codex…", Box::new(AskAgent)),
        ("Join Live Session…", Box::new(JoinLive)),
        ("Add Comment", Box::new(AddComment)),
        ("Show Comments", Box::new(ToggleComments)),
        ("Duplicate Cell", Box::new(DuplicateCell)),
        ("Close Other Tabs", Box::new(CloseOtherTabs)),
        ("Close All Tabs", Box::new(CloseAllTabs)),
        ("Reopen Closed Tab", Box::new(ReopenClosedTab)),
        ("Open Extensions Folder", Box::new(OpenExtensionsFolder)),
        ("Install Command Line Tool…", Box::new(InstallCommandLineTool)),
        ("Reload Extensions", Box::new(ReloadExtensions)),
        ("Extensions…", Box::new(ShowExtensions)),
        ("Settings…", Box::new(ShowSettings)),
        ("Keyboard Shortcuts", Box::new(ShowShortcuts)),
        ("Toggle Full Screen", Box::new(ToggleFullScreen)),
        ("Emoji & Symbols", Box::new(ShowEmoji)),
        ("About Herse", Box::new(ShowAbout)),
    ]
}

impl CommandPalette {
    pub fn new(return_focus: Option<FocusHandle>, window: &Window) -> Self {
        let commands: Vec<_> = command_list()
            .into_iter()
            .map(|(label, action)| {
                let hint = shortcut_text(action.as_ref(), return_focus.as_ref(), window);
                (SharedString::from(label), action, hint)
            })
            .collect();
        let labels = commands.iter().map(|c| c.0.clone()).collect();
        Self {
            commands,
            labels,
            matcher: Matcher::new(Config::DEFAULT),
            matches: Vec::new(),
            match_ix: Vec::new(),
            return_focus,
        }
    }
}

/// A shortcut in macOS notation ("⇧⌘P", "⌘K ⌘T"), from the live keymap.
pub fn shortcut_text(action: &dyn Action, focus: Option<&FocusHandle>, window: &Window) -> Option<SharedString> {
    let bindings = match focus {
        Some(focus) => window.bindings_for_action_in(action, focus),
        None => window.bindings_for_action(action),
    };
    // Prefer the Cmd spelling when an action has several bindings.
    let binding = bindings
        .iter()
        .rev()
        .find(|b| b.keystrokes().first().is_some_and(|k| k.modifiers().platform))
        .or_else(|| bindings.last())?;
    let parts: Vec<String> = binding.keystrokes().iter().map(|ks| keystroke_glyphs(ks.modifiers(), ks.key())).collect();
    Some(parts.join(" ").into())
}

/// One keystroke as macOS writes it: modifiers in ⌃⌥⇧⌘ order, then the key.
pub fn keystroke_glyphs(m: &gpui_kit::Modifiers, key: &str) -> String {
    let mut s = String::new();
    if m.control {
        s.push('⌃');
    }
    if m.alt {
        s.push('⌥');
    }
    // A shifted punctuation key is its own character: ⇧/ is "?".
    let shifted = match key {
        "/" => Some("?"),
        "=" => Some("+"),
        "1" => Some("!"),
        "2" => Some("@"),
        "3" => Some("#"),
        ";" => Some(":"),
        "," => Some("<"),
        "." => Some(">"),
        _ => None,
    }
    .filter(|_| m.shift && !m.platform && !m.control && !m.alt);
    if let Some(ch) = shifted {
        return ch.to_string();
    }
    if m.shift {
        s.push('⇧');
    }
    if m.platform {
        s.push('⌘');
    }
    s.push_str(&match key {
        "enter" => "↩".to_string(),
        "escape" => "⎋".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "tab" => "⇥".to_string(),
        "backspace" => "⌫".to_string(),
        "delete" => "⌦".to_string(),
        "space" => "Space".to_string(),
        "=" | "+" => "+".to_string(),
        k if k.len() == 1 => k.to_uppercase(),
        k => {
            let mut c = k.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    });
    s
}

impl PickerDelegate for CommandPalette {
    fn confirm_verb(&self) -> &'static str {
        "to run"
    }

    fn placeholder(&self) -> SharedString {
        "Run a command".into()
    }

    fn update_matches(&mut self, query: &str, _cx: &mut Context<Picker<Self>>) {
        let hits = fuzzy(query, &self.labels, &mut self.matcher, 100);
        self.match_ix = hits.iter().map(|(ix, _)| *ix).collect();
        self.matches = hits
            .into_iter()
            .map(|(ix, positions)| Match {
                label: self.labels[ix].clone(),
                highlights: char_positions_to_ranges(&self.labels[ix], &positions),
                detail: None,
                hint: self.commands[ix].2.clone(),
                icon: None,
            })
            .collect();
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        let Some(&cmd_ix) = self.match_ix.get(ix) else { return true };
        let action = self.commands[cmd_ix].1.boxed_clone();
        // Run the command where the user was, not in the palette's input.
        if let Some(focus) = &self.return_focus {
            focus.focus(window, cx);
        }
        window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
        true
    }
}

// ------------------------------------------------------------------ outline

pub struct OutlinePicker {
    notebook: Entity<NotebookView>,
    /// (cell index, heading level, heading text)
    headings: Vec<(usize, usize, SharedString)>,
    labels: Vec<SharedString>,
    matcher: Matcher,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
}

impl OutlinePicker {
    pub fn new(notebook: Entity<NotebookView>, cx: &App) -> Self {
        let headings = notebook.read(cx).headings();
        let labels = headings.iter().map(|h| h.2.clone()).collect();
        Self {
            notebook,
            headings,
            labels,
            matcher: Matcher::new(Config::DEFAULT),
            matches: Vec::new(),
            match_ix: Vec::new(),
        }
    }
}

impl PickerDelegate for OutlinePicker {
    fn confirm_verb(&self) -> &'static str {
        "to go"
    }

    fn placeholder(&self) -> SharedString {
        "Go to heading".into()
    }

    fn update_matches(&mut self, query: &str, _cx: &mut Context<Picker<Self>>) {
        let hits = fuzzy(query, &self.labels, &mut self.matcher, 500);
        self.match_ix = hits.iter().map(|(ix, _)| *ix).collect();
        self.matches = hits
            .into_iter()
            .map(|(ix, positions)| {
                let (cell, level, text) = &self.headings[ix];
                let indent = "    ".repeat(level.saturating_sub(1));
                let label = format!("{indent}{text}");
                let shift = indent.chars().count() as u32;
                let positions: Vec<u32> = positions.iter().map(|p| p + shift).collect();
                Match {
                    highlights: char_positions_to_ranges(&label, &positions),
                    label: label.into(),
                    detail: None,
                    hint: Some(format!("cell {}", cell + 1).into()),
                    icon: None,
                }
            })
            .collect();
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(&h) = self.match_ix.get(ix) {
            let cell = self.headings[h].0;
            self.notebook.update(cx, |nb, cx| nb.jump_to_cell(cell, window, cx));
        }
        true
    }

    fn empty_text(&self) -> SharedString {
        if self.headings.is_empty() {
            "This notebook has no markdown headings".into()
        } else {
            "No matching headings".into()
        }
    }
}

// ------------------------------------------------------------------ kernels

pub struct KernelPicker {
    notebook: Entity<NotebookView>,
    specs: Vec<herse_kernel::KernelSpec>,
    labels: Vec<SharedString>,
    current: Option<String>,
    matcher: Matcher,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
}

impl KernelPicker {
    pub fn new(notebook: Entity<NotebookView>, cx: &App) -> Self {
        let specs = herse_kernel::discover();
        let labels = specs.iter().map(|s| SharedString::from(s.display_name.clone())).collect();
        let current = notebook.read(cx).doc.nb.kernel_name().map(str::to_string);
        Self {
            notebook,
            specs,
            labels,
            current,
            matcher: Matcher::new(Config::DEFAULT),
            matches: Vec::new(),
            match_ix: Vec::new(),
        }
    }
}

impl PickerDelegate for KernelPicker {
    fn confirm_verb(&self) -> &'static str {
        "to switch"
    }

    fn placeholder(&self) -> SharedString {
        "Select a kernel".into()
    }

    fn update_matches(&mut self, query: &str, _cx: &mut Context<Picker<Self>>) {
        let hits = fuzzy(query, &self.labels, &mut self.matcher, 100);
        self.match_ix = hits.iter().map(|(ix, _)| *ix).collect();
        self.matches = hits
            .into_iter()
            .map(|(ix, positions)| {
                let spec = &self.specs[ix];
                Match {
                    label: self.labels[ix].clone(),
                    highlights: char_positions_to_ranges(&self.labels[ix], &positions),
                    detail: Some(format!("{} · {}", spec.language, spec.resource_dir.display()).into()),
                    hint: (self.current.as_deref() == Some(spec.name.as_str())).then(|| "current".into()),
                    icon: None,
                }
            })
            .collect();
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(&spec_ix) = self.match_ix.get(ix) {
            let spec = self.specs[spec_ix].clone();
            self.notebook.update(cx, |nb, cx| nb.set_kernel(&spec, cx));
        }
        true
    }

    fn empty_text(&self) -> SharedString {
        if self.specs.is_empty() {
            "No kernels installed — python3 -m pip install ipykernel".into()
        } else {
            "No matching kernels".into()
        }
    }
}

// ------------------------------------------------------------------ workspace search

pub struct WorkspaceSearch {
    root: Option<PathBuf>,
    query: String,
    hits: Vec<herse_workspace::SearchHit>,
    matches: Vec<Match>,
    cancel: Arc<AtomicBool>,
    status: Option<SharedString>,
    _task: Option<Task<()>>,
    on_open: OnOpenHit,
}

const MAX_SHOWN_HITS: usize = 2000;

impl WorkspaceSearch {
    pub fn new(
        root: Option<PathBuf>,
        on_open: impl Fn(PathBuf, usize, String, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            root,
            query: String::new(),
            hits: Vec::new(),
            matches: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            status: None,
            _task: None,
            on_open: Arc::new(on_open),
        }
    }

    fn push_hits(&mut self, batch: Vec<herse_workspace::SearchHit>) {
        for hit in batch {
            if self.hits.len() >= MAX_SHOWN_HITS {
                break;
            }
            let rel = self
                .root
                .as_ref()
                .and_then(|r| hit.path.strip_prefix(r).ok())
                .unwrap_or(&hit.path)
                .to_string_lossy()
                .into_owned();
            let where_ = match hit.location {
                herse_workspace::HitLocation::Source => format!("{rel} · cell {}", hit.cell_index + 1),
                herse_workspace::HitLocation::Output { .. } => format!("{rel} · cell {} output", hit.cell_index + 1),
            };
            self.matches.push(Match {
                label: SharedString::from(hit.line_text.clone()),
                highlights: hit.ranges.clone(),
                detail: Some(where_.into()),
                hint: None,
                icon: None,
            });
            self.hits.push(hit);
        }
    }
}

impl PickerDelegate for WorkspaceSearch {
    fn placeholder(&self) -> SharedString {
        "Search all notebooks in this folder".into()
    }

    fn update_matches(&mut self, query: &str, cx: &mut Context<Picker<Self>>) {
        // Cancel the search still running for the previous keystroke.
        self.cancel.store(true, Ordering::Relaxed);
        self.hits.clear();
        self.matches.clear();
        self.query = query.to_string();
        let Some(root) = self.root.clone() else {
            self.status = Some("Open a folder to search it".into());
            return;
        };
        if query.trim().chars().count() < 2 {
            self.status = None;
            self._task = None;
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.status = Some("Searching…".into());
        let mut q = herse_workspace::SearchQuery::new(query.to_string());
        q.case_sensitive = query.chars().any(char::is_uppercase);
        q.include_outputs = true;
        q.max_hits = Some(MAX_SHOWN_HITS);
        enum Msg {
            Hit(herse_workspace::SearchHit),
            Done(anyhow::Result<herse_workspace::SearchStats>),
        }
        let (tx, rx) = herse_kernel::async_channel::unbounded();
        cx.background_spawn(async move {
            let result = herse_workspace::search(&root, &q, &cancel, |hit| {
                let _ = tx.try_send(Msg::Hit(hit));
            });
            let _ = tx.try_send(Msg::Done(result));
        })
        .detach();
        self._task = Some(cx.spawn(async move |picker, cx| {
            while let Ok(first) = rx.recv().await {
                let mut hits = Vec::new();
                let mut done = None;
                let mut handle = |msg: Msg| match msg {
                    Msg::Hit(h) => hits.push(h),
                    Msg::Done(r) => done = Some(r),
                };
                handle(first);
                while let Ok(msg) = rx.try_recv() {
                    handle(msg);
                }
                let finished = done.is_some();
                let ok = picker.update(cx, |picker, cx| {
                    picker.delegate.push_hits(hits);
                    if let Some(result) = done {
                        picker.delegate.status = Some(match result {
                            Ok(s) => format!(
                                "{} match{} in {} notebook{} · {:.0} ms{}",
                                s.hits,
                                if s.hits == 1 { "" } else { "es" },
                                s.files_matched,
                                if s.files_matched == 1 { "" } else { "s" },
                                s.elapsed.as_secs_f64() * 1000.,
                                if s.truncated { " · showing the first 2,000" } else { "" },
                            )
                            .into(),
                            Err(e) => format!("{e:#}").into(),
                        });
                    }
                    cx.notify();
                });
                if ok.is_err() || finished {
                    break;
                }
                // Let a burst accumulate rather than re-rendering per hit.
                cx.background_executor().timer(std::time::Duration::from_millis(16)).await;
            }
        }));
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(hit) = self.hits.get(ix) {
            (self.on_open)(hit.path.clone(), hit.cell_index, self.query.clone(), window, cx);
        }
        true
    }

    fn empty_text(&self) -> SharedString {
        if self.query.trim().chars().count() < 2 {
            "Type to search cell sources and outputs in every notebook".into()
        } else if self.status.as_deref() == Some("Searching…") {
            "Searching…".into()
        } else {
            "No matches".into()
        }
    }

    fn status(&self) -> Option<SharedString> {
        self.status.clone()
    }
}

// ------------------------------------------------------------------ ⌘K

/// What a ⌘K row does.
enum MenuTarget {
    Command(Box<dyn Action>),
    Notebook(PathBuf),
    Heading(usize),
}

/// Narrows ⌘K to one kind of row.
type KindFilter = Option<fn(&MenuTarget) -> bool>;
/// What choosing a setting does.
type ApplySetting = Box<dyn Fn(&mut Window, &mut App)>;
/// Builds the settings rows from the current preferences.
type SettingRows = Box<dyn Fn(&App) -> Vec<SettingRow>>;

/// Which rows ⌘K shows: everything, or only commands (the shortcut sheet).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuScope {
    All,
    Commands,
}

/// The command menu (⌘K): commands with their keys, open and recent
/// notebooks, the folder's notebooks, and this notebook's headings, in one
/// fuzzy list. `>` narrows to commands, `#` to headings, `/` to notebooks.
pub struct CommandMenu {
    rows: Vec<MenuRow>,
    labels: Vec<SharedString>,
    /// Rows shown before anything is typed, in order.
    suggested: Vec<usize>,
    scope: MenuScope,
    matcher: Matcher,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
    return_focus: Option<FocusHandle>,
    on_open: OnOpen,
    notebook: Option<Entity<NotebookView>>,
    /// A pasted link (a notebook on the web, or a live session): the menu
    /// offers to open it and nothing else.
    link: Option<String>,
    /// What was typed, when it reads as a question for the assistant.
    ask: Option<String>,
}

struct MenuRow {
    label: SharedString,
    detail: Option<SharedString>,
    hint: Option<SharedString>,
    icon: &'static str,
    target: MenuTarget,
}

/// Commands the empty menu offers first.
const SUGGESTED_COMMANDS: &[&str] = &[
    "Run All Cells",
    "New Notebook",
    "Find in Notebook",
    "Go to Heading",
    "Change Kernel…",
    "Toggle Dark Mode",
    "Settings…",
    "Keyboard Shortcuts",
];

impl CommandMenu {
    /// `tabs` are the open notebooks, `recent` the recently opened ones,
    /// `files` the folder's notebooks; `notebook` the active one, for its
    /// headings.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scope: MenuScope,
        tabs: &[PathBuf],
        recent: &[PathBuf],
        files: &[PathBuf],
        root: Option<&std::path::Path>,
        notebook: Option<Entity<NotebookView>>,
        return_focus: Option<FocusHandle>,
        window: &Window,
        cx: &App,
        on_open: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    ) -> Self {
        let mut rows: Vec<MenuRow> = Vec::new();
        let mut suggested = Vec::new();
        let folder = |p: &PathBuf| -> Option<SharedString> {
            let dir = p.parent()?;
            let shown = match root.and_then(|r| dir.strip_prefix(r).ok()) {
                Some(rel) if rel.as_os_str().is_empty() => return None,
                Some(rel) => rel.display().to_string(),
                None => crate::workspace::tilde(dir),
            };
            Some(shown.into())
        };
        let name = |p: &PathBuf| SharedString::from(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if scope == MenuScope::All {
            let mut seen = std::collections::HashSet::new();
            for (paths, tag) in [(tabs, "Open"), (recent, "Recent"), (files, "")] {
                for p in paths {
                    if !seen.insert(p.clone()) {
                        continue;
                    }
                    if tag == "Recent" && !p.exists() {
                        continue;
                    }
                    let detail = match (tag, folder(p)) {
                        ("", f) => f,
                        (t, Some(f)) => Some(format!("{t} · {f}").into()),
                        (t, None) => Some(t.into()),
                    };
                    if !tag.is_empty() && suggested.len() < 6 {
                        suggested.push(rows.len());
                    }
                    rows.push(MenuRow { label: name(p), detail, hint: None, icon: "icons/herse-notebook.svg", target: MenuTarget::Notebook(p.clone()) });
                }
            }
        }
        let command_start = rows.len();
        for (label, action) in command_list() {
            let hint = shortcut_text(action.as_ref(), return_focus.as_ref(), window);
            rows.push(MenuRow { label: label.into(), detail: None, hint, icon: "icons/command.svg", target: MenuTarget::Command(action) });
        }
        // What the installed extensions add.
        for (label, action, allowed) in crate::extensions::commands(cx) {
            rows.push(MenuRow {
                label: label.into(),
                // One not allowed to run yet asks when chosen.
                detail: Some(if allowed { "Extension" } else { "Extension · asks first" }.into()),
                hint: None,
                icon: "icons/command.svg",
                target: MenuTarget::Command(action),
            });
        }
        for &want in SUGGESTED_COMMANDS {
            if let Some(ix) = rows[command_start..].iter().position(|r| r.label == want) {
                suggested.push(command_start + ix);
            }
        }
        if scope == MenuScope::Commands {
            // The shortcut sheet: every command, in list order.
            suggested = (command_start..rows.len()).collect();
        }
        if scope == MenuScope::All
            && let Some(nb) = &notebook
        {
            for (cell, level, text) in nb.read(cx).headings() {
                rows.push(MenuRow {
                    label: text,
                    detail: Some(format!("Heading {level}").into()),
                    hint: None,
                    icon: "icons/hash.svg",
                    target: MenuTarget::Heading(cell),
                });
            }
        }
        // Commands also match words people use for them ("theme" finds the
        // appearance commands); only the label shows.
        let labels = rows
            .iter()
            .map(|r| match (&r.target, keywords(&r.label)) {
                (MenuTarget::Command(_), Some(words)) => SharedString::from(format!("{} {words}", r.label)),
                _ => r.label.clone(),
            })
            .collect();
        Self {
            rows,
            labels,
            suggested,
            scope,
            matcher: Matcher::new(Config::DEFAULT),
            matches: Vec::new(),
            match_ix: Vec::new(),
            return_focus,
            on_open: Box::new(on_open),
            notebook,
            link: None,
            ask: None,
        }
    }

    fn row_match(&self, ix: usize, highlights: Vec<std::ops::Range<usize>>) -> Match {
        let row = &self.rows[ix];
        // Hits in the hidden keywords have nothing to mark.
        let shown = row.label.len();
        let highlights = highlights.into_iter().filter(|r| r.end <= shown).collect();
        Match {
            label: row.label.clone(),
            highlights,
            detail: row.detail.clone(),
            hint: row.hint.clone(),
            icon: Some(row.icon),
        }
    }
}

impl PickerDelegate for CommandMenu {
    fn confirm_verb(&self) -> &'static str {
        "to go"
    }

    fn placeholder(&self) -> SharedString {
        match self.scope {
            MenuScope::All => "Search commands, notebooks and headings…".into(),
            MenuScope::Commands => "Keyboard shortcuts".into(),
        }
    }

    fn update_matches(&mut self, query: &str, _cx: &mut Context<Picker<Self>>) {
        let pasted = query.trim();
        // A question (a few words, or one ending in "?"): offer to ask it.
        self.ask = (self.scope == MenuScope::All
            && !pasted.starts_with(['>', '#', '/'])
            && (pasted.ends_with('?') || pasted.split_whitespace().count() >= 4))
            .then(|| pasted.to_string());
        self.link = (self.scope == MenuScope::All && (pasted.starts_with("herse://") || crate::remote::is_web_link(pasted)))
            .then(|| pasted.to_string());
        if let Some(link) = &self.link {
            let (label, icon) = if link.starts_with("herse://") {
                ("Join Live Session", "icons/users.svg")
            } else {
                ("Open Link", "icons/link.svg")
            };
            self.match_ix = Vec::new();
            self.matches = vec![Match {
                label: label.into(),
                highlights: Vec::new(),
                detail: Some(link.clone().into()),
                hint: None,
                icon: Some(icon),
            }];
            return;
        }
        // A leading sigil narrows the kind.
        let (kind, query): (KindFilter, &str) = match query.chars().next() {
            Some('>') => (Some(|t| matches!(t, MenuTarget::Command(_))), &query[1..]),
            Some('#') => (Some(|t| matches!(t, MenuTarget::Heading(_))), &query[1..]),
            Some('/') => (Some(|t| matches!(t, MenuTarget::Notebook(_))), &query[1..]),
            _ => (None, query),
        };
        let query = query.trim();
        let keep = |ix: &usize| kind.is_none_or(|k| k(&self.rows[*ix].target));
        let hits: Vec<(usize, Vec<std::ops::Range<usize>>)> = if query.is_empty() {
            let ixs: Vec<usize> = match kind {
                None => self.suggested.clone(),
                Some(_) => (0..self.rows.len()).filter(keep).collect(),
            };
            ixs.into_iter().map(|ix| (ix, Vec::new())).collect()
        } else {
            // Labels that contain the query as typed come first (fuzzy order
            // within each group), so "not" finds "Notebook" before
            // "nbformat_4_0_no_ids".
            let needle = query.to_lowercase();
            let (mut exact, loose): (Vec<_>, Vec<_>) = fuzzy(query, &self.labels, &mut self.matcher, 400)
                .into_iter()
                .filter(|(ix, _)| keep(ix))
                .partition(|(ix, _)| self.labels[*ix].to_lowercase().contains(&needle));
            // Among those, a word starting with the query first, then
            // commands before notebooks before headings (stable: fuzzy
            // order breaks ties). "share" runs Share… before opening
            // share_test.ipynb.
            let rows = &self.rows;
            let labels = &self.labels;
            exact.sort_by_key(|(ix, _)| {
                let label = labels[*ix].to_lowercase();
                let word_start = label.starts_with(&needle) || label.contains(&format!(" {needle}"));
                let kind = match rows[*ix].target {
                    MenuTarget::Command(_) => 0,
                    MenuTarget::Notebook(_) => 1,
                    MenuTarget::Heading(_) => 2,
                };
                (!word_start, kind)
            });
            exact.extend(loose);
            exact
                .into_iter()
                .take(100)
                .map(|(ix, pos)| (ix, char_positions_to_ranges(&self.labels[ix], &pos)))
                .collect()
        };
        self.match_ix = hits.iter().map(|(ix, _)| *ix).collect();
        self.matches = hits.into_iter().map(|(ix, hl)| self.row_match(ix, hl)).collect();
        // The question goes first: few commands read like sentences.
        if let Some(question) = &self.ask {
            let preferred = crate::settings::assistant().and_then(|id| crate::agent::Provider::from_id(&id));
            let name = crate::agent::Provider::installed(preferred).first().map_or("Claude", |p| p.name());
            self.matches.insert(
                0,
                Match {
                    label: format!("Ask {name}").into(),
                    highlights: Vec::new(),
                    detail: Some(question.clone().into()),
                    hint: Some("⌘I".into()),
                    icon: Some("icons/sparkles.svg"),
                },
            );
        }
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(link) = self.link.clone() {
            if let Some(focus) = &self.return_focus {
                focus.focus(window, cx);
            }
            window.defer(cx, move |window, cx| window.dispatch_action(Box::new(OpenLinkText(link)), cx));
            return true;
        }
        let ix = match &self.ask {
            Some(question) if ix == 0 => {
                let question = question.clone();
                if let Some(focus) = &self.return_focus {
                    focus.focus(window, cx);
                }
                window.defer(cx, move |window, cx| window.dispatch_action(Box::new(AskAgentText(question)), cx));
                return true;
            }
            Some(_) => ix - 1,
            None => ix,
        };
        let Some(&row) = self.match_ix.get(ix) else { return true };
        match &self.rows[row].target {
            MenuTarget::Command(action) => {
                let action = action.boxed_clone();
                if let Some(focus) = &self.return_focus {
                    focus.focus(window, cx);
                }
                window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
            }
            MenuTarget::Notebook(path) => (self.on_open)(path.clone(), window, cx),
            MenuTarget::Heading(cell) => {
                if let Some(nb) = &self.notebook {
                    let cell = *cell;
                    nb.update(cx, |nb, cx| nb.jump_to_cell(cell, window, cx));
                }
            }
        }
        true
    }

    fn empty_text(&self) -> SharedString {
        "Nothing matches".into()
    }
}

// ------------------------------------------------------------------ settings

/// A setting's row and what choosing it does.
pub struct SettingRow {
    pub label: SharedString,
    /// The current value, or a check mark for the chosen option.
    pub value: Option<SharedString>,
    pub apply: ApplySetting,
}

/// Settings (⌘,): the preferences, in the ⌘K overlay. Choosing a row applies
/// it at once and leaves the list open to see the change.
pub struct SettingsMenu {
    placeholder: SharedString,
    rows: SettingRows,
    current: Vec<SettingRow>,
    query: String,
    matches: Vec<Match>,
    match_ix: Vec<usize>,
}

impl SettingsMenu {
    pub fn new(rows: impl Fn(&App) -> Vec<SettingRow> + 'static) -> Self {
        Self {
            placeholder: "Settings".into(),
            rows: Box::new(rows),
            current: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            match_ix: Vec::new(),
        }
    }

    /// A list of its own within Settings ("Extensions").
    pub fn titled(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn refresh(&mut self, cx: &App) {
        self.current = (self.rows)(cx);
        let q = self.query.to_lowercase();
        self.match_ix = (0..self.current.len()).filter(|&ix| self.current[ix].label.to_lowercase().contains(&q)).collect();
        self.matches = self
            .match_ix
            .iter()
            .map(|&ix| Match {
                label: self.current[ix].label.clone(),
                highlights: Vec::new(),
                detail: None,
                hint: self.current[ix].value.clone(),
                icon: Some("icons/settings.svg"),
            })
            .collect();
    }
}

impl PickerDelegate for SettingsMenu {
    fn confirm_verb(&self) -> &'static str {
        "to choose"
    }

    fn placeholder(&self) -> SharedString {
        self.placeholder.clone()
    }

    fn update_matches(&mut self, query: &str, cx: &mut Context<Picker<Self>>) {
        self.query = query.to_string();
        self.refresh(cx);
    }

    fn matches(&self) -> &[Match] {
        &self.matches
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        if let Some(&row) = self.match_ix.get(ix) {
            (self.current[row].apply)(window, cx);
        }
        self.refresh(cx);
        cx.notify();
        false
    }
}
