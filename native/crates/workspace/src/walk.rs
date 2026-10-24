//! Folder walk: directories + notebooks (optionally other files), gitignore-
//! aware, in tree order (pre-order, directories first, natural sort).

use crate::natural::natural_cmp;
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Directories that are never part of a notebook workspace (same list the
/// Electron app used). Matched against the exact directory name at any depth,
/// even when hidden files are shown.
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".ipynb_checkpoints",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    "dist",
    "build",
    "target",
];

/// Extensions listed by [`OtherFiles::Text`].
pub const TEXT_EXTENSIONS: &[&str] = &[
    "py", "pyi", "md", "markdown", "rst", "txt", "json", "jsonl", "geojson", "csv", "tsv", "toml", "yaml",
    "yml", "ini", "cfg", "conf", "r", "rmd", "qmd", "jl", "sql", "sh", "bash", "zsh", "js", "ts", "jsx",
    "tsx", "html", "htm", "css", "xml", "svg", "tex", "bib", "log", "lock", "rs", "go", "c", "h", "cpp",
    "hpp", "java", "scala", "kt", "lua", "m", "swift", "proto", "dockerfile", "makefile",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Dir,
    Notebook,
    /// Any other file; the lowercased extension without the dot ("" if none).
    Other(String),
}

/// Which non-notebook files [`walk`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtherFiles {
    /// Directories and `.ipynb` only.
    #[default]
    None,
    /// Plus files whose extension is in [`TEXT_EXTENSIONS`].
    Text,
    /// Plus every file.
    All,
}

#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub other_files: OtherFiles,
    /// List dot-files and descend into dot-directories (default false).
    pub include_hidden: bool,
    /// Honour `.gitignore` / `.ignore` / `.git/info/exclude` / the global git
    /// excludes file. `.gitignore` is honoured even outside a git repository.
    pub respect_gitignore: bool,
    /// Directory names skipped at any depth (default [`DEFAULT_IGNORED_DIRS`]).
    pub ignored_dirs: Vec<String>,
    /// Drop directories whose subtree contains no notebook (and no file
    /// matching [`WalkOptions::listed_extensions`]).
    pub prune_empty_dirs: bool,
    /// Also list files whose name ends in one of these extensions (lowercase,
    /// without the leading dot; `csv.gz` works), as [`EntryKind::Other`],
    /// whatever `other_files` says. Pruning keeps the directories holding them.
    pub listed_extensions: Vec<String>,
    pub follow_links: bool,
    /// Maximum depth below the root (1 = only the root's children).
    pub max_depth: Option<usize>,
    /// Stop after this many entries (sets [`Walk::truncated`]).
    pub max_entries: Option<usize>,
    /// Walker threads, 0 = automatic.
    pub threads: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            other_files: OtherFiles::None,
            include_hidden: false,
            respect_gitignore: true,
            ignored_dirs: DEFAULT_IGNORED_DIRS.iter().map(|s| s.to_string()).collect(),
            prune_empty_dirs: false,
            listed_extensions: Vec::new(),
            follow_links: false,
            max_depth: None,
            max_entries: Some(200_000),
            threads: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: PathBuf,
    /// Path relative to the root, `/`-separated (what the fuzzy finder indexes).
    pub rel: String,
    /// 0 for the root's direct children.
    pub depth: usize,
    pub kind: EntryKind,
    /// File size in bytes (0 for directories).
    pub size: u64,
    pub mtime: SystemTime,
    /// Number of notebooks in this directory's subtree (1 for a notebook, 0 otherwise).
    pub notebooks: u32,
}

impl WalkEntry {
    /// Last path component.
    pub fn name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn is_notebook(&self) -> bool {
        self.kind == EntryKind::Notebook
    }
}

/// Result of [`walk_report`].
#[derive(Debug, Clone, Default)]
pub struct Walk {
    pub entries: Vec<WalkEntry>,
    /// `max_entries` was hit; the listing is incomplete.
    pub truncated: bool,
    /// Entries that could not be read (permissions, races with deletes).
    pub errors: usize,
}

