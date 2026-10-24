//! Deterministic synthetic notebook workspace generator (shared by
//! `examples/gen_workspace.rs`, `examples/bench.rs` and `tests/corpus.rs`).
#![allow(dead_code)]

use serde_json::{Map, Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "analysis", "cleaning", "eda", "model", "train", "eval", "plot", "features", "pipeline", "report", "summary",
    "explore", "baseline", "tuning", "inference", "Data Cleaning", "customer churn", "timeseries", "forecast",
    "clustering", "regression", "embedding", "sales", "survey", "ab_test", "nlp", "vision",
];

const CODE: &[&str] = &[
    "import numpy as np",
    "import pandas as pd",
    "import matplotlib.pyplot as plt",
    "from sklearn.model_selection import train_test_split",
    "from sklearn.ensemble import RandomForestClassifier",
    "df = pd.read_csv(\"data/raw.csv\")",
    "df.head()",
    "df.describe()",
    "X_train, X_test, y_train, y_test = train_test_split(X, y, test_size=0.2, random_state=42)",
    "model = RandomForestClassifier(n_estimators=200)",
    "model.fit(X_train, y_train)",
    "preds = model.predict(X_test)",
    "print(f\"accuracy: {(preds == y_test).mean():.3f}\")",
    "fig, ax = plt.subplots(figsize=(8, 4))",
    "ax.plot(history[\"loss\"], label=\"loss\")",
    "def train_model(cfg):\n    model = build(cfg)\n    for epoch in range(cfg.epochs):\n        loss = step(model)\n        print(\"Epoch\", epoch, loss)\n    return model",
    "def evaluate_model(model, data):\n    return {\"f1\": f1_score(data.y, model.predict(data.X))}",
    "for col in df.columns:\n    if df[col].dtype == object:\n        df[col] = df[col].astype(\"category\")",
    "# TODO: handle missing values\ndf = df.dropna(subset=[\"target\"])",
    "results = pd.DataFrame(rows).sort_values(\"score\", ascending=False)",
    "%matplotlib inline",
    "with open(\"config.yaml\") as f:\n    cfg = yaml.safe_load(f)",
    "s = \"quote \\\" and backslash \\\\ and tab\\t\"",
    "caf\u{e9} = \"na\u{ef}ve \u{2014} \u{4e2d}\u{6587}\"",
];

const PROSE: &[&str] = &[
    "This notebook explores the dataset and builds a baseline model.",
    "We split the data with `train_test_split` and evaluate on the held-out set.",
    "**Note:** the results below depend on the random seed.",
    "See the [docs](https://example.com/docs) for details.",
    "The DataFrame has a few missing values that we drop.",
    "- step one\n- step two\n- step three",
    "$$ L = \\frac{1}{n} \\sum_i (y_i - \\hat{y}_i)^2 $$",
];

const KERNELS: &[(&str, &str, &str)] = &[
    ("python3", "Python 3 (ipykernel)", "python"),
    ("python3", "Python 3", "python"),
    ("ir", "R", "R"),
    ("julia-1.10", "Julia 1.10.0", "julia"),
    ("conda-env-ml-py", "Python [conda env:ml]", "python"),
];

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn random_bytes(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(&rng.next().to_le_bytes());
    }
    v.truncate(n);
    v
}

/// nbformat-style multiline string: list of lines keeping their "\n".
pub fn lines(s: &str) -> Value {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(p) = rest.find('\n') {
        out.push(Value::String(rest[..=p].to_string()));
        rest = &rest[p + 1..];
    }
    if !rest.is_empty() {
        out.push(Value::String(rest.to_string()));
    }
    Value::Array(out)
}

pub struct NbSpec {
    pub cells: usize,
    /// Percent of code cells with outputs.
    pub output_pct: u64,
    /// Percent of code cells with an image output, and its decoded size.
    pub image_pct: u64,
    pub image_bytes: usize,
    /// Lines of stream output per output-bearing cell.
    pub stream_lines: (u64, u64),
    /// Store sources as one string instead of a line array.
    pub string_sources: bool,
    pub needle: Option<String>,
}

impl NbSpec {
    pub fn typical(rng: &mut Rng) -> NbSpec {
        let heavy = rng.chance(3);
        NbSpec {
            cells: rng.range(4, 70) as usize,
            output_pct: 70,
            image_pct: if heavy { 60 } else { 12 },
            image_bytes: if heavy { rng.range(100_000, 400_000) as usize } else { rng.range(8_000, 40_000) as usize },
            stream_lines: (1, 15),
            string_sources: rng.chance(10),
            needle: None,
        }
    }
}

