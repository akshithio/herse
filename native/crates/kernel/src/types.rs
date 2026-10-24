//! Public event and reply types.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

/// Coarse kernel state, suitable for a status indicator.
///
/// `Busy`/`Idle` are coalesced over executions: the kernel is `Busy` from the
/// first `execute_request` it starts until no execution we sent is
/// outstanding, so a burst of queued cells produces one `Busy` and one
/// `Idle`, not one pair per cell. Requests like completion do not flip it.
/// (Every raw IOPub `status` message for an execution is still delivered as
/// [`KernelEvent::Iopub`].)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KernelStatus {
    /// Process spawned, waiting for the kernel to answer `kernel_info`.
    Starting,
    Idle,
    Busy,
    /// Restart requested; the new process is booting.
    Restarting,
    /// The process is gone (died, or was shut down).
    Dead,
}

impl KernelStatus {
    /// `Idle` or `Busy`: requests are being sent straight to the kernel.
    pub fn is_ready(self) -> bool {
        matches!(self, KernelStatus::Idle | KernelStatus::Busy)
    }
}

/// Everything a kernel tells the app, in arrival order, on one channel.
///
/// For one `execute(code)` returning `id`, expect (with ready kernel):
///
/// ```text
/// Status(Busy)                                   // only if it was Idle
/// Iopub { parent: id, "status" {busy} }
/// Iopub { parent: id, "execute_input" }
/// Iopub { parent: id, "stream" | "display_data" | "execute_result" | "error" | ... }*
/// InputRequest { parent: id, .. }                // if the code calls input()
/// Iopub { parent: id, "status" {idle} }
/// ExecutePayload { parent: id, .. }              // only if the reply had payloads
/// ExecuteReply { parent: id, status, execution_count }
/// Status(Idle)                                   // when nothing else is queued
/// ```
///
/// Guarantees:
/// * `ExecuteReply` is held back until the IOPub `idle` for its request has
///   been delivered, so when you see it, all of that cell's outputs have been
///   delivered too (except output printed later by background threads, which
///   arrives as more `Iopub` events with the same parent). If the idle was
///   lost, the reply is released once a later execution shows IOPub traffic,
///   or after 3 s.
/// * Every `execute` made before `shutdown` gets exactly one `ExecuteReply`,
///   in request order (synthesized as `aborted` if the kernel never answers).
/// * With several queued cells, the next cell's first IOPub events may be
///   delivered before the previous cell's `ExecuteReply` (shell and IOPub
///   are separate sockets); route by `parent_msg_id`, not by position.
#[derive(Clone, Debug)]
pub enum KernelEvent {
    Status(KernelStatus),
    /// A message from the IOPub channel, verbatim. `parent_msg_id` is the
    /// `msg_id` returned by [`Kernel::execute`](crate::Kernel::execute) for
    /// outputs of that execution. Status messages for our own internal
    /// requests (startup probes, completion, inspection) are filtered out.
    Iopub {
        parent_msg_id: Option<String>,
        msg_type: String,
        content: Value,
    },
    /// `execute_reply` on shell. `status` is `"ok"`, `"error"`, or
    /// `"aborted"` (also synthesized for executions lost to a restart, a
    /// death, an interrupt before the kernel was ready, or a shutdown).
    ExecuteReply {
        parent_msg_id: String,
        status: String,
        execution_count: Option<i64>,
    },
    /// The kernel stopped unexpectedly (process exit, heartbeat failure, or
    /// failed startup). Preceded by `Status(Dead)`. `restart()` brings it back.
    Died {
        reason: String,
    },
    /// The code called `input()` / `getpass()`: answer with
    /// [`Kernel::input_reply`](crate::Kernel::input_reply). The kernel blocks
    /// until then (or until interrupted).
    InputRequest {
        parent_msg_id: String,
        prompt: String,
        password: bool,
    },
    /// The kernel answered `kernel_info` and IOPub is live; sent right before
    /// `Status(Idle)` after every launch and restart.
    Ready {
        info: Box<KernelInfo>,
        timing: LaunchTiming,
    },
    /// `payload` list of an `execute_reply` (e.g. IPython's `page` payload for
    /// `obj?`, `set_next_input`), sent right before that `ExecuteReply`.
    ExecutePayload {
        parent_msg_id: String,
        payload: Vec<Value>,
    },
}

