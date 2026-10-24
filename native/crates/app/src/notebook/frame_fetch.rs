//! Load All Rows: a DataFrame output's whole frame, from the kernel.
//!
//! A notebook saves only the rows pandas printed. While the kernel that made
//! an output runs, the frame is still in its memory: Herse asks it, outside
//! any cell (as the assistant's evaluate does), to write the frame to a
//! temporary Parquet file (CSV when pyarrow is missing), then reads that file
//! like any other and deletes it.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use gpui_kit::{AsyncApp, WeakEntity};
use herse_kernel::KernelStatus;
use herse_nb::Output;

use super::{NotebookView, session};

/// How long the kernel may take to write a frame.
const TIMEOUT: Duration = Duration::from_secs(300);

impl NotebookView {
    /// A Python kernel is running for this notebook.
    pub(crate) fn frame_kernel_live(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.python && !matches!(s.status, KernelStatus::Dead | KernelStatus::Starting | KernelStatus::Restarting))
    }

    /// The cell's number (from 1) and what to call output `n` of the cell
    /// with `uid`: the name its last line shows (`df`), else `Out[12]`.
    pub(crate) fn frame_name(&self, uid: u64, output: usize) -> (usize, String) {
        let Some(ix) = self.cells.iter().position(|c| c.uid == uid) else { return (0, "DataFrame".into()) };
        let cell = &self.doc.nb.cells[ix];
        let (count, expr) = frame_expression(&cell.source, cell.outputs.get(output));
        let name = match (expr, count) {
            (Some(e), _) => e.base,
            (None, Some(n)) => format!("Out[{n}]"),
            (None, None) => format!("Cell {}", ix + 1),
        };
        (ix + 1, name)
    }
}

/// What the kernel should look up: `Out[N]` of a result, and the frame's
/// name from the cell's last line.
fn frame_expression(source: &str, output: Option<&Output>) -> (Option<i64>, Option<Expr>) {
    let count = match output {
        Some(Output::ExecuteResult { execution_count, .. }) => *execution_count,
        _ => None,
    };
    (count, last_expression(source))
}

/// A name the kernel can evaluate without running anything: `df`,
/// `results.frame`. `whole` is set when the line was a view of fewer rows
/// (`df.head()`, `df.tail(3)`, `df.sample(5)`, `display(df)`, `df.style…`),
/// so the name gives more than the output did.
#[derive(Clone, Debug, PartialEq)]
struct Expr {
    base: String,
    whole: bool,
}

fn last_expression(source: &str) -> Option<Expr> {
    let line = source.lines().map(str::trim).rfind(|l| !l.is_empty() && !l.starts_with('#'))?;
    let line = line.trim_end_matches(';').trim();
    let (inner, mut whole) = match line.strip_prefix("display(").and_then(|r| r.strip_suffix(')')) {
        Some(inner) => (inner.trim(), false),
        None => (line, false),
    };
    let mut base = inner;
    for view in [".head(", ".tail(", ".sample(", ".style"] {
        if let Some(at) = inner.find(view) {
            base = &inner[..at];
            whole = true;
            break;
        }
    }
    let ok = !base.is_empty()
        && base.split('.').all(|part| {
            let mut chars = part.chars();
            chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
                && !matches!(part, "import" | "from" | "def" | "class" | "return" | "lambda" | "print" | "None" | "True" | "False")
        });
    ok.then(|| Expr { base: base.to_string(), whole })
}

