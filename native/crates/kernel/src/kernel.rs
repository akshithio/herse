//! The [`Kernel`] handle and the per-kernel actor thread.
//!
//! Each kernel gets one thread running a single-threaded smol executor. On
//! it, an actor owns all state (pending requests, the pre-ready queue, the
//! current process "generation") and processes one input at a time from one
//! channel: commands from handles, decoded messages from the socket reader
//! tasks, process exits from the watcher thread, heartbeat failures. Socket
//! reads and writes are separate tasks on the same thread, so the actor never
//! blocks on I/O.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::{Receiver, Sender};
use serde::Serialize;
use serde_json::{Map, Value, json};
use smol::io::{AsyncWriteExt, BufReader};
use smol::net::TcpStream;
use smol::{LocalExecutor, Task, Timer};

use crate::connection::{self, ConnectionInfo};
use crate::process;
use crate::spec::{self, InterruptMode, KernelSpec};
use crate::types::*;
use crate::util::{self, with_deadline};
use crate::wire::{self, Signer};
use crate::zmtp::{self, SocketType};

/// Relaunch attempts when the kernel can't bind its ports.
const MAX_ATTEMPTS: u32 = 3;
/// First startup probe interval (doubles up to `PROBE_MAX`).
const PROBE_FIRST: Duration = Duration::from_millis(250);
const PROBE_MAX: Duration = Duration::from_secs(2);
/// If `kernel_info_reply` arrived but no IOPub message yet, re-probe this soon.
const PROBE_IOPUB_RETRY: Duration = Duration::from_millis(10);
/// How long an `execute_reply` waits for its IOPub `idle` before being
/// delivered anyway (the idle may have been dropped by a full IOPub queue).
const REPLY_GRACE: Duration = Duration::from_secs(3);
/// After SIGKILL, how long to wait for the reap before forgetting a process.
const KILL_GIVE_UP: Duration = Duration::from_secs(2);
/// After a connection error, how long to wait for the exit status (which
/// usually explains it better) before declaring the kernel dead.
const CONNECT_ERROR_GRACE: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Shared state (handle <-> actor)
// ---------------------------------------------------------------------------

struct State {
    status: KernelStatus,
    /// The actor has finished shutting down; nothing will happen any more.
    closed: bool,
    pid: Option<u32>,
    info: Option<KernelInfo>,
    timing: Option<LaunchTiming>,
    connection_file: PathBuf,
    dead_reason: Option<String>,
    restarts_requested: u64,
    restarts_handled: u64,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    event: event_listener::Event,
}

enum Readiness {
    Ready,
    Pending,
    Failed(String),
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
        self.event.notify(usize::MAX);
    }

    fn readiness(state: &State) -> Readiness {
        if state.closed {
            return Readiness::Failed("kernel was shut down".into());
        }
        if state.restarts_handled < state.restarts_requested {
            return Readiness::Pending;
        }
        match state.status {
            KernelStatus::Idle | KernelStatus::Busy => Readiness::Ready,
            KernelStatus::Dead => Readiness::Failed(
                state
                    .dead_reason
                    .clone()
                    .unwrap_or_else(|| "kernel is dead".into()),
            ),
            KernelStatus::Starting | KernelStatus::Restarting => Readiness::Pending,
        }
    }

    fn wait_closed(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        loop {
            if state.closed {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// A running kernel. Cheap to clone; all clones talk to the same kernel.
/// `Send + Sync`; every method is non-blocking except [`wait_ready`] and
/// [`shutdown_and_wait`]. Dropping the last clone shuts the kernel down.
///
/// [`wait_ready`]: Kernel::wait_ready
/// [`shutdown_and_wait`]: Kernel::shutdown_and_wait
#[derive(Clone)]
pub struct Kernel {
    inner: Arc<Inner>,
}

struct Inner {
    tx: Sender<Input>,
    shared: Arc<Shared>,
    spec: KernelSpec,
    session: String,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Input::Cmd(Cmd::Shutdown));
    }
}

static KERNELS: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("name", &self.inner.spec.name)
            .field("session", &self.inner.session)
            .field("status", &self.status())
            .field("pid", &self.pid())
            .finish()
    }
}

impl Kernel {
    /// Launch a kernel from `spec` with working directory `cwd`.
    ///
    /// Blocks only for the process spawn (a few ms); returns before the
    /// kernel is ready. Readiness is signaled by `Ready` + `Status(Idle)`
    /// events (or [`wait_ready`](Kernel::wait_ready) / [`ready`](Kernel::ready)).
    /// Requests made before then are queued and sent, in order, once ready.
    pub fn launch(spec: &KernelSpec, cwd: &Path) -> Result<(Kernel, Receiver<KernelEvent>)> {
        Self::launch_with(spec, cwd, LaunchOptions::default())
    }

