//! Preflight: catch the errors a notebook is going to hit before it hits
//! them. Herse reads the notebook's imports, asks the kernel's Python which
//! of them it can't find (`importlib.util.find_spec`, so nothing is
//! imported), and offers to install them, or to switch to another installed
//! kernel that already has all of them. After a run fails, the same engine
//! turns `ModuleNotFoundError` and `NameError` into one-click fixes.
//!
//! Installing never happens on open: a package's build can run code, and
//! opening a file someone sent you must not. It happens when you press
//! Install, or (with the setting on) when you run the notebook, which runs
//! its code anyway.

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, prelude::FluentBuilder as _, px, svg,
};
use herse_kernel::KernelSpec;
use herse_nb::{CellType, Output};

use super::{NotebookView, session};
use crate::theme;
use crate::ui::{Tone, banner};

/// Modules whose pip package has another name.
const PIP_NAMES: &[(&str, &str)] = &[
    ("sklearn", "scikit-learn"),
    ("skimage", "scikit-image"),
    ("cv2", "opencv-python"),
    ("PIL", "pillow"),
    ("yaml", "pyyaml"),
    ("bs4", "beautifulsoup4"),
    ("dateutil", "python-dateutil"),
    ("dotenv", "python-dotenv"),
    ("docx", "python-docx"),
    ("pptx", "python-pptx"),
    ("Crypto", "pycryptodome"),
    ("OpenSSL", "pyopenssl"),
    ("serial", "pyserial"),
    ("usb", "pyusb"),
    ("magic", "python-magic"),
    ("jwt", "pyjwt"),
    ("git", "gitpython"),
    ("google.generativeai", "google-generativeai"),
    ("attr", "attrs"),
    ("fitz", "pymupdf"),
    ("Levenshtein", "python-Levenshtein"),
    ("mpl_toolkits", "matplotlib"),
    ("pydantic_settings", "pydantic-settings"),
    ("faiss", "faiss-cpu"),
    ("umap", "umap-learn"),
    ("hdbscan", "hdbscan"),
    ("tensorflow_datasets", "tensorflow-datasets"),
    ("tensorflow_hub", "tensorflow-hub"),
    ("sentence_transformers", "sentence-transformers"),
    ("llama_cpp", "llama-cpp-python"),
    ("lightgbm", "lightgbm"),
    ("Bio", "biopython"),
    ("osgeo", "gdal"),
    ("wx", "wxpython"),
    ("zmq", "pyzmq"),
    ("ruamel", "ruamel.yaml"),
    ("pkg_resources", "setuptools"),
    ("IPython", "ipython"),
    ("ipywidgets", "ipywidgets"),
    ("duckdb", "duckdb"),
    ("polars", "polars"),
];

/// Modules that come with Python or with the kernel itself: never missing,
/// so never worth a subprocess (and never installable).
const BUILT_IN: &[&str] = &[
    "__future__", "abc", "argparse", "array", "ast", "asyncio", "base64", "bisect", "builtins", "bz2", "calendar",
    "cmath", "codecs", "collections", "colorsys", "concurrent", "configparser", "contextlib", "copy", "csv",
    "ctypes", "dataclasses", "datetime", "decimal", "difflib", "dis", "email", "enum", "errno", "fnmatch",
    "fractions", "functools", "gc", "getpass", "glob", "gzip", "hashlib", "heapq", "hmac", "html", "http",
    "importlib", "inspect", "io", "ipaddress", "itertools", "json", "keyword", "linecache", "locale", "logging",
    "lzma", "math", "mimetypes", "multiprocessing", "numbers", "operator", "os", "pathlib", "pickle", "platform",
    "pprint", "queue", "random", "re", "secrets", "select", "shlex", "shutil", "signal", "socket", "sqlite3",
    "statistics", "string", "struct", "subprocess", "sys", "tarfile", "tempfile", "textwrap", "threading",
    "time", "timeit", "tkinter", "token", "tokenize", "traceback", "types", "typing", "unicodedata", "unittest",
    "urllib", "uuid", "warnings", "weakref", "xml", "zipfile", "zlib", "zoneinfo", "IPython", "ipykernel",
];

/// What `import`s a cell makes at its top level: top-level module names,
/// in order, without duplicates. Indented imports (inside `try`, `if`, a
/// function) are optional or late and are left alone, as are relative
/// imports and IPython magics.
pub fn imports(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |module: &str| {
        let top = module.split('.').next().unwrap_or("").trim();
        if !top.is_empty() && is_identifier(top) && !out.iter().any(|m| m == top) {
            out.push(top.to_string());
        }
    };
    for line in logical_lines(source) {
        for stmt in line.split(';') {
            let stmt = stmt.trim();
            if let Some(rest) = stmt.strip_prefix("import ") {
                for part in rest.split(',') {
                    let name = part.split_whitespace().next().unwrap_or("");
                    push(name);
                }
            } else if let Some(rest) = stmt.strip_prefix("from ") {
                let module = rest.split_whitespace().next().unwrap_or("");
                if !module.starts_with('.') && rest[module.len()..].trim_start().starts_with("import") {
                    push(module);
                }
            }
        }
    }
    out
}

