//! Folder scanning for the sidebar, backed by `herse-workspace`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use gpui_kit::SharedString;

use crate::sidebar::Entry;

pub fn scan(root: &Path) -> Vec<Entry> {
    // Notebooks, and the data files Herse opens as tables.
    let mut listed: Vec<String> = herse_data::EXTENSIONS.iter().map(|e| e.to_string()).collect();
    listed.extend(["csv.gz".into(), "tsv.gz".into()]);
    let opts = herse_workspace::WalkOptions {
        prune_empty_dirs: true,
        listed_extensions: listed,
        ..Default::default()
    };
    herse_workspace::walk(root, &opts)
        .into_iter()
        .filter_map(|e| {
            let data = match &e.kind {
                // `.db` is listed only when it really is SQLite.
                herse_workspace::EntryKind::Other(_) => Some(herse_data::Format::of(&e.path)?),
                _ => None,
            };
            Some(Entry {
                name: SharedString::from(e.name().to_string()),
                is_dir: e.is_dir(),
                depth: e.depth,
                icon: data.map(data_icon),
                size: e.size,
                path: e.path,
            })
        })
        .collect()
}

/// A data file's icon in the tree: a sheet for tables, braces for JSON,
/// a cylinder for databases. In pencil: only notebooks wear a colour.
fn data_icon(format: herse_data::Format) -> &'static str {
    use herse_data::Format as F;
    match format {
        F::Json | F::JsonLines => "icons/file-braces.svg",
        F::Sqlite => "icons/database.svg",
        F::Csv { .. } | F::Tsv { .. } | F::Parquet | F::Arrow | F::Excel => "icons/file-spreadsheet.svg",
    }
}

pub struct Summary {
    /// Compact annotation shown on the row.
    pub short: SharedString,
    /// Full description for the tooltip.
    pub long: SharedString,
}

/// Cell counts and kernels for the tree's annotations, in parallel.
pub fn summarize(paths: &[PathBuf]) -> HashMap<PathBuf, Summary> {
    let out = Mutex::new(HashMap::with_capacity(paths.len()));
    herse_workspace::summarize_many(paths, |ix, result| {
        let Ok(s) = result else { return };
        let kernel = s.kernel.clone().unwrap_or_else(|| "no kernel".into());
        let short = match s.language.as_deref() {
            Some(lang) if !lang.eq_ignore_ascii_case("python") => format!("{} · {}", s.cells, short_language(lang)),
            _ => s.cells.to_string(),
        };
        let long = format!(
            "{} cells ({} code, {} markdown) · {}{}",
            s.cells,
            s.code,
            s.markdown,
            kernel,
            s.title.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
        );
        out.lock().unwrap().insert(
            paths[ix].clone(),
            Summary {
                short: short.into(),
                long: long.into(),
            },
        );
    });
    out.into_inner().unwrap()
}

fn short_language(lang: &str) -> String {
    match lang.to_ascii_lowercase().as_str() {
        "julia" => "jl".into(),
        "r" => "R".into(),
        "javascript" | "typescript" => "js".into(),
        other => other.chars().take(4).collect(),
    }
}