    pub fn launch_with(
        spec: &KernelSpec,
        cwd: &Path,
        opts: LaunchOptions,
    ) -> Result<(Kernel, Receiver<KernelEvent>)> {
        if spec.argv.is_empty() {
            bail!("kernelspec `{}` has an empty argv", spec.name);
        }
        let session = util::new_id();
        let ctx = Ctx {
            spec: spec.clone(),
            cwd: cwd.to_path_buf(),
            opts,
            key: util::new_key(),
            session: session.clone(),
        };
        let (tx, rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let g = spawn_generation(&ctx, 1, 1, &tx)?;
        let pid = g.pid;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: KernelStatus::Starting,
                closed: false,
                pid: Some(pid),
                info: None,
                timing: None,
                connection_file: g.conn_file.clone(),
                dead_reason: None,
                restarts_requested: 0,
                restarts_handled: 0,
            }),
            changed: Condvar::new(),
            event: event_listener::Event::new(),
        });
        let _ = events_tx.try_send(KernelEvent::Status(KernelStatus::Starting));

        let thread_shared = shared.clone();
        let thread_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("herse-kernel-{}", spec.name))
            .spawn(move || run_actor(ctx, thread_shared, thread_tx, rx, events_tx, g));
        if let Err(e) = spawned {
            process::kill(pid);
            return Err(e).context("failed to start kernel thread");
        }

        let inner = Arc::new(Inner {
            tx,
            shared,
            spec: spec.clone(),
            session,
        });
        let mut kernels = KERNELS.lock().unwrap_or_else(|e| e.into_inner());
        kernels.retain(|k| k.strong_count() > 0);
        kernels.push(Arc::downgrade(&inner));
        drop(kernels);
        Ok((Kernel { inner }, events_rx))
    }

    fn send(&self, cmd: Cmd) -> bool {
        self.inner.tx.try_send(Input::Cmd(cmd)).is_ok()
    }

    /// Run `code` (`execute_request` with `allow_stdin` and `stop_on_error`).
    /// Returns the request's `msg_id`, which is the `parent_msg_id` of every
    /// event it produces.
    pub fn execute(&self, code: &str) -> String {
        self.execute_with(code, ExecuteOptions::default())
    }

    pub fn execute_with(&self, code: &str, opts: ExecuteOptions) -> String {
        let msg_id = util::new_id();
        self.send(Cmd::Execute {
            msg_id: msg_id.clone(),
            code: code.to_owned(),
            opts,
        });
        msg_id
    }

    /// Answer the pending [`KernelEvent::InputRequest`]. Ignored if none.
    pub fn input_reply(&self, value: &str) {
        self.send(Cmd::InputReply {
            value: value.to_owned(),
        });
    }

    /// Interrupt the running execution: SIGINT to the kernel's process group
    /// (`interrupt_mode: signal`) or `interrupt_request` on control
    /// (`message`). Before the kernel is ready this drops queued executions
    /// (each gets an `aborted` reply), mirroring `stop_on_error`.
    pub fn interrupt(&self) -> Result<()> {
        let state = self.inner.shared.lock();
        if state.closed {
            bail!("kernel was shut down");
        }
        if state.status == KernelStatus::Dead {
            bail!("kernel is dead");
        }
        drop(state);
        if !self.send(Cmd::Interrupt) {
            bail!("kernel was shut down");
        }
        Ok(())
    }

    /// Restart: outstanding executions get `aborted` replies, pending
    /// completion/inspection fail, the old process is asked to shut down
    /// (SIGKILL after `restart_grace`) while a fresh one boots in parallel.
    /// Events: `Status(Restarting)` ... `Ready` + `Status(Idle)`. Requests
    /// made after this call are queued for the new process. Also revives a
    /// dead kernel. Non-blocking.
    pub fn restart(&self) -> Result<()> {
        let mut state = self.inner.shared.lock();
        if state.closed {
            bail!("kernel was shut down");
        }
        state.restarts_requested += 1;
        state.status = KernelStatus::Restarting;
        drop(state);
        if !self.send(Cmd::Restart) {
            bail!("kernel was shut down");
        }
        Ok(())
    }

    /// Shut down: `shutdown_request` on control, SIGKILL to the process
    /// group after `shutdown_grace`. Non-blocking; ends with `Status(Dead)`
    /// and the event channel closing.
    pub fn shutdown(&self) {
        self.send(Cmd::Shutdown);
    }

    /// [`shutdown`](Kernel::shutdown) and block until the process is gone or
    /// `timeout` passes (then SIGKILL). Returns whether it exited in time.
    pub fn shutdown_and_wait(&self, timeout: Duration) -> bool {
        self.shutdown();
        let done = self.inner.shared.wait_closed(Instant::now() + timeout);
        if !done && let Some(pid) = self.pid() {
            process::kill(pid);
        }
        done
    }

    /// `complete_request`. `cursor` is a Unicode code point offset
    /// ([`byte_to_cursor_pos`](crate::byte_to_cursor_pos) converts).
    pub fn complete(&self, code: &str, cursor: usize) -> Receiver<Result<CompleteReply>> {
        let (tx, rx) = async_channel::bounded(1);
        self.request_raw(
            "complete_request",
            json!({ "code": code, "cursor_pos": cursor }),
            ReplyTx::Complete(tx),
        );
        rx
    }

    /// `inspect_request` with `detail_level` 0 (docstring / signature).
    pub fn inspect(&self, code: &str, cursor: usize) -> Receiver<Result<InspectReply>> {
        self.inspect_detail(code, cursor, 0)
    }

    /// `inspect_request`; `detail_level` 1 includes source (IPython's `??`).
    pub fn inspect_detail(
        &self,
        code: &str,
        cursor: usize,
        detail_level: u8,
    ) -> Receiver<Result<InspectReply>> {
        let (tx, rx) = async_channel::bounded(1);
        self.request_raw(
            "inspect_request",
            json!({ "code": code, "cursor_pos": cursor, "detail_level": detail_level }),
            ReplyTx::Inspect(tx),
        );
        rx
    }

    /// `is_complete_request`: should Enter submit or insert a newline?
    pub fn is_complete(&self, code: &str) -> Receiver<Result<IsCompleteReply>> {
        let (tx, rx) = async_channel::bounded(1);
        self.request_raw(
            "is_complete_request",
            json!({ "code": code }),
            ReplyTx::IsComplete(tx),
        );
        rx
    }

    /// A shell message that gets no reply (`comm_msg`, `comm_open`,
    /// `comm_close`: a widget frontend talking to its model). Returns its
    /// `msg_id`, the `parent_msg_id` of the outputs it causes; its own
    /// busy/idle status is filtered out like an internal request's.
    pub fn send_shell(&self, msg_type: &str, content: Value) -> String {
        let msg_id = util::new_id();
        self.send(Cmd::Send {
            msg_id: msg_id.clone(),
            msg_type: msg_type.to_owned(),
            content: serde_json::to_vec(&content).expect("JSON value serializes"),
        });
        msg_id
    }

    /// Any other shell request (`history_request`, `comm_info_request`, ...);
    /// resolves with the reply's type and content.
    pub fn request(&self, msg_type: &str, content: Value) -> Receiver<Result<Reply>> {
        let (tx, rx) = async_channel::bounded(1);
        self.request_raw(msg_type, content, ReplyTx::Raw(tx));
        rx
    }

    fn request_raw(&self, msg_type: &str, content: Value, reply: ReplyTx) {
        let content = serde_json::to_vec(&content).expect("JSON value serializes");
        let cmd = Cmd::Request {
            msg_type: msg_type.to_owned(),
            content,
            reply,
        };
        if let Err(async_channel::TrySendError::Closed(Input::Cmd(Cmd::Request {
            reply, ..
        }))) = self.inner.tx.try_send(Input::Cmd(cmd))
        {
            reply.fail(anyhow!("kernel was shut down"));
        }
    }

    /// The `kernel_info_reply` of the current process (`None` until ready).
    pub fn info(&self) -> Option<KernelInfo> {
        self.inner.shared.lock().info.clone()
    }

    pub fn status(&self) -> KernelStatus {
        self.inner.shared.lock().status
    }

    /// Pid of the current kernel process (its process group id on Unix).
    pub fn pid(&self) -> Option<u32> {
        self.inner.shared.lock().pid
    }

    /// Timing of the last completed (re)launch.
    pub fn timing(&self) -> Option<LaunchTiming> {
        self.inner.shared.lock().timing
    }

    pub fn spec(&self) -> &KernelSpec {
        &self.inner.spec
    }

    /// The Jupyter session id in our message headers (stable across restarts).
    pub fn session_id(&self) -> &str {
        &self.inner.session
    }

    /// Connection file of the current process (e.g. for `jupyter console
    /// --existing <file>`). Changes on restart.
    pub fn connection_file(&self) -> PathBuf {
        self.inner.shared.lock().connection_file.clone()
    }

    /// Block until the kernel is ready, dead, or `timeout` passes.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.shared.lock();
        loop {
            match Shared::readiness(&state) {
                Readiness::Ready => return Ok(()),
                Readiness::Failed(reason) => bail!("{reason}"),
                Readiness::Pending => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("kernel not ready after {timeout:?}");
            }
            state = self
                .inner
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Resolves when the kernel is ready (or fails if it dies first). Works
    /// on any executor.
    pub async fn ready(&self) -> Result<()> {
        loop {
            let listener = self.inner.shared.event.listen();
            match Shared::readiness(&self.inner.shared.lock()) {
                Readiness::Ready => return Ok(()),
                Readiness::Failed(reason) => bail!("{reason}"),
                Readiness::Pending => {}
            }
            listener.await;
        }
    }
}

