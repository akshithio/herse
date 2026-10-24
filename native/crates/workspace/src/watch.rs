//! Debounced recursive folder watching (notify + notify-debouncer-full).
//!
//! FSEvents (macOS) coalesces flags per path and cannot pair the two sides of
//! a rename, so each debounced batch is reconciled against the file system:
//! a path that no longer exists is `Removed`; one that exists is `Created`
//! (if the OS reported a create or an unpaired rename-to) or `Modified`.
//! Within a batch, exactly one vanished + one appeared rename path become a
//! single `Renamed { paths: [from, to] }`.
//!
//! Paths inside ignored directories (`.ipynb_checkpoints`, `.git`,
//! `node_modules`, ...) and hidden paths are dropped, so our own checkpoint
//! writes never wake the app. Paths are reported under `root` exactly as
//! given (FSEvents reports canonical `/private/var/...` paths; those are
//! mapped back).

use crate::walk::DEFAULT_IGNORED_DIRS;
use anyhow::{Context, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, Debouncer, NoCache, new_debouncer_opt};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchEventKind {
    /// The path exists now and did not before (as far as the OS reported).
    /// Can also mean "replaced" (atomic save = rename over); treat a `Created`
    /// for a path you already know like `Modified`. For a directory, re-walk
    /// it: files moved in along with it get no events of their own.
    Created,
    Removed,
    Modified,
    /// `paths == [from, to]`.
    Renamed,
    /// Events were dropped or the watcher hit an error; re-walk `paths[0]`
    /// (the root).
    Rescan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    pub kind: WatchEventKind,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Quiet period before a burst of changes to a path is reported.
    pub debounce: Duration,
    /// Report hidden paths (dot-files / inside dot-directories).
    pub include_hidden: bool,
    /// Directory names whose contents are never reported.
    pub ignored_dirs: Vec<String>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            debounce: Duration::from_millis(150),
            include_hidden: false,
            ignored_dirs: DEFAULT_IGNORED_DIRS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Watch guard; dropping it stops watching (the debouncer thread exits
/// within one tick).
pub struct Watcher {
    _debouncer: Debouncer<RecommendedWatcher, NoCache>,
    root: PathBuf,
}

impl Watcher {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").field("root", &self.root).finish()
    }
}

/// [`watch_with`] with default options (150 ms debounce).
pub fn watch(root: &Path, on_event: impl Fn(WatchEvent) + Send + 'static) -> Result<Watcher> {
    watch_with(root, &WatchOptions::default(), on_event)
}

/// Watches `root` recursively. `on_event` is called on the debouncer's own
/// thread (one event at a time, never concurrently), in batches every
/// `debounce / 4`.
pub fn watch_with(root: &Path, opts: &WatchOptions, on_event: impl Fn(WatchEvent) + Send + 'static) -> Result<Watcher> {
    let filter = Filter {
        root: root.to_path_buf(),
        canonical: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
        include_hidden: opts.include_hidden,
        ignored: opts.ignored_dirs.iter().map(OsString::from).collect(),
    };
    let handler = move |res: DebounceEventResult| match res {
        Ok(events) => {
            for ev in reconcile(&filter, events) {
                on_event(ev);
            }
        }
        Err(errors) => {
            for e in &errors {
                log::warn!("watch {}: {e}", filter.root.display());
            }
            on_event(WatchEvent { kind: WatchEventKind::Rescan, paths: vec![filter.root.clone()] });
        }
    };
    let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
        opts.debounce,
        None,
        handler,
        NoCache,
        notify::Config::default(),
    )
    .context("starting file watcher")?;
    debouncer.watch(root, RecursiveMode::Recursive).with_context(|| format!("watching {}", root.display()))?;
    Ok(Watcher { _debouncer: debouncer, root: root.to_path_buf() })
}

struct Filter {
    root: PathBuf,
    canonical: PathBuf,
    include_hidden: bool,
    ignored: HashSet<OsString>,
}