pub fn notebook(rng: &mut Rng, spec: &NbSpec) -> Value {
    let (kname, kdisp, lang) = *rng.pick(KERNELS);
    let mut cells = Vec::with_capacity(spec.cells);
    let mut exec = 0;
    let needle_at = spec.needle.as_ref().map(|_| rng.below(spec.cells as u64) as usize);
    for i in 0..spec.cells {
        let roll = rng.below(100);
        let id = format!("{:08x}", rng.next() as u32);
        let mut c = Map::new();
        let src = |rng: &mut Rng, s: String, string_sources: bool| -> Value {
            let _ = rng;
            if string_sources { Value::String(s) } else { lines(&s) }
        };
        if i == 0 || roll < 30 {
            let mut text = if i == 0 {
                format!("# {} {}\n\n{}", rng.pick(WORDS), rng.pick(WORDS), rng.pick(PROSE))
            } else {
                let n = rng.range(1, 4);
                (0..n).map(|_| rng.pick(PROSE).to_string()).collect::<Vec<_>>().join("\n\n")
            };
            if needle_at == Some(i) {
                text.push_str(&format!("\n\nThe {} lives here.", spec.needle.as_ref().unwrap()));
            }
            c.insert("cell_type".into(), json!("markdown"));
            c.insert("id".into(), json!(id));
            c.insert("metadata".into(), json!({}));
            c.insert("source".into(), src(rng, text, spec.string_sources));
        } else if roll < 95 {
            let n = rng.range(1, 6);
            let mut code = (0..n).map(|_| rng.pick(CODE).to_string()).collect::<Vec<_>>().join("\n");
            if needle_at == Some(i) {
                code.push_str(&format!("\n{} = 1", spec.needle.as_ref().unwrap()));
            }
            let mut outputs = Vec::new();
            if rng.chance(spec.output_pct) {
                exec += 1;
                let nlines = rng.range(spec.stream_lines.0, spec.stream_lines.1);
                let text: String = (0..nlines).map(|k| format!("Epoch {k}: loss={:.4} acc={:.3}\n", 1.0 / (k + 1) as f64, k as f64 / nlines as f64)).collect();
                outputs.push(json!({"name": "stdout", "output_type": "stream", "text": lines(&text)}));
                if rng.chance(spec.image_pct) {
                    let png = base64(&random_bytes(rng, spec.image_bytes));
                    outputs.push(json!({
                        "data": {"image/png": png, "text/plain": ["<Figure size 800x400 with 1 Axes>"]},
                        "metadata": {"needs_background": "light"},
                        "output_type": "display_data"
                    }));
                }
                if rng.chance(20) {
                    let rows: String = (0..rng.range(3, 10)).map(|r| format!("<tr><td>{r}</td><td>{}</td></tr>\n", rng.below(1000))).collect();
                    outputs.push(json!({
                        "data": {"text/html": lines(&format!("<table>\n{rows}</table>")), "text/plain": lines("   a    b\n0  1  2\n1  3  4")},
                        "execution_count": exec,
                        "metadata": {},
                        "output_type": "execute_result"
                    }));
                }
                if rng.chance(3) {
                    outputs.push(json!({
                        "ename": "ValueError",
                        "evalue": "could not convert string to float",
                        "output_type": "error",
                        "traceback": ["\u{1b}[0;31m---------------------------------------------------------------------------\u{1b}[0m", "\u{1b}[0;31mValueError\u{1b}[0m: could not convert string to float"]
                    }));
                }
            }
            c.insert("cell_type".into(), json!("code"));
            c.insert("execution_count".into(), if outputs.is_empty() { Value::Null } else { json!(exec) });
            c.insert("id".into(), json!(id));
            c.insert("metadata".into(), json!({}));
            c.insert("outputs".into(), Value::Array(outputs));
            c.insert("source".into(), src(rng, code, spec.string_sources));
        } else {
            c.insert("cell_type".into(), json!("raw"));
            c.insert("id".into(), json!(id));
            c.insert("metadata".into(), json!({}));
            c.insert("source".into(), src(rng, "raw cell \\ {not: json}".into(), spec.string_sources));
        }
        cells.push(Value::Object(c));
    }
    json!({
        "cells": cells,
        "metadata": {
            "kernelspec": {"display_name": kdisp, "language": lang, "name": kname},
            "language_info": {"codemirror_mode": {"name": "ipython", "version": 3}, "file_extension": ".py", "name": lang, "version": "3.11.5"}
        },
        "nbformat": 4,
        "nbformat_minor": 5
    })
}

/// Writes like `nbformat.write`: indent=1, trailing newline.
pub fn write_nb(path: &Path, v: &Value) {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).unwrap();
    }
    let f = fs::File::create(path).unwrap();
    let mut w = std::io::BufWriter::new(f);
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut w, fmt);
    serde::Serialize::serialize(v, &mut ser).unwrap();
    w.write_all(b"\n").unwrap();
}

fn touch(path: &Path, body: &str) {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(path, body).unwrap();
}

pub struct Generated {
    pub root: PathBuf,
    pub notebooks: Vec<PathBuf>,
    /// Notebooks containing `herse_needle_<k>` for the k-th planted needle.
    pub needles: Vec<(String, PathBuf)>,
}