/// Gracefully shut down every kernel this process launched, waiting up to
/// `timeout` in total, then SIGKILL whatever is left. Call on app quit.
pub fn shutdown_all(timeout: Duration) {
    let kernels: Vec<Arc<Inner>> = KERNELS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for k in &kernels {
        let _ = k.tx.try_send(Input::Cmd(Cmd::Shutdown));
    }
    let deadline = Instant::now() + timeout;
    for k in &kernels {
        k.shared.wait_closed(deadline);
    }
    drop(kernels);
    process::kill_all();
}

// ---------------------------------------------------------------------------
// Actor inputs
// ---------------------------------------------------------------------------

enum Cmd {
    Execute {
        msg_id: String,
        code: String,
        opts: ExecuteOptions,
    },
    Request {
        msg_type: String,
        content: Vec<u8>,
        reply: ReplyTx,
    },
    InputReply {
        value: String,
    },
    Send {
        msg_id: String,
        msg_type: String,
        content: Vec<u8>,
    },
    Interrupt,
    Restart,
    Shutdown,
}

enum ReplyTx {
    Complete(Sender<Result<CompleteReply>>),
    Inspect(Sender<Result<InspectReply>>),
    IsComplete(Sender<Result<IsCompleteReply>>),
    Raw(Sender<Result<Reply>>),
}

impl ReplyTx {
    fn fail(self, error: anyhow::Error) {
        match self {
            ReplyTx::Complete(tx) => drop(tx.try_send(Err(error))),
            ReplyTx::Inspect(tx) => drop(tx.try_send(Err(error))),
            ReplyTx::IsComplete(tx) => drop(tx.try_send(Err(error))),
            ReplyTx::Raw(tx) => drop(tx.try_send(Err(error))),
        }
    }

    fn resolve(self, msg: wire::Message) {
        match self {
            ReplyTx::Complete(tx) => drop(tx.try_send(CompleteReply::from_content(msg.content))),
            ReplyTx::Inspect(tx) => drop(tx.try_send(InspectReply::from_content(msg.content))),
            ReplyTx::IsComplete(tx) => {
                drop(tx.try_send(IsCompleteReply::from_content(msg.content)))
            }
            ReplyTx::Raw(tx) => drop(tx.try_send(Ok(Reply {
                msg_type: msg.msg_type,
                content: msg.content,
            }))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chan {
    Shell,
    Iopub,
    Stdin,
    Control,
}

struct Socks {
    iopub: zmtp::Conn,
    shell: zmtp::Conn,
    stdin: zmtp::Conn,
    control: zmtp::Conn,
}

enum Input {
    Cmd(Cmd),
    Connected {
        g: u64,
        socks: Box<Socks>,
    },
    ConnectFailed {
        g: u64,
        error: String,
    },
    Msg {
        g: u64,
        chan: Chan,
        msg: wire::Message,
    },
    Closed {
        g: u64,
        chan: Chan,
        error: String,
    },
    Exited {
        g: u64,
        status: String,
    },
    HeartbeatFailed {
        g: u64,
        missed: u32,
    },
}

// ---------------------------------------------------------------------------
// Process generations
// ---------------------------------------------------------------------------

/// Immutable launch context.
struct Ctx {
    spec: KernelSpec,
    cwd: PathBuf,
    opts: LaunchOptions,
    key: String,
    session: String,
}

/// One kernel process and its connections. A restart creates a new one.
struct Gen {
    id: u64,
    attempt: u32,
    pid: u32,
    conn: ConnectionInfo,
    conn_file: PathBuf,
    spawned_at: Instant,
    spawn_time: Duration,
    tail: Arc<Mutex<process::Tail>>,
    /// Startup deadline (until ready).
    deadline: Instant,
    connect_error: Option<String>,
    connect_task: Option<Task<()>>,
    io: Option<GenIo>,
    connected_at: Option<Instant>,
    probes: HashSet<String>,
    probe_interval: Duration,
    next_probe: Option<Instant>,
    info: Option<KernelInfo>,
    iopub_seen: bool,
}

impl Gen {
    fn tail(&self) -> String {
        self.tail.lock().unwrap_or_else(|e| e.into_inner()).text(25)
    }
}

struct GenIo {
    shell: Sender<Vec<u8>>,
    control: Sender<Vec<u8>>,
    stdin: Sender<Vec<u8>>,
    _tasks: Vec<Task<()>>,
    heartbeat: Option<Task<()>>,
}

impl GenIo {
    fn send(&self, chan: Chan, bytes: Vec<u8>) {
        let tx = match chan {
            Chan::Shell => &self.shell,
            Chan::Control => &self.control,
            Chan::Stdin => &self.stdin,
            Chan::Iopub => return,
        };
        let _ = tx.try_send(bytes);
    }
}

/// A process we've asked to exit (shutdown or restart) and are waiting on.
struct Retiring {
    g: Gen,
    kill_at: Instant,
    killed_at: Option<Instant>,
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "herse".into())
}

fn kernel_env(ctx: &Ctx) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ctx
        .spec
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                // `${PATH}` means the PATH the kernel would otherwise get:
                // the login shell's, for an app started from Finder.
                spec::expand_env_value(v, |name| {
                    crate::shell_env::var_os(name).map(|v| v.to_string_lossy().into_owned())
                }),
            )
        })
        .collect();
    env.sort();
    // ipykernel polls for reparenting and exits when we're gone.
    env.push(("JPY_PARENT_PID".into(), std::process::id().to_string()));
    // Silences debugpy's "frozen modules" warning on Python >= 3.11.
    if crate::shell_env::var_os("PYDEVD_DISABLE_FILE_VALIDATION").is_none()
        && !ctx.spec.env.contains_key("PYDEVD_DISABLE_FILE_VALIDATION")
    {
        env.push(("PYDEVD_DISABLE_FILE_VALIDATION".into(), "1".into()));
    }
    env.extend(ctx.opts.env.iter().cloned());
    env
}

