//! What a coding agent can do to the open notebook (see [`crate::agent`]):
//! read it, change it, run it, and look at data in its kernel. Every change
//! takes the same path as the reader's own (one undo step each, the same
//! autosave), and the view follows it: the cell scrolls into view and wears
//! the agent's mark while the agent works there.
//!
//! Cells are numbered from 1, as the status line and the reader number them.
//! Running code (run_cells, evaluate) waits for the reader's yes, which the
//! conversation asks for before it calls in here (see [`runs_code`]).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyElement, AsyncWindowContext, Context, IntoElement, ParentElement as _, SharedString, Styled as _, WeakEntity,
    div, px, svg,
};
use herse_nb::{CellType, Output};
use serde_json::{Value, json};

use super::{NotebookView, session};
use crate::theme;

/// The longest reply a tool gives: an agent reads every character of it.
const BUDGET: usize = 24_000;
/// How long the agent's mark stays on a cell after its last touch.
const MARK_FOR: Duration = Duration::from_millis(2500);

/// Told to the agent when it connects.
pub const INSTRUCTIONS: &str = "Herse is a notebook app. These tools work on the notebook the user has open \
and is watching: every edit and run you make scrolls their view to that cell. Cells are numbered from 1. \
Read before you edit (overview, then read_cells). To look at data without changing the notebook, use \
evaluate (it runs in the notebook's kernel and sees its variables). Change cells with edit_cell, \
insert_cell and delete_cell, and run them with run_cells. Never write the .ipynb file yourself. \
The user approves each run_cells and evaluate before it starts, so a call can wait a while for them; \
if they deny it, the call fails and says so.";

/// The tools, as MCP lists them.
pub fn definitions() -> Value {
    let cell = json!({ "type": "integer", "minimum": 1, "description": "Cell number, from 1." });
    json!([
        {
            "name": "overview",
            "description": "The open notebook: its file, kernel, and one line per cell (number, type, execution count, first line, whether it raised an error).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "read_cells",
            "description": "The source and outputs of cells `from` through `to` (inclusive; `to` defaults to `from`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": cell,
                    "to": cell,
                    "outputs": { "type": "boolean", "description": "Include outputs (default true)." }
                },
                "required": ["from"]
            }
        },
        {
            "name": "edit_cell",
            "description": "Replace a cell's source. The user can undo it.",
            "inputSchema": {
                "type": "object",
                "properties": { "cell": cell, "source": { "type": "string" } },
                "required": ["cell", "source"]
            }
        },
        {
            "name": "insert_cell",
            "description": "Add a cell so it becomes number `at` (1 puts it first; one past the last cell appends).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "at": cell,
                    "type": { "type": "string", "enum": ["code", "markdown"] },
                    "source": { "type": "string" }
                },
                "required": ["at", "type", "source"]
            }
        },
        {
            "name": "delete_cell",
            "description": "Remove a cell. The user can undo it.",
            "inputSchema": { "type": "object", "properties": { "cell": cell }, "required": ["cell"] }
        },
        {
            "name": "run_cells",
            "description": "Run cells `from` through `to` in the notebook's kernel, as the user would, and return their outputs once they finish. The user approves the run first; the call waits for them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": cell,
                    "to": cell,
                    "timeout_seconds": { "type": "integer", "description": "Stop waiting after this long (default 180)." }
                },
                "required": ["from"]
            }
        },
        {
            "name": "evaluate",
            "description": "Run Python in the notebook's kernel without adding a cell, and return what it printed or evaluated to: for looking at data (df.head(), df.dtypes, df.describe()). It shares the notebook's variables. The user approves the run first; the call waits for them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "timeout_seconds": { "type": "integer", "description": "Stop waiting after this long (default 60)." }
                },
                "required": ["code"]
            }
        }
    ])
}

/// Where the agent is working, shown on that cell.
pub(super) struct AgentMark {
    pub uid: u64,
    pub label: SharedString,
    pub at: Instant,
}

/// The tools that run code in the kernel, which is the whole machine: the
/// reader approves them. Edits don't ask, since each one is undoable.
pub fn runs_code(tool: &str) -> bool {
    matches!(tool, "run_cells" | "evaluate")
}

/// What one conversation's calls started in the kernel, so its Stop
/// interrupts those (by execute request, so never a run of the reader's).
#[derive(Default)]
pub struct Started {
    /// Execute requests (cells and evaluations), by msg_id.
    runs: HashSet<String>,
    /// Bumped by Stop: calls from before it stop waiting and following.
    turn: u64,
}

impl Started {
    pub fn stop(&mut self) {
        self.turn += 1;
    }
}