impl Filter {
    /// Maps an event path under `root` as the caller spelled it; `None` if it
    /// should not be reported.
    fn map(&self, p: &Path) -> Option<PathBuf> {
        let rel = p.strip_prefix(&self.canonical).or_else(|_| p.strip_prefix(&self.root)).ok()?;
        if rel.as_os_str().is_empty() {
            return None; // the root itself
        }
        for c in rel.components() {
            let Component::Normal(name) = c else { return None };
            if self.ignored.contains(name) {
                return None;
            }
            if !self.include_hidden && name.as_encoded_bytes().first() == Some(&b'.') {
                return None;
            }
        }
        Some(self.root.join(rel))
    }
}

#[derive(Default, Clone, Copy)]
struct Flags {
    created: bool,
    removed: bool,
    modified: bool,
    renamed: bool,
}

fn reconcile(filter: &Filter, events: Vec<DebouncedEvent>) -> Vec<WatchEvent> {
    let mut out = Vec::new();
    let mut order: Vec<PathBuf> = Vec::new();
    let mut flags: HashMap<PathBuf, Flags> = HashMap::new();
    let mut touch = |p: PathBuf, f: &dyn Fn(&mut Flags)| {
        let e = flags.entry(p.clone()).or_insert_with(|| {
            order.push(p);
            Flags::default()
        });
        f(e);
    };
    let mut rescan = false;
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for ev in events {
        if ev.event.need_rescan() {
            rescan = true;
            continue;
        }
        match ev.event.kind {
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.event.paths.len() == 2 => {
                let from = filter.map(&ev.event.paths[0]);
                let to = filter.map(&ev.event.paths[1]);
                match (from, to) {
                    (Some(a), Some(b)) => pairs.push((a, b)),
                    (Some(a), None) => touch(a, &|f| f.removed = true),
                    (None, Some(b)) => touch(b, &|f| f.created = true),
                    (None, None) => {}
                }
            }
            kind => {
                for p in &ev.event.paths {
                    let Some(p) = filter.map(p) else { continue };
                    match kind {
                        EventKind::Create(_) => touch(p, &|f| f.created = true),
                        EventKind::Remove(_) => touch(p, &|f| f.removed = true),
                        EventKind::Modify(ModifyKind::Name(_)) => touch(p, &|f| f.renamed = true),
                        EventKind::Access(_) => {}
                        _ => touch(p, &|f| f.modified = true),
                    }
                }
            }
        }
    }
    if rescan {
        out.push(WatchEvent { kind: WatchEventKind::Rescan, paths: vec![filter.root.clone()] });
    }
    for (from, to) in pairs {
        out.push(WatchEvent { kind: WatchEventKind::Renamed, paths: vec![from, to] });
    }

    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let renamed: Vec<&PathBuf> = order.iter().filter(|p| flags[*p].renamed).collect();
    let (gone, here): (Vec<&PathBuf>, Vec<&PathBuf>) = renamed.into_iter().partition(|p| !exists(p));
    let pair = (gone.len() == 1 && here.len() == 1).then(|| (gone[0].clone(), here[0].clone()));
    if let Some((from, to)) = &pair {
        // `to` may also have changed content; consumers re-read it anyway.
        out.push(WatchEvent { kind: WatchEventKind::Renamed, paths: vec![from.clone(), to.clone()] });
    }
    for p in &order {
        if pair.as_ref().is_some_and(|(a, b)| p == a || p == b) {
            continue;
        }
        let f = flags[p];
        let kind = if !exists(p) {
            WatchEventKind::Removed
        } else if f.created || f.renamed {
            WatchEventKind::Created
        } else {
            WatchEventKind::Modified
        };
        out.push(WatchEvent { kind, paths: vec![p.clone()] });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    fn collect_until(rx: &mpsc::Receiver<WatchEvent>, timeout: Duration, mut done: impl FnMut(&[WatchEvent]) -> bool) -> Vec<WatchEvent> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        while !done(&got) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if let Ok(ev) = rx.recv_timeout(left) {
                got.push(ev);
            }
        }
        got
    }

    fn drain(rx: &mpsc::Receiver<WatchEvent>, quiet: Duration) -> Vec<WatchEvent> {
        let mut got = Vec::new();
        while let Ok(ev) = rx.recv_timeout(quiet) {
            got.push(ev);
        }
        got
    }

    #[test]
    fn create_modify_delete_rename_debounced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let opts = WatchOptions { debounce: Duration::from_millis(100), ..Default::default() };
        let w = watch_with(&root, &opts, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        assert_eq!(w.root(), root);
        std::thread::sleep(Duration::from_millis(300));
        let _ = drain(&rx, Duration::from_millis(100));

        let a = root.join("a.ipynb");
        std::fs::write(&a, "{}").unwrap();
        let got = collect_until(&rx, Duration::from_secs(5), |g| g.iter().any(|e| e.paths == [a.clone()]));
        assert!(got.iter().any(|e| e.kind == WatchEventKind::Created && e.paths == [a.clone()]), "{got:?}");
        let _ = drain(&rx, Duration::from_millis(300));

        // A burst of 20 writes is debounced into very few events.
        for i in 0..20 {
            std::fs::write(&a, format!("{{\"n\": {i}}}")).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        // Our own checkpoint writes are invisible.
        std::fs::create_dir_all(root.join(".ipynb_checkpoints")).unwrap();
        std::fs::write(root.join(".ipynb_checkpoints/a-checkpoint.ipynb"), "{}").unwrap();
        std::fs::write(root.join(".hidden.ipynb"), "{}").unwrap();
        let got = drain(&rx, Duration::from_millis(500));
        let for_a: Vec<_> = got.iter().filter(|e| e.paths == [a.clone()]).collect();
        assert!(!for_a.is_empty() && for_a.len() <= 3, "{got:?}");
        assert!(for_a.iter().all(|e| matches!(e.kind, WatchEventKind::Modified | WatchEventKind::Created)), "{got:?}");
        assert!(got.iter().all(|e| e.paths.iter().all(|p| !p.to_string_lossy().contains("checkpoint") && !p.to_string_lossy().contains(".hidden"))), "{got:?}");

        // Rename.
        let b = root.join("b.ipynb");
        std::fs::rename(&a, &b).unwrap();
        let got = collect_until(&rx, Duration::from_secs(5), |g| g.iter().any(|e| e.paths.contains(&b)));
        let renamed = got.iter().any(|e| e.kind == WatchEventKind::Renamed && e.paths == [a.clone(), b.clone()]);
        let split = got.iter().any(|e| e.kind == WatchEventKind::Removed && e.paths == [a.clone()])
            && got.iter().any(|e| e.kind == WatchEventKind::Created && e.paths == [b.clone()]);
        assert!(renamed || split, "{got:?}");
        let _ = drain(&rx, Duration::from_millis(300));

        // Delete.
        std::fs::remove_file(&b).unwrap();
        let got = collect_until(&rx, Duration::from_secs(5), |g| g.iter().any(|e| e.paths == [b.clone()]));
        assert!(got.iter().any(|e| e.kind == WatchEventKind::Removed && e.paths == [b.clone()]), "{got:?}");

        // Subdirectory file.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.ipynb"), "{}").unwrap();
        let c = root.join("sub/c.ipynb");
        let got = collect_until(&rx, Duration::from_secs(5), |g| g.iter().any(|e| e.paths == [c.clone()]));
        assert!(got.iter().any(|e| e.kind == WatchEventKind::Created && e.paths == [c.clone()]), "{got:?}");
        drop(w);
    }

    #[test]
    fn filter_maps_and_drops() {
        let f = Filter {
            root: PathBuf::from("/var/x"),
            canonical: PathBuf::from("/private/var/x"),
            include_hidden: false,
            ignored: ["node_modules", ".ipynb_checkpoints"].into_iter().map(OsString::from).collect(),
        };
        assert_eq!(f.map(Path::new("/private/var/x/a/b.ipynb")), Some(PathBuf::from("/var/x/a/b.ipynb")));
        assert_eq!(f.map(Path::new("/var/x/b.ipynb")), Some(PathBuf::from("/var/x/b.ipynb")));
        assert_eq!(f.map(Path::new("/private/var/x/node_modules/b.ipynb")), None);
        assert_eq!(f.map(Path::new("/private/var/x/a/.ipynb_checkpoints/b.ipynb")), None);
        assert_eq!(f.map(Path::new("/private/var/x/.~b.ipynb")), None);
        assert_eq!(f.map(Path::new("/private/var/x")), None);
        assert_eq!(f.map(Path::new("/elsewhere/b.ipynb")), None);
    }
}
