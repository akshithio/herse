//! A notebook that won't open, explained: what's wrong in a sentence, the
//! file's own text to look at (or its bytes, when it isn't text), and the
//! fix when there is one. A file cut off mid-save gives back the cells
//! before the cut; one with git conflict markers opens as either side; an
//! empty file becomes a new notebook. Recovered cells open in a tab for the
//! file, unsaved: ⌘S writes them over it, and nothing changes until then.

use std::path::Path;

use herse_nb::Notebook;

/// What Herse says about a file it couldn't open.
pub struct Diagnosis {
    pub title: String,
    pub detail: String,
    pub fixes: Vec<Fix>,
}

pub enum Fix {
    /// Open this notebook for the file (unsaved).
    Open { label: String, notebook: Box<Notebook>, note: String },
    /// The file is empty: make it a new notebook.
    StartFresh,
}

/// How much of the file to show.
const PREVIEW_LINES: usize = 40;
const PREVIEW_BYTES: usize = 16 * 1024;

pub fn diagnose(path: &Path, err: &anyhow::Error) -> Diagnosis {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Not a parse problem (permissions, a vanished file): the error says it.
        Err(_) => {
            return Diagnosis { title: format!("Couldn't open {name}"), detail: format!("{err:#}"), fixes: Vec::new() };
        }
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Diagnosis {
            title: format!("{name} is empty"),
            detail: format!(
                "The file has {}. It was probably being created or synced when it was cut off. \
                 Start a new notebook in it, or delete it.",
                if bytes.is_empty() { "nothing in it (0 bytes)".to_string() } else { "only blank space in it".to_string() }
            ),
            fixes: vec![Fix::StartFresh],
        };
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) if !text.contains('\0') => text,
        _ => {
            let kind = sniff(&bytes).map_or_else(String::new, |k| format!(" It looks like {k}, named as a notebook."));
            return Diagnosis {
                title: format!("{name} isn't text"),
                detail: format!("A notebook is JSON text; this file holds binary data.{kind}\n\n{}", hex_dump(&bytes)),
                fixes: Vec::new(),
            };
        }
    };
    if let Some(sides) = conflict_sides(text) {
        let mut fixes = Vec::new();
        for (label, version) in &sides {
            if let Ok(nb) = herse_nb::parse(version.as_bytes()) {
                let cells = nb.cells.len();
                fixes.push(Fix::Open {
                    label: format!("Open {label}'s Version"),
                    notebook: Box::new(nb),
                    note: format!(
                        "{label}'s version, {cells} cell{}. ⌘S writes it over the conflicted file.",
                        if cells == 1 { "" } else { "s" }
                    ),
                });
            }
        }
        let names: Vec<&str> = sides.iter().map(|(l, _)| l.as_str()).collect();
        return Diagnosis {
            title: format!("{name} has unresolved git conflicts"),
            detail: format!(
                "A merge left both versions in the file, between <<<<<<< and >>>>>>> lines ({}). \
                 Open one side to keep it, or resolve the conflict in your editor.\n\n{}",
                names.join(" and "),
                preview(text, None)
            ),
            fixes,
        };
    }
    let nb_err = err.chain().find_map(|e| e.downcast_ref::<herse_nb::Error>());
    match nb_err {
        Some(herse_nb::Error::Json { line, column, message }) => {
            let cut_off = message.contains("end of input") || message.contains("EOF");
            if cut_off && let Some((nb, cells)) = salvage(text) {
                let fixes = vec![Fix::Open {
                    label: format!("Open the {cells} Recovered Cell{}", if cells == 1 { "" } else { "s" }),
                    notebook: Box::new(nb),
                    note: format!("Recovered {cells} cell{} from the cut-off file. ⌘S writes them over it.", if cells == 1 { "" } else { "s" }),
                }];
                return Diagnosis {
                    title: format!("{name} was cut off"),
                    detail: format!(
                        "The file stops partway (line {line}): it was probably saved or synced only in part. \
                         The {cells} cell{} before the cut can be recovered.\n\n{}",
                        if cells == 1 { "" } else { "s" },
                        preview_end(text)
                    ),
                    fixes,
                };
            }
            Diagnosis {
                title: if cut_off { format!("{name} was cut off") } else { format!("{name} isn't valid JSON") },
                detail: format!("Line {line}, column {column}: {message}.\n\n{}", preview(text, Some(*line))),
                fixes: Vec::new(),
            }
        }
        Some(herse_nb::Error::Invalid(why)) => {
            let what = match text.trim_start().chars().next() {
                Some('[') => "It's JSON, but a list ([…]); a notebook is an object ({…}) with cells.".to_string(),
                Some('{') => format!("It's JSON, but not shaped like a notebook: {why}."),
                _ => format!("It's JSON, but not a notebook: {why}."),
            };
            Diagnosis { title: format!("{name} isn't a notebook"), detail: format!("{what}\n\n{}", preview(text, None)), fixes: Vec::new() }
        }
        _ => Diagnosis { title: format!("Couldn't open {name}"), detail: format!("{err:#}\n\n{}", preview(text, None)), fixes: Vec::new() },
    }
}