/// Packages a notebook installs itself (`%pip install x`, `!pip install
/// x`): those it will have once it runs.
pub fn self_installed(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = ["%pip install", "!pip install", "!pip3 install", "%uv pip install", "!uv pip install"]
            .iter()
            .find_map(|p| line.strip_prefix(p))
        else {
            continue;
        };
        for word in rest.split_whitespace() {
            let word = word.trim_matches(['\'', '"']);
            if word.starts_with('-') || word.contains('/') || word.starts_with('$') || word.starts_with('{') {
                continue;
            }
            let name = word.split(['=', '<', '>', '!', '~', '[', ';']).next().unwrap_or("");
            if !name.is_empty() {
                out.push(normalize(name));
            }
        }
    }
    out
}

/// The pip package that provides `module`.
pub fn pip_name(module: &str) -> String {
    PIP_NAMES
        .iter()
        .find(|(m, _)| *m == module)
        .map_or_else(|| module.replace('_', "-"), |(_, p)| (*p).to_string())
}

/// Package names compare case-, `-`/`_`- and `.`-insensitively (PEP 503).
fn normalize(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// Lines with no indentation, continuation lines (`\`, open brackets)
/// joined, comments dropped. Inside a string a line is skipped.
fn logical_lines(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut continued = false;
    for raw in source.lines() {
        let code = raw.split('#').next().unwrap_or("");
        let starts_statement = depth == 0 && !continued;
        if starts_statement && (raw.starts_with([' ', '\t']) || raw.starts_with(['%', '!'])) {
            continue;
        }
        let trimmed = code.trim_end();
        continued = trimmed.ends_with('\\');
        current.push_str(trimmed.trim_end_matches('\\'));
        current.push(' ');
        let mut quote = None;
        for c in trimmed.chars() {
            match (quote, c) {
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '(' | '[' | '{') => depth += 1,
                (None, ')' | ']' | '}') => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 && !continued {
            depth = 0;
            let line = current.trim();
            if !line.is_empty() {
                out.push(line.replace(['(', ')'], " "));
            }
            current.clear();
        }
    }
    out
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The interpreter a Python kernel runs, when its kernelspec says so
/// (`argv[0]` is a `python…` binary; a bare one is looked up on the
/// user's shell PATH).
pub fn python_of(spec: &KernelSpec) -> Option<PathBuf> {
    if !spec.language.eq_ignore_ascii_case("python") {
        return None;
    }
    let exe = spec.argv.first()?;
    let path = Path::new(exe);
    if !path.file_name()?.to_str()?.starts_with("python") {
        return None;
    }
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }
    which(exe)
}

/// `name` on the user's shell PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = herse_kernel::shell_env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// `uv`, if the user has it: it installs in a fraction of pip's time and
/// into environments that have no pip.
fn uv() -> Option<PathBuf> {
    which("uv").or_else(|| {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        [home.join(".local/bin/uv"), home.join(".cargo/bin/uv"), PathBuf::from("/opt/homebrew/bin/uv")]
            .into_iter()
            .find(|p| p.is_file())
    })
}

const CHECK: &str = "\
import sys, importlib.util as u
def ok(m):
    try:
        return u.find_spec(m) is not None
    except Exception:
        return True
print('\\n'.join(m for m in sys.argv[1:] if not ok(m)))
";

/// Which of `modules` `python` can't find, run in `cwd` (a notebook's own
/// modules sit beside it) with the kernelspec's environment.
pub fn missing(python: &Path, spec: &KernelSpec, cwd: &Path, modules: &[String]) -> Result<Vec<String>, String> {
    let modules: Vec<&String> = modules.iter().filter(|m| !BUILT_IN.contains(&m.as_str())).collect();
    if modules.is_empty() {
        return Ok(Vec::new());
    }
    let mut cmd = command(python);
    cmd.arg("-c").arg(CHECK).args(&modules).current_dir(cwd).envs(&spec.env);
    let out = run_with_timeout(cmd, Duration::from_secs(15))?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
}

fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    if let Some(env) = herse_kernel::shell_env::get() {
        cmd.envs(env);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut s) = child.stdout.take() {
                    s.read_to_string(&mut out).ok();
                }
                if let Some(mut s) = child.stderr.take() {
                    s.read_to_string(&mut err).ok();
                }
                return if status.success() { Ok(out) } else { Err(err.trim().to_string()) };
            }
            Ok(None) if started.elapsed() > timeout => {
                child.kill().ok();
                return Err("Python took too long to answer".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// How packages get installed for one interpreter.
#[derive(Clone, Debug)]
pub struct Installer {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Installer {
    pub fn new(python: &Path, packages: &[String]) -> Installer {
        let python_arg = python.to_string_lossy().into_owned();
        match uv() {
            Some(uv) => Installer {
                program: uv,
                args: ["pip", "install", "--python", &python_arg].iter().map(|s| s.to_string()).chain(packages.iter().cloned()).collect(),
            },
            None => Installer {
                program: python.to_path_buf(),
                args: ["-m", "pip", "install"].iter().map(|s| s.to_string()).chain(packages.iter().cloned()).collect(),
            },
        }
    }

    /// The command as a shell line, to show and to copy (paths in the home
    /// folder as `~/…`, which a shell expands).
    pub fn command_line(&self) -> String {
        let program = if self.program.file_name().is_some_and(|n| n == "uv") {
            "uv".to_string()
        } else {
            home_relative(&self.program.to_string_lossy())
        };
        std::iter::once(program).chain(self.args.iter().map(|a| home_relative(a))).collect::<Vec<_>>().join(" ")
    }

    /// Run it, calling `line` with each line of output as it comes; the
    /// whole output on failure.
    pub fn run(&self, mut line: impl FnMut(String)) -> Result<(), String> {
        let mut cmd = command(&self.program);
        cmd.args(&self.args).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", self.program.display()))?;
        let stderr = child.stderr.take();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let err_thread = stderr.map(|s| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(s).lines().map_while(Result::ok) {
                    tx.send(l).ok();
                }
            })
        });
        if let Some(out) = child.stdout.take() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(out).lines().map_while(Result::ok) {
                    tx.send(l).ok();
                }
            });
        }
        drop(tx);
        let mut all = String::new();
        for l in rx {
            all.push_str(&l);
            all.push('\n');
            line(l);
        }
        if let Some(t) = err_thread {
            t.join().ok();
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        if status.success() { Ok(()) } else { Err(all.trim().to_string()) }
    }
}

