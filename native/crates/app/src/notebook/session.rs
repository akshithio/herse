//! A notebook's kernel: launched lazily on first run, events applied to cells
//! in batches so a chatty cell (thousands of prints a second) costs one
//! re-render per frame, not one per message.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use herse_kernel::{Kernel, KernelEvent, KernelStatus};

/// Options for code Herse runs on its own behalf: no outputs, no history,
/// never stops the queue.
pub fn silent() -> herse_kernel::ExecuteOptions {
    herse_kernel::ExecuteOptions {
        silent: true,
        store_history: false,
        allow_stdin: false,
        stop_on_error: false,
        ..Default::default()
    }
}

pub struct Session {
    pub kernel: Kernel,
    pub name: String,
    pub display_name: String,
    pub status: KernelStatus,
    /// msg_id of an execute request -> the cell it runs (by stable uid).
    pub running: HashMap<String, Run>,
    /// Cells whose next output replaces their outputs (`clear_output(wait=True)`).
    pub clear_on_next: Vec<u64>,
    pub boot_ms: Option<f32>,
    pub pending_input: Option<InputPrompt>,
    /// A Python kernel (Herse's chart style and preflight apply).
    pub python: bool,
    /// Code run outside any cell (an agent looking at data), by msg_id.
    pub scratch: HashMap<String, Scratch>,
}

/// What a run outside the cells printed, and whether it's finished.
#[derive(Default)]
pub struct Scratch {
    pub outputs: Vec<herse_nb::Output>,
    pub done: bool,
}

/// Options for a run outside the cells: visible output, no history, no
/// input prompt, and an error doesn't cancel the cells queued after it.
pub fn scratch() -> herse_kernel::ExecuteOptions {
    herse_kernel::ExecuteOptions { store_history: false, allow_stdin: false, stop_on_error: false, ..Default::default() }
}

pub struct Run {
    pub cell_uid: u64,
    pub queued_at: Instant,
    pub started_at: Option<Instant>,
}

pub struct InputPrompt {
    pub cell_uid: u64,
    pub prompt: String,
    pub password: bool,
}

impl Session {
    pub fn is_busy(&self) -> bool {
        !self.running.is_empty()
    }
}

/// Receive at least one event, then everything already queued, so the UI
/// applies a burst in a single update.
pub async fn next_batch(rx: &herse_kernel::async_channel::Receiver<KernelEvent>) -> Option<Vec<KernelEvent>> {
    let first = rx.recv().await.ok()?;
    let mut batch = vec![first];
    while let Ok(ev) = rx.try_recv() {
        batch.push(ev);
        if batch.len() >= 4096 {
            break;
        }
    }
    Some(batch)
}

pub fn status_label(status: KernelStatus) -> &'static str {
    match status {
        KernelStatus::Starting => "Starting",
        KernelStatus::Idle => "Idle",
        KernelStatus::Busy => "Busy",
        KernelStatus::Restarting => "Restarting",
        KernelStatus::Dead => "Dead",
    }
}

pub fn format_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.;
    if ms < 1000. {
        format!("{ms:.0} ms")
    } else if ms < 60_000. {
        format!("{:.1} s", ms / 1000.)
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}
