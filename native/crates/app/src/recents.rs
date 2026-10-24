//! Remembered folders, notebooks and per-folder tabs, persisted across
//! launches in `~/Library/Application Support/Herse/recents.json`.

use std::path::{Path, PathBuf};

use gpui_kit::{App, Global};
use herse_workspace::Recents;

pub struct RecentsStore {
    pub recents: Recents,
    path: Option<PathBuf>,
}

impl Global for RecentsStore {}

fn store_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Herse/recents.json"))
}

pub fn get(cx: &mut App) -> &mut RecentsStore {
    if !cx.has_global::<RecentsStore>() {
        let path = store_path();
        // Tests and the headless snapshot tool must not touch the user's file.
        let disabled = std::env::var_os("HERSE_NO_RECENTS").is_some();
        let recents = match (&path, disabled) {
            (Some(p), false) => Recents::load(p),
            _ => Recents::default(),
        };
        cx.set_global(RecentsStore {
            recents,
            path: if disabled { None } else { path },
        });
    }
    cx.global_mut::<RecentsStore>()
}

impl RecentsStore {
    /// Write in the background: a few KB, but never on the frame path.
    pub fn save(&self) {
        let Some(path) = self.path.clone() else { return };
        let recents = self.recents.clone();
        std::thread::spawn(move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = recents.save(&path);
        });
    }

    pub fn folder_tabs(&self, folder: &Path) -> (Vec<PathBuf>, Option<PathBuf>) {
        match self.recents.tabs(folder) {
            Some(t) => (t.tabs.clone(), t.active.clone()),
            None => (Vec::new(), None),
        }
    }
}
