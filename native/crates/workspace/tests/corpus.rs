//! Summary correctness against independent parsers:
//! * a full `serde_json::Value` parse (always), and
//! * Python `nbformat` (when an interpreter with nbformat is available),
//!
//! over the real notebooks under /opt/anaconda3 (skipped when absent) and a
//! generated synthetic corpus (always).

#[path = "../examples/common/gen.rs"]
mod generator;

use herse_workspace::{NotebookSummary, OtherFiles, SearchQuery, WalkOptions, serde_impl, summarize, walk};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

const ANACONDA: &str = "/opt/anaconda3";

fn anaconda_notebooks() -> Option<Vec<PathBuf>> {
    let root = Path::new(ANACONDA);
    if !root.is_dir() {
        eprintln!("skipping: {ANACONDA} not present");
        return None;
    }
    let opts = WalkOptions {
        include_hidden: true,
        respect_gitignore: false,
        ignored_dirs: vec![],
        max_entries: None,
        other_files: OtherFiles::None,
        ..Default::default()
    };
    let nbs: Vec<PathBuf> = walk(root, &opts).into_iter().filter(|e| e.is_notebook()).map(|e| e.path).collect();
    assert!(!nbs.is_empty());
    Some(nbs)
}

fn synthetic(dir: &Path) -> Vec<PathBuf> {
    let g = generator::workspace(dir, 80, 0, 42);
    // nbformat 3 and odd-but-valid layouts.
    let v3 = dir.join("v3.ipynb");
    std::fs::write(
        &v3,
        r##"{"metadata": {"name": "old"}, "nbformat": 3, "nbformat_minor": 0, "worksheets": [{"cells": [
            {"cell_type": "heading", "level": 1, "metadata": {}, "source": ["Old Title"]},
            {"cell_type": "code", "collapsed": false, "input": ["print(1)"], "language": "python", "metadata": {}, "outputs": [{"output_type": "stream", "stream": "stdout", "text": ["1\n"]}], "prompt_number": 1},
            {"cell_type": "code", "collapsed": false, "input": [], "language": "python", "metadata": {}, "outputs": []},
            {"cell_type": "markdown", "metadata": {}, "source": "# md"}
        ], "metadata": {}}]}"##,
    )
    .unwrap();
    let reordered = dir.join("reordered.ipynb");
    std::fs::write(
        &reordered,
        "\u{feff}{\"nbformat_minor\":5,\"nbformat\":4,\"metadata\":{\"language_info\":{\"name\":\"R\"},\"kernelspec\":{\"name\":\"ir\",\"display_name\":\"\",\"language\":\"R\"}},\
         \"cells\":[{\"source\":\"Title\\n=====\",\"metadata\":{},\"cell_type\":\"markdown\",\"id\":\"x\"},\
         {\"outputs\":[{\"output_type\":\"stream\",\"name\":\"stdout\",\"text\":\"\\\"]}\"}],\"source\":[],\"metadata\":{},\"id\":\"y\",\"execution_count\":1,\"cell_type\":\"code\"},\
         {\"cell_type\":\"raw\",\"id\":\"z\",\"metadata\":{\"format\":\"text/x-python\"},\"source\":[\"r\\\\\"]}]}",
    )
    .unwrap();
    let mut v = g.notebooks;
    v.push(v3);
    v.push(reordered);
    v
}

fn check_against_value(paths: &[PathBuf]) {
    let mut checked = 0;
    for p in paths {
        let bytes = std::fs::read(p).unwrap();
        let reference = match serde_json::from_slice::<serde_json::Value>(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)) {
            Ok(v) => serde_impl::summarize_value(&v),
            Err(_) => {
                assert!(summarize(p).is_err(), "{} is invalid JSON but summarized", p.display());
                continue;
            }
        };
        let got = summarize(p).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
        assert_eq!(got, reference, "{}", p.display());
        assert_eq!(serde_impl::summarize_bytes_serde(&bytes).unwrap(), reference, "serde path {}", p.display());
        checked += 1;
    }
    eprintln!("checked {checked} notebooks against a full serde_json parse");
}

#[test]
fn anaconda_corpus_matches_full_parse() {
    if let Some(nbs) = anaconda_notebooks() {
        check_against_value(&nbs);
    }
}

#[test]
fn synthetic_corpus_matches_full_parse() {
    let dir = tempfile::tempdir().unwrap();
    check_against_value(&synthetic(dir.path()));
}

/// (cells, code, markdown, raw, kernel, language, has_outputs) as Python's nbformat sees them.
type NbfRow = (u32, u32, u32, u32, Option<String>, Option<String>, bool);