/// What a run would execute, for the reader to approve.
pub struct RunPreview {
    /// The first cell it runs (to show it), if it runs cells.
    pub first: Option<usize>,
    /// The code, by cell number (`None`: code outside any cell).
    pub code: Vec<(Option<usize>, String)>,
}

/// Answer one tool call. `who` names the agent on the cell it marks;
/// `started` collects what it runs.
pub async fn call(
    nb: WeakEntity<NotebookView>,
    tool: String,
    args: Value,
    who: SharedString,
    started: Rc<RefCell<Started>>,
    cx: &mut AsyncWindowContext,
) -> Result<String, String> {
    let gone = |_| "The notebook was closed.".to_string();
    let stopped = || "The user stopped you.".to_string();
    let turn = started.borrow().turn;
    match tool.as_str() {
        "overview" => nb.update(cx, |nb, _| nb.agent_overview()).map_err(gone),
        "read_cells" => {
            let outputs = args.get("outputs").and_then(Value::as_bool).unwrap_or(true);
            nb.update(cx, |nb, cx| {
                let range = nb.agent_range(&args)?;
                nb.agent_follow(range.start, format!("{who} is reading"), cx);
                Ok(clip(range.map(|ix| nb.agent_cell_text(ix, outputs)).collect::<Vec<_>>().join("\n\n")))
            })
            .map_err(gone)?
        }
        "edit_cell" => {
            let source = string(&args, "source")?;
            nb.update_in(cx, |nb, window, cx| {
                let ix = nb.agent_cell(&args, "cell")?;
                if nb.editing.as_ref().is_some_and(|e| e.uid == nb.cells[ix].uid) {
                    nb.stop_editing(window, cx);
                }
                nb.set_sources(vec![(ix, source)], cx);
                nb.agent_follow(ix, format!("{who} edited this"), cx);
                Ok(format!("Cell {} updated.", ix + 1))
            })
            .map_err(gone)?
        }
        "insert_cell" => {
            let source = string(&args, "source")?;
            let cell_type = match args.get("type").and_then(Value::as_str) {
                Some("markdown") => CellType::Markdown,
                _ => CellType::Code,
            };
            nb.update_in(cx, |nb, window, cx| {
                let at = args.get("at").and_then(Value::as_u64).ok_or("`at` is required.")? as usize;
                let count = nb.doc.nb.cells.len();
                if at == 0 || at > count + 1 {
                    return Err(format!("`at` must be 1 to {}.", count + 1));
                }
                if nb.editing.is_some() {
                    nb.stop_editing(window, cx);
                }
                let mut cell = nb.new_cell(cell_type);
                cell.source = source;
                nb.splice_cells(at - 1..at - 1, vec![cell], true, cx);
                nb.agent_follow(at - 1, format!("{who} added this"), cx);
                Ok(format!("Inserted cell {at}. The notebook has {} cells.", count + 1))
            })
            .map_err(gone)?
        }
        "delete_cell" => nb
            .update_in(cx, |nb, window, cx| {
                let ix = nb.agent_cell(&args, "cell")?;
                if nb.editing.is_some() {
                    nb.stop_editing(window, cx);
                }
                nb.splice_cells(ix..ix + 1, Vec::new(), true, cx);
                if !nb.doc.nb.cells.is_empty() {
                    nb.select(ix.min(nb.doc.nb.cells.len() - 1), false, cx);
                }
                Ok(format!("Deleted cell {}. The notebook has {} cells.", ix + 1, nb.doc.nb.cells.len()))
            })
            .map_err(gone)?,
        "run_cells" => {
            let timeout = Duration::from_secs(args.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(5, 3600));
            let (uids, mut held) = nb
                .update(cx, |nb, cx| {
                    let range = nb.agent_range(&args)?;
                    nb.agent_follow(range.start, format!("{who} is running this"), cx);
                    let before = nb.running_ids();
                    nb.execute(range.clone(), cx);
                    started.borrow_mut().runs.extend(nb.running_ids().difference(&before).cloned());
                    let uids: Vec<u64> = range.map(|ix| nb.cells[ix].uid).collect();
                    // Held for an install: they're queued once it's done.
                    let held: Vec<u64> = uids.iter().copied().filter(|&uid| nb.preflight_waiting(uid)).collect();
                    Ok::<_, String>((uids, held))
                })
                .map_err(gone)??;
            let since = Instant::now();
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                let stopped_now = started.borrow().turn != turn;
                let busy = nb
                    .update(cx, |nb, cx| {
                        let mut started = started.borrow_mut();
                        held.retain(|&uid| {
                            if nb.preflight_waiting(uid) {
                                return true;
                            }
                            let run = nb.session.as_ref().and_then(|s| s.running.iter().find(|(_, r)| r.cell_uid == uid));
                            started.runs.extend(run.map(|(id, _)| id.clone()));
                            false
                        });
                        // Stopped while held: interrupt them as they start.
                        if stopped_now {
                            let _ = nb.agent_interrupt(&mut started, cx);
                            return !held.is_empty();
                        }
                        let busy = uids.iter().copied().find(|&uid| nb.agent_busy(uid));
                        if let Some(ix) = busy.and_then(|uid| nb.cells.iter().position(|c| c.uid == uid)) {
                            nb.agent_follow(ix, format!("{who} is running this"), cx);
                        }
                        busy.is_some()
                    })
                    .map_err(gone)?;
                if !busy {
                    if stopped_now {
                        return Err(stopped());
                    }
                    break;
                }
                if stopped_now {
                    continue;
                }
                if since.elapsed() > timeout {
                    let partial = nb.update(cx, |nb, _| nb.agent_outputs_of(&uids)).map_err(gone)?;
                    return Ok(format!(
                        "Still running after {} s (it keeps running; call run_cells again later or ask the user).\n\n{partial}",
                        timeout.as_secs()
                    ));
                }
            }
            nb.update(cx, |nb, _| {
                let mut text = nb.agent_outputs_of(&uids);
                if let Some(problem) = &nb.problem {
                    text.push_str(&format!("\n\nHerse reports: {}\n{}", problem.title, problem.detail));
                }
                text
            })
            .map_err(gone)
        }
        "evaluate" => {
            let code = string(&args, "code")?;
            let timeout = Duration::from_secs(args.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(60).clamp(2, 1800));
            let msg_id = nb
                .update(cx, |nb, cx| {
                    if !nb.ensure_kernel(cx) {
                        let why = nb.problem.as_ref().map_or_else(String::new, |p| format!(": {}\n{}", p.title, p.detail));
                        return Err(format!("The kernel couldn't start{why}"));
                    }
                    let session = nb.session.as_mut().expect("ensured");
                    let id = session.kernel.execute_with(&code, session::scratch());
                    session.scratch.insert(id.clone(), Default::default());
                    started.borrow_mut().runs.insert(id.clone());
                    cx.notify();
                    Ok(id)
                })
                .map_err(gone)??;
            let since = Instant::now();
            loop {
                cx.background_executor().timer(Duration::from_millis(60)).await;
                if started.borrow().turn != turn {
                    return Err(stopped());
                }
                let finished = nb
                    .update(cx, |nb, _| {
                        let session = nb.session.as_mut()?;
                        let scratch = session.scratch.get_mut(&msg_id)?;
                        if !scratch.done && since.elapsed() < timeout {
                            return Some(None);
                        }
                        if !scratch.done {
                            // Still running: Stop can interrupt it while it's tracked.
                            return Some(Some((false, std::mem::take(&mut scratch.outputs))));
                        }
                        let scratch = session.scratch.remove(&msg_id)?;
                        Some(Some((true, scratch.outputs)))
                    })
                    .map_err(gone)?;
                match finished {
                    Some(None) => continue,
                    Some(Some((true, outputs))) => {
                        let text = outputs_text(&outputs);
                        return Ok(if text.trim().is_empty() { "(no output)".into() } else { clip(text) });
                    }
                    Some(Some((false, outputs))) => {
                        return Ok(format!(
                            "Still running after {} s; output so far:\n{}",
                            timeout.as_secs(),
                            clip(outputs_text(&outputs))
                        ));
                    }
                    None => return Err("The kernel stopped before it answered.".into()),
                }
            }
        }
        other => Err(format!("Herse has no tool called {other}.")),
    }
}

