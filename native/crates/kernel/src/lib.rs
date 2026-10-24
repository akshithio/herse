//! # herse-kernel
//!
//! Jupyter kernels without `jupyter server`: discover the kernelspecs
//! installed on this machine, launch kernels ourselves, and speak the Jupyter
//! messaging protocol (v5.3) to them over ZeroMQ directly.
//!
//! ```no_run
//! use herse_kernel::{Kernel, KernelEvent, discover, default_spec};
//! let specs = discover();                                   // ~1 ms, synchronous
//! let spec = default_spec(&specs).expect("no kernels installed");
//! let (kernel, events) = Kernel::launch(spec, std::path::Path::new("."))?;
//! let id = kernel.execute("print('hi')");                   // queued until ready
//! while let Ok(event) = events.recv_blocking() {
//!     match event {
//!         KernelEvent::Iopub { parent_msg_id, msg_type, content } if parent_msg_id.as_deref() == Some(&id) => {
//!             println!("{msg_type}: {content}");
//!         }
//!         KernelEvent::ExecuteReply { parent_msg_id, .. } if parent_msg_id == id => break,
//!         _ => {}
//!     }
//! }
//! # anyhow::Ok(())
//! ```
//!
//! ## Threading
//!
//! The crate is runtime-agnostic. Each kernel runs on its own thread
//! (`herse-kernel-<name>`) with a private single-threaded executor; nothing
//! needs the caller to run tokio or smol. [`Kernel`] is a `Clone + Send +
//! Sync` handle whose methods never block (except [`Kernel::wait_ready`] and
//! [`Kernel::shutdown_and_wait`]); results come back on `async_channel`
//! receivers, which work on any executor (GPUI's included) or blocking via
//! `recv_blocking`. The event channel is unbounded: drain it promptly.
//!
//! ## Lifecycle
//!
//! * [`Kernel::launch`] blocks only for the spawn. Events: `Status(Starting)`,
//!   then `Ready` + `Status(Idle)` once the kernel answered `kernel_info` *and*
//!   an IOPub message arrived (so the SUB subscription is live and the first
//!   cell's output can't be dropped). Requests before that are queued in order.
//! * Restart boots a fresh process in parallel with the old one's graceful
//!   shutdown; queued work runs once the old process is gone. The session id
//!   stays the same.
//! * Death is detected instantly from process exit, and from heartbeats for
//!   hung processes: `Status(Dead)` + `Died { reason }` (with the kernel's
//!   last stderr lines).
//! * Kernels run in their own process group (terminal Ctrl-C doesn't reach
//!   them) and are never orphaned: graceful shutdown on drop/[`shutdown_all`],
//!   an `atexit` SIGKILL, and a watchdog process that kills them if the app
//!   dies abruptly.
//!
//! See [`KernelEvent`] for the event order of one execution.
//!
//! ## Wire protocol
//!
//! A small pure-Rust ZMTP 3.0 client (NULL mechanism, TCP; DEALER for
//! shell/control/stdin/heartbeat, SUB for IOPub) and HMAC-SHA256 signing with
//! a fresh key per kernel; incoming signatures are verified and bad messages
//! dropped. No libzmq, no tokio.
//!
//! ## Caveats
//!
//! * Cursor positions (`complete`, `inspect`, replies) are Unicode code point
//!   offsets, per the protocol; see [`byte_to_cursor_pos`].
//! * ipykernel handles shell requests one at a time, so `complete`/`inspect`
//!   wait behind a running cell. Give them a UI timeout. Requests made before
//!   the kernel is ready are sent once it is; drop stale ones yourself.
//! * Comm (ipywidgets) messages arrive as `Iopub` events; binary buffers are
//!   not surfaced yet.
//! * Call [`shutdown_all`] when the app quits for a graceful shutdown; without
//!   it, kernels are SIGKILLed at exit.
//! * An app started from Finder has a minimal `PATH` and no activated env:
//!   call [`shell_env::preload`] at startup and discovery and kernels use the
//!   login shell's environment instead. Discovery also scans common
//!   conda/Homebrew prefixes, and [`discover_via_jupyter`] is the slow fallback.
//! * Unix (macOS, Linux) is the supported platform. Windows compiles in
//!   principle (interrupts fall back to `interrupt_request`, no watchdog)
//!   but is untested.

mod connection;
mod kernel;
mod process;
pub mod shell_env;
mod spec;
mod types;
mod util;
mod wire;
mod zmtp;

pub use async_channel;
pub use kernel::{Kernel, shutdown_all};
pub use process::kill_all;
pub use spec::{
    InterruptMode, KernelSpec, default_spec, discover, discover_in, discover_via_jupyter, find,
    jupyter_data_dirs,
};
pub use types::{
    CompleteReply, ExecuteOptions, HeartbeatConfig, HelpLink, InspectReply, IsCompleteReply,
    KernelEvent, KernelInfo, KernelStatus, LanguageInfo, LaunchOptions, LaunchTiming, Reply,
};
pub use util::{byte_to_cursor_pos, cursor_pos_to_byte};

impl KernelEvent {
    /// The request this event belongs to, if any.
    pub fn parent_msg_id(&self) -> Option<&str> {
        match self {
            KernelEvent::Iopub { parent_msg_id, .. } => parent_msg_id.as_deref(),
            KernelEvent::ExecuteReply { parent_msg_id, .. }
            | KernelEvent::InputRequest { parent_msg_id, .. }
            | KernelEvent::ExecutePayload { parent_msg_id, .. } => Some(parent_msg_id),
            KernelEvent::Status(_) | KernelEvent::Died { .. } | KernelEvent::Ready { .. } => None,
        }
    }
}
