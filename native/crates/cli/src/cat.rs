//! `herse cat FILE`: a notebook as it reads (prose, code with its prompt,
//! outputs as text), or a data file's columns and first rows.

use std::io::{IsTerminal as _, Write};
use std::path::Path;

use herse_data::{Event, Flow, Frame, Kind};
use herse_nb::{CellType, Output};
use herse_rich::{ansi, stream};

use crate::{Args, Failure};

/// Rows of a data file shown.
const ROWS: usize = 20;
/// A table cell's width at most, in characters.
const CELL: usize = 24;

pub fn main(mut args: Args) -> Result<(), Failure> {
    let outputs = !args.flag(&["--no-outputs"]);
    let path = args.path("file")?;
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let text = if herse_data::Format::of(&path).is_some() && !is_notebook(&path) {
        table(&path)?
    } else {
        notebook(&crate::read_notebook(&path)?, outputs, color)
    };
    // Into `head` or `less`: stop quietly when the reader goes away.
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
    Ok(())
}

fn is_notebook(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

struct Style {
    color: bool,
}

impl Style {
    fn dim(&self, s: &str) -> String {
        if self.color { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(&self, s: &str) -> String {
        if self.color { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() }
    }
    fn red(&self, s: &str) -> String {
        if self.color { format!("\x1b[31m{s}\x1b[0m") } else { s.to_string() }
    }
    /// Text that may carry the kernel's own colors: kept for a terminal.
    fn colored(&self, s: &str) -> String {
        if self.color { s.to_string() } else { ansi::strip(s).into_owned() }
    }
}

/// The whole notebook as text.
pub fn notebook(nb: &herse_nb::Notebook, outputs: bool, color: bool) -> String {
    let style = Style { color };
    const GUTTER: usize = 6;
    let pad = " ".repeat(GUTTER);
    let mut out = String::new();
    for (i, cell) in nb.cells.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match cell.cell_type {
            CellType::Markdown => {
                for line in cell.source.lines() {
                    if line.starts_with('#') {
                        out.push_str(&style.bold(line));
                    } else {
                        out.push_str(line);
                    }
                    out.push('\n');
                }
            }
            CellType::Raw => {
                for line in cell.source.lines() {
                    out.push_str(&style.dim(line));
                    out.push('\n');
                }
            }
            CellType::Code => {
                let prompt = match cell.execution_count {
                    Some(n) => format!("[{n}]"),
                    None => "[ ]".to_string(),
                };
                let mut lines = cell.source.lines();
                let first = lines.next().unwrap_or("");
                out.push_str(&style.dim(&format!("{prompt:<GUTTER$}")));
                out.push_str(first);
                out.push('\n');
                for line in lines {
                    out.push_str(&pad);
                    out.push_str(line);
                    out.push('\n');
                }
                if outputs && !cell.outputs.is_empty() {
                    out.push('\n');
                    for output in &cell.outputs {
                        for line in output_text(output, &style).lines() {
                            out.push_str(&pad);
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
            }
        }
    }
    out
}

/// One output as text: streams and errors as printed, rich outputs by their
/// plain text (or what they are, when they have none).
fn output_text(output: &Output, style: &Style) -> String {
    match output {
        Output::Stream { name, text } => {
            let text = stream::normalize(text);
            let text = style.colored(&text);
            if *name == herse_nb::StreamName::Stderr { style.red(&text) } else { text }
        }
        Output::Error { ename, evalue, traceback } => {
            if traceback.is_empty() {
                style.red(&format!("{ename}: {evalue}"))
            } else {
                style.colored(&traceback.join("\n"))
            }
        }
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            if let Some(text) = data.text("text/plain") {
                return style.colored(text);
            }
            if let Some(text) = data.text("text/markdown") {
                return text.to_string();
            }
            let kinds: Vec<&str> = data.mime_types().collect();
            style.dim(&format!("[{}]", kinds.join(", ")))
        }
        Output::Unknown(map) => {
            let kind = map.get("output_type").and_then(|v| v.as_str()).unwrap_or("output");
            style.dim(&format!("[{kind}]"))
        }
    }
}

/// A data file: its size, a line per column, then the first rows.
fn table(path: &Path) -> Result<String, Failure> {
    let mut frame = Frame::default();
    let loaded = herse_data::read(path, None, &mut |event| {
        match event {
            Event::Columns(columns) => frame.set_columns(columns),
            Event::Chunk(chunk) => frame.push_chunk(chunk),
            Event::Progress(_) => {}
        }
        Flow::Continue
    })
    .map_err(|e| Failure::Failed(format!("couldn't read {}: {e}", path.display())))?;
    let columns = frame.columns();
    let mut out = format!("{} rows × {} columns\n", group(frame.rows()), columns.len());
    for note in &loaded.warnings {
        out.push_str(&format!("note: {note}\n"));
    }
    out.push('\n');
    let width = columns.iter().map(|c| c.name.chars().count()).max().unwrap_or(0).min(CELL);
    for (c, column) in columns.iter().enumerate() {
        let stats = frame.stats(c);
        let kind = frame.kind(c);
        let mut line = format!("  {:<width$}  {:<5}", clip(&column.name, width), column.dtype.as_deref().unwrap_or(kind.label()));
        if stats.nulls > 0 {
            line.push_str(&format!("  {} missing", group(stats.nulls)));
        }
        if matches!(kind, Kind::Int | Kind::Float) && stats.numbers > 0 {
            line.push_str(&format!("  {} to {}", number(stats.min), number(stats.max)));
            if let Some(mean) = stats.mean() {
                line.push_str(&format!(", mean {}", number(mean)));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&rows(&frame, terminal_width()));
    if frame.rows() > ROWS {
        out.push_str(&format!("… {} more rows\n", group(frame.rows() - ROWS)));
    }
    Ok(out)
}

/// The first rows, as many columns as fit the terminal.
fn rows(frame: &Frame, width: usize) -> String {
    let columns = frame.columns();
    let shown = frame.rows().min(ROWS);
    let number_width = shown.to_string().len();
    let widths: Vec<usize> = (0..columns.len())
        .map(|c| {
            let values = (0..shown).map(|r| frame.text(r, c).chars().count()).max().unwrap_or(0);
            values.max(columns[c].name.chars().count()).min(CELL)
        })
        .collect();
    let mut fit = 0;
    let mut used = number_width;
    for w in &widths {
        if used + 2 + w > width && fit > 0 {
            break;
        }
        used += 2 + w;
        fit += 1;
    }
    let right = |c: usize| frame.kind(c).is_numeric();
    let cell = |text: &str, c: usize| {
        let text = clip(text, widths[c]);
        if right(c) { format!("{text:>w$}", w = widths[c]) } else { format!("{text:<w$}", w = widths[c]) }
    };
    let mut out = " ".repeat(number_width);
    for (c, column) in columns.iter().enumerate().take(fit) {
        out.push_str("  ");
        out.push_str(&cell(&column.name, c));
    }
    if fit < columns.len() {
        out.push_str(&format!("  (+{} more)", columns.len() - fit));
    }
    out = out.trim_end().to_string();
    out.push('\n');
    for r in 0..shown {
        let mut line = format!("{:>number_width$}", r + 1);
        for c in 0..fit {
            line.push_str("  ");
            line.push_str(&cell(if frame.is_null(r, c) { "" } else { frame.text(r, c) }, c));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn terminal_width() -> usize {
    if let Some(cols) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return cols;
    }
    // SAFETY: TIOCGWINSZ fills a winsize; a failure leaves it zeroed.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 { size.ws_col as usize } else { 120 }
}

/// At most `n` characters, an ellipsis standing for the rest; one line.
fn clip(text: &str, n: usize) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).collect();
    if flat.chars().count() <= n {
        return flat;
    }
    let mut short: String = flat.chars().take(n.saturating_sub(1)).collect();
    short.push('…');
    short
}

fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn number(x: f64) -> String {
    if x.fract() == 0. && x.abs() < 1e15 {
        let whole = group(x.abs() as usize);
        return if x < 0. { format!("-{whole}") } else { whole };
    }
    let s = format!("{x:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_a_notebook_as_it_reads() {
        let nb = herse_nb::parse(include_bytes!("../../../fixtures/formats/crlf_in_sources.ipynb")).unwrap();
        let text = notebook(&nb, true, false);
        assert!(text.starts_with("# CRLF inside strings\n"), "{text}");
        assert!(text.contains("[1]   print('line 1')\n      print('line 2')"), "{text}");
        assert!(!text.contains('\x1b'), "no color into a pipe");
    }

    #[test]
    fn clips_and_groups() {
        assert_eq!(clip("a\nlong value here", 8), "a long …");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(number(2.5), "2.5");
        assert_eq!(number(-1200.), "-1,200");
    }
}
