//! `herse-workspace`: a folder of notebooks as a workspace.
//!
//! * [`walk`] — gitignore-aware tree listing (dirs + notebooks, optionally other files).
//! * [`summarize`] / [`summarize_many`] — cell counts, kernel, title; outputs are skipped
//!   at the byte level so a 50 MB image-heavy notebook costs a few ms.
//! * [`FuzzyIndex`] — Cmd-P quick open (nucleo, path-aware, incremental).
//! * [`search`] — parallel cross-notebook search over cell sources (optionally outputs).
//! * [`watch`] — debounced recursive watching.
//! * [`Recents`] — persisted recent folders / notebooks / open tabs.
//!
//! # Threading
//!
//! Everything is synchronous and blocking; call it from a background thread
//! (`cx.background_spawn`). No async runtime is used.
//!
//! * [`walk`] and [`search`] run the `ignore` crate's parallel walker on
//!   scoped threads they create and join before returning; [`search`] calls
//!   `on_hit` concurrently from those threads.
//! * [`summarize_many`] and [`search_paths`] use the rayon global pool and
//!   call their callbacks concurrently from rayon workers.
//! * [`watch`]'s callback runs on the debouncer's dedicated thread, one event
//!   at a time, until the [`Watcher`] is dropped.
//! * [`FuzzyIndex`] is `Send` but needs `&mut` per query (it caches the
//!   previous query's matches); keep one per picker.

mod fuzzy;
mod io;
mod json;
mod natural;
mod nbscan;
mod recents;
mod search;
mod summary;
mod walk;
mod watch;

pub use fuzzy::{FuzzyHit, FuzzyIndex};
pub use natural::natural_cmp;
pub use recents::{FolderTabs, MAX_RECENT_FOLDERS, MAX_RECENT_NOTEBOOKS, MAX_TAB_FOLDERS, Recents};
pub use search::{CompiledQuery, HitLocation, SearchHit, SearchQuery, SearchStats, search, search_paths};
pub use summary::{NotebookSummary, markdown_title, serde_impl, summarize, summarize_bytes, summarize_many};
pub use walk::{
    DEFAULT_IGNORED_DIRS, EntryKind, OtherFiles, TEXT_EXTENSIONS, Walk, WalkEntry, WalkOptions, is_notebook_path, walk,
    walk_report,
};
pub use watch::{WatchEvent, WatchEventKind, WatchOptions, Watcher, watch, watch_with};

#[cfg(test)]
mod thread_safety {
    fn send<T: Send>() {}
    fn send_sync<T: Send + Sync>() {}

    #[test]
    fn handle_types_cross_threads() {
        send::<super::Watcher>();
        send::<super::FuzzyIndex>();
        send_sync::<super::CompiledQuery>();
        send_sync::<super::SearchQuery>();
        send_sync::<super::Recents>();
        send_sync::<super::WalkEntry>();
        send_sync::<super::NotebookSummary>();
        send_sync::<super::SearchHit>();
    }
}