/// True for `*.ipynb` (case-insensitive).
pub fn is_notebook_path(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

/// Lists `root` recursively. See [`walk_report`].
pub fn walk(root: &Path, opts: &WalkOptions) -> Vec<WalkEntry> {
    walk_report(root, opts).entries
}

/// Builds the gitignore-aware walker used by both [`walk`] and [`crate::search`].
pub(crate) fn walker(root: &Path, opts: &WalkOptions) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.hidden(!opts.include_hidden)
        // Ignore files above the root don't apply: a folder you open is shown.
        // pip and uv put a `.gitignore` of `*` in every site-packages and venv,
        // which would otherwise hide every notebook under them.
        .parents(false)
        .ignore(opts.respect_gitignore)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .require_git(false)
        .follow_links(opts.follow_links)
        .max_depth(opts.max_depth)
        .threads(opts.threads);
    if !opts.ignored_dirs.is_empty() {
        let set: Arc<HashSet<OsString>> = Arc::new(opts.ignored_dirs.iter().map(OsString::from).collect());
        b.filter_entry(move |e| {
            !(e.depth() > 0 && e.file_type().is_some_and(|t| t.is_dir()) && set.contains(e.file_name()))
        });
    }
    b
}

/// Relative `/`-separated path of `path` under `root`.
pub(crate) fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let s = rel.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// Whether `name` ends in one of `listed` (see [`WalkOptions::listed_extensions`]).
fn is_listed(name: &Path, listed: &[String]) -> bool {
    if listed.is_empty() {
        return false;
    }
    let Some(file) = name.file_name() else { return false };
    let file = file.to_string_lossy().to_ascii_lowercase();
    listed.iter().any(|ext| {
        file.len() > ext.len() + 1 && file.ends_with(ext.as_str()) && file.as_bytes()[file.len() - ext.len() - 1] == b'.'
    })
}

fn classify_file(name: &Path, opts: &WalkOptions) -> Option<EntryKind> {
    if is_notebook_path(name) {
        return Some(EntryKind::Notebook);
    }
    let other = opts.other_files;
    let ext = name.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if other != OtherFiles::All && is_listed(name, &opts.listed_extensions) {
        return Some(EntryKind::Other(ext));
    }
    if other == OtherFiles::None {
        return None;
    }
    match other {
        OtherFiles::All => Some(EntryKind::Other(ext)),
        OtherFiles::Text => {
            let key = if ext.is_empty() {
                name.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
            } else {
                ext.clone()
            };
            TEXT_EXTENSIONS.contains(&key.as_str()).then_some(EntryKind::Other(ext))
        }
        OtherFiles::None => None,
    }
}

struct Raw {
    path: PathBuf,
    rel: String,
    kind: EntryKind,
    /// A file [`WalkOptions::listed_extensions`] asked for: keeps its directory.
    listed: bool,
    size: u64,
    mtime: SystemTime,
}

struct Shared<'s> {
    root: &'s Path,
    opts: &'s WalkOptions,
    sink: Mutex<Vec<Raw>>,
    count: AtomicUsize,
    truncated: AtomicBool,
    errors: AtomicUsize,
}

struct Collector<'s> {
    shared: &'s Shared<'s>,
    local: Vec<Raw>,
}