/// Reserve ports, write the connection file, spawn the process.
fn spawn_generation(ctx: &Ctx, id: u64, attempt: u32, tx: &Sender<Input>) -> Result<Gen> {
    let ports = connection::reserve_ports().context("reserving ports for the kernel")?;
    let conn = ConnectionInfo::new(&ctx.spec.name, &ctx.key, ports);
    let file_name = format!("kernel-{}-{id}.json", ctx.session);
    let conn_file = match connection::write_connection_file(&conn, &file_name) {
        Ok(path) => path,
        Err(e) => {
            connection::release_ports(ports);
            return Err(e).context("writing the kernel connection file");
        }
    };
    let conn_file_str = conn_file.to_string_lossy();
    let resource_dir = ctx.spec.resource_dir.to_string_lossy();
    let argv: Vec<String> = ctx
        .spec
        .argv
        .iter()
        .map(|a| {
            a.replace("{connection_file}", &conn_file_str)
                .replace("{resource_dir}", &resource_dir)
        })
        .collect();
    let env = kernel_env(ctx);
    let exit_tx = tx.clone();
    // Reading the login shell's environment (a Finder launch, once) is not
    // the kernel's startup time: finish it before the clock starts.
    let _ = crate::shell_env::get();
    let spawned_at = Instant::now();
    let spawned = process::spawn(
        &argv,
        &env,
        &ctx.cwd,
        &ctx.spec.name,
        &conn_file,
        move |status| {
            let _ = exit_tx.send_blocking(Input::Exited { g: id, status });
        },
    );
    let spawned = match spawned {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&conn_file);
            connection::release_ports(ports);
            // Where to fix it: the kernelspec that named the program.
            return Err(e.context(format!(
                "kernelspec {}",
                ctx.spec.resource_dir.join("kernel.json").display()
            )));
        }
    };
    log::debug!(
        "kernel {} generation {id} spawned pid {} in {:?}",
        ctx.spec.name,
        spawned.pid,
        spawned.spawn_time
    );
    Ok(Gen {
        id,
        attempt,
        pid: spawned.pid,
        conn,
        conn_file,
        spawned_at,
        spawn_time: spawned.spawn_time,
        tail: spawned.tail,
        deadline: spawned_at + ctx.opts.startup_timeout,
        connect_error: None,
        connect_task: None,
        io: None,
        connected_at: None,
        probes: HashSet::new(),
        probe_interval: PROBE_FIRST,
        next_probe: None,
        info: None,
        iopub_seen: false,
    })
}

// ---------------------------------------------------------------------------
// Socket tasks
// ---------------------------------------------------------------------------

async fn connect_all(
    conn: &ConnectionInfo,
    identity: &[u8],
    deadline: Instant,
) -> io::Result<Socks> {
    let addr = ConnectionInfo::addr;
    // IOPub first and subscribed before any request goes out, so the
    // kernel's replies to our first probe have the best chance of being seen.
    let mut iopub = zmtp::connect(addr(conn.iopub_port), SocketType::Sub, None, deadline).await?;
    iopub.writer.write_all(&zmtp::subscribe_all()).await?;
    let id = Some(identity);
    // shell, stdin, control share one routing identity: ipykernel routes
    // `input_request` on stdin to the identity that sent the execute on shell.
    let shell = zmtp::connect(addr(conn.shell_port), SocketType::Dealer, id, deadline).await?;
    let stdin = zmtp::connect(addr(conn.stdin_port), SocketType::Dealer, id, deadline).await?;
    let control = zmtp::connect(addr(conn.control_port), SocketType::Dealer, id, deadline).await?;
    Ok(Socks {
        iopub,
        shell,
        stdin,
        control,
    })
}

async fn read_loop(
    mut reader: BufReader<TcpStream>,
    chan: Chan,
    g: u64,
    signer: Signer,
    tx: Sender<Input>,
    writer: Sender<Vec<u8>>,
) {
    loop {
        match zmtp::read(&mut reader).await {
            Ok(zmtp::Incoming::Message(frames)) => match wire::decode(frames, &signer) {
                Ok(msg) => {
                    if tx.send(Input::Msg { g, chan, msg }).await.is_err() {
                        return;
                    }
                }
                Err(e) => log::warn!("dropping {chan:?} message: {e}"),
            },
            Ok(zmtp::Incoming::Command { name, body }) => {
                if let Some(answer) = zmtp::answer_command(&name, &body) {
                    let _ = writer.try_send(answer);
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Input::Closed {
                        g,
                        chan,
                        error: e.to_string(),
                    })
                    .await;
                return;
            }
        }
    }
}

async fn write_loop(mut writer: TcpStream, rx: Receiver<Vec<u8>>) {
    while let Ok(mut buf) = rx.recv().await {
        // Coalesce bursts (e.g. 1,000 queued executes) into few syscalls.
        while buf.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(more) => buf.extend_from_slice(&more),
                Err(_) => break,
            }
        }
        if writer.write_all(&buf).await.is_err() {
            return;
        }
    }
}