fn home_relative(arg: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match arg.strip_prefix(&home).and_then(|rest| rest.strip_prefix('/')) {
        Some(rest) if !home.is_empty() => format!("~/{}", shell_quote(rest)),
        _ => shell_quote(arg),
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:+@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Every module the notebook imports at top level, and the packages its own
/// `%pip install` lines will add.
pub fn notebook_imports<'a>(sources: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut modules = Vec::new();
    let mut installs = BTreeSet::new();
    for source in sources {
        for m in imports(source) {
            if !modules.contains(&m) {
                modules.push(m);
            }
        }
        installs.extend(self_installed(source));
    }
    modules.retain(|m| !installs.contains(&normalize(&pip_name(m))));
    modules
}

// ---------------------------------------------------------------------------
// Fixes for a failed run.

/// A one-click answer to a cell's error.
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    /// `ModuleNotFoundError`: install the package.
    Install { module: String, package: String },
    /// `NameError`, and an earlier cell defines the name: run that cell, then
    /// this one.
    RunDefining { name: String, cell: usize },
    /// `NameError` on a well-known alias (`np`, `pd`…): add its import.
    AddImport { name: String, line: &'static str },
}

/// Conventional aliases and the import that defines them.
const ALIASES: &[(&str, &str)] = &[
    ("np", "import numpy as np"),
    ("pd", "import pandas as pd"),
    ("pl", "import polars as pl"),
    ("plt", "import matplotlib.pyplot as plt"),
    ("mpl", "import matplotlib as mpl"),
    ("sns", "import seaborn as sns"),
    ("alt", "import altair as alt"),
    ("px", "import plotly.express as px"),
    ("go", "import plotly.graph_objects as go"),
    ("pio", "import plotly.io as pio"),
    ("tf", "import tensorflow as tf"),
    ("torch", "import torch"),
    ("nn", "import torch.nn as nn"),
    ("F", "import torch.nn.functional as F"),
    ("nx", "import networkx as nx"),
    ("sp", "import scipy as sp"),
    ("stats", "from scipy import stats"),
    ("sm", "import statsmodels.api as sm"),
    ("smf", "import statsmodels.formula.api as smf"),
    ("xr", "import xarray as xr"),
    ("gpd", "import geopandas as gpd"),
    ("dd", "import dask.dataframe as dd"),
    ("jnp", "import jax.numpy as jnp"),
    ("jax", "import jax"),
    ("duckdb", "import duckdb"),
    ("os", "import os"),
    ("sys", "import sys"),
    ("re", "import re"),
    ("json", "import json"),
    ("math", "import math"),
    ("time", "import time"),
    ("random", "import random"),
    ("datetime", "import datetime"),
    ("dt", "import datetime as dt"),
    ("Path", "from pathlib import Path"),
    ("defaultdict", "from collections import defaultdict"),
    ("Counter", "from collections import Counter"),
    ("display", "from IPython.display import display"),
    ("HTML", "from IPython.display import HTML"),
    ("Markdown", "from IPython.display import Markdown"),
    ("tqdm", "from tqdm.auto import tqdm"),
    ("figure", "from bokeh.plotting import figure"),
    ("show", "from bokeh.plotting import show"),
];

/// The fix for an error a cell raised. `sources` are the notebook's cells
/// (code ones), `ix` the failing cell.
pub fn fix_for(ename: &str, evalue: &str, sources: &[(usize, &str)], ix: usize) -> Option<Fix> {
    match ename {
        "ModuleNotFoundError" | "ImportError" => {
            let module = quoted(evalue.strip_prefix("No module named ")?)?;
            let top = module.split('.').next()?.to_string();
            let package = PIP_NAMES
                .iter()
                .find(|(m, _)| *m == module)
                .map_or_else(|| pip_name(&top), |(_, p)| (*p).to_string());
            Some(Fix::Install { module: top, package })
        }
        "NameError" | "UnboundLocalError" => {
            let name = quoted(evalue.strip_prefix("name ")?)?;
            let defining = sources
                .iter()
                .rev()
                .filter(|(i, _)| *i < ix)
                .find(|(_, s)| defines(s, &name))
                .map(|(i, _)| *i);
            if let Some(cell) = defining {
                return Some(Fix::RunDefining { name, cell });
            }
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == name)
                .map(|(_, line)| Fix::AddImport { name: name.clone(), line })
        }
        _ => None,
    }
}