impl Collector<'_> {
    fn accept(&mut self, entry: DirEntry) -> WalkState {
        if entry.depth() == 0 {
            return WalkState::Continue;
        }
        let Some(ft) = entry.file_type() else {
            return WalkState::Continue;
        };
        let opts = self.shared.opts;
        let (kind, md) = if ft.is_dir() {
            (EntryKind::Dir, entry.metadata().ok())
        } else if ft.is_file() {
            let Some(kind) = classify_file(entry.path(), opts) else {
                return WalkState::Continue;
            };
            (kind, entry.metadata().ok())
        } else if ft.is_symlink() {
            // Unfollowed symlink: list it if it points at a file we'd list.
            let Some(kind) = classify_file(entry.path(), opts) else {
                return WalkState::Continue;
            };
            match std::fs::metadata(entry.path()) {
                Ok(md) if md.is_file() => (kind, Some(md)),
                _ => return WalkState::Continue,
            }
        } else {
            return WalkState::Continue;
        };
        let Some(md) = md else {
            self.shared.errors.fetch_add(1, AtomicOrdering::Relaxed);
            return WalkState::Continue;
        };
        if let Some(max) = opts.max_entries
            && self.shared.count.fetch_add(1, AtomicOrdering::Relaxed) >= max
        {
            self.shared.truncated.store(true, AtomicOrdering::Relaxed);
            return WalkState::Quit;
        }
        let rel = rel_path(self.shared.root, entry.path());
        let size = if kind == EntryKind::Dir { 0 } else { md.len() };
        let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let listed = matches!(kind, EntryKind::Other(_)) && is_listed(entry.path(), &opts.listed_extensions);
        self.local.push(Raw { path: entry.into_path(), rel, kind, listed, size, mtime });
        WalkState::Continue
    }
}

impl ParallelVisitor for Collector<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        match entry {
            Ok(e) => self.accept(e),
            Err(err) => {
                log::debug!("walk: {err}");
                self.shared.errors.fetch_add(1, AtomicOrdering::Relaxed);
                WalkState::Continue
            }
        }
    }
}

impl Drop for Collector<'_> {
    fn drop(&mut self) {
        if let Ok(mut sink) = self.shared.sink.lock() {
            sink.append(&mut self.local);
        }
    }
}

struct Builder<'s>(&'s Shared<'s>);

impl<'s> ParallelVisitorBuilder<'s> for Builder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Collector { shared: self.0, local: Vec::with_capacity(1024) })
    }
}

/// Lists `root` recursively in tree order: each directory is followed by its
/// subtree; siblings are ordered directories first, then by [`natural_cmp`].
/// The root itself is not included. Never fails: unreadable entries are
/// counted in [`Walk::errors`] and skipped.
pub fn walk_report(root: &Path, opts: &WalkOptions) -> Walk {
    let shared = Shared {
        root,
        opts,
        sink: Mutex::new(Vec::new()),
        count: AtomicUsize::new(0),
        truncated: AtomicBool::new(false),
        errors: AtomicUsize::new(0),
    };
    walker(root, opts).build_parallel().visit(&mut Builder(&shared));
    let raws = shared.sink.into_inner().unwrap_or_else(|e| e.into_inner());
    Walk {
        entries: assemble(raws, opts.prune_empty_dirs),
        truncated: shared.truncated.into_inner(),
        errors: shared.errors.into_inner(),
    }
}

fn name_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Sorts `children` the way the tree shows them.
pub(crate) fn tree_cmp(a_dir: bool, a_name: &str, b_dir: bool, b_name: &str) -> Ordering {
    b_dir.cmp(&a_dir).then_with(|| natural_cmp(a_name, b_name))
}