async fn heartbeat(
    addr: SocketAddr,
    cfg: HeartbeatConfig,
    g: u64,
    tx: Sender<Input>,
    ex: Rc<LocalExecutor<'static>>,
) {
    let conn = match zmtp::connect(
        addr,
        SocketType::Dealer,
        None,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("heartbeat channel unavailable, relying on process exit only: {e}");
            return;
        }
    };
    let zmtp::Conn {
        mut reader,
        mut writer,
    } = conn;
    let (echo_tx, echo_rx) = async_channel::unbounded::<Vec<Vec<u8>>>();
    let _reader = ex.spawn(async move {
        while let Ok(incoming) = zmtp::read(&mut reader).await {
            if let zmtp::Incoming::Message(frames) = incoming
                && echo_tx.send(frames).await.is_err()
            {
                return;
            }
        }
    });
    let mut missed = 0u32;
    let mut n = 0u64;
    loop {
        Timer::after(cfg.interval).await;
        n += 1;
        let ping = format!("ping {n}");
        let mut buf = Vec::with_capacity(32);
        // REQ envelope (empty delimiter frame), so REP and ROUTER-echo
        // heartbeats both answer.
        zmtp::encode_message(&mut buf, &[b"", ping.as_bytes()]);
        if writer.write_all(&buf).await.is_err() {
            return;
        }
        let deadline = Instant::now() + cfg.timeout;
        let answered = loop {
            match with_deadline(echo_rx.recv(), deadline).await {
                Some(Ok(frames)) => {
                    if frames.last().is_some_and(|f| f == ping.as_bytes()) {
                        break true;
                    }
                }
                // Connection closed; process exit is reported by the watcher.
                Some(Err(_)) => return,
                None => break false,
            }
        };
        if answered {
            missed = 0;
        } else {
            missed += 1;
            log::warn!("kernel missed heartbeat {missed}/{}", cfg.max_missed);
            if missed >= cfg.max_missed {
                let _ = tx.send(Input::HeartbeatFailed { g, missed }).await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The actor
// ---------------------------------------------------------------------------

struct Exec {
    seq: u64,
    idle: bool,
    reply: Option<ExecReply>,
    reply_at: Option<Instant>,
}

struct ExecReply {
    status: String,
    execution_count: Option<i64>,
    payload: Vec<Value>,
}

struct Queued {
    chan: Chan,
    bytes: Vec<u8>,
    exec: Option<String>,
}

#[derive(Serialize)]
struct ExecuteRequest<'a> {
    code: &'a str,
    silent: bool,
    store_history: bool,
    user_expressions: &'a Map<String, Value>,
    allow_stdin: bool,
    stop_on_error: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecState {
    Busy,
    Idle,
    Other,
}

struct Actor {
    ctx: Ctx,
    shared: Arc<Shared>,
    ex: Rc<LocalExecutor<'static>>,
    tx: Sender<Input>,
    rx: Receiver<Input>,
    events: Sender<KernelEvent>,
    signer: Signer,
    next_gen: u64,
    cur: Option<Gen>,
    retiring: Vec<Retiring>,
    /// The current generation answered kernel_info, IOPub is live, and no
    /// old process is still exiting: requests go straight out.
    ready: bool,
    /// Died and not restarted: requests fail immediately.
    dead: bool,
    shutting_down: bool,
    finished: bool,
    status: KernelStatus,
    /// Requests made while not ready, in order.
    queue: VecDeque<Queued>,
    execs: HashMap<String, Exec>,
    /// Executions whose reply arrived before their IOPub idle, by sequence.
    stashed: BTreeMap<u64, String>,
    exec_seq: u64,
    /// Highest execution sequence number seen on IOPub. IOPub is FIFO, so
    /// any execution below it has published everything it will publish.
    iopub_seq: u64,
    requests: HashMap<String, ReplyTx>,
    /// Requests whose IOPub status messages we swallow; `true` = internal
    /// (swallow everything parented to it).
    quiet: HashMap<String, bool>,
    quiet_order: VecDeque<String>,
    /// Raw header of the pending input_request (parent of our input_reply).
    input_parent: Option<Vec<u8>>,
    input_exec: Option<String>,
}

fn run_actor(
    ctx: Ctx,
    shared: Arc<Shared>,
    tx: Sender<Input>,
    rx: Receiver<Input>,
    events: Sender<KernelEvent>,
    g: Gen,
) {
    let ex = Rc::new(LocalExecutor::new());
    let signer = Signer::new(ctx.key.as_bytes(), ctx.session.clone(), username());
    let mut actor = Actor {
        ctx,
        shared,
        ex: ex.clone(),
        tx,
        rx,
        events,
        signer,
        next_gen: g.id + 1,
        cur: Some(g),
        retiring: Vec::new(),
        ready: false,
        dead: false,
        shutting_down: false,
        finished: false,
        status: KernelStatus::Starting,
        queue: VecDeque::new(),
        execs: HashMap::new(),
        stashed: BTreeMap::new(),
        exec_seq: 0,
        iopub_seq: 0,
        requests: HashMap::new(),
        quiet: HashMap::new(),
        quiet_order: VecDeque::new(),
        input_parent: None,
        input_exec: None,
    };
    actor.start_connect();
    smol::block_on(ex.run(actor.run()));
}

impl Drop for Actor {
    /// Normally a no-op (shutdown already reaped everything). If the actor
    /// panicked, make sure no kernel process outlives it.
    fn drop(&mut self) {
        if let Some(g) = &self.cur {
            process::kill(g.pid);
        }
        for r in &self.retiring {
            process::kill(r.g.pid);
        }
        if !self.finished {
            log::error!("kernel {} actor stopped unexpectedly", self.ctx.spec.name);
            self.emit(KernelEvent::Status(KernelStatus::Dead));
            self.emit(KernelEvent::Died {
                reason: "internal error in the kernel client".into(),
            });
            self.shared.update(|s| {
                s.status = KernelStatus::Dead;
                s.closed = true;
                s.pid = None;
            });
        }
    }
}

impl Actor {
    async fn run(mut self) {
        while !self.finished {
            let now = Instant::now();
            let next = self.next_deadline();
            if next.is_some_and(|d| d <= now) {
                self.on_timer(now);
                continue;
            }
            let input = match next {
                Some(deadline) => with_deadline(self.rx.recv(), deadline).await,
                None => Some(self.rx.recv().await),
            };
            match input {
                Some(Ok(input)) => self.handle(input),
                Some(Err(_)) => break,
                None => self.on_timer(Instant::now()),
            }
        }
        // `Drop` cleans up anything still around; tasks are cancelled.
    }

    fn emit(&self, event: KernelEvent) {
        let _ = self.events.try_send(event);
    }

    fn set_status(&mut self, status: KernelStatus) {
        if self.status == status {
            return;
        }
        self.status = status;
        self.shared.update(|s| s.status = status);
        self.emit(KernelEvent::Status(status));
    }

    fn mark_quiet(&mut self, id: String, internal: bool) {
        if self.quiet_order.len() >= 4096
            && let Some(old) = self.quiet_order.pop_front()
        {
            self.quiet.remove(&old);
        }
        self.quiet_order.push_back(id.clone());
        self.quiet.insert(id, internal);
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Cmd(cmd) => self.on_cmd(cmd),
            Input::Connected { g, socks } => self.on_connected(g, *socks),
            Input::ConnectFailed { g, error } => self.on_connect_failed(g, error),
            Input::Msg {
                g: gen_id,
                chan,
                msg,
            } => {
                if self.cur.as_ref().is_some_and(|cur| cur.id == gen_id) {
                    match chan {
                        Chan::Iopub => self.on_iopub(msg),
                        Chan::Shell | Chan::Control => self.on_reply(msg),
                        Chan::Stdin => self.on_stdin(msg),
                    }
                }
            }
            Input::Closed { g, chan, error } => {
                log::debug!("kernel generation {g} {chan:?} channel closed: {error}");
            }
            Input::Exited { g, status } => self.on_exited(g, status),
            Input::HeartbeatFailed { g, missed } => self.on_heartbeat_failed(g, missed),
        }
    }

    // -- lifecycle ----------------------------------------------------------

    fn start_connect(&mut self) {
        let Some(g) = &mut self.cur else { return };
        let tx = self.tx.clone();
        let id = g.id;
        let conn = g.conn.clone();
        let deadline = g.deadline;
        let identity = self.ctx.session.clone().into_bytes();
        g.connect_task = Some(self.ex.spawn(async move {
            let input = match connect_all(&conn, &identity, deadline).await {
                Ok(socks) => Input::Connected {
                    g: id,
                    socks: Box::new(socks),
                },
                Err(e) => Input::ConnectFailed {
                    g: id,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(input).await;
        }));
    }

    fn spawn_next(&mut self, attempt: u32) -> Result<()> {
        let id = self.next_gen;
        self.next_gen += 1;
        let g = spawn_generation(&self.ctx, id, attempt, &self.tx)?;
        let (pid, file) = (g.pid, g.conn_file.clone());
        self.shared.update(|s| {
            s.pid = Some(pid);
            s.connection_file = file;
        });
        self.cur = Some(g);
        self.start_connect();
        Ok(())
    }

    fn on_connected(&mut self, gen_id: u64, socks: Socks) {
        let Some(g) = self.cur.as_mut().filter(|g| g.id == gen_id) else {
            return;
        };
        g.connect_task = None;
        g.connected_at = Some(Instant::now());
        let mut tasks = Vec::with_capacity(8);
        let mut writers = Vec::with_capacity(4);
        for (chan, conn) in [
            (Chan::Iopub, socks.iopub),
            (Chan::Shell, socks.shell),
            (Chan::Stdin, socks.stdin),
            (Chan::Control, socks.control),
        ] {
            let (wtx, wrx) = async_channel::unbounded();
            tasks.push(self.ex.spawn(write_loop(conn.writer, wrx)));
            tasks.push(self.ex.spawn(read_loop(
                conn.reader,
                chan,
                gen_id,
                self.signer.clone(),
                self.tx.clone(),
                wtx.clone(),
            )));
            writers.push(wtx);
        }
        let control = writers.pop().expect("four writers");
        let stdin = writers.pop().expect("four writers");
        let shell = writers.pop().expect("four writers");
        g.io = Some(GenIo {
            shell,
            control,
            stdin,
            _tasks: tasks,
            heartbeat: None,
        });
        self.send_probe();
    }

    fn on_connect_failed(&mut self, gen_id: u64, error: String) {
        let Some(g) = self.cur.as_mut().filter(|g| g.id == gen_id) else {
            return;
        };
        log::warn!(
            "could not connect to kernel {}: {error}",
            self.ctx.spec.name
        );
        g.connect_task = None;
        // The exit status (if the process is dying) explains more; give it a
        // moment, then the startup deadline path reports this error.
        g.connect_error = Some(error);
        g.deadline = g.deadline.min(Instant::now() + CONNECT_ERROR_GRACE);
    }

    fn send_probe(&mut self) {
        let id = util::new_id();
        let bytes = self.signer.encode("kernel_info_request", &id, b"{}", b"{}");
        self.mark_quiet(id.clone(), true);
        let Some(g) = &mut self.cur else { return };
        let Some(io) = &g.io else { return };
        io.send(Chan::Shell, bytes);
        g.probes.insert(id);
        g.next_probe = Some(Instant::now() + g.probe_interval);
        g.probe_interval = (g.probe_interval * 2).min(PROBE_MAX);
    }

    fn maybe_ready(&mut self) {
        if self.ready || self.shutting_down || !self.retiring.is_empty() {
            return;
        }
        let Some(g) = self.cur.as_mut() else { return };
        if g.info.is_none() || !g.iopub_seen || g.io.is_none() {
            return;
        }
        let now = Instant::now();
        g.next_probe = None;
        let timing = LaunchTiming {
            spawn: g.spawn_time,
            connect: g.connected_at.map_or(Duration::ZERO, |t| t - g.spawned_at),
            ready: now - g.spawned_at,
            attempt: g.attempt,
        };
        let info = g.info.clone().expect("checked above");
        if let Some(cfg) = self.ctx.opts.heartbeat {
            let addr = ConnectionInfo::addr(g.conn.hb_port);
            let task = self
                .ex
                .spawn(heartbeat(addr, cfg, g.id, self.tx.clone(), self.ex.clone()));
            if let Some(io) = &mut g.io {
                io.heartbeat = Some(task);
            }
        }
        let (pid, file) = (g.pid, g.conn_file.clone());
        log::debug!("kernel {} ready in {:?}", self.ctx.spec.name, timing.ready);
        self.ready = true;
        self.shared.update(|s| {
            s.info = Some(info.clone());
            s.timing = Some(timing);
            s.pid = Some(pid);
            s.connection_file = file;
            s.dead_reason = None;
        });
        self.emit(KernelEvent::Ready {
            info: Box::new(info),
            timing,
        });
        self.set_status(KernelStatus::Idle);
        let queued = std::mem::take(&mut self.queue);
        if let Some(io) = self.cur.as_ref().and_then(|g| g.io.as_ref()) {
            for q in queued {
                io.send(q.chan, q.bytes);
            }
        }
    }

    /// Ask a process to exit (if we can talk to it) and track it until reaped.
    fn retire(&mut self, mut g: Gen, grace: Duration, restart: bool) {
        g.next_probe = None;
        g.connect_task = None;
        let now = Instant::now();
        let kill_at = match &mut g.io {
            Some(io) => {
                io.heartbeat = None;
                let id = util::new_id();
                let content: &[u8] = if restart {
                    br#"{"restart":true}"#
                } else {
                    br#"{"restart":false}"#
                };
                io.send(
                    Chan::Control,
                    self.signer.encode("shutdown_request", &id, b"{}", content),
                );
                now + grace
            }
            // Not connected yet: nobody to ask.
            None => now,
        };
        self.retiring.push(Retiring {
            g,
            kill_at,
            killed_at: None,
        });
    }

    fn release(&mut self, g: Gen) {
        connection::release_ports(g.conn.ports());
        drop(g);
    }

    fn die(&mut self, reason: String) {
        log::warn!("kernel {} died: {reason}", self.ctx.spec.name);
        self.ready = false;
        self.dead = true;
        let r = reason.clone();
        self.shared.update(|s| {
            s.dead_reason = Some(r);
            s.pid = None;
        });
        self.set_status(KernelStatus::Dead);
        self.emit(KernelEvent::Died { reason });
        self.abort_all("kernel died");
    }

    /// Fail everything in flight: executions get (real or `aborted`)
    /// replies, other requests get errors.
    fn abort_all(&mut self, reason: &str) {
        let mut ids: Vec<(u64, String)> = self
            .execs
            .iter()
            .map(|(id, e)| (e.seq, id.clone()))
            .collect();
        ids.sort_unstable();
        for (_, id) in ids {
            self.finalize(&id);
        }
        for (_, reply) in self.requests.drain() {
            reply.fail(anyhow!("{reason}"));
        }
        self.queue.clear();
        self.stashed.clear();
        self.iopub_seq = 0;
        self.input_parent = None;
        self.input_exec = None;
        self.quiet.clear();
        self.quiet_order.clear();
    }

    fn maybe_finish(&mut self) {
        if !self.shutting_down || self.finished || self.cur.is_some() || !self.retiring.is_empty() {
            return;
        }
        self.finished = true;
        self.set_status(KernelStatus::Dead);
        self.shared.update(|s| {
            s.closed = true;
            s.pid = None;
        });
    }

    fn on_exited(&mut self, gen_id: u64, status: String) {
        if let Some(i) = self.retiring.iter().position(|r| r.g.id == gen_id) {
            let r = self.retiring.swap_remove(i);
            self.release(r.g);
            self.maybe_ready();
            self.maybe_finish();
            return;
        }
        let Some(g) = self.cur.take_if(|g| g.id == gen_id) else {
            return;
        };
        let was_ready = self.ready;
        self.ready = false;
        let tail = g.tail();
        let attempt = g.attempt;
        self.release(g);
        if self.shutting_down {
            self.maybe_finish();
            return;
        }
        if !was_ready
            && attempt < MAX_ATTEMPTS
            && (tail.contains("Address already in use") || tail.contains("EADDRINUSE"))
        {
            log::info!(
                "kernel port collision; relaunching (attempt {})",
                attempt + 1
            );
            if let Err(e) = self.spawn_next(attempt + 1) {
                self.die(format!("Failed to relaunch kernel: {e:#}"));
            }
            return;
        }
        let what = if was_ready {
            "Kernel died"
        } else {
            "Kernel exited during startup"
        };
        let reason = if tail.is_empty() {
            format!("{what} ({status})")
        } else {
            format!("{what} ({status}):\n{tail}")
        };
        self.die(reason);
    }

    fn on_heartbeat_failed(&mut self, gen_id: u64, missed: u32) {
        if !self.ready || !self.cur.as_ref().is_some_and(|g| g.id == gen_id) {
            return;
        }
        let g = self.cur.take().expect("checked above");
        process::kill(g.pid);
        let now = Instant::now();
        self.retiring.push(Retiring {
            g,
            kill_at: now,
            killed_at: Some(now),
        });
        self.die(format!(
            "Kernel stopped responding to heartbeats ({missed} missed); it was killed"
        ));
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        if !self.ready
            && let Some(g) = &self.cur
        {
            consider(g.deadline);
            if g.io.is_some()
                && let Some(t) = g.next_probe
            {
                consider(t);
            }
        }
        for r in &self.retiring {
            consider(r.killed_at.map_or(r.kill_at, |t| t + KILL_GIVE_UP));
        }
        if let Some((_, id)) = self.stashed.first_key_value()
            && let Some(t) = self.execs.get(id).and_then(|e| e.reply_at)
        {
            consider(t + REPLY_GRACE);
        }
        next
    }

    fn on_timer(&mut self, now: Instant) {
        // Startup: probe, or give up.
        if !self.ready
            && let Some(g) = &self.cur
        {
            if now >= g.deadline {
                let g = self.cur.take().expect("checked above");
                let reason = match &g.connect_error {
                    Some(e) => format!("Could not connect to the kernel: {e}"),
                    None => format!(
                        "Kernel did not become ready within {:.0}s",
                        self.ctx.opts.startup_timeout.as_secs_f64()
                    ),
                };
                let tail = g.tail();
                process::kill(g.pid);
                self.retiring.push(Retiring {
                    g,
                    kill_at: now,
                    killed_at: Some(now),
                });
                if self.shutting_down {
                    return;
                }
                self.die(if tail.is_empty() {
                    reason
                } else {
                    format!("{reason}:\n{tail}")
                });
            } else if g.io.is_some() && g.next_probe.is_some_and(|t| t <= now) {
                self.send_probe();
            }
        }

        // Retiring processes: SIGKILL after their grace; forget unkillable ones.
        let mut changed = false;
        let mut i = 0;
        while i < self.retiring.len() {
            let r = &mut self.retiring[i];
            match r.killed_at {
                None if r.kill_at <= now => {
                    process::kill(r.g.pid);
                    r.killed_at = Some(now);
                }
                Some(t) if now >= t + KILL_GIVE_UP => {
                    log::warn!("kernel pid {} did not exit after SIGKILL", r.g.pid);
                    let r = self.retiring.swap_remove(i);
                    self.release(r.g);
                    changed = true;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        if changed {
            self.maybe_ready();
            self.maybe_finish();
        }

        // Replies whose IOPub idle never came.
        while let Some((_, id)) = self.stashed.first_key_value() {
            let due = self
                .execs
                .get(id)
                .and_then(|e| e.reply_at)
                .is_none_or(|t| t + REPLY_GRACE <= now);
            if !due {
                break;
            }
            let id = id.clone();
            self.finalize(&id);
        }
    }

    // -- commands -----------------------------------------------------------

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Execute { msg_id, code, opts } => self.on_execute(msg_id, code, opts),
            Cmd::Request {
                msg_type,
                content,
                reply,
            } => {
                if self.shutting_down || self.dead {
                    reply.fail(anyhow!("kernel is not running"));
                    return;
                }
                let id = util::new_id();
                let bytes = self.signer.encode(&msg_type, &id, b"{}", &content);
                self.mark_quiet(id.clone(), false);
                self.requests.insert(id, reply);
                self.send(Chan::Shell, bytes, None);
            }
            Cmd::Send {
                msg_id,
                msg_type,
                content,
            } => {
                if self.shutting_down || self.dead {
                    return;
                }
                let bytes = self.signer.encode(&msg_type, &msg_id, b"{}", &content);
                self.mark_quiet(msg_id, false);
                self.send(Chan::Shell, bytes, None);
            }
            Cmd::InputReply { value } => {
                let Some(parent) = self.input_parent.take() else {
                    log::warn!("input_reply with no pending input_request; ignored");
                    return;
                };
                self.input_exec = None;
                let content = serde_json::to_vec(&json!({ "value": value })).expect("serializes");
                let bytes = self
                    .signer
                    .encode("input_reply", &util::new_id(), &parent, &content);
                self.send(Chan::Stdin, bytes, None);
            }
            Cmd::Interrupt => self.on_interrupt(),
            Cmd::Restart => self.on_restart(),
            Cmd::Shutdown => self.on_shutdown(),
        }
    }

    fn send(&mut self, chan: Chan, bytes: Vec<u8>, exec: Option<String>) {
        if self.ready
            && let Some(io) = self.cur.as_ref().and_then(|g| g.io.as_ref())
        {
            io.send(chan, bytes);
        } else {
            self.queue.push_back(Queued { chan, bytes, exec });
        }
    }

    fn on_execute(&mut self, msg_id: String, code: String, opts: ExecuteOptions) {
        if self.shutting_down || self.dead {
            self.emit(KernelEvent::ExecuteReply {
                parent_msg_id: msg_id,
                status: "aborted".into(),
                execution_count: None,
            });
            return;
        }
        let content = serde_json::to_vec(&ExecuteRequest {
            code: &code,
            silent: opts.silent,
            store_history: opts.store_history,
            user_expressions: &opts.user_expressions,
            allow_stdin: opts.allow_stdin,
            stop_on_error: opts.stop_on_error,
        })
        .expect("serializes");
        let bytes = self
            .signer
            .encode("execute_request", &msg_id, b"{}", &content);
        self.exec_seq += 1;
        self.execs.insert(
            msg_id.clone(),
            Exec {
                seq: self.exec_seq,
                idle: false,
                reply: None,
                reply_at: None,
            },
        );
        self.send(Chan::Shell, bytes, Some(msg_id));
    }

    fn on_interrupt(&mut self) {
        if !self.ready {
            // Nothing is running yet. Drop queued executions, as the kernel
            // would after a KeyboardInterrupt with stop_on_error.
            let mut dropped = Vec::new();
            self.queue.retain(|q| match &q.exec {
                Some(id) => {
                    dropped.push(id.clone());
                    false
                }
                None => true,
            });
            for id in dropped {
                self.finalize(&id);
            }
            return;
        }
        let Some(g) = &self.cur else { return };
        let by_message = match self.ctx.spec.interrupt_mode {
            InterruptMode::Message => true,
            InterruptMode::Signal => match process::interrupt(g.pid) {
                Ok(()) => false,
                Err(e) if e.kind() == io::ErrorKind::Unsupported => true,
                Err(e) => {
                    log::warn!("interrupt failed: {e}");
                    false
                }
            },
        };
        if by_message {
            let id = util::new_id();
            let bytes = self.signer.encode("interrupt_request", &id, b"{}", b"{}");
            self.mark_quiet(id, true);
            if let Some(io) = self.cur.as_ref().and_then(|g| g.io.as_ref()) {
                io.send(Chan::Control, bytes);
            }
        }
    }

    fn on_restart(&mut self) {
        self.shared.update(|s| s.restarts_handled += 1);
        if self.shutting_down {
            return;
        }
        self.ready = false;
        self.dead = false;
        // `restart()` already set the shared status; make sure the event goes out.
        self.status = KernelStatus::Idle;
        self.set_status(KernelStatus::Restarting);
        self.abort_all("kernel restarted");
        if let Some(g) = self.cur.take() {
            self.retire(g, self.ctx.opts.restart_grace, true);
        }
        if let Err(e) = self.spawn_next(1) {
            self.die(format!("Failed to restart kernel: {e:#}"));
        }
    }

    fn on_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.ready = false;
        self.abort_all("kernel shut down");
        if let Some(g) = self.cur.take() {
            self.retire(g, self.ctx.opts.shutdown_grace, false);
        }
        self.maybe_finish();
    }

    // -- messages -----------------------------------------------------------

    fn on_iopub(&mut self, msg: wire::Message) {
        if !self.ready
            && let Some(g) = &mut self.cur
        {
            g.iopub_seen = true;
        }
        let state = if msg.msg_type == "status" {
            match msg.content.get("execution_state").and_then(Value::as_str) {
                Some("busy") => ExecState::Busy,
                Some("idle") => ExecState::Idle,
                _ => ExecState::Other,
            }
        } else {
            ExecState::Other
        };
        let is_status = msg.msg_type == "status";

        if let Some(parent) = msg.parent_msg_id.as_deref()
            && let Some(&internal) = self.quiet.get(parent)
        {
            if state == ExecState::Idle {
                self.quiet.remove(parent);
            }
            if internal || is_status {
                self.maybe_ready();
                return;
            }
        }

        let exec = msg
            .parent_msg_id
            .as_deref()
            .and_then(|p| self.execs.get_mut(p));
        let Some(exec) = exec else {
            self.emit(KernelEvent::Iopub {
                parent_msg_id: msg.parent_msg_id,
                msg_type: msg.msg_type,
                content: msg.content,
            });
            self.maybe_ready();
            return;
        };
        let seq = exec.seq;
        let finished = state == ExecState::Idle;
        if finished {
            exec.idle = true;
        }
        let has_reply = exec.reply.is_some();
        self.iopub_seq = self.iopub_seq.max(seq);
        // IOPub is FIFO: traffic for a later execution means earlier ones are
        // done even if their idle was lost.
        self.finalize_stashed_before(seq);
        if state == ExecState::Busy && self.ready {
            self.set_status(KernelStatus::Busy);
        }
        let parent = msg.parent_msg_id.expect("exec has a parent");
        self.emit(KernelEvent::Iopub {
            parent_msg_id: Some(parent.clone()),
            msg_type: msg.msg_type,
            content: msg.content,
        });
        if finished && has_reply {
            self.finalize(&parent);
        }
    }

    fn on_reply(&mut self, msg: wire::Message) {
        let Some(parent) = msg.parent_msg_id.clone() else {
            return;
        };
        if let Some(g) = self.cur.as_mut()
            && g.probes.remove(&parent)
        {
            if msg.msg_type == "kernel_info_reply" && !self.ready {
                if g.info.is_none() {
                    g.info = Some(KernelInfo::from_content(msg.content));
                }
                if !g.iopub_seen {
                    let soon = Instant::now() + PROBE_IOPUB_RETRY;
                    g.next_probe = Some(g.next_probe.map_or(soon, |t| t.min(soon)));
                }
                self.maybe_ready();
            }
            return;
        }
        if msg.msg_type == "execute_reply" {
            let Some(exec) = self.execs.get_mut(&parent) else {
                return;
            };
            let c = &msg.content;
            exec.reply = Some(ExecReply {
                status: c
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("ok")
                    .to_owned(),
                execution_count: c.get("execution_count").and_then(Value::as_i64),
                payload: c
                    .get("payload")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            });
            if exec.idle || exec.seq < self.iopub_seq {
                self.finalize(&parent);
            } else {
                exec.reply_at = Some(Instant::now());
                let seq = exec.seq;
                self.stashed.insert(seq, parent);
            }
            return;
        }
        if let Some(reply) = self.requests.remove(&parent) {
            reply.resolve(msg);
        } else {
            log::trace!("unsolicited {} (parent {parent})", msg.msg_type);
        }
    }

    fn on_stdin(&mut self, msg: wire::Message) {
        if msg.msg_type != "input_request" {
            return;
        }
        let parent = msg.parent_msg_id.unwrap_or_default();
        let prompt = msg
            .content
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let password = msg
            .content
            .get("password")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.input_parent = Some(msg.raw_header);
        self.input_exec = Some(parent.clone());
        self.emit(KernelEvent::InputRequest {
            parent_msg_id: parent,
            prompt,
            password,
        });
    }

    fn finalize_stashed_before(&mut self, seq: u64) {
        while let Some((&s, _)) = self.stashed.first_key_value() {
            if s >= seq {
                break;
            }
            let (_, id) = self.stashed.pop_first().expect("non-empty");
            self.finalize(&id);
        }
    }

    /// Deliver an execution's reply (or `aborted` if it never got one),
    /// after any earlier replies still waiting for their idle: replies are
    /// always delivered in request order.
    fn finalize(&mut self, msg_id: &str) {
        let Some(seq) = self.execs.get(msg_id).map(|e| e.seq) else {
            return;
        };
        self.finalize_stashed_before(seq);
        let Some(exec) = self.execs.remove(msg_id) else {
            return;
        };
        self.stashed.remove(&exec.seq);
        if self.input_exec.as_deref() == Some(msg_id) {
            self.input_exec = None;
            self.input_parent = None;
        }
        let reply = exec.reply.unwrap_or(ExecReply {
            status: "aborted".into(),
            execution_count: None,
            payload: Vec::new(),
        });
        if !reply.payload.is_empty() {
            self.emit(KernelEvent::ExecutePayload {
                parent_msg_id: msg_id.to_owned(),
                payload: reply.payload,
            });
        }
        self.emit(KernelEvent::ExecuteReply {
            parent_msg_id: msg_id.to_owned(),
            status: reply.status,
            execution_count: reply.execution_count,
        });
        if self.ready && self.execs.is_empty() {
            self.set_status(KernelStatus::Idle);
        }
    }
}
