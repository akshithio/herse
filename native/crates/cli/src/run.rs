//! `herse run NOTEBOOK`: Restart and Run All, then save. Every code cell
//! runs top to bottom in a fresh kernel (the notebook's own, as Herse picks
//! it); the outputs go into the file, formatted as it was.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use herse_kernel::async_channel::{Receiver, TryRecvError};
use herse_kernel::{Kernel, KernelEvent, KernelSpec};
use herse_nb::{CellType, Notebook, Output, Value};
use herse_rich::ansi;

use crate::{Args, Failure};

pub fn main(mut args: Args) -> Result<(), Failure> {
    let keep_going = args.flag(&["--keep-going"]);
    let out = args.value(&["-o", "--out"])?.map(PathBuf::from);
    let kernel_name = args.value(&["--kernel"])?.map(|k| k.to_string_lossy().into_owned());
    let timeout = match args.value(&["--timeout"])? {
        Some(t) => Some(
            t.to_str()
                .and_then(|t| t.parse::<f64>().ok())
                .filter(|t| *t > 0.)
                .map(Duration::from_secs_f64)
                .ok_or_else(|| Failure::Usage("--timeout takes a number of seconds".into()))?,
        ),
        None => None,
    };
    let path = args.path("notebook")?;
    let mut nb = crate::read_notebook(&path)?;
    let spec = pick_spec(&nb, kernel_name.as_deref())?;
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let tty = std::io::stderr().is_terminal();
    let progress = Progress { tty };

    let cwd = std::fs::canonicalize(&path)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    progress.line(&format!("Starting {}…", spec.display_name));
    let (kernel, events) =
        Kernel::launch(&spec, &cwd).map_err(|e| Failure::Failed(format!("couldn't start {}: {e}", spec.display_name)))?;
    kernel
        .wait_ready(Duration::from_secs(120))
        .map_err(|e| Failure::Failed(format!("{} didn't start: {e}", spec.display_name)))?;

    // A clean slate, as Restart and Run All leaves it: no output is older
    // than this run.
    let code: Vec<usize> = nb.cells.iter().enumerate().filter(|(_, c)| c.cell_type == CellType::Code).map(|(i, _)| i).collect();
    for &ix in &code {
        nb.cells[ix].outputs.clear();
        nb.cells[ix].execution_count = None;
    }
    let to_run: Vec<usize> = code.iter().copied().filter(|&ix| !nb.cells[ix].source.trim().is_empty()).collect();
    let mut failures = Vec::new();
    let started = Instant::now();
    for (k, &ix) in to_run.iter().enumerate() {
        progress.line(&format!("Running cell {} of {} ({} of {})…", ix + 1, nb.cells.len(), k + 1, to_run.len()));
        let id = kernel.execute(&nb.cells[ix].source);
        let status = match collect(&kernel, &events, &id, &mut nb, ix, timeout) {
            Ok(status) => status,
            Err(why) => {
                progress.done();
                // Keep what ran: it says how far it got.
                save(&nb, out.as_deref().unwrap_or(&path))?;
                return Err(Failure::Failed(format!("{why} (cell {}). The outputs so far are saved.", ix + 1)));
            }
        };
        if status != "ok" {
            progress.done();
            eprintln!("Cell {} {}:", ix + 1, if status == "timeout" { "ran too long and was stopped" } else { "failed" });
            for output in &nb.cells[ix].outputs {
                if let Output::Error { ename, evalue, traceback } = output {
                    let text = if traceback.is_empty() { format!("{ename}: {evalue}") } else { traceback.join("\n") };
                    eprintln!("{}", if tty { text } else { ansi::strip(&text).into_owned() });
                }
            }
            failures.push(ix);
            if !keep_going {
                break;
            }
        }
    }
    progress.done();
    let _ = kernel.shutdown_and_wait(Duration::from_secs(5));
    let target = out.as_deref().unwrap_or(&path);
    save(&nb, target)?;
    let saved = if out.is_some() { format!("Saved to {}.", target.display()) } else { format!("Saved {name}.") };
    match failures.as_slice() {
        [] => {
            eprintln!("Ran {} in {} ({}). {saved}", plural(to_run.len(), "cell"), spec.display_name, seconds(started.elapsed()));
            Ok(())
        }
        [one] if !keep_going => {
            eprintln!("Stopped at cell {}; the cells after it didn't run. {saved}", one + 1);
            Err(Failure::Failed(String::new()))
        }
        [one] => {
            eprintln!("Cell {} failed. {saved}", one + 1);
            Err(Failure::Failed(String::new()))
        }
        many => {
            let list: Vec<String> = many.iter().map(|ix| (ix + 1).to_string()).collect();
            eprintln!("{} failed (cells {}). {saved}", plural(many.len(), "cell"), list.join(", "));
            Err(Failure::Failed(String::new()))
        }
    }
}

