//! Preferences that outlive a session: content zoom and an explicit theme
//! choice. `~/Library/Application Support/Herse/settings.json`.

use std::path::PathBuf;

use serde_json::{Value, json};

fn path() -> Option<PathBuf> {
    // Tests (herse-snap) bring their own.
    if let Some(file) = std::env::var_os("HERSE_SETTINGS_FILE") {
        return Some(file.into());
    }
    if std::env::var_os("HERSE_NO_RECENTS").is_some() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Herse/settings.json"))
}

fn read() -> Value {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}

fn write(value: Value) {
    let Some(path) = path() else { return };
    std::thread::spawn(move || {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&value).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    });
}

fn update(key: &str, v: Value) {
    let mut all = read();
    if let Some(obj) = all.as_object_mut() {
        obj.insert(key.into(), v);
    }
    write(all);
}

pub fn zoom() -> Option<f32> {
    read().get("zoom").and_then(|v| v.as_f64()).map(|z| z as f32)
}

pub fn set_zoom(z: f32) {
    update("zoom", json!(z));
}

/// `Some(true)` dark, `Some(false)` light, `None` follow the system.
pub fn dark_theme() -> Option<bool> {
    match read().get("theme").and_then(|v| v.as_str()) {
        Some("dark") => Some(true),
        Some("light") => Some(false),
        _ => None,
    }
}

pub fn set_dark_theme(dark: bool) {
    update("theme", json!(if dark { "dark" } else { "light" }));
}

/// Follow macOS's appearance again (no explicit choice).
pub fn follow_system_theme() {
    update("theme", json!("system"));
}

/// Whether code cells show line numbers (the last `Shift-L` choice).
pub fn line_numbers() -> bool {
    read().get("line_numbers").and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn set_line_numbers(on: bool) {
    update("line_numbers", json!(on));
}

/// Whether the margin marks changes since the last commit (on unless turned off).
/// Completion opens as you type (on: the default), or only on Tab.
pub fn complete_as_you_type() -> bool {
    read().get("complete_as_you_type").and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_complete_as_you_type(on: bool) {
    update("complete_as_you_type", json!(on));
}

pub fn show_changes() -> bool {
    read().get("show_changes").and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_show_changes(on: bool) {
    update("show_changes", json!(on));
}

/// Last selected cell per notebook, so reopening one returns to where you were.
pub fn position(path: &std::path::Path) -> Option<usize> {
    read()
        .get("positions")?
        .get(path.to_string_lossy().as_ref())?
        .as_u64()
        .map(|v| v as usize)
}

pub fn set_positions(entries: Vec<(std::path::PathBuf, usize)>) {
    if entries.is_empty() {
        return;
    }
    let mut all = read();
    let Some(obj) = all.as_object_mut() else { return };
    let positions = obj.entry("positions").or_insert_with(|| json!({}));
    if let Some(map) = positions.as_object_mut() {
        for (path, cell) in entries {
            map.insert(path.to_string_lossy().into_owned(), json!(cell));
        }
        // Keep the file small: drop the oldest when it grows past a few hundred.
        while map.len() > 400 {
            let Some(first) = map.keys().next().cloned() else { break };
            map.remove(&first);
        }
    }
    write(all);
}

/// Charts sit on the page (no background of their own, the page's ink,
/// quiet grids) instead of as cards; on unless turned off.
pub fn blend_charts() -> bool {
    read().get("blend_charts").and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_blend_charts(on: bool) {
    update("blend_charts", json!(on));
}

/// Which assistant answers ⌘I: `"claude"` or `"codex"` (`None`: the first
/// one installed).
pub fn assistant() -> Option<String> {
    read().get("assistant").and_then(|v| v.as_str()).map(str::to_string)
}

pub fn set_assistant(name: &str) {
    update("assistant", json!(name));
}

/// The model an assistant uses (`"claude"` → `"sonnet"`), when the user
/// picked one; else its own default.
pub fn assistant_model(assistant: &str) -> Option<String> {
    read().get("assistant_models").and_then(|m| m.get(assistant)).and_then(|v| v.as_str()).map(str::to_string)
}

/// Whether the Ask box has said, once, what an assistant sends where.
pub fn assistant_notice_seen() -> bool {
    read().get("assistant_notice_seen").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The notice was read, as `assistant` (if given) was asked: one write, so
/// neither is lost, and made now, since the conversation reads it next.
pub fn set_assistant_notice_seen(assistant: Option<&str>) {
    let mut all = read();
    let Some(obj) = all.as_object_mut() else { return };
    if let Some(name) = assistant {
        obj.insert("assistant".into(), json!(name));
    }
    obj.insert("assistant_notice_seen".into(), json!(true));
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.notice-tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&all).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// What was answered when each extension first wanted to run, by its
/// folder: the fingerprint it was answered for and whether it may run.
pub fn extension_trust() -> std::collections::HashMap<String, (String, bool)> {
    let all = read();
    let Some(map) = all.get("extension_trust").and_then(Value::as_object) else { return Default::default() };
    map.iter()
        .filter_map(|(folder, v)| {
            let hash = v.get("hash")?.as_str()?.to_string();
            Some((folder.clone(), (hash, v.get("allowed")?.as_bool()?)))
        })
        .collect()
}

/// Remember an extension's answer. Written at once, not in the background:
/// it's rare, and an Allow lost to a quit would ask again.
pub fn set_extension_trust(folder: &str, hash: &str, allowed: bool) {
    let mut all = read();
    let Some(obj) = all.as_object_mut() else { return };
    let trust = obj.entry("extension_trust").or_insert_with(|| json!({}));
    if let Some(map) = trust.as_object_mut() {
        map.insert(folder.into(), json!({ "hash": hash, "allowed": allowed }));
    }
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.trust-tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&all).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Install a notebook's missing packages without asking (off: Herse offers).
pub fn auto_install() -> bool {
    read().get("auto_install").and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn set_auto_install(on: bool) {
    update("auto_install", json!(on));
}
