//! Golden tests: every table output of the fixture notebooks and every
//! generated sample in `tests/data` (see `tests/data/generate.py`), rendered
//! with `common::render` and compared to `tests/golden/*.txt`.
//!
//! Regenerate after an intended change: `UPDATE_GOLDEN=1 cargo test -p herse-table --test golden`
//! and review the diff.

mod common;

fn run(cases: Vec<(String, String)>) {
    let mut failures = Vec::new();
    for (name, html) in &cases {
        let rendered = match herse_table::parse(html) {
            Some(t) => common::render(&t),
            None => "None\n".to_string(),
        };
        if let Err(e) = common::check_golden(name, &rendered) {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{} of {} golden cases failed:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

fn notebook(file: &str, prefix: &str) -> Vec<(String, String)> {
    let path = common::native_root().join("fixtures").join(file);
    let cases: Vec<_> =
        common::notebook_tables(&path).into_iter().map(|(label, html)| (format!("{prefix}_{label}"), html)).collect();
    assert!(!cases.is_empty(), "{file}: no tables found");
    cases
}

#[test]
fn dataframes_notebook() {
    let cases = notebook("dataframes.ipynb", "dataframes");
    assert_eq!(cases.len(), 28);
    run(cases);
}

#[test]
fn showcase_notebook() {
    run(notebook("showcase.ipynb", "showcase"));
}

#[test]
fn other_kernels_and_formats() {
    let mut cases = notebook("formats/kernel_r.ipynb", "kernel_r");
    cases.extend(notebook("formats/kernel_julia.ipynb", "kernel_julia"));
    cases.extend(notebook("formats/colab_style.ipynb", "colab"));
    run(cases);
}

#[test]
fn generated_samples() {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(common::data_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "html") {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            cases.push((format!("sample_{name}"), std::fs::read_to_string(&path).unwrap()));
        }
    }
    cases.sort();
    assert!(cases.len() >= 20);
    run(cases);
}