/// Python that writes the frame to `target` + `.parquet` (or `.csv`) and
/// prints `HERSE_FRAME:<index columns>:<path>`.
fn export_code(count: Option<i64>, expr: Option<&Expr>, target: &Path) -> String {
    let py = |s: &str| format!("{s:?}");
    let name = expr.map(|e| py(&e.base)).unwrap_or_else(|| "None".into());
    let prefer_name = expr.is_some_and(|e| e.whole);
    let count = count.map_or_else(|| "None".to_string(), |n| n.to_string());
    let target = py(&target.to_string_lossy());
    format!(
        r#"def __herse_frame(target, count, name, prefer_name):
    import sys
    ns = get_ipython().user_ns
    def by_name():
        if name is None:
            return None
        try:
            return eval(name, ns)
        except Exception:
            return None
    def by_count():
        return ns.get("Out", {{}}).get(count) if count is not None else None
    obj = by_name() if prefer_name else None
    if obj is None:
        obj = by_count()
    if obj is None:
        obj = by_name()
    if obj is None:
        raise LookupError("The kernel no longer holds this output. Run its cell again, then load all rows.")
    pl = sys.modules.get("polars")
    if pl is not None and isinstance(obj, (pl.DataFrame, pl.LazyFrame, pl.Series)):
        if isinstance(obj, pl.LazyFrame):
            obj = obj.collect()
        if isinstance(obj, pl.Series):
            obj = obj.to_frame()
        path = target + ".parquet"
        obj.write_parquet(path)
        return 0, path
    pd = sys.modules.get("pandas")
    if pd is not None:
        try:
            from pandas.io.formats.style import Styler
            if isinstance(obj, Styler):
                obj = obj.data
        except Exception:
            pass
        if isinstance(obj, pd.Series):
            obj = obj.to_frame()
        if isinstance(obj, pd.DataFrame):
            df, index = obj, 0
            ix = df.index
            if not (isinstance(ix, pd.RangeIndex) and ix.start == 0 and ix.step == 1 and ix.name is None):
                index = ix.nlevels
                df = df.reset_index(allow_duplicates=True)
            df.columns = [" / ".join(map(str, c)) if isinstance(c, tuple) else str(c) for c in df.columns]
            try:
                path = target + ".parquet"
                df.to_parquet(path, index=False)
            except Exception:
                path = target + ".csv"
                df.to_csv(path, index=False)
            return index, path
    raise TypeError(f"This output is a {{type(obj).__name__}}, not a DataFrame.")
__herse_index, __herse_path = __herse_frame({target}, {count}, {name}, {prefer})
print(f"HERSE_FRAME:{{__herse_index}}:{{__herse_path}}")
del __herse_frame, __herse_index, __herse_path"#,
        prefer = if prefer_name { "True" } else { "False" },
    )
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Ask the kernel for the whole frame behind output `output` of the cell
/// `uid`; the file it wrote and how many index columns lead it.
pub async fn fetch(nb: WeakEntity<NotebookView>, uid: u64, output: usize, cx: &mut AsyncApp) -> Result<(PathBuf, usize), String> {
    let gone = |_| "The notebook was closed.".to_string();
    let target = std::env::temp_dir().join(format!("herse-frame-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let msg_id = nb
        .update(cx, |nb, cx| {
            if !nb.frame_kernel_live() {
                return Err("The notebook's kernel isn't running. Run the notebook, then load all rows.".to_string());
            }
            let ix = nb.cells.iter().position(|c| c.uid == uid).ok_or("The cell is gone.")?;
            let cell = &nb.doc.nb.cells[ix];
            let (count, expr) = frame_expression(&cell.source, cell.outputs.get(output));
            if count.is_none() && expr.is_none() {
                return Err("Herse can't tell which DataFrame this output shows: its cell doesn't end with a name.".into());
            }
            let code = export_code(count, expr.as_ref(), &target);
            let session = nb.session.as_mut().ok_or("The notebook's kernel isn't running.")?;
            let id = session.kernel.execute_with(&code, session::scratch());
            session.scratch.insert(id.clone(), Default::default());
            cx.notify();
            Ok(id)
        })
        .map_err(gone)??;
    let started = Instant::now();
    loop {
        cx.background_executor().timer(Duration::from_millis(50)).await;
        let finished = nb
            .update(cx, |nb, _| {
                let session = nb.session.as_mut()?;
                let scratch = session.scratch.get(&msg_id)?;
                if !scratch.done && started.elapsed() < TIMEOUT {
                    return Some(None);
                }
                let done = scratch.done;
                let scratch = session.scratch.remove(&msg_id)?;
                Some(Some((done, scratch.outputs)))
            })
            .map_err(gone)?;
        match finished {
            Some(None) => continue,
            Some(Some((true, outputs))) => return parse_reply(&outputs),
            Some(Some((false, _))) => return Err(format!("The kernel took more than {} s to write the frame.", TIMEOUT.as_secs())),
            None => return Err("The kernel stopped before it answered.".into()),
        }
    }
}

/// The file named in the kernel's reply, or its error.
fn parse_reply(outputs: &[Output]) -> Result<(PathBuf, usize), String> {
    for output in outputs {
        match output {
            Output::Stream { text, .. } => {
                if let Some(rest) = text.lines().find_map(|l| l.strip_prefix("HERSE_FRAME:"))
                    && let Some((index, path)) = rest.split_once(':')
                {
                    return Ok((PathBuf::from(path.trim()), index.parse().unwrap_or(0)));
                }
            }
            Output::Error { ename, evalue, .. } => {
                return Err(if matches!(ename.as_str(), "LookupError" | "TypeError") {
                    evalue.clone()
                } else {
                    format!("{ename}: {evalue}")
                });
            }
            _ => {}
        }
    }
    Err("The kernel didn't say where it wrote the frame.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_last_lines() {
        let e = |s: &str| last_expression(s);
        assert_eq!(e("df = load()\ndf"), Some(Expr { base: "df".into(), whole: false }));
        assert_eq!(e("df.head(10)"), Some(Expr { base: "df".into(), whole: true }));
        assert_eq!(e("display(sales.frame)"), Some(Expr { base: "sales.frame".into(), whole: false }));
        assert_eq!(e("df.style.background_gradient()"), Some(Expr { base: "df".into(), whole: true }));
        assert_eq!(e("x = 1\n# comment\n"), None);
        assert_eq!(e("df.groupby('a').sum()"), None);
        assert_eq!(e("pd.read_csv('x.csv')"), None);
        assert_eq!(e("df;"), Some(Expr { base: "df".into(), whole: false }));
    }

    #[test]
    fn replies() {
        let ok = vec![Output::Stream { name: herse_nb::StreamName::Stdout, text: "HERSE_FRAME:1:/tmp/x.parquet\n".into() }];
        assert_eq!(parse_reply(&ok), Ok((PathBuf::from("/tmp/x.parquet"), 1)));
        let err = vec![Output::Error { ename: "TypeError".into(), evalue: "This output is a list, not a DataFrame.".into(), traceback: vec![] }];
        assert_eq!(parse_reply(&err), Err("This output is a list, not a DataFrame.".into()));
    }

    #[test]
    fn code_names_the_target() {
        let code = export_code(Some(3), Some(&Expr { base: "df".into(), whole: true }), Path::new("/tmp/herse-frame-1-0"));
        assert!(code.contains("__herse_frame(\"/tmp/herse-frame-1-0\", 3, \"df\", True)"), "{code}");
    }
}
