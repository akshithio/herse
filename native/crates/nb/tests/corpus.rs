//! Round-trip tests over real notebooks.
//!
//! * `tests/data/*.ipynb`: one edge-case notebook written in many writers'
//!   styles (see `gen_edge_corpus.py`). Each must re-serialize byte-identically
//!   with `to_string`, and `to_string_with(.., jupyter())` must equal what
//!   nbformat 5.10 itself writes for it (frozen in `tests/data/jupyter/`).
//! * `/opt/anaconda3/**/*.ipynb` (skipped when absent): every notebook must
//!   parse and re-serialize byte-identically.

use std::path::{Path, PathBuf};

use herse_nb::{WriteOptions, parse, to_string, to_string_with};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn mask_ids(s: &str) -> String {
    // v3 upgrades get random ids, on both sides.
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        match line.trim_start().strip_prefix("\"id\": \"") {
            Some(rest) if rest.len() >= 9 && rest.as_bytes()[8] == b'"' => {
                out.push_str(&line[..line.len() - rest.len()]);
                out.push_str("<id>");
                out.push_str(&rest[8..]);
            }
            _ => out.push_str(line),
        }
    }
    out
}

#[test]
fn edge_corpus() {
    let mut checked = 0;
    for entry in std::fs::read_dir(data_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|x| x != "ipynb") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(&path).unwrap();
        let nb = parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        if nb.format.converted_from.is_none() {
            assert!(to_string(&nb).as_bytes() == bytes.as_slice(), "{name}: preserve-mode round trip differs");
        }
        let expected = data_dir().join("jupyter").join(&name);
        if let Ok(want) = std::fs::read_to_string(&expected) {
            let ours = to_string_with(&nb, &WriteOptions::jupyter());
            let (ours, want) = if nb.format.converted_from.is_some() { (mask_ids(&ours), mask_ids(&want)) } else { (ours, want) };
            if ours != want {
                let line = ours.lines().zip(want.lines()).position(|(a, b)| a != b).unwrap_or(0);
                panic!(
                    "{name}: canonical output differs from nbformat at line {}\n  ours:     {:?}\n  nbformat: {:?}",
                    line + 1,
                    ours.lines().nth(line),
                    want.lines().nth(line)
                );
            }
        }
        checked += 1;
    }
    assert!(checked >= 15, "edge corpus missing ({checked} files)");
}

/// All `*.ipynb` under `root` (skipping `.ipynb_checkpoints`), walked in parallel.
fn collect(root: &Path) -> Vec<PathBuf> {
    use std::sync::{Condvar, Mutex};
    struct Q {
        dirs: Vec<PathBuf>,
        busy: usize,
        found: Vec<PathBuf>,
    }
    let q = Mutex::new(Q { dirs: vec![root.to_path_buf()], busy: 0, found: Vec::new() });
    let cv = Condvar::new();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let dir = {
                    let mut g = q.lock().unwrap();
                    loop {
                        if let Some(d) = g.dirs.pop() {
                            g.busy += 1;
                            break d;
                        }
                        if g.busy == 0 {
                            cv.notify_all();
                            return;
                        }
                        g = cv.wait(g).unwrap();
                    }
                };
                let (mut dirs, mut files) = (Vec::new(), Vec::new());
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let Ok(ft) = e.file_type() else { continue };
                        let p = e.path();
                        if ft.is_dir() {
                            if p.file_name().is_some_and(|n| n != ".ipynb_checkpoints") {
                                dirs.push(p);
                            }
                        } else if ft.is_file() && p.extension().is_some_and(|x| x == "ipynb") {
                            files.push(p);
                        }
                    }
                }
                let mut g = q.lock().unwrap();
                g.dirs.extend(dirs);
                g.found.extend(files);
                g.busy -= 1;
                cv.notify_all();
            });
        }
    });
    let mut found = q.into_inner().unwrap().found;
    found.sort();
    found
}

/// Parse + re-serialize every notebook under `root`; returns (identical, total, failures).
fn check_tree(root: &Path) -> (usize, usize, Vec<String>) {
    let paths = collect(root);
    let mut identical = 0;
    let mut failures = Vec::new();
    for p in &paths {
        let Ok(bytes) = std::fs::read(p) else { continue };
        match parse(&bytes) {
            Ok(nb) if nb.format.converted_from.is_some() => identical += 1,
            Ok(nb) if to_string(&nb).as_bytes() == bytes.as_slice() => identical += 1,
            // A notebook without a `cells` key gets one on save (Jupyter cannot open it at all).
            Ok(nb) if nb.cells.is_empty() && memchr_cells(&bytes) => identical += 1,
            Ok(_) => failures.push(format!("differs: {}", p.display())),
            Err(e) => failures.push(format!("parse error: {}: {e}", p.display())),
        }
    }
    (identical, paths.len(), failures)
}

fn memchr_cells(b: &[u8]) -> bool {
    !b.windows(7).any(|w| w == b"\"cells\"")
}

#[test]
fn anaconda_corpus() {
    let root = Path::new("/opt/anaconda3");
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let (identical, total, failures) = check_tree(root);
    eprintln!("anaconda corpus: {identical} / {total} identical");
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

/// The app's fixture corpus (`native/fixtures`, produced by its own
/// generator, so it changes independently of this crate): everything that
/// parses should round-trip; `corrupt_*` files are expected not to parse.
/// Report-only unless `HERSE_NB_STRICT_FIXTURES=1`, since the generator may be
/// mid-run (half-written files) or add new cases.
#[test]
fn app_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let (identical, total, failures) = check_tree(&root);
    let failures: Vec<_> = failures.into_iter().filter(|f| !(f.starts_with("parse error") && f.contains("corrupt_"))).collect();
    eprintln!("fixtures: {identical} / {total} identical");
    for f in &failures {
        eprintln!("  {f}");
    }
    if std::env::var_os("HERSE_NB_STRICT_FIXTURES").is_some() {
        assert!(failures.is_empty(), "{} failures", failures.len());
    }
}

/// Truncations and byte mutations of the edge corpus must never panic, and
/// whatever parses must survive a write/parse cycle unchanged (model-wise).
#[test]
fn mutations_never_panic() {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut parsed = 0;
    for entry in std::fs::read_dir(data_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|x| x != "ipynb") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        for i in 0..300 {
            let mut b = bytes.clone();
            match i % 3 {
                0 => b.truncate((rand() as usize) % b.len()),
                1 => {
                    for _ in 0..1 + rand() % 4 {
                        let at = (rand() as usize) % b.len();
                        const ALPHABET: &[u8] = b"{}[]\",:\\0123456789eE.-+ \nnultrfaseu";
                        b[at] = ALPHABET[(rand() as usize) % ALPHABET.len()];
                    }
                }
                _ => {
                    let at = (rand() as usize) % b.len();
                    let len = ((rand() as usize) % 64).min(b.len() - at);
                    b.drain(at..at + len);
                }
            }
            if let Ok(nb) = parse(&b) {
                parsed += 1;
                for out in [to_string(&nb), to_string_with(&nb, &WriteOptions::jupyter())] {
                    let back = parse(out.as_bytes()).unwrap_or_else(|e| panic!("{}: reparse failed: {e}\n{out}", path.display()));
                    assert_eq!(back.cells.len(), nb.cells.len());
                    for (a, c) in back.cells.iter().zip(&nb.cells) {
                        assert_eq!(a.source, c.source, "{}", path.display());
                        assert_eq!(a.outputs.len(), c.outputs.len(), "{}", path.display());
                    }
                }
            }
        }
    }
    assert!(parsed > 100, "too few mutants parsed ({parsed})");
}