fn string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key).and_then(Value::as_str).map(String::from).ok_or_else(|| format!("`{key}` is required."))
}

/// `text`, cut to the budget with a note saying so.
fn clip(text: String) -> String {
    if text.len() <= BUDGET {
        return text;
    }
    let mut end = BUDGET;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more characters; ask for fewer cells)", &text[..end], text.len() - end)
}

/// Outputs as an agent reads them: text as printed, errors as their
/// traceback's end, rich outputs by their plain-text form or kind.
pub fn outputs_text(outputs: &[Output]) -> String {
    let mut out = String::new();
    for output in outputs {
        match output {
            Output::Stream { text, .. } => out.push_str(&herse_rich::ansi::strip(text)),
            Output::Error { ename, evalue, traceback } => {
                let tb = herse_rich::ansi::strip(&traceback.join("\n")).into_owned();
                let lines: Vec<&str> = tb.lines().collect();
                let tail = &lines[lines.len().saturating_sub(25)..];
                if tail.is_empty() {
                    out.push_str(&format!("{ename}: {evalue}"));
                } else {
                    out.push_str(&tail.join("\n"));
                }
            }
            Output::ExecuteResult { data, .. } | Output::DisplayData { data, .. } => {
                let chart = data.0.iter().find_map(|(mime, _)| match mime.as_str() {
                    m if m.contains("plotly") => Some("Plotly chart"),
                    m if m.contains("vegalite") || m.contains("vega") => Some("Vega-Lite chart"),
                    m if m.contains("widget-view") => Some("widget"),
                    "image/png" | "image/jpeg" | "image/svg+xml" => Some("image"),
                    _ => None,
                });
                match (data.text("text/plain"), chart) {
                    (Some(text), Some(kind)) => out.push_str(&format!("[{kind}] {text}")),
                    (Some(text), None) => out.push_str(text),
                    (None, Some(kind)) => out.push_str(&format!("[{kind}]")),
                    (None, None) => {
                        if let Some(html) = data.text("text/html") {
                            out.push_str(&format!("[HTML, {} characters]", html.len()));
                        }
                    }
                }
            }
            Output::Unknown(_) => {}
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn type_name(t: CellType) -> &'static str {
    match t {
        CellType::Code => "code",
        CellType::Markdown => "markdown",
        CellType::Raw => "raw",
    }
}

impl NotebookView {
    fn agent_overview(&self) -> String {
        let mut out = String::new();
        let name = self.title();
        match self.doc.path.as_deref() {
            Some(path) => out.push_str(&format!("Notebook: {name} ({})\n", path.display())),
            None => out.push_str(&format!("Notebook: {name} (not saved yet)\n")),
        }
        if let Some(label) = self.kernel_label() {
            out.push_str(&format!("Kernel: {label}\n"));
        }
        let cells = &self.doc.nb.cells;
        out.push_str(&format!("{} cells. Selected: cell {}.\n\n", cells.len(), self.selected + 1));
        for (ix, cell) in cells.iter().enumerate() {
            let first = cell.source.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            let first: String = first.chars().take(100).collect();
            let count = cell.execution_count.map_or_else(String::new, |n| format!(" [{n}]"));
            let error = cell
                .outputs
                .iter()
                .find_map(|o| match o {
                    Output::Error { ename, .. } => Some(format!(" (raised {ename})")),
                    _ => None,
                })
                .unwrap_or_default();
            let lines = cell.source.lines().count();
            out.push_str(&format!("{}. {}{count}{error}, {lines} lines: {first}\n", ix + 1, type_name(cell.cell_type)));
            if out.len() > BUDGET {
                out.push_str(&format!("… and {} more cells\n", cells.len() - ix - 1));
                break;
            }
        }
        out
    }

    fn agent_cell_text(&self, ix: usize, outputs: bool) -> String {
        let cell = &self.doc.nb.cells[ix];
        let count = cell.execution_count.map_or_else(String::new, |n| format!(", ran as [{n}]"));
        let mut out = format!("## Cell {} ({}{count})\n{}", ix + 1, type_name(cell.cell_type), cell.source);
        if outputs && !cell.outputs.is_empty() {
            out.push_str("\n### Output\n");
            let text = outputs_text(&cell.outputs);
            let text: String = if text.len() > 6000 {
                let mut end = 6000;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}… (output cut)", &text[..end])
            } else {
                text
            };
            out.push_str(text.trim_end());
        }
        out
    }

    fn agent_outputs_of(&self, uids: &[u64]) -> String {
        let text = uids
            .iter()
            .filter_map(|uid| self.cells.iter().position(|c| c.uid == *uid))
            .filter(|&ix| self.doc.nb.cells[ix].cell_type == CellType::Code)
            .map(|ix| {
                let cell = &self.doc.nb.cells[ix];
                let body = outputs_text(&cell.outputs);
                let body = if body.trim().is_empty() { "(no output)".to_string() } else { body.trim_end().to_string() };
                format!("## Cell {}\n{body}", ix + 1)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        clip(text)
    }

    /// A cell from a 1-based number in `args[key]`.
    fn agent_cell(&self, args: &Value, key: &str) -> Result<usize, String> {
        let n = args.get(key).and_then(Value::as_u64).ok_or_else(|| format!("`{key}` is required."))? as usize;
        let count = self.doc.nb.cells.len();
        if n == 0 || n > count {
            return Err(format!("There is no cell {n}: the notebook has {count} cells."));
        }
        Ok(n - 1)
    }

    /// Cells `from` through `to` (1-based, inclusive).
    fn agent_range(&self, args: &Value) -> Result<std::ops::Range<usize>, String> {
        let from = self.agent_cell(args, "from")?;
        let to = if args.get("to").is_some_and(|v| !v.is_null()) { self.agent_cell(args, "to")? } else { from };
        if to < from {
            return Err("`to` comes before `from`.".into());
        }
        Ok(from..to + 1)
    }

    /// The cell is queued, running, or waiting for an install.
    fn agent_busy(&self, uid: u64) -> bool {
        self.run_state(uid).is_some() || self.preflight_waiting(uid)
    }

    /// The execute requests queued or running for cells.
    fn running_ids(&self) -> HashSet<String> {
        self.session.as_ref().map(|s| s.running.keys().cloned().collect()).unwrap_or_default()
    }

    /// What a run_cells or evaluate call would execute. `None` when it
    /// would run nothing (or its arguments are wrong, which the call says).
    pub fn agent_run_preview(&self, tool: &str, args: &Value) -> Option<RunPreview> {
        match tool {
            "run_cells" => {
                let range = self.agent_range(args).ok()?;
                let code: Vec<(Option<usize>, String)> = range
                    .clone()
                    .map(|ix| (ix, &self.doc.nb.cells[ix]))
                    .filter(|(_, c)| c.cell_type == CellType::Code && !c.source.trim().is_empty())
                    .map(|(ix, c)| (Some(ix + 1), c.source.clone()))
                    .collect();
                (!code.is_empty()).then_some(RunPreview { first: Some(range.start), code })
            }
            "evaluate" => {
                let code = args.get("code").and_then(Value::as_str).filter(|c| !c.trim().is_empty())?;
                Some(RunPreview { first: None, code: vec![(None, code.to_string())] })
            }
            _ => None,
        }
    }

    /// Stop what a conversation started that's still in the kernel: if one
    /// of its runs is queued or running, interrupt the kernel (which also
    /// drops the cells queued after it). Whether it did.
    pub fn agent_interrupt(&mut self, started: &mut Started, cx: &mut Context<Self>) -> Result<bool, String> {
        let Some(session) = self.session.as_mut() else {
            started.runs.clear();
            return Ok(false);
        };
        started.runs.retain(|id| match session.scratch.get(id) {
            Some(scratch) if scratch.done => {
                session.scratch.remove(id);
                false
            }
            Some(_) => true,
            None => session.running.contains_key(id),
        });
        if started.runs.is_empty() {
            return Ok(false);
        }
        let result = session.kernel.interrupt().map_err(|e| format!("{e:#}"));
        // No one waits for these evaluations now; what they print is dropped.
        for id in started.runs.drain() {
            session.scratch.remove(&id);
        }
        self.agent_mark = None;
        cx.notify();
        result.map(|()| true)
    }

    /// Show the reader where the agent is: select and reveal the cell, and
    /// mark it for a moment.
    pub(crate) fn agent_follow(&mut self, ix: usize, label: String, cx: &mut Context<Self>) {
        let Some(uid) = self.cells.get(ix).map(|c| c.uid) else { return };
        if self.selected != ix {
            self.select(ix, false, cx);
        } else {
            self.reveal(ix);
        }
        self.agent_mark = Some(AgentMark { uid, label: label.into(), at: Instant::now() });
        cx.notify();
        self.agent_mark_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MARK_FOR).await;
            this.update(cx, |this, cx| {
                if this.agent_mark.as_ref().is_some_and(|m| m.at.elapsed() >= MARK_FOR) {
                    this.agent_mark = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The agent's mark over cell `uid`, while it works there: a ring and a
    /// label at the top right.
    pub(super) fn render_agent_mark(&self, uid: u64, cx: &Context<Self>) -> Option<AnyElement> {
        let mark = self.agent_mark.as_ref().filter(|m| m.uid == uid)?;
        let theme = cx.theme();
        let accent = theme::accent(cx);
        Some(
            div()
                .absolute()
                .inset_0()
                .rounded(px(8.))
                .border_2()
                .border_color(accent.opacity(0.55))
                .child(
                    div()
                        .absolute()
                        .top(px(-11.))
                        .right(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.))
                        .px(px(8.))
                        .h(px(20.))
                        .rounded_full()
                        .bg(accent)
                        .text_color(theme.background)
                        .text_size(px(11.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(svg().path("icons/sparkles.svg").size(px(11.)).text_color(theme.background))
                        .child(mark.label.clone()),
                )
                .into_any_element(),
        )
    }
}
