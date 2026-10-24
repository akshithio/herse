//! `herse export NOTEBOOK --to html|py|md`.

use std::io::Write as _;
use std::path::PathBuf;

use herse_nb::{CellType, Notebook, Output};
use herse_rich::{ansi, stream};

use crate::{Args, Failure};

pub fn main(mut args: Args) -> Result<(), Failure> {
    let to = args
        .value(&["--to", "-t"])?
        .ok_or_else(|| Failure::Usage("export needs --to html, py or md".into()))?;
    let out = args.value(&["-o", "--out"])?.map(PathBuf::from);
    let path = args.path("notebook")?;
    let nb = crate::read_notebook(&path)?;
    let title = path.file_name().map_or_else(|| "Notebook".to_string(), |n| n.to_string_lossy().into_owned());
    let text = match to.to_str().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => herse_html::render(&nb, &herse_html::Options { title, ..Default::default() }),
        "py" | "script" => script(&nb),
        "md" | "markdown" => markdown(&nb),
        other => return Err(Failure::Usage(format!("can't export to {other:?}: html, py or md"))),
    };
    match out {
        Some(out) => crate::write_atomically(&out, &text),
        None => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(text.as_bytes()).and_then(|_| stdout.flush());
            Ok(())
        }
    }
}

/// How the notebook's language starts a comment.
fn comment(nb: &Notebook) -> &'static str {
    match nb.language().unwrap_or("python").to_ascii_lowercase().as_str() {
        "javascript" | "typescript" | "rust" | "go" | "java" | "kotlin" | "scala" | "c" | "c++" | "cpp" | "swift" | "csharp"
        | "c#" => "//",
        "sql" | "lua" | "haskell" => "--",
        _ => "#",
    }
}

/// A script with `# %%` cells (the percent format VS Code, Spyder and
/// Jupytext read back as cells); prose as comments.
pub fn script(nb: &Notebook) -> String {
    let c = comment(nb);
    let mut out = String::new();
    for (i, cell) in nb.cells.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let source = unix_lines(&cell.source);
        match cell.cell_type {
            CellType::Code => {
                out.push_str(&format!("{c} %%\n"));
                out.push_str(source.trim_end_matches('\n'));
                out.push('\n');
            }
            CellType::Markdown | CellType::Raw => {
                let kind = if cell.cell_type == CellType::Markdown { "markdown" } else { "raw" };
                out.push_str(&format!("{c} %% [{kind}]\n"));
                for line in source.lines() {
                    if line.is_empty() {
                        out.push_str(&format!("{c}\n"));
                    } else {
                        out.push_str(&format!("{c} {line}\n"));
                    }
                }
            }
        }
    }
    out
}

/// Markdown: prose as is, code in fences, outputs as text in fences and
/// images inline.
pub fn markdown(nb: &Notebook) -> String {
    let lang = nb.language().unwrap_or("python").to_ascii_lowercase();
    let mut out = String::new();
    for cell in &nb.cells {
        if !out.is_empty() {
            out.push('\n');
        }
        let source = unix_lines(&cell.source);
        match cell.cell_type {
            CellType::Markdown => {
                out.push_str(source.trim_end_matches('\n'));
                out.push('\n');
            }
            CellType::Raw => {
                out.push_str(&fence("", &source));
            }
            CellType::Code => {
                out.push_str(&fence(&lang, &source));
                for output in &cell.outputs {
                    out.push('\n');
                    out.push_str(&output_markdown(output));
                }
            }
        }
    }
    out
}

fn output_markdown(output: &Output) -> String {
    match output {
        Output::Stream { text, .. } => fence("", &ansi::strip(&stream::normalize(text))),
        Output::Error { ename, evalue, traceback } => {
            let text = if traceback.is_empty() { format!("{ename}: {evalue}") } else { ansi::strip(&traceback.join("\n")).into_owned() };
            fence("", &text)
        }
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            for mime in ["image/png", "image/jpeg", "image/gif"] {
                if let Some(b64) = data.text(mime) {
                    let b64: String = b64.split_whitespace().collect();
                    return format!("![output](data:{mime};base64,{b64})\n");
                }
            }
            if let Some(md) = data.text("text/markdown") {
                return format!("{}\n", md.trim_end());
            }
            match data.text("text/plain") {
                Some(text) => fence("", &ansi::strip(text)),
                None => String::new(),
            }
        }
        Output::Unknown(_) => String::new(),
    }
}

/// A file written out has one kind of line ending, whatever a cell kept.
fn unix_lines(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n").into() } else { text.into() }
}

/// `text` in a fenced block, its fence longer than any run of backticks in it.
fn fence(lang: &str, text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest.max(2) + 1);
    format!("{ticks}{lang}\n{}\n{ticks}\n", text.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb() -> Notebook {
        herse_nb::parse(include_bytes!("../../../fixtures/formats/crlf_in_sources.ipynb")).unwrap()
    }

    #[test]
    fn a_script_keeps_the_cells() {
        let py = script(&nb());
        assert!(py.starts_with("# %% [markdown]\n# # CRLF inside strings\n"), "{py}");
        assert!(py.contains("\n# %%\nprint('line 1')\n"), "{py}");
    }

    #[test]
    fn markdown_fences_code_and_outputs() {
        let md = markdown(&nb());
        assert!(md.contains("```python\nprint('line 1')"), "{md}");
        assert!(md.contains("```\nline 1\nline 2\n0\n1\n```"), "{md}");
    }

    #[test]
    fn a_fence_outlasts_backticks_inside() {
        assert_eq!(fence("", "a ```` b"), "`````\na ```` b\n`````\n");
    }
}