const NBFORMAT_SCRIPT: &str = r#"
import sys, json, warnings
warnings.simplefilter("ignore")
import nbformat
for line in sys.stdin:
    p = line.rstrip("\n")
    try:
        nb = nbformat.read(p, as_version=4)
    except Exception as e:
        print(json.dumps({"path": p, "error": repr(e)}))
        continue
    cells = nb.cells
    ks = nb.metadata.get("kernelspec") or {}
    li = nb.metadata.get("language_info") or {}
    print(json.dumps({
        "path": p,
        "cells": len(cells),
        "code": sum(c.cell_type == "code" for c in cells),
        "markdown": sum(c.cell_type == "markdown" for c in cells),
        "raw": sum(c.cell_type == "raw" for c in cells),
        "kernel": ks.get("display_name") or ks.get("name") or None,
        "language": li.get("name") or ks.get("language") or None,
        "has_outputs": any(c.cell_type == "code" and len(c.get("outputs", [])) > 0 for c in cells),
    }))
"#;

fn nbformat_rows(paths: &[PathBuf]) -> Option<Vec<(PathBuf, Option<NbfRow>)>> {
    let pythons = [format!("{ANACONDA}/bin/python"), "python3".to_string()];
    let python = pythons.iter().find(|py| {
        Command::new(py).args(["-I", "-c", "import nbformat"]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    });
    let Some(python) = python else {
        eprintln!("skipping: no python with nbformat");
        return None;
    };
    let mut child = Command::new(python)
        .args(["-I", "-c", NBFORMAT_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success());
    let rows = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            let path = PathBuf::from(v["path"].as_str().unwrap());
            if v.get("error").is_some() {
                return (path, None);
            }
            let n = |k: &str| v[k].as_u64().unwrap() as u32;
            let s = |k: &str| v[k].as_str().map(str::to_string);
            (path, Some((n("cells"), n("code"), n("markdown"), n("raw"), s("kernel"), s("language"), v["has_outputs"].as_bool().unwrap())))
        })
        .collect();
    Some(rows)
}

fn check_against_nbformat(paths: &[PathBuf]) {
    let Some(rows) = nbformat_rows(paths) else { return };
    assert_eq!(rows.len(), paths.len());
    let mut checked = 0;
    for (path, row) in rows {
        let Some(want) = row else {
            eprintln!("nbformat could not read {}", path.display());
            continue;
        };
        let s: NotebookSummary = summarize(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        let got = (s.cells, s.code, s.markdown, s.raw, s.kernel, s.language, s.has_outputs);
        assert_eq!(got, want, "{}", path.display());
        checked += 1;
    }
    eprintln!("checked {checked} notebooks against python nbformat");
    assert!(checked > 0);
}

#[test]
fn anaconda_corpus_matches_nbformat() {
    if let Some(nbs) = anaconda_notebooks() {
        check_against_nbformat(&nbs);
    }
}

#[test]
fn synthetic_corpus_matches_nbformat() {
    let dir = tempfile::tempdir().unwrap();
    check_against_nbformat(&synthetic(dir.path()));
}

#[test]
fn search_finds_planted_needles() {
    let dir = tempfile::tempdir().unwrap();
    let g = generator::workspace(dir.path(), 300, 20, 9);
    let cancel = AtomicBool::new(false);
    for (needle, path) in &g.needles {
        let hits = std::sync::Mutex::new(Vec::new());
        let st = herse_workspace::search(dir.path(), &SearchQuery::new(needle.as_str()), &cancel, |h| hits.lock().unwrap().push(h)).unwrap();
        let hits = hits.into_inner().unwrap();
        assert!(!hits.is_empty(), "{needle}");
        // herse_needle_1 also matches herse_needle_10.. — every hit must contain the needle.
        assert!(hits.iter().any(|h| &h.path == path), "{needle} not found in {}", path.display());
        assert!(hits.iter().all(|h| h.ranges.iter().all(|r| &h.line_text[r.clone()] == needle)));
        // Junk dirs, checkpoints and gitignored notebooks are never searched.
        assert!(hits.iter().all(|h| !h.path.to_string_lossy().contains("checkpoint")));
        assert_eq!(st.errors, 0);
    }
    // The walk excludes junk.
    let entries = walk(dir.path(), &WalkOptions::default());
    let nbs: Vec<_> = entries.iter().filter(|e| e.is_notebook()).collect();
    assert_eq!(nbs.len(), 300);
    assert!(nbs.iter().all(|e| !e.rel.starts_with("node_modules") && !e.rel.starts_with("outputs") && !e.rel.contains("scratch") && !e.rel.contains("__pycache__")));
}