/// How long one (re)launch took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchTiming {
    /// `fork`/`exec` of the kernel process.
    pub spawn: Duration,
    /// Spawn until all four channels completed the ZMTP handshake.
    pub connect: Duration,
    /// Spawn until ready (kernel_info answered and IOPub live): boot-to-idle.
    pub ready: Duration,
    /// 1, unless a port collision forced a relaunch.
    pub attempt: u32,
}

/// Options for one execution.
#[derive(Clone, Debug)]
pub struct ExecuteOptions {
    pub silent: bool,
    pub store_history: bool,
    pub allow_stdin: bool,
    pub stop_on_error: bool,
    pub user_expressions: Map<String, Value>,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        ExecuteOptions {
            silent: false,
            store_history: true,
            allow_stdin: true,
            stop_on_error: true,
            user_expressions: Map::new(),
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt_str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn usize_field(v: &Value, key: &str) -> usize {
    v.get(key).and_then(Value::as_u64).unwrap_or_default() as usize
}

pub(crate) fn reply_error(content: &Value) -> Option<anyhow::Error> {
    (content.get("status").and_then(Value::as_str) == Some("error")).then(|| {
        anyhow!(
            "kernel error: {}: {}",
            str_field(content, "ename"),
            str_field(content, "evalue")
        )
    })
}

/// `kernel_info_reply`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KernelInfo {
    pub protocol_version: String,
    pub implementation: String,
    pub implementation_version: String,
    pub banner: String,
    pub language_info: LanguageInfo,
    pub help_links: Vec<HelpLink>,
    /// Kernel supports the debug adapter protocol on control.
    pub debugger: bool,
    /// The full reply content.
    pub raw: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LanguageInfo {
    pub name: String,
    pub version: String,
    pub mimetype: Option<String>,
    pub file_extension: Option<String>,
    /// `pygments_lexer`, e.g. `ipython3`.
    pub pygments_lexer: Option<String>,
    /// A string (`"python"`) or an object (`{"name": "ipython", "version": 3}`).
    pub codemirror_mode: Option<Value>,
    pub nbconvert_exporter: Option<String>,
}

impl LanguageInfo {
    /// The CodeMirror mode name whether given as a string or an object.
    pub fn codemirror_mode_name(&self) -> Option<&str> {
        match &self.codemirror_mode {
            Some(Value::String(s)) => Some(s),
            Some(Value::Object(o)) => o.get("name").and_then(Value::as_str),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HelpLink {
    pub text: String,
    pub url: String,
}

impl KernelInfo {
    pub(crate) fn from_content(content: Value) -> KernelInfo {
        let li = content.get("language_info").cloned().unwrap_or(Value::Null);
        let version = match li.get("version") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        KernelInfo {
            protocol_version: str_field(&content, "protocol_version"),
            implementation: str_field(&content, "implementation"),
            implementation_version: str_field(&content, "implementation_version"),
            banner: str_field(&content, "banner"),
            language_info: LanguageInfo {
                name: str_field(&li, "name"),
                version,
                mimetype: opt_str_field(&li, "mimetype"),
                file_extension: opt_str_field(&li, "file_extension"),
                pygments_lexer: opt_str_field(&li, "pygments_lexer"),
                codemirror_mode: li.get("codemirror_mode").filter(|v| !v.is_null()).cloned(),
                nbconvert_exporter: opt_str_field(&li, "nbconvert_exporter"),
            },
            help_links: content
                .get("help_links")
                .and_then(Value::as_array)
                .map(|links| {
                    links
                        .iter()
                        .map(|l| HelpLink {
                            text: str_field(l, "text"),
                            url: str_field(l, "url"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            debugger: content
                .get("debugger")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            raw: content,
        }
    }
}

/// `complete_reply`. Positions are Unicode code point offsets into the code
/// (see [`crate::cursor_pos_to_byte`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompleteReply {
    pub matches: Vec<String>,
    pub cursor_start: usize,
    pub cursor_end: usize,
    /// e.g. `_jupyter_types_experimental` from IPython.
    pub metadata: Map<String, Value>,
}

impl CompleteReply {
    pub(crate) fn from_content(content: Value) -> Result<CompleteReply> {
        if let Some(e) = reply_error(&content) {
            return Err(e);
        }
        Ok(CompleteReply {
            matches: content
                .get("matches")
                .and_then(Value::as_array)
                .map(|m| {
                    m.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            cursor_start: usize_field(&content, "cursor_start"),
            cursor_end: usize_field(&content, "cursor_end"),
            metadata: content
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// The replaced range as byte offsets into `code`.
    pub fn byte_range(&self, code: &str) -> std::ops::Range<usize> {
        crate::cursor_pos_to_byte(code, self.cursor_start)
            ..crate::cursor_pos_to_byte(code, self.cursor_end)
    }
}

/// `inspect_reply` (hover / Shift-Tab docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InspectReply {
    pub found: bool,
    /// Mime bundle, e.g. `text/plain` (with ANSI colors) and `text/html`.
    pub data: Map<String, Value>,
    pub metadata: Map<String, Value>,
}

impl InspectReply {
    pub(crate) fn from_content(content: Value) -> Result<InspectReply> {
        if let Some(e) = reply_error(&content) {
            return Err(e);
        }
        Ok(InspectReply {
            found: content
                .get("found")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            data: content
                .get("data")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            metadata: content
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// `text/plain`, if present.
    pub fn text(&self) -> Option<&str> {
        self.data.get("text/plain").and_then(Value::as_str)
    }
}

/// `is_complete_reply`: should Enter run the cell or insert a newline?
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IsCompleteReply {
    /// `complete`, `incomplete`, `invalid`, or `unknown`.
    pub status: String,
    /// Suggested indentation for the next line when `incomplete`.
    pub indent: String,
}

impl IsCompleteReply {
    pub(crate) fn from_content(content: Value) -> Result<IsCompleteReply> {
        if let Some(e) = reply_error(&content) {
            return Err(e);
        }
        Ok(IsCompleteReply {
            status: str_field(&content, "status"),
            indent: str_field(&content, "indent"),
        })
    }
}

/// A raw reply to [`Kernel::request`](crate::Kernel::request).
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub msg_type: String,
    pub content: Value,
}

/// Knobs for [`Kernel::launch_with`](crate::Kernel::launch_with).
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// Extra environment for the kernel process, applied after the
    /// kernelspec's `env` (e.g. `JPY_SESSION_NAME` = notebook path).
    pub env: Vec<(String, String)>,
    /// Give up on a kernel that hasn't answered `kernel_info` by then.
    pub startup_timeout: Duration,
    /// `None` disables heartbeat monitoring (process exit is still watched).
    pub heartbeat: Option<HeartbeatConfig>,
    /// How long `shutdown` waits for a graceful exit before SIGKILL.
    pub shutdown_grace: Duration,
    /// How long `restart` waits for the old process before SIGKILL. The new
    /// process boots in parallel; queued executions run only once the old
    /// one is gone.
    pub restart_grace: Duration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        LaunchOptions {
            env: Vec::new(),
            startup_timeout: Duration::from_secs(60),
            heartbeat: Some(HeartbeatConfig::default()),
            shutdown_grace: Duration::from_secs(2),
            restart_grace: Duration::from_secs(1),
        }
    }
}

/// Heartbeat: ping every `interval`; declare the kernel dead after
/// `max_missed` consecutive pings without an echo within `timeout`.
/// ipykernel echoes from a GIL-free C thread, so long computations don't
/// cause misses; only a hung or stopped process does.
#[derive(Clone, Copy, Debug)]
pub struct HeartbeatConfig {
    pub interval: Duration,
    pub timeout: Duration,
    pub max_missed: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        HeartbeatConfig {
            interval: Duration::from_secs(3),
            timeout: Duration::from_secs(3),
            max_missed: 3,
        }
    }
}
