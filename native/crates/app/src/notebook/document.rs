//! A notebook file on disk and its in-memory model.

use std::{
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use herse_nb::Notebook;

pub struct Document {
    pub nb: Notebook,
    /// `None` until an untitled notebook is first saved.
    pub path: Option<PathBuf>,
    pub dirty: bool,
    /// mtime of the file as we last read or wrote it. Auto-save only writes
    /// when the file on disk still has this mtime, so an edit made outside the
    /// app is never silently clobbered.
    pub disk_mtime: Option<SystemTime>,
    /// Auto-save stops for this document after a conflict until an explicit save.
    pub autosave_blocked: bool,
    pub load_stats: Option<LoadStats>,
}

#[derive(Clone, Copy, Debug)]
pub struct LoadStats {
    pub bytes: usize,
    pub read_ms: f32,
    pub parse_ms: f32,
}

pub enum SaveOutcome {
    Saved,
    /// The file changed on disk since we read it; nothing was written.
    Conflict,
}

impl Document {
    pub fn open(path: &Path) -> Result<Self> {
        let t0 = Instant::now();
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let t1 = Instant::now();
        let nb = herse_nb::parse(&bytes).with_context(|| format!("parsing {}", path.display()))?;
        let t2 = Instant::now();
        let disk_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(Self {
            nb,
            path: Some(path.to_path_buf()),
            dirty: false,
            disk_mtime,
            autosave_blocked: false,
            load_stats: Some(LoadStats {
                bytes: bytes.len(),
                read_ms: (t1 - t0).as_secs_f32() * 1000.,
                parse_ms: (t2 - t1).as_secs_f32() * 1000.,
            }),
        })
    }

    pub fn untitled(kernel: &str, display_name: &str, language: &str) -> Self {
        let nb = Notebook::new_empty(kernel, display_name, language);
        Self {
            nb,
            path: None,
            dirty: false,
            disk_mtime: None,
            autosave_blocked: false,
            load_stats: None,
        }
    }

    /// An untitled document holding `nb` (a live session's copy).
    pub fn from_notebook(nb: Notebook) -> Self {
        Self { nb, path: None, dirty: false, disk_mtime: None, autosave_blocked: false, load_stats: None }
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled.ipynb".into())
    }

    pub fn directory(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// Serialize and write. `conditional` writes only if the file on disk is
    /// still the version we last saw (auto-save); an explicit save is not
    /// conditional, because the user asked for their version to win.
    pub fn save(&mut self, to: Option<PathBuf>, conditional: bool) -> Result<SaveOutcome> {
        if let Some(to) = to {
            self.path = Some(to);
            self.disk_mtime = None;
        }
        let path = self.path.clone().context("untitled notebook has no path")?;
        if conditional && let Some(seen) = self.disk_mtime {
            let current = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if current != Some(seen) {
                self.autosave_blocked = true;
                return Ok(SaveOutcome::Conflict);
            }
        }
        let text = herse_nb::to_string(&self.nb);
        // Write to a sibling temp file and rename, so a crash mid-write can
        // never leave a truncated notebook behind.
        let tmp = path.with_extension("ipynb.herse-tmp");
        std::fs::write(&tmp, text.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        self.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        self.dirty = false;
        self.autosave_blocked = false;
        Ok(SaveOutcome::Saved)
    }
}