/// The notebook's kernel by name, else one for its language, else the
/// default: as the app chooses.
fn pick_spec(nb: &Notebook, asked: Option<&str>) -> Result<KernelSpec, Failure> {
    let specs = herse_kernel::discover();
    if let Some(name) = asked {
        return specs.iter().find(|s| s.name == name).cloned().ok_or_else(|| {
            let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
            Failure::Failed(format!("there's no kernel called {name}. Installed: {}.", names.join(", ")))
        });
    }
    if let Some(name) = nb.kernel_name()
        && let Some(spec) = specs.iter().find(|s| s.name == name)
    {
        return Ok(spec.clone());
    }
    if let Some(lang) = nb.language()
        && let Some(spec) = specs.iter().find(|s| s.language.eq_ignore_ascii_case(lang))
    {
        return Ok(spec.clone());
    }
    herse_kernel::default_spec(&specs).cloned().ok_or_else(|| {
        Failure::Failed("no Jupyter kernel is installed. Install one (python3 -m pip install ipykernel) and try again.".into())
    })
}

/// Take in what the kernel says about request `id` until it's done; its
/// status: `ok`, `error`, `aborted`, or `timeout`.
fn collect(
    kernel: &Kernel,
    events: &Receiver<KernelEvent>,
    id: &str,
    nb: &mut Notebook,
    ix: usize,
    timeout: Option<Duration>,
) -> Result<String, String> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut timed_out = false;
    let mut clear_next = false;
    loop {
        let event = match events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => {
                if !timed_out && deadline.is_some_and(|d| Instant::now() >= d) {
                    timed_out = true;
                    let _ = kernel.interrupt();
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(TryRecvError::Closed) => return Err("the kernel went away".into()),
        };
        match event {
            KernelEvent::Iopub { parent_msg_id, msg_type, content } => {
                if msg_type == "update_display_data" {
                    update_display(nb, &content);
                    continue;
                }
                if parent_msg_id.as_deref() != Some(id) {
                    continue;
                }
                let cell = &mut nb.cells[ix];
                match msg_type.as_str() {
                    "execute_input" => {
                        if let Some(n) = content.get("execution_count").and_then(Value::as_i64) {
                            cell.execution_count = Some(n);
                        }
                    }
                    "clear_output" => {
                        if content.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                            clear_next = true;
                        } else {
                            cell.outputs.clear();
                        }
                    }
                    _ => {
                        if let Some(output) = herse_nb::output_from_iopub(&msg_type, &content) {
                            if std::mem::take(&mut clear_next) {
                                cell.outputs.clear();
                            }
                            push_output(&mut cell.outputs, output);
                        }
                    }
                }
            }
            KernelEvent::ExecuteReply { parent_msg_id, status, execution_count } if parent_msg_id == id => {
                if let Some(n) = execution_count {
                    nb.cells[ix].execution_count = Some(n);
                }
                return Ok(if timed_out { "timeout".into() } else { status });
            }
            KernelEvent::InputRequest { parent_msg_id, prompt, .. } if parent_msg_id == id => {
                eprintln!("Cell {} asked for input ({}); it gets an empty line.", ix + 1, prompt.trim());
                kernel.input_reply("");
            }
            KernelEvent::Died { reason } => return Err(format!("the kernel stopped: {reason}")),
            _ => {}
        }
    }
}

/// Append an output, merging consecutive chunks of the same stream the way
/// Jupyter stores them.
fn push_output(outputs: &mut Vec<Output>, output: Output) {
    if let Output::Stream { name, text } = &output
        && let Some(Output::Stream { name: last, text: last_text }) = outputs.last_mut()
        && last == name
    {
        last_text.push_str(text);
        return;
    }
    outputs.push(output);
}

/// `update_display_data`: every output showing that display, redrawn.
fn update_display(nb: &mut Notebook, content: &Value) {
    let Some(display) = content.pointer("/transient/display_id").and_then(Value::as_str) else { return };
    let Some(new) = herse_nb::output_from_iopub("display_data", content) else { return };
    let Some(data) = new.data() else { return };
    for cell in &mut nb.cells {
        for output in &mut cell.outputs {
            if output.display_id() == Some(display)
                && let Some(old) = output.data_mut()
            {
                *old = data.clone();
            }
        }
    }
}

fn save(nb: &Notebook, path: &Path) -> Result<(), Failure> {
    crate::write_atomically(path, &herse_nb::to_string(nb))
}

/// One line of progress: rewritten in place on a terminal, else one per step.
struct Progress {
    tty: bool,
}

impl Progress {
    fn line(&self, text: &str) {
        if self.tty {
            eprint!("\r\x1b[2K{text}");
        } else {
            eprintln!("{text}");
        }
    }

    fn done(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn seconds(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60. { format!("{s:.1} s") } else { format!("{}m {:02}s", s as u64 / 60, s as u64 % 60) }
}