/// The first lines of the file (around `line`, when one is named, with a
/// marker), numbered.
fn preview(text: &str, line: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let (start, mark) = match line {
        Some(l) if l > 0 => (l.saturating_sub(8), Some(l)),
        _ => (0, None),
    };
    let end = (start + PREVIEW_LINES).min(lines.len());
    let mut out = String::new();
    for (i, l) in lines[start..end].iter().enumerate() {
        let n = start + i + 1;
        let l: String = l.chars().take(200).collect();
        let arrow = if mark == Some(n) { "→" } else { " " };
        out.push_str(&format!("{arrow}{n:>5}  {l}\n"));
        if out.len() > PREVIEW_BYTES {
            break;
        }
    }
    if end < lines.len() {
        out.push_str(&format!("       … {} more lines\n", lines.len() - end));
    }
    out.trim_end().to_string()
}

/// The last lines of the file: where a cut-off file stops.
fn preview_end(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(12);
    let mut out = "       … the file's last lines:\n".to_string();
    for (i, l) in lines[start..].iter().enumerate() {
        let l: String = l.chars().take(200).collect();
        out.push_str(&format!(" {:>5}  {l}\n", start + i + 1));
    }
    out.push_str("       (ends here)");
    out
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    // Twelve bytes a row: narrow enough for the banner.
    for (row, chunk) in bytes.chunks(12).take(10).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
        out.push_str(&format!("{:04x}  {:<35}  {ascii}\n", row * 12, hex.join(" ")));
    }
    if bytes.len() > 120 {
        out.push_str(&format!("… {} bytes in all", bytes.len()));
    }
    out.trim_end().to_string()
}

/// A guess at what binary data is, from its first bytes.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    Some(if starts(b"\x89PNG") {
        "a PNG image"
    } else if starts(b"\xff\xd8\xff") {
        "a JPEG image"
    } else if starts(b"%PDF") {
        "a PDF"
    } else if starts(b"PK\x03\x04") {
        "a zip archive"
    } else if starts(b"\x1f\x8b") {
        "gzip-compressed data"
    } else if starts(b"bplist") {
        "a property list"
    } else {
        return None;
    })
}

/// Each side of git conflict markers (`<<<<<<< ours` … `=======` …
/// `>>>>>>> theirs`, with diff3's `|||||||` base dropped), whole files.
fn conflict_sides(text: &str) -> Option<Vec<(String, String)>> {
    if !text.lines().any(|l| l.starts_with("<<<<<<<")) || !text.lines().any(|l| l.starts_with(">>>>>>>")) {
        return None;
    }
    #[derive(PartialEq)]
    enum In {
        Both,
        Ours,
        Base,
        Theirs,
    }
    let (mut ours, mut theirs) = (String::new(), String::new());
    let (mut ours_name, mut theirs_name) = (None, None);
    let mut state = In::Both;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end();
        if let Some(rest) = bare.strip_prefix("<<<<<<<") {
            state = In::Ours;
            ours_name.get_or_insert_with(|| rest.trim().to_string());
            continue;
        }
        if bare.starts_with("|||||||") && state == In::Ours {
            state = In::Base;
            continue;
        }
        if bare == "=======" && matches!(state, In::Ours | In::Base) {
            state = In::Theirs;
            continue;
        }
        if let Some(rest) = bare.strip_prefix(">>>>>>>").filter(|_| state == In::Theirs) {
            state = In::Both;
            theirs_name.get_or_insert_with(|| rest.trim().to_string());
            continue;
        }
        match state {
            In::Both => {
                ours.push_str(line);
                theirs.push_str(line);
            }
            In::Ours => ours.push_str(line),
            In::Theirs => theirs.push_str(line),
            In::Base => {}
        }
    }
    let name = |n: Option<String>, fallback: &str| n.filter(|n| !n.is_empty()).unwrap_or_else(|| fallback.to_string());
    Some(vec![(name(ours_name, "ours"), ours), (name(theirs_name, "theirs"), theirs)])
}