/// The text between the first pair of quotes (`'x'` or `"x"`).
fn quoted(s: &str) -> Option<String> {
    let s = s.trim();
    let q = s.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let end = s[1..].find(q)?;
    Some(s[1..1 + end].to_string())
}

/// Whether `source` defines `name` at its top level: imports it, assigns
/// it, or makes it a function, class, loop or `with` variable.
pub fn defines(source: &str, name: &str) -> bool {
    if !source.contains(name) {
        return false;
    }
    for line in logical_lines(source) {
        for stmt in line.split(';') {
            let stmt = stmt.trim();
            if let Some(rest) = stmt.strip_prefix("import ") {
                for part in rest.split(',') {
                    let words: Vec<&str> = part.split_whitespace().collect();
                    let bound = match words.as_slice() {
                        [_, "as", alias] => *alias,
                        [module, ..] => module.split('.').next().unwrap_or(""),
                        [] => "",
                    };
                    if bound == name {
                        return true;
                    }
                }
            } else if let Some(rest) = stmt.strip_prefix("from ") {
                let Some((_, names)) = rest.split_once(" import ") else { continue };
                for part in names.split(',') {
                    let words: Vec<&str> = part.split_whitespace().collect();
                    let bound = match words.as_slice() {
                        [_, "as", alias] => *alias,
                        [n, ..] => n,
                        [] => "",
                    };
                    if bound == name {
                        return true;
                    }
                }
            } else if let Some(rest) = stmt.strip_prefix("def ").or_else(|| stmt.strip_prefix("async def ")).or_else(|| stmt.strip_prefix("class ")) {
                if rest.split(|c: char| !c.is_alphanumeric() && c != '_').next() == Some(name) {
                    return true;
                }
            } else if let Some(rest) = stmt.strip_prefix("for ") {
                let targets = rest.split(" in ").next().unwrap_or("");
                if names_in(targets).any(|n| n == name) {
                    return true;
                }
            } else if stmt.starts_with("with ") {
                if stmt.split(" as ").skip(1).any(|t| names_in(t.split([',', ':']).next().unwrap_or("")).any(|n| n == name)) {
                    return true;
                }
            } else if let Some(lhs) = assignment_target(stmt)
                && names_in(lhs).any(|n| n == name)
            {
                return true;
            }
        }
    }
    false
}

/// The left side of `a, b = …`, `x: int = …`, `x += …`, `x := …` aside.
fn assignment_target(stmt: &str) -> Option<&str> {
    let bytes = stmt.as_bytes();
    let mut depth = 0;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'\'' | b'"' => return None,
            b'=' if depth == 0 => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                let next = bytes.get(i + 1);
                if next == Some(&b'=') || matches!(prev, Some(b'=' | b'!' | b'<' | b'>' | b':')) {
                    return None;
                }
                let lhs = &stmt[..i];
                // `x += 1` needs x already; `x: int = 1` binds x.
                let lhs = lhs.trim_end_matches(['+', '-', '*', '/', '%', '&', '|', '^', '@']);
                return Some(lhs.split(':').next().unwrap_or(lhs));
            }
            _ => {}
        }
    }
    None
}