/// Generates `n` notebooks under `root` with realistic layout, plus junk that
/// the walker must skip (node_modules, .git, checkpoints, venvs, gitignored
/// dirs) and `other_files` non-notebook files.
pub fn workspace(root: &Path, n: usize, other_files: usize, seed: u64) -> Generated {
    let mut rng = Rng::new(seed);
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    touch(&root.join(".gitignore"), "outputs/\n*.scratch.ipynb\n");
    touch(&root.join("README.md"), "# Synthetic workspace\n");
    let projects = (n / 40).max(1);
    let subdirs = ["", "experiments", "reports", "notebooks/archive", "eda/2024/q3", "models/v2"];
    let mut notebooks = Vec::with_capacity(n);
    let mut needles = Vec::new();
    for i in 0..n {
        let p = rng.below(projects as u64);
        let sub = rng.pick(&subdirs);
        let name = match rng.below(4) {
            0 => format!("nb{i}.ipynb"),
            1 => format!("{} {i}.ipynb", rng.pick(WORDS)),
            _ => format!("{}_{}_{i}.ipynb", rng.pick(WORDS), rng.pick(WORDS)),
        };
        let dir = root.join(format!("projects/p{p}")).join(sub);
        let path = dir.join(&name);
        let mut spec = NbSpec::typical(&mut rng);
        if i % 100 == 7 {
            let needle = format!("herse_needle_{}", needles.len());
            spec.needle = Some(needle.clone());
            needles.push((needle, path.clone()));
        }
        let nb = notebook(&mut rng, &spec);
        write_nb(&path, &nb);
        if rng.chance(8) {
            // Jupyter's checkpoint copy; must never be listed or searched.
            write_nb(&dir.join(".ipynb_checkpoints").join(name.replace(".ipynb", "-checkpoint.ipynb")), &nb);
        }
        notebooks.push(path);
    }
    let exts = ["py", "csv", "md", "txt", "json", "png", "parquet"];
    for i in 0..other_files {
        let p = rng.below(projects as u64);
        let ext = rng.pick(&exts);
        touch(&root.join(format!("projects/p{p}/{}/file_{i}.{ext}", rng.pick(&["src", "data", "docs", ""]))), "x\n");
    }
    // Junk the walker must skip.
    let small = notebook(&mut rng, &NbSpec { cells: 3, output_pct: 0, image_pct: 0, image_bytes: 0, stream_lines: (0, 0), string_sources: false, needle: None });
    for i in 0..(n / 5).max(10) {
        touch(&root.join(format!("node_modules/pkg{}/lib/f{i}.js", i % 50)), "module.exports = 1;\n");
        touch(&root.join(format!(".git/objects/{:02x}/{i:038x}", i % 256)), "blob");
    }
    for i in 0..10 {
        write_nb(&root.join(format!("node_modules/pkg{i}/demo.ipynb")), &small);
        write_nb(&root.join(format!(".venv/lib/python3.11/site-packages/pkg{i}/example.ipynb")), &small);
        write_nb(&root.join(format!("projects/p0/__pycache__/x{i}.ipynb")), &small);
        write_nb(&root.join(format!("outputs/run{i}/result.ipynb")), &small);
        write_nb(&root.join(format!("projects/p0/tmp{i}.scratch.ipynb")), &small);
    }
    Generated { root: root.to_path_buf(), notebooks, needles }
}

/// One huge notebook: `target_mb` of base64 PNG outputs (`image = true`) or
/// of short stream-output lines (`image = false`, the scanner's worst case).
pub fn big_notebook(path: &Path, target_mb: usize, image: bool, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut cells = Vec::new();
    let target = target_mb * 1024 * 1024;
    let mut size = 0;
    let mut i = 0;
    while size < target {
        if i % 4 == 0 {
            cells.push(json!({"cell_type": "markdown", "id": format!("m{i}"), "metadata": {}, "source": lines(&format!("## Section {i}\nSome words about train_test_split."))}));
        }
        let output = if image {
            let png = base64(&random_bytes(&mut rng, 180_000));
            size += png.len();
            json!({"data": {"image/png": png, "text/plain": ["<Figure size 640x480 with 1 Axes>"]}, "metadata": {}, "output_type": "display_data"})
        } else {
            let text: String = (0..5000).map(|k| format!("step {k:>6} | loss {:.6} | lr 3e-4\n", 1.0 / (k + 1) as f64)).collect();
            size += text.len() + 5000 * 4;
            json!({"name": "stdout", "output_type": "stream", "text": lines(&text)})
        };
        cells.push(json!({
            "cell_type": "code", "execution_count": i, "id": format!("c{i}"), "metadata": {},
            "outputs": [output],
            "source": lines("plt.plot(x, y)\nplt.show()")
        }));
        i += 1;
    }
    let nb = json!({
        "cells": cells,
        "metadata": {"kernelspec": {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"}, "language_info": {"name": "python"}},
        "nbformat": 4, "nbformat_minor": 5
    });
    write_nb(path, &nb);
}
