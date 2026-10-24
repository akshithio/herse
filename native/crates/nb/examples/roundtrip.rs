//! Round-trip report over a corpus of notebooks.
//!
//! ```text
//! cargo run -p herse-nb --release --example roundtrip -- [PATH_LIST_FILE | DIR ...]
//! ```
//!
//! With no arguments, scans `/opt/anaconda3` for `*.ipynb` (skipping
//! `.ipynb_checkpoints`). Reports how many notebooks parse, how many
//! re-serialize byte-identically with `to_string` (preserve mode), and a
//! categorized reason for every mismatch. `--verbose` prints each mismatch's
//! path and first differing line. `--canonical OUT_DIR` also writes the
//! Jupyter-canonical output of every notebook to `OUT_DIR` (for the Python
//! cross-check in `tests/py_crosscheck.py`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use herse_nb::{Indent, WriteOptions, parse, to_string, to_string_with};

fn collect(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if p.file_name().is_some_and(|n| n == ".ipynb_checkpoints") {
                continue;
            }
            collect(&p, out);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "ipynb") {
            out.push(p);
        }
    }
}

fn first_diff(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()))
}

fn line_at(s: &[u8], at: usize) -> String {
    let start = s[..at.min(s.len())].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
    let end = s[start..].iter().position(|&c| c == b'\n').map_or(s.len(), |i| start + i);
    String::from_utf8_lossy(&s[start..end]).chars().take(160).collect()
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "--verbose");
    args.retain(|a| a != "--verbose");
    let mut canonical_dir = None;
    if let Some(i) = args.iter().position(|a| a == "--canonical") {
        canonical_dir = Some(PathBuf::from(args.remove(i + 1)));
        args.remove(i);
    }
    let mut paths = Vec::new();
    if args.is_empty() {
        collect(Path::new("/opt/anaconda3"), &mut paths);
    }
    for a in &args {
        let p = Path::new(a);
        if p.is_dir() {
            collect(p, &mut paths);
        } else if p.extension().is_some_and(|x| x == "ipynb") {
            paths.push(p.to_path_buf());
        } else if let Ok(list) = std::fs::read_to_string(p) {
            paths.extend(list.lines().filter(|l| !l.is_empty()).map(PathBuf::from));
        }
    }
    paths.sort();
    if let Some(d) = &canonical_dir {
        std::fs::create_dir_all(d).unwrap();
    }

    let mut parsed = 0;
    let mut identical = 0;
    let mut jupyter_hint = 0;
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut mismatch: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut total_bytes = 0usize;
    let t0 = std::time::Instant::now();
    for (i, p) in paths.iter().enumerate() {
        let Ok(bytes) = std::fs::read(p) else {
            *errors.entry("unreadable".into()).or_default() += 1;
            continue;
        };
        total_bytes += bytes.len();
        let nb = match parse(&bytes) {
            Ok(nb) => nb,
            Err(e) => {
                let key = match &e {
                    herse_nb::Error::UnsupportedVersion { nbformat, .. } => format!("unsupported nbformat v{nbformat}"),
                    herse_nb::Error::Json { .. } => "invalid JSON".into(),
                    herse_nb::Error::Invalid(m) => format!("invalid: {}", m.chars().take(60).collect::<String>()),
                };
                if verbose {
                    println!("ERR {}: {e}", p.display());
                }
                *errors.entry(key).or_default() += 1;
                continue;
            }
        };
        parsed += 1;
        if nb.format.is_jupyter() {
            jupyter_hint += 1;
        }
        if let Some(d) = &canonical_dir {
            std::fs::write(d.join(format!("{i:05}.ipynb")), to_string_with(&nb, &WriteOptions::jupyter())).unwrap();
            let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            std::fs::write(d.join(format!("{i:05}.path")), abs.to_string_lossy().as_bytes()).unwrap();
        }
        if let Some((major, _)) = nb.format.converted_from {
            mismatch.entry(format!("converted from nbformat v{major} (expected)")).or_default().push(p.clone());
            continue;
        }
        let out = to_string(&nb);
        if out.as_bytes() == bytes.as_slice() {
            identical += 1;
            continue;
        }
        let at = first_diff(out.as_bytes(), &bytes);
        let ours = line_at(out.as_bytes(), at);
        let theirs = line_at(&bytes, at);
        let category = categorize(&nb, &ours, &theirs);
        if verbose {
            println!("MISMATCH [{category}] {}\n  ours:   {ours}\n  theirs: {theirs}", p.display());
        }
        mismatch.entry(category).or_default().push(p.clone());
    }
    let dt = t0.elapsed();
    println!("notebooks:   {}", paths.len());
    println!("parsed:      {parsed}");
    println!("identical:   {identical} / {parsed} (to_string, preserve mode)");
    println!("jupyter fmt: {jupyter_hint} / {parsed} detected as nbformat-canonical formatting");
    println!("bytes:       {:.1} MB in {:.1} ms (read+parse+write)", total_bytes as f64 / 1e6, dt.as_secs_f64() * 1e3);
    for (k, v) in &errors {
        println!("  parse error  {v:4}  {k}");
    }
    for (k, v) in &mismatch {
        println!("  mismatch     {:4}  {k}", v.len());
        for p in v.iter().take(3) {
            println!("                     e.g. {}", p.display());
        }
    }
}

fn categorize(nb: &herse_nb::Notebook, ours: &str, theirs: &str) -> String {
    let t = theirs.trim_start();
    let o = ours.trim_start();
    if ours.trim() == theirs.trim() {
        return "whitespace/indent".into();
    }
    if t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-') || t.contains("e-") || t.contains("e+") {
        return "number formatting".into();
    }
    if t.contains("\\u") || o.contains("\\u") {
        return "string escaping".into();
    }
    if !nb.format.sort_keys {
        return format!("key order (unsorted file, indent {:?})", nb.format.indent);
    }
    if nb.format.indent == Indent::Compact {
        return "compact file".into();
    }
    format!("other: ours `{}` vs theirs `{}`", o.chars().take(50).collect::<String>(), t.chars().take(50).collect::<String>())
}
