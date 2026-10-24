//! A tiny terminal REPL over herse-kernel.
//!
//! ```sh
//! cargo run -p herse-kernel --example repl            # default kernel (python3)
//! cargo run -p herse-kernel --example repl -- ir      # a kernel by name
//! echo 'print(1+1)' | cargo run -p herse-kernel --example repl
//! ```
//!
//! Multi-line input is driven by the kernel's `is_complete`. `:restart` restarts
//! the kernel, `:quit` (or EOF) exits.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use herse_kernel::{Kernel, KernelEvent, KernelStatus};
use serde_json::Value;

fn main() -> Result<()> {
    let specs = herse_kernel::discover();
    let spec = match std::env::args().nth(1) {
        Some(name) => specs.iter().find(|s| s.name == name).with_context(|| {
            let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
            format!("no kernel named {name:?}; installed: {names:?}")
        })?,
        None => herse_kernel::default_spec(&specs).context("no Jupyter kernels installed")?,
    };
    let cwd = std::env::current_dir()?;
    let launched = Instant::now();
    let (kernel, events) = Kernel::launch(spec, &cwd)?;
    eprintln!(
        "starting {} (pid {})...",
        spec.display_name,
        kernel.pid().unwrap_or(0)
    );
    wait_ready(&events)?;
    let timing = kernel.timing().context("ready without timing")?;
    eprintln!(
        "ready in {:.0?} (spawn {:.1?}, sockets up after {:.0?})",
        launched.elapsed(),
        timing.spawn,
        timing.connect
    );

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut count = 1;
    loop {
        if interactive {
            print!("In [{count}]: ");
            std::io::stdout().flush()?;
        }
        let Some(first) = lines.next().transpose()? else {
            break;
        };
        match first.trim() {
            ":quit" | ":q" => break,
            ":restart" => {
                kernel.restart()?;
                wait_ready(&events)?;
                eprintln!("restarted");
                count = 1;
                continue;
            }
            "" => continue,
            _ => {}
        }
        let mut code = first;
        // Keep reading while the kernel says the cell is incomplete.
        loop {
            let reply = kernel.is_complete(&code).recv_blocking()??;
            if reply.status != "incomplete" {
                break;
            }
            if interactive {
                print!(
                    "{:>width$}: {}",
                    "...",
                    reply.indent,
                    width = format!("In [{count}]").len()
                );
                std::io::stdout().flush()?;
            }
            match lines.next().transpose()? {
                Some(line) if !line.trim().is_empty() => {
                    code.push('\n');
                    code.push_str(&line);
                }
                _ => break,
            }
        }
        let started = Instant::now();
        let id = kernel.execute(&code);
        if let Some(n) = run(&kernel, &events, &id, &mut lines)? {
            count = n as usize + 1;
        }
        if interactive {
            eprintln!("\x1b[2m({:.1?})\x1b[0m", started.elapsed());
        }
    }
    kernel.shutdown_and_wait(Duration::from_secs(3));
    Ok(())
}

fn wait_ready(events: &herse_kernel::async_channel::Receiver<KernelEvent>) -> Result<()> {
    loop {
        match events.recv_blocking()? {
            KernelEvent::Status(KernelStatus::Idle) => return Ok(()),
            KernelEvent::Died { reason } => return Err(anyhow!("kernel died: {reason}")),
            _ => {}
        }
    }
}

/// Print one execution's output; returns its execution count.
fn run(
    kernel: &Kernel,
    events: &herse_kernel::async_channel::Receiver<KernelEvent>,
    id: &str,
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
) -> Result<Option<i64>> {
    let mut out = std::io::stdout();
    loop {
        match events.recv_blocking()? {
            KernelEvent::Iopub {
                parent_msg_id,
                msg_type,
                content,
            } if parent_msg_id.as_deref() == Some(id) => {
                match msg_type.as_str() {
                    "stream" => {
                        let text = content["text"].as_str().unwrap_or_default();
                        if content["name"] == "stderr" {
                            eprint!("{text}");
                        } else {
                            print!("{text}");
                        }
                    }
                    "execute_result" => {
                        let n = content["execution_count"].as_i64().unwrap_or(0);
                        println!("\x1b[31mOut[{n}]:\x1b[0m {}", render(&content["data"]));
                    }
                    "display_data" | "update_display_data" => {
                        println!("{}", render(&content["data"]))
                    }
                    "error" => {
                        for line in content["traceback"].as_array().into_iter().flatten() {
                            eprintln!("{}", line.as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
                out.flush()?;
            }
            KernelEvent::InputRequest {
                prompt, password, ..
            } => {
                print!("{prompt}");
                out.flush()?;
                let value = lines.next().transpose()?.unwrap_or_else(|| "\u{4}".into());
                if password {
                    println!();
                }
                kernel.input_reply(&value);
            }
            KernelEvent::ExecutePayload { payload, .. } => {
                for p in payload {
                    if p["source"] == "page" {
                        println!("{}", render(&p["data"]));
                    }
                }
            }
            KernelEvent::ExecuteReply {
                parent_msg_id,
                execution_count,
                ..
            } if parent_msg_id == id => {
                return Ok(execution_count);
            }
            KernelEvent::Died { reason } => return Err(anyhow!("kernel died: {reason}")),
            _ => {}
        }
    }
}

/// Best text rendering of a mime bundle.
fn render(data: &Value) -> String {
    if let Some(text) = data.get("text/plain").and_then(Value::as_str) {
        return text.to_owned();
    }
    let kinds: Vec<String> = data
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(mime, v)| format!("{mime}, {} bytes", v.as_str().map_or(0, str::len)))
                .collect()
        })
        .unwrap_or_default();
    format!("[{}]", kinds.join("; "))
}
