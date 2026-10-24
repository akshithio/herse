#![allow(dead_code)]

use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use herse_kernel::async_channel::Receiver;
use herse_kernel::{Kernel, KernelEvent, KernelSpec, KernelStatus, LaunchOptions};
use serde_json::Value;

pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The real `python3` kernel on this machine. Tests fail loudly without it.
pub fn python3() -> KernelSpec {
    herse_kernel::find("python3").expect("these tests need the `python3` kernelspec (ipykernel)")
}

/// Cap concurrent kernels: cargo runs tests in parallel and each ipykernel
/// boot burns ~0.5 s of CPU.
static SLOTS: Mutex<usize> = Mutex::new(0);
static FREED: Condvar = Condvar::new();
const MAX_KERNELS: usize = 4;

pub struct Slot;

impl Slot {
    pub fn acquire() -> Slot {
        let mut n = SLOTS.lock().unwrap();
        while *n >= MAX_KERNELS {
            n = FREED.wait(n).unwrap();
        }
        *n += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *SLOTS.lock().unwrap() -= 1;
        FREED.notify_one();
    }
}

pub struct Session {
    pub kernel: Kernel,
    pub events: Receiver<KernelEvent>,
    pub _slot: Slot,
}

pub fn next(rx: &Receiver<KernelEvent>, timeout: Duration) -> Option<KernelEvent> {
    smol::block_on(async {
        smol::future::or(async { rx.recv().await.ok() }, async {
            smol::Timer::after(timeout).await;
            None
        })
        .await
    })
}

/// Launch without waiting.
pub fn launch_with(opts: LaunchOptions) -> Session {
    let slot = Slot::acquire();
    let (kernel, events) =
        Kernel::launch_with(&python3(), Path::new(env!("CARGO_MANIFEST_DIR")), opts)
            .expect("launch");
    Session {
        kernel,
        events,
        _slot: slot,
    }
}

/// Launch and wait for `Status(Idle)`.
pub fn start() -> Session {
    start_with(LaunchOptions::default())
}

pub fn start_with(opts: LaunchOptions) -> Session {
    let s = launch_with(opts);
    s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Idle))
    });
    s
}

#[derive(Debug, Default)]
pub struct Run {
    pub msg_id: String,
    /// IOPub messages parented to this execution, in order.
    pub iopub: Vec<(String, Value)>,
    pub status: String,
    pub execution_count: Option<i64>,
    pub elapsed: Duration,
}

impl Run {
    pub fn stdout(&self) -> String {
        self.iopub
            .iter()
            .filter(|(t, c)| t == "stream" && c["name"] == "stdout")
            .map(|(_, c)| c["text"].as_str().unwrap_or_default())
            .collect()
    }

    pub fn of_type(&self, msg_type: &str) -> Vec<&Value> {
        self.iopub
            .iter()
            .filter(|(t, _)| t == msg_type)
            .map(|(_, c)| c)
            .collect()
    }

    pub fn result_text(&self) -> Option<String> {
        self.of_type("execute_result")
            .first()
            .and_then(|c| c["data"]["text/plain"].as_str().map(str::to_owned))
    }
}

impl Session {
    /// Consume events until `pred` matches; panics on timeout.
    pub fn wait_for(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&KernelEvent) -> bool,
    ) -> KernelEvent {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match next(&self.events, left) {
                Some(e) if pred(&e) => return e,
                Some(KernelEvent::Died { reason }) => panic!("kernel died: {reason}"),
                Some(_) => {}
                None => panic!("timed out after {timeout:?} waiting for event"),
            }
        }
    }

    /// Collect the events of an already-sent execution until its reply.
    pub fn collect(&self, msg_id: &str, timeout: Duration) -> Run {
        let started = Instant::now();
        let mut run = Run {
            msg_id: msg_id.to_owned(),
            ..Default::default()
        };
        let deadline = started + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match next(&self.events, left) {
                Some(KernelEvent::Iopub {
                    parent_msg_id,
                    msg_type,
                    content,
                }) if parent_msg_id.as_deref() == Some(msg_id) => {
                    run.iopub.push((msg_type, content));
                }
                Some(KernelEvent::ExecuteReply {
                    parent_msg_id,
                    status,
                    execution_count,
                }) if parent_msg_id == msg_id => {
                    run.status = status;
                    run.execution_count = execution_count;
                    run.elapsed = started.elapsed();
                    return run;
                }
                Some(KernelEvent::Died { reason }) => panic!("kernel died: {reason}"),
                Some(_) => {}
                None => panic!("timed out waiting for reply to {msg_id}; got {run:#?}"),
            }
        }
    }

    pub fn run(&self, code: &str) -> Run {
        let id = self.kernel.execute(code);
        self.collect(&id, TIMEOUT)
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // Our watcher thread reaps promptly, so a zombie doesn't linger.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
pub fn signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}