fn names_in(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .filter(|w| !w.is_empty() && !w.contains('.') && is_identifier(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_top_level_imports() {
        assert_eq!(imports("s = '('\nimport json"), ["json"]);
        let src = "import numpy as np, pandas\nfrom sklearn.model_selection import (\n    train_test_split,\n)\nfrom . import local\nfrom .x import y\ntry:\n    import optional\nexcept ImportError:\n    pass\n%matplotlib inline\nimport os; import cv2  # vision\n";
        assert_eq!(imports(src), ["numpy", "pandas", "sklearn", "os", "cv2"]);
        assert!(imports("x = 'import foo'").is_empty());
    }

    #[test]
    fn notebooks_that_install_their_own_packages() {
        let cells = ["%pip install -q scikit-learn==1.5 'altair>=5'", "import sklearn\nimport altair\nimport bokeh"];
        assert_eq!(notebook_imports(cells.into_iter()), ["bokeh"]);
        assert_eq!(pip_name("PIL"), "pillow");
        assert_eq!(pip_name("my_pkg"), "my-pkg");
    }

    #[test]
    fn fixes_for_failed_runs() {
        let sources = [(0, "import numpy as np\nimport pandas as pd"), (1, "x = 1"), (3, "df = pd.DataFrame()")];
        assert_eq!(
            fix_for("NameError", "name 'np' is not defined", &sources, 3),
            Some(Fix::RunDefining { name: "np".into(), cell: 0 })
        );
        assert_eq!(
            fix_for("NameError", "name 'np' is not defined", &sources[1..], 3),
            Some(Fix::AddImport { name: "np".into(), line: "import numpy as np" })
        );
        assert_eq!(fix_for("NameError", "name 'zzz' is not defined", &sources, 3), None);
        assert_eq!(
            fix_for("ModuleNotFoundError", "No module named 'sklearn.ensemble'", &sources, 3),
            Some(Fix::Install { module: "sklearn".into(), package: "scikit-learn".into() })
        );
    }

    #[test]
    fn what_counts_as_defining() {
        assert!(defines("a, (b, c) = f()", "c"));
        assert!(defines("x: int = 3", "x"));
        assert!(defines("def go(x):\n    pass", "go"));
        assert!(defines("for i, row in df.iterrows():\n    pass", "row"));
        assert!(defines("with open(p) as fh:\n    pass", "fh"));
        assert!(!defines("from math import *", "np"));
        assert!(!defines("x == 3", "x"));
        assert!(!defines("print(x)", "x"));
        assert!(!defines("def f():\n    y = 2", "y"));
        assert!(!defines("x += 1", "y"));
    }

    #[test]
    fn checks_against_a_real_python() {
        let Some(python) = which("python3").or_else(|| Some(PathBuf::from("/usr/bin/python3")).filter(|p| p.exists())) else {
            return;
        };
        let spec = KernelSpec {
            name: "t".into(),
            display_name: "t".into(),
            language: "python".into(),
            argv: vec![python.to_string_lossy().into_owned()],
            env: Default::default(),
            resource_dir: PathBuf::new(),
            interrupt_mode: Default::default(),
            metadata: Default::default(),
        };
        let missing = missing(&python, &spec, Path::new("/"), &["json".into(), "herse_surely_not_a_module".into(), "email".into()]);
        assert_eq!(missing.unwrap(), ["herse_surely_not_a_module"]);
    }
}

// ---------------------------------------------------------------------------
// In the notebook: the banner above it and the fixes under failed cells.


/// What preflight knows about the notebook and its kernel.
#[derive(Default)]
pub(super) struct State {
    /// What was last checked (kernel name, imports): an edit that changes
    /// neither checks nothing.
    checked: Option<(String, Vec<String>)>,
    /// Modules the kernel lacks, with the package that provides each.
    missing: Vec<(String, String)>,
    kernel: String,
    python: Option<PathBuf>,
    /// Another installed kernel that has all of them.
    alternative: Option<KernelSpec>,
    /// The banner was closed for this set of missing modules.
    dismissed: bool,
    install: Option<Install>,
    /// Cells (by uid) to run once the install finishes.
    then_run: Vec<u64>,
    task: Option<Task<()>>,
}

struct Install {
    packages: Vec<String>,
    command: String,
    log: String,
    failed: Option<String>,
    /// Closed while it runs: shown again only if it fails.
    hidden: bool,
    _task: Task<()>,
}

struct Survey {
    kernel: String,
    python: Option<PathBuf>,
    missing: Vec<(String, String)>,
    alternative: Option<KernelSpec>,
}

/// Ask `spec`'s Python what it lacks and, when it lacks something, ask the
/// other installed Python kernels (at once) whether one has everything.
fn survey(spec: &KernelSpec, cwd: &Path, modules: &[String]) -> Survey {
    let python = python_of(spec);
    let lacking = match &python {
        // A Python that can't answer is the kernel's problem to report, when it runs.
        Some(py) => missing(py, spec, cwd, modules).unwrap_or_default(),
        None => Vec::new(),
    };
    let alternative = if lacking.is_empty() {
        None
    } else {
        let others: Vec<KernelSpec> = herse_kernel::discover()
            .into_iter()
            .filter(|s| s.name != spec.name && s.language.eq_ignore_ascii_case("python"))
            .take(16)
            .collect();
        let python = python.as_ref();
        std::thread::scope(|scope| {
            let handles: Vec<_> = others
                .iter()
                .map(|other| {
                    scope.spawn(move || {
                        let py = python_of(other)?;
                        // The same interpreter answers the same.
                        if Some(&py) == python {
                            return None;
                        }
                        missing(&py, other, cwd, modules).ok().filter(|m| m.is_empty()).map(|_| other.clone())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok().flatten()).collect::<Vec<_>>().into_iter().flatten().next()
        })
    };
    Survey {
        kernel: spec.display_name.clone(),
        python,
        missing: lacking
            .into_iter()
            .map(|m| {
                let package = pip_name(&m);
                (m, package)
            })
            .collect(),
        alternative,
    }
}

/// `a`, `a and b`, `a, b and c`, `a, b, c and 2 more`.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] if names.len() <= 4 => format!("{} and {last}", rest.join(", ")),
        _ => format!("{} and {} more", names[..3].join(", "), names.len() - 3),
    }
}

/// The last `n` lines of `log`.
fn tail(log: &str, n: usize) -> &str {
    let mut start = log.len();
    for _ in 0..n {
        match log[..start.saturating_sub(1)].rfind('\n') {
            Some(i) => start = i + 1,
            None => return log,
        }
    }
    &log[start..]
}

impl NotebookView {
    /// The kernel the notebook runs, or would run once started.
    fn current_spec(&self) -> Option<KernelSpec> {
        match &self.session {
            Some(s) => herse_kernel::discover().into_iter().find(|spec| spec.name == s.name),
            None => self.pick_spec(),
        }
    }

    /// Check the notebook's imports against its kernel, in the background:
    /// on open, after an edit, a kernel change or an install.
    pub(super) fn check_packages(&mut self, cx: &mut Context<Self>) {
        if self.live_view_only() || self.doc.nb.language().is_some_and(|l| !l.eq_ignore_ascii_case("python")) {
            return;
        }
        let modules = notebook_imports(
            self.doc.nb.cells.iter().filter(|c| c.cell_type == CellType::Code).map(|c| c.source.as_str()),
        );
        let Some(spec) = self.current_spec() else { return };
        let key = (spec.name.clone(), modules.clone());
        if self.preflight.checked.as_ref() == Some(&key) {
            return;
        }
        self.preflight.checked = Some(key);
        let cwd = self.kernel_cwd();
        self.preflight.task = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_spawn(async move { survey(&spec, &cwd, &modules) }).await;
            this.update(cx, |this, cx| {
                let p = &mut this.preflight;
                if p.missing.iter().map(|m| &m.0).ne(found.missing.iter().map(|m| &m.0)) {
                    p.dismissed = false;
                }
                p.kernel = found.kernel;
                p.python = found.python;
                p.missing = found.missing;
                p.alternative = found.alternative;
                cx.notify();
            })
            .ok();
        }));
    }

    /// A run of cell `uid` waits for an install.
    pub(super) fn preflight_waiting(&self, uid: u64) -> bool {
        self.preflight.then_run.contains(&uid)
    }

    /// With "Install Missing Packages When Running" on: hold a run of
    /// `cells` that import what the kernel lacks until it's installed.
    pub(super) fn hold_for_install(&mut self, cells: std::ops::Range<usize>, cx: &mut Context<Self>) -> bool {
        if !crate::settings::auto_install() || self.preflight.install.as_ref().is_some_and(|i| i.failed.is_some()) {
            return false;
        }
        let imported: Vec<String> = self.doc.nb.cells[cells.clone()]
            .iter()
            .filter(|c| c.cell_type == CellType::Code)
            .flat_map(|c| imports(&c.source))
            .collect();
        let packages: Vec<String> =
            self.preflight.missing.iter().filter(|(m, _)| imported.contains(m)).map(|(_, p)| p.clone()).collect();
        let installing = self.preflight.install.is_some();
        if packages.is_empty() && !installing {
            return false;
        }
        let uids: Vec<u64> = cells
            .filter(|&ix| self.doc.nb.cells[ix].cell_type == CellType::Code)
            .map(|ix| self.cells[ix].uid)
            .collect();
        self.preflight.then_run.extend(uids);
        if !installing {
            self.install_packages(packages, cx);
        }
        true
    }

    /// Install `packages` for the kernel's Python, showing the installer's
    /// output above the notebook; then run the cells waiting on it.
    pub(super) fn install_packages(&mut self, packages: Vec<String>, cx: &mut Context<Self>) {
        if self.preflight.install.as_ref().is_some_and(|i| i.failed.is_none()) || packages.is_empty() {
            return;
        }
        let Some(python) = self.preflight.python.clone().or_else(|| self.current_spec().as_ref().and_then(python_of))
        else {
            self.report(
                super::ProblemKind::Kernel,
                "Couldn't tell which Python this kernel runs",
                "Its kernelspec doesn't start a python binary. Install the packages from a terminal in that environment.",
                cx,
            );
            return;
        };
        let installer = Installer::new(&python, &packages);
        let command = installer.command_line();
        let (tx, rx) = smol::channel::unbounded::<String>();
        let task = cx.spawn(async move |this, cx| {
            let job = cx.background_spawn(async move {
                installer.run(move |line| {
                    tx.try_send(line).ok();
                })
            });
            while let Ok(line) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    if let Some(install) = this.preflight.install.as_mut() {
                        install.log.push_str(&line);
                        install.log.push('\n');
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            let result = job.await;
            this.update(cx, |this, cx| this.finish_install(result, cx)).ok();
        });
        self.preflight.install =
            Some(Install { packages, command, log: String::new(), failed: None, hidden: false, _task: task });
        cx.notify();
    }

    fn finish_install(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        let Some(install) = self.preflight.install.as_mut() else { return };
        match result {
            Ok(()) => {
                let names = join_names(&install.packages);
                let installed = std::mem::take(&mut install.packages);
                self.preflight.install = None;
                // Until the re-check answers: what was installed isn't missing
                // (and the runs below mustn't wait for it again).
                self.preflight.missing.retain(|(_, p)| !installed.contains(p));
                // A running kernel sees new packages once its import caches are cleared.
                if let Some(s) = self.session.as_ref().filter(|s| s.python) {
                    s.kernel.execute_with("__import__('importlib').invalidate_caches()", session::silent());
                }
                self.flash(format!("Installed {names}"), cx);
                self.preflight.checked = None;
                self.check_packages(cx);
            }
            Err(log) => {
                install.hidden = false;
                install.failed =
                    Some(if log.is_empty() { "The installer stopped without saying why.".into() } else { log });
            }
        }
        // What was waiting runs either way: the reader asked for it, and a
        // failed install's error is then theirs to see in the cell too.
        for uid in std::mem::take(&mut self.preflight.then_run) {
            if let Some(ix) = self.cells.iter().position(|c| c.uid == uid) {
                self.execute(ix..ix + 1, cx);
            }
        }
        cx.notify();
    }

    /// Above the notebook: what it imports that the kernel lacks, or the
    /// install under way, or why it failed.
    pub(super) fn render_preflight(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let p = &self.preflight;
        let copied = cx.listener(|this, _, _, cx| this.flash("Copied", cx));
        let card = if let Some(install) = p.install.as_ref().filter(|i| !i.hidden) {
            let names = join_names(&install.packages);
            match &install.failed {
                None => banner(
                    Tone::Notice,
                    "preflight",
                    format!("Installing {names}…").into(),
                    format!("$ {}\n{}", install.command, tail(install.log.trim_end(), 6)).trim_end().to_string().into(),
                    Vec::new(),
                    copied,
                    cx.listener(|this, _, _, cx| {
                        if let Some(install) = this.preflight.install.as_mut() {
                            install.hidden = true;
                        }
                        cx.notify();
                    }),
                    cx,
                ),
                Some(log) => {
                    let packages = install.packages.clone();
                    banner(
                        Tone::Problem,
                        "preflight",
                        format!("Couldn't install {names}").into(),
                        format!("$ {}\n{log}", install.command).into(),
                        vec![
                            crate::ui::button("preflight-retry", "Try Again", cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.preflight.install = None;
                                    this.install_packages(packages.clone(), cx);
                                }))
                                .into_any_element(),
                        ],
                        copied,
                        cx.listener(|this, _, _, cx| {
                            this.preflight.install = None;
                            this.preflight.dismissed = true;
                            cx.notify();
                        }),
                        cx,
                    )
                }
            }
        } else if p.install.is_none() && !p.missing.is_empty() && !p.dismissed {
            let modules: Vec<String> = p.missing.iter().map(|m| m.0.clone()).collect();
            let packages: Vec<String> = p.missing.iter().map(|m| m.1.clone()).collect();
            let verb = if modules.len() == 1 { "isn't" } else { "aren't" };
            let command = p.python.as_ref().map(|py| Installer::new(py, &packages).command_line()).unwrap_or_default();
            let mut actions = vec![
                crate::ui::primary_button("preflight-install", "Install", true, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.install_packages(packages.clone(), cx)))
                    .into_any_element(),
            ];
            if let Some(alt) = p.alternative.clone() {
                actions.push(
                    crate::ui::button("preflight-switch", format!("Use {}", alt.display_name), cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_kernel(&alt, cx)))
                        .into_any_element(),
                );
            }
            banner(
                Tone::Notice,
                "preflight",
                format!("{} {verb} installed in {}", join_names(&modules), p.kernel).into(),
                command.into(),
                actions,
                copied,
                cx.listener(|this, _, _, cx| {
                    this.preflight.dismissed = true;
                    cx.notify();
                }),
                cx,
            )
        } else {
            return None;
        };
        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(16.))
                .pt(px(12.))
                .child(div().w_full().max_w(theme::CONTENT_MAX_WIDTH).mx_auto().child(card))
                .into_any_element(),
        )
    }

    /// The fix for the error cell `ix` raised, if Herse knows one (cached
    /// until the next edit).
    fn cell_fix(&mut self, ix: usize) -> Option<Fix> {
        if let Some((rev, fix)) = &self.cells[ix].fix
            && *rev == self.doc_rev
        {
            return fix.clone();
        }
        let fix = self.doc.nb.cells[ix].outputs.iter().rev().find_map(|o| match o {
            Output::Error { ename, evalue, .. } => {
                let sources: Vec<(usize, &str)> = self
                    .doc
                    .nb
                    .cells
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.cell_type == CellType::Code)
                    .map(|(i, c)| (i, c.source.as_str()))
                    .collect();
                fix_for(ename, evalue, &sources, ix)
            }
            _ => None,
        });
        self.cells[ix].fix = Some((self.doc_rev, fix.clone()));
        fix
    }

    /// Under a failed cell: what went wrong in a sentence, and the button
    /// that fixes it.
    pub(super) fn render_fix(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.live_view_only() {
            return None;
        }
        let fix = self.cell_fix(ix)?;
        let uid = self.cells[ix].uid;
        let kernel = self.session.as_ref().map_or_else(|| self.preflight.kernel.clone(), |s| s.display_name.clone());
        let installing = self.preflight.install.as_ref().is_some_and(|i| i.failed.is_none());
        // What went wrong, with any code in it set as code.
        let (text, code, label, enabled) = match &fix {
            Fix::Install { module, package } => (
                format!("isn't installed in {}.", if kernel.is_empty() { "this kernel" } else { &kernel }),
                Some(module.clone()),
                if installing { "Installing…".to_string() } else { format!("Install {package} and Run") },
                !installing,
            ),
            Fix::RunDefining { name, cell } => (
                format!("is defined in cell {}.", cell + 1),
                Some(name.clone()),
                format!("Run Cell {} and This One", cell + 1),
                true,
            ),
            Fix::AddImport { line, .. } => {
                ("Missing an import:".to_string(), Some(line.to_string()), "Add It and Run".to_string(), true)
            }
        };
        // `np is defined…` reads code first; `Missing an import: …` code last.
        let code_first = !matches!(fix, Fix::AddImport { .. });
        let theme = cx.theme();
        let accent = theme::accent(cx);
        Some(
            div()
                .id(("fix", uid))
                .mx(theme::CELL_PAD_X)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(accent.opacity(0.25))
                .bg(accent.opacity(0.06))
                .child(svg().path("icons/lightbulb.svg").flex_none().size(px(14.)).text_color(accent))
                .child({
                    let code = code.map(|code| {
                        div()
                            .flex_none()
                            .font_family(theme::mono_font())
                            .text_size(theme::mono_size())
                            .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("fix-code-{uid}")), code))
                    });
                    let prose = gpui_kit::base::SelectableText::new(SharedString::from(format!("fix-text-{uid}")), text);
                    let row = div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_baseline()
                        .gap(px(5.))
                        .text_size(px(12.5))
                        .text_color(theme.foreground);
                    if code_first { row.children(code).child(prose) } else { row.child(prose).children(code) }
                })
                .child(
                    crate::ui::button(("fix-apply", uid), label, cx)
                        .when(enabled, |b| {
                            b.on_click(cx.listener(move |this, _, window, cx| this.apply_fix(uid, fix.clone(), window, cx)))
                        }),
                )
                .into_any_element(),
        )
    }

    fn apply_fix(&mut self, uid: u64, fix: Fix, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.cells.iter().position(|c| c.uid == uid) else { return };
        match fix {
            Fix::Install { package, .. } => {
                self.preflight.then_run.push(uid);
                self.preflight.install = None;
                self.install_packages(vec![package], cx);
            }
            Fix::RunDefining { cell, .. } => {
                self.execute(cell..cell + 1, cx);
                self.execute(ix..ix + 1, cx);
            }
            Fix::AddImport { line, .. } => {
                if self.editing.as_ref().is_some_and(|e| e.uid == uid) {
                    self.stop_editing(window, cx);
                }
                let source = format!("{line}\n{}", self.doc.nb.cells[ix].source);
                self.set_sources(vec![(ix, source)], cx);
                self.execute(ix..ix + 1, cx);
            }
        }
    }
}