/// The cells a cut-off notebook still has whole: everything up to the last
/// complete cell in `"cells": [ … ]`, closed off as a notebook.
fn salvage(text: &str) -> Option<(Notebook, usize)> {
    let b = text.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    let mut cells_open: Option<usize> = None;
    let mut last_cell_end: Option<usize> = None;
    let mut key_start: Option<usize> = None;
    let mut last_key = String::new();
    for (i, &c) in b.iter().enumerate() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    if let Some(s) = key_start.take() {
                        last_key = text[s..i].to_string();
                    }
                }
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => {
                in_string = true;
                // A key at the notebook's top level: remember which.
                if depth == 1 {
                    key_start = Some(i + 1);
                }
            }
            b'{' | b'[' => {
                depth += 1;
                if c == b'[' && depth == 2 && last_key == "cells" {
                    cells_open = Some(i);
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if c == b'}' && depth == 2 && cells_open.is_some() {
                    last_cell_end = Some(i + 1);
                }
                if c == b']' && depth == 1 && cells_open.is_some() {
                    // The cells closed whole; the cut came later.
                    last_cell_end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = last_cell_end?;
    let closed = format!("{}\n ],\n \"metadata\": {{}},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}}\n", &text[..end]);
    let nb = herse_nb::parse(closed.as_bytes()).ok()?;
    let cells = nb.cells.len();
    (cells > 0).then_some((nb, cells))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnose_text(text: &str) -> Diagnosis {
        let dir = std::env::temp_dir().join(format!("herse-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t{}.ipynb", text.len()));
        std::fs::write(&path, text).unwrap();
        let err = crate::notebook::document::Document::open(&path).err().expect("broken");
        let d = diagnose(&path, &err);
        std::fs::remove_file(&path).ok();
        d
    }

    #[test]
    fn a_cut_off_notebook_gives_back_its_whole_cells() {
        let text = r##"{
 "cells": [
  {"cell_type": "markdown", "metadata": {}, "source": "# Hi"},
  {"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [], "source": "x = 1"},
  {"cell_type": "code", "metadata": {}, "outputs": [{"name": "stdout", "text": ["mo"##;
        let d = diagnose_text(text);
        assert!(d.title.ends_with("was cut off"), "{}", d.title);
        match d.fixes.as_slice() {
            [Fix::Open { notebook, label, .. }] => {
                assert_eq!(notebook.cells.len(), 2);
                assert_eq!(label, "Open the 2 Recovered Cells");
            }
            _ => panic!("no recovery"),
        }
    }

    #[test]
    fn conflict_markers_offer_each_side() {
        let text = "{\"cells\": [\n<<<<<<< HEAD\n{\"cell_type\": \"markdown\", \"metadata\": {}, \"source\": \"ours\"}\n=======\n1\n>>>>>>> branch\n], \"metadata\": {}, \"nbformat\": 4, \"nbformat_minor\": 5}\n";
        let d = diagnose_text(text);
        assert!(d.title.contains("unresolved git conflicts"));
        let labels: Vec<&str> = d
            .fixes
            .iter()
            .filter_map(|f| match f {
                Fix::Open { label, .. } => Some(label.as_str()),
                Fix::StartFresh => None,
            })
            .collect();
        assert_eq!(labels, ["Open HEAD's Version"]);
    }

    #[test]
    fn plain_explanations() {
        assert!(diagnose_text("[1, 2, 3]").detail.starts_with("It's JSON, but a list"));
        assert!(matches!(diagnose_text("").fixes.as_slice(), [Fix::StartFresh]));
        assert!(diagnose_text("not json {").title.ends_with("isn't valid JSON"));
        let d = diagnose_text("\u{89}PNG\0\0");
        assert!(d.title.ends_with("isn't text"), "{}", d.title);
    }
}