fn assemble(raws: Vec<Raw>, prune: bool) -> Vec<WalkEntry> {
    let n = raws.len();
    let root = n; // virtual root node index
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n + 1];
    {
        let dirs: HashMap<&str, usize> =
            raws.iter().enumerate().filter(|(_, r)| r.kind == EntryKind::Dir).map(|(i, r)| (r.rel.as_str(), i)).collect();
        for (i, r) in raws.iter().enumerate() {
            let parent = match r.rel.rfind('/') {
                None => Some(root),
                Some(p) => dirs.get(&r.rel[..p]).copied(),
            };
            // An orphan can only appear when the walk was truncated mid-way.
            if let Some(p) = parent {
                children[p].push(i as u32);
            }
        }
    }
    for list in &mut children {
        list.sort_by(|&a, &b| {
            let (ra, rb) = (&raws[a as usize], &raws[b as usize]);
            tree_cmp(ra.kind == EntryKind::Dir, name_of(&ra.rel), rb.kind == EntryKind::Dir, name_of(&rb.rel))
        });
    }

    // Pre-order traversal; notebook counts are filled in post-order.
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
    let mut counts = vec![0u32; n + 1];
    // Files that keep their directories when pruning: notebooks and listed files.
    let mut kept = vec![0u32; n + 1];
    while let Some(&mut (node, ref mut next)) = stack.last_mut() {
        if let Some(&child) = children[node].get(*next) {
            *next += 1;
            let child = child as usize;
            order.push(child as u32);
            match raws[child].kind {
                EntryKind::Dir => stack.push((child, 0)),
                EntryKind::Notebook => {
                    counts[child] = 1;
                    counts[node] += 1;
                    kept[node] += 1;
                }
                EntryKind::Other(_) => kept[node] += u32::from(raws[child].listed),
            }
        } else {
            stack.pop();
            if let Some(&(parent, _)) = stack.last() {
                counts[parent] += counts[node];
                kept[parent] += kept[node];
            }
        }
    }

    let mut slots: Vec<Option<Raw>> = raws.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(order.len());
    let mut skip_under: Option<String> = None;
    for idx in order {
        let idx = idx as usize;
        let Some(r) = slots[idx].take() else { continue };
        if let Some(prefix) = &skip_under {
            if r.rel.len() > prefix.len() && r.rel.starts_with(prefix.as_str()) && r.rel.as_bytes()[prefix.len()] == b'/' {
                continue;
            }
            skip_under = None;
        }
        if prune && r.kind == EntryKind::Dir && kept[idx] == 0 {
            skip_under = Some(r.rel);
            continue;
        }
        let depth = r.rel.bytes().filter(|&c| c == b'/').count();
        out.push(WalkEntry {
            path: r.path,
            depth,
            kind: r.kind,
            size: r.size,
            mtime: r.mtime,
            notebooks: counts[idx],
            rel: r.rel,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "{}").unwrap();
    }

    fn rels(entries: &[WalkEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.rel.as_str()).collect()
    }

    #[test]
    fn listed_extensions_keep_their_folders() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for f in ["a.ipynb", "data/sales.csv", "data/raw/events.CSV.GZ", "data/readme.md", "empty/notes.md", "x.parquet"] {
            let p = r.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "x").unwrap();
        }
        let opts = WalkOptions {
            prune_empty_dirs: true,
            listed_extensions: vec!["csv".into(), "csv.gz".into(), "parquet".into()],
            ..Default::default()
        };
        let got = walk(r, &opts);
        assert_eq!(rels(&got), ["data", "data/raw", "data/raw/events.CSV.GZ", "data/sales.csv", "a.ipynb", "x.parquet"]);
        assert_eq!(got[3].kind, EntryKind::Other("csv".into()));
        assert_eq!(got[0].notebooks, 0, "data files aren't notebooks");
        // Without the option, data folders are pruned as before.
        let plain = walk(r, &WalkOptions { prune_empty_dirs: true, ..Default::default() });
        assert_eq!(rels(&plain), ["a.ipynb"]);
    }

    #[test]
    fn skip_rules_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for f in [
            "nb10.ipynb",
            "nb2.ipynb",
            "Alpha.IPYNB",
            "notes.md",
            "script.py",
            "zeta/inner.ipynb",
            "a_dir/deep/x.ipynb",
            "a_dir/b.ipynb",
            "empty_dir/readme.txt",
            ".hidden.ipynb",
            ".hidden_dir/h.ipynb",
            ".ipynb_checkpoints/nb2-checkpoint.ipynb",
            "zeta/.ipynb_checkpoints/inner-checkpoint.ipynb",
            "node_modules/pkg/demo.ipynb",
            "__pycache__/x.ipynb",
            "venv/lib/demo.ipynb",
            ".venv/lib/demo.ipynb",
            ".git/objects/x.ipynb",
        ] {
            touch(r, f);
        }
        let entries = walk(r, &WalkOptions::default());
        assert_eq!(
            rels(&entries),
            [
                "a_dir",
                "a_dir/deep",
                "a_dir/deep/x.ipynb",
                "a_dir/b.ipynb",
                "empty_dir",
                "zeta",
                "zeta/inner.ipynb",
                "Alpha.IPYNB",
                "nb2.ipynb",
                "nb10.ipynb",
            ]
        );
        let a = &entries[0];
        assert_eq!((a.depth, a.notebooks, a.kind.clone()), (0, 2, EntryKind::Dir));
        assert_eq!(entries[2].depth, 2);
        assert_eq!(entries[2].size, 2);
        assert_eq!(entries[2].path, r.join("a_dir/deep/x.ipynb"));
        assert_eq!(entries[4].notebooks, 0);

        // Pruning drops directories without notebooks.
        let pruned = walk(r, &WalkOptions { prune_empty_dirs: true, ..Default::default() });
        assert!(!rels(&pruned).contains(&"empty_dir"));
        assert_eq!(pruned.len(), entries.len() - 1);

        // Hidden files on: dot entries show up, but .git / checkpoints stay out.
        let hidden = walk(r, &WalkOptions { include_hidden: true, ..Default::default() });
        let hr = rels(&hidden);
        assert!(hr.contains(&".hidden.ipynb") && hr.contains(&".hidden_dir/h.ipynb"));
        assert!(!hr.iter().any(|p| p.contains(".git/") || p.contains("checkpoint") || p.starts_with(".venv")));

        // Other files.
        let text = walk(r, &WalkOptions { other_files: OtherFiles::Text, ..Default::default() });
        let tr = rels(&text);
        assert!(tr.contains(&"notes.md") && tr.contains(&"script.py") && tr.contains(&"empty_dir/readme.txt"));
        let md = text.iter().find(|e| e.rel == "notes.md").unwrap();
        assert_eq!(md.kind, EntryKind::Other("md".into()));
        // Files sort after directories, naturally.
        let top: Vec<&str> = text.iter().filter(|e| e.depth == 0).map(|e| e.name()).collect();
        assert_eq!(top, ["a_dir", "empty_dir", "zeta", "Alpha.IPYNB", "nb2.ipynb", "nb10.ipynb", "notes.md", "script.py"]);
    }

    #[test]
    fn gitignore_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::write(r.join(".gitignore"), "scratch/\n*.secret.ipynb\n!keep.secret.ipynb\n/rootonly.ipynb\n").unwrap();
        for f in [
            "a.ipynb",
            "scratch/s.ipynb",
            "x.secret.ipynb",
            "keep.secret.ipynb",
            "rootonly.ipynb",
            "sub/rootonly.ipynb",
            "sub/y.secret.ipynb",
        ] {
            touch(r, f);
        }
        fs::write(r.join("sub/.gitignore"), "local.ipynb\n").unwrap();
        touch(r, "sub/local.ipynb");
        let entries = walk(r, &WalkOptions::default());
        assert_eq!(rels(&entries), ["sub", "sub/rootonly.ipynb", "a.ipynb", "keep.secret.ipynb"]);

        let all = walk(r, &WalkOptions { respect_gitignore: false, ..Default::default() });
        assert_eq!(all.iter().filter(|e| e.is_notebook()).count(), 8);
    }

    #[test]
    fn truncation_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for i in 0..50 {
            touch(r, &format!("d{}/n{i}.ipynb", i % 5));
        }
        let w = walk_report(r, &WalkOptions { max_entries: Some(10), ..Default::default() });
        assert!(w.truncated);
        assert!(w.entries.len() <= 10);
        let shallow = walk(r, &WalkOptions { max_depth: Some(1), ..Default::default() });
        assert_eq!(rels(&shallow), ["d0", "d1", "d2", "d3", "d4"]);
    }

    #[test]
    fn an_ignore_file_above_the_root_hides_nothing() {
        // site-packages and venvs carry a `.gitignore` of `*`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*\n").unwrap();
        let root = dir.path().join("examples");
        touch(&root, "a.ipynb");
        touch(&root, "sub/b.ipynb");
        assert_eq!(rels(&walk(&root, &WalkOptions::default())), ["sub", "sub/b.ipynb", "a.ipynb"]);
    }
}
