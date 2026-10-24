//! Persisted workspace state: recent folders, recent notebooks, and the open
//! tabs per folder. A small JSON file, written atomically; loading never
//! fails (a missing or corrupt file, or a bad field, yields defaults).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_RECENT_FOLDERS: usize = 12;
pub const MAX_RECENT_NOTEBOOKS: usize = 50;
/// Folders whose open tabs are remembered (least recently used are dropped).
pub const MAX_TAB_FOLDERS: usize = 24;
const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderTabs {
    /// Open notebooks in tab order.
    pub tabs: Vec<PathBuf>,
    pub active: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recents {
    pub version: u32,
    /// The folder that was open last (reopen it on launch).
    pub last_folder: Option<PathBuf>,
    /// Most recent first.
    pub folders: Vec<PathBuf>,
    /// Most recent first, across all folders.
    pub notebooks: Vec<PathBuf>,
    /// Keyed by folder path.
    pub tabs: BTreeMap<String, FolderTabs>,
    /// Folder keys of `tabs`, most recent first (for LRU eviction).
    #[serde(default)]
    tab_order: Vec<String>,
}

impl Default for Recents {
    fn default() -> Self {
        Recents {
            version: VERSION,
            last_folder: None,
            folders: Vec::new(),
            notebooks: Vec::new(),
            tabs: BTreeMap::new(),
            tab_order: Vec::new(),
        }
    }
}

fn key(folder: &Path) -> String {
    folder.to_string_lossy().into_owned()
}

fn push_front(list: &mut Vec<PathBuf>, p: &Path, max: usize) {
    list.retain(|x| x != p);
    list.insert(0, p.to_path_buf());
    list.truncate(max);
}

impl Recents {
    /// Loads from `path`. Missing / unreadable / corrupt files give defaults;
    /// individual malformed fields are dropped, the rest is kept.
    pub fn load(path: &Path) -> Recents {
        let Ok(bytes) = std::fs::read(path) else { return Recents::default() };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("ignoring corrupt {}: {e}", path.display());
                return Recents::default();
            }
        };
        fn field<T: serde::de::DeserializeOwned + Default>(v: &serde_json::Value, k: &str) -> T {
            v.get(k).and_then(|x| serde_json::from_value(x.clone()).ok()).unwrap_or_default()
        }
        // Accept the Electron app's `recentFolders` / `recentNotebooks` / `root` too.
        let folders: Vec<PathBuf> = if v.get("folders").is_some() { field(&v, "folders") } else { field(&v, "recentFolders") };
        let notebooks: Vec<PathBuf> =
            if v.get("notebooks").is_some() { field(&v, "notebooks") } else { field(&v, "recentNotebooks") };
        let last_folder: Option<PathBuf> =
            if v.get("last_folder").is_some() { field(&v, "last_folder") } else { field(&v, "root") };
        let mut r = Recents {
            version: VERSION,
            last_folder,
            folders,
            notebooks,
            tabs: field(&v, "tabs"),
            tab_order: field(&v, "tab_order"),
        };
        r.folders.truncate(MAX_RECENT_FOLDERS);
        r.notebooks.truncate(MAX_RECENT_NOTEBOOKS);
        r.tab_order.retain(|k| r.tabs.contains_key(k));
        let missing: Vec<String> = r.tabs.keys().filter(|k| !r.tab_order.contains(k)).cloned().collect();
        r.tab_order.extend(missing);
        r
    }

    /// Writes atomically (temp file + rename), creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Records `folder` as opened: becomes `last_folder` and the most recent folder.
    pub fn open_folder(&mut self, folder: &Path) {
        self.last_folder = Some(folder.to_path_buf());
        push_front(&mut self.folders, folder, MAX_RECENT_FOLDERS);
    }

    /// Records a notebook as opened (most recent first).
    pub fn open_notebook(&mut self, notebook: &Path) {
        push_front(&mut self.notebooks, notebook, MAX_RECENT_NOTEBOOKS);
    }

    pub fn forget_folder(&mut self, folder: &Path) {
        self.folders.retain(|f| f != folder);
        if self.last_folder.as_deref() == Some(folder) {
            self.last_folder = None;
        }
        let k = key(folder);
        self.tabs.remove(&k);
        self.tab_order.retain(|x| *x != k);
    }

    pub fn forget_notebook(&mut self, notebook: &Path) {
        self.notebooks.retain(|n| n != notebook);
    }

    /// A notebook was renamed / moved: rewrite it everywhere.
    pub fn rename_notebook(&mut self, from: &Path, to: &Path) {
        for n in self.notebooks.iter_mut().chain(self.tabs.values_mut().flat_map(|t| t.tabs.iter_mut().chain(t.active.iter_mut()))) {
            if n == from {
                *n = to.to_path_buf();
            }
        }
    }

    /// Remembers the open tabs of `folder` (an empty list forgets them).
    pub fn set_tabs(&mut self, folder: &Path, tabs: Vec<PathBuf>, active: Option<PathBuf>) {
        let k = key(folder);
        self.tab_order.retain(|x| *x != k);
        if tabs.is_empty() {
            self.tabs.remove(&k);
            return;
        }
        self.tabs.insert(k.clone(), FolderTabs { tabs, active });
        self.tab_order.insert(0, k);
        while self.tab_order.len() > MAX_TAB_FOLDERS {
            if let Some(old) = self.tab_order.pop() {
                self.tabs.remove(&old);
            }
        }
    }

    pub fn tabs(&self, folder: &Path) -> Option<&FolderTabs> {
        self.tabs.get(&key(folder))
    }

    /// Recent notebooks under `folder` as `/`-separated relative paths, most
    /// recent first — feed to [`crate::FuzzyIndex::set_recents`].
    pub fn recent_rels(&self, folder: &Path) -> Vec<String> {
        self.notebooks.iter().filter_map(|n| n.strip_prefix(folder).ok()).map(|r| crate::walk::rel_path(Path::new(""), r)).collect()
    }

    /// Drops folders / notebooks / tabs that no longer exist on disk.
    pub fn prune_missing(&mut self) {
        self.folders.retain(|p| p.is_dir());
        self.notebooks.retain(|p| p.is_file());
        if self.last_folder.as_ref().is_some_and(|p| !p.is_dir()) {
            self.last_folder = None;
        }
        self.tabs.retain(|k, t| {
            t.tabs.retain(|p| p.is_file());
            if t.active.as_ref().is_some_and(|a| !t.tabs.contains(a)) {
                t.active = None;
            }
            Path::new(k).is_dir() && !t.tabs.is_empty()
        });
        let tabs = &self.tabs;
        self.tab_order.retain(|k| tabs.contains_key(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state/workspace.json");
        let mut r = Recents::load(&file);
        assert_eq!(r, Recents::default());
        for i in 0..20 {
            r.open_folder(Path::new(&format!("/f{i}")));
        }
        r.open_folder(Path::new("/f5"));
        assert_eq!(r.folders.len(), MAX_RECENT_FOLDERS);
        assert_eq!(r.folders[0], Path::new("/f5"));
        assert_eq!(r.folders.iter().filter(|f| *f == Path::new("/f5")).count(), 1);
        assert_eq!(r.last_folder.as_deref(), Some(Path::new("/f5")));

        r.open_notebook(Path::new("/f5/a/x.ipynb"));
        r.open_notebook(Path::new("/f1/y.ipynb"));
        r.open_notebook(Path::new("/f5/z.ipynb"));
        assert_eq!(r.recent_rels(Path::new("/f5")), ["z.ipynb", "a/x.ipynb"]);

        r.set_tabs(Path::new("/f5"), vec!["/f5/a/x.ipynb".into(), "/f5/z.ipynb".into()], Some("/f5/z.ipynb".into()));
        r.rename_notebook(Path::new("/f5/z.ipynb"), Path::new("/f5/w.ipynb"));
        assert_eq!(r.tabs(Path::new("/f5")).unwrap().active.as_deref(), Some(Path::new("/f5/w.ipynb")));
        assert_eq!(r.notebooks[0], Path::new("/f5/w.ipynb"));

        for i in 0..30 {
            r.set_tabs(Path::new(&format!("/t{i}")), vec!["/t/a.ipynb".into()], None);
        }
        assert_eq!(r.tabs.len(), MAX_TAB_FOLDERS);
        assert!(r.tabs(Path::new("/f5")).is_none() && r.tabs(Path::new("/t29")).is_some());

        r.save(&file).unwrap();
        assert_eq!(Recents::load(&file), r);
    }

    #[test]
    fn tolerant_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("w.json");
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(Recents::load(&file), Recents::default());
        // Bad field dropped, others kept; Electron-era keys accepted.
        std::fs::write(&file, r#"{"root": "/old", "recentFolders": ["/old", "/older"], "recentNotebooks": 7}"#).unwrap();
        let r = Recents::load(&file);
        assert_eq!(r.last_folder.as_deref(), Some(Path::new("/old")));
        assert_eq!(r.folders, [PathBuf::from("/old"), PathBuf::from("/older")]);
        assert!(r.notebooks.is_empty());
    }

    #[test]
    fn prune() {
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("a.ipynb");
        std::fs::write(&nb, "{}").unwrap();
        let mut r = Recents::default();
        r.open_folder(dir.path());
        r.open_folder(Path::new("/definitely/not/here"));
        r.open_notebook(&nb);
        r.open_notebook(Path::new("/nope.ipynb"));
        r.set_tabs(dir.path(), vec![nb.clone(), "/nope.ipynb".into()], Some("/nope.ipynb".into()));
        r.prune_missing();
        assert_eq!(r.folders, [dir.path().to_path_buf()]);
        assert_eq!(r.notebooks, std::slice::from_ref(&nb));
        assert_eq!(r.tabs(dir.path()), Some(&FolderTabs { tabs: vec![nb], active: None }));
        assert!(r.last_folder.is_none());
    }
}
