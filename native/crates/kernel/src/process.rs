//! Kernel processes: spawn in a new process group, watch for exit, capture
//! stderr, and make sure nothing outlives the app.
//!
//! Three layers keep kernels from being orphaned:
//! 1. graceful shutdown on `Kernel::shutdown` / last-handle drop / [`shutdown_all`],
//! 2. an `atexit` hook that SIGKILLs every live kernel process group on a
//!    normal `exit()`,
//! 3. a watchdog: one tiny `/bin/sh` per app, in its own process group,
//!    reading kernel pgids from a pipe. If the app dies any other way (crash,
//!    SIGKILL, Ctrl-C in the terminal) the pipe closes and the watchdog kills
//!    every registered group. (ipykernel additionally exits on its own when
//!    reparented, via `JPY_PARENT_PID`.)
//!
//! [`shutdown_all`]: crate::shutdown_all

use std::collections::VecDeque;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

/// The last bytes a kernel wrote to stdout/stderr, for death reasons.
#[derive(Default)]
pub(crate) struct Tail {
    buf: VecDeque<u8>,
}

const TAIL_BYTES: usize = 16 * 1024;

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        let excess = self.buf.len().saturating_sub(TAIL_BYTES);
        self.buf.drain(..excess);
    }

    /// The last `max_lines` lines, lossily decoded.
    pub fn text(&self, max_lines: usize) -> String {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(max_lines)..].join("\n")
    }
}

pub(crate) struct Spawned {
    pub pid: u32,
    pub tail: Arc<Mutex<Tail>>,
    pub spawn_time: Duration,
}

/// Spawn `argv` in its own process group. `on_exit` runs on a watcher thread
/// once the process has exited and been reaped (and its group, connection
/// file and registry entry cleaned up), with a description of the exit status.
pub(crate) fn spawn(
    argv: &[String],
    env: &[(String, String)],
    cwd: &Path,
    label: &str,
    conn_file: &Path,
    on_exit: impl FnOnce(String) + Send + 'static,
) -> Result<Spawned> {
    let started = Instant::now();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Started from Finder: run the kernel in the user's shell environment, as
    // `jupyter` from a terminal would. A bare argv[0] (`python`) is then
    // looked up on that PATH too.
    if let Some(shell) = crate::shell_env::get() {
        cmd.envs(shell);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: terminal Ctrl-C (SIGINT to the app's foreground
        // group) must not reach kernels, and we can signal the kernel plus
        // its children as a unit.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            let place = if argv[0].contains('/') { "" } else { " on PATH" };
            anyhow::anyhow!("`{}` was not found{place}", argv[0])
        } else {
            anyhow::Error::new(e).context(format!(
                "failed to start kernel `{}` (in {})",
                argv[0],
                cwd.display()
            ))
        }
    })?;
    let spawn_time = started.elapsed();
    let pid = child.id();
    register(pid, conn_file);

    let tail = Arc::new(Mutex::new(Tail::default()));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    for (stream, is_err) in [
        (
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
            false,
        ),
        (
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
            true,
        ),
    ] {
        let Some(mut stream) = stream else { continue };
        let tail = tail.clone();
        let done = done_tx.clone();
        let label = label.to_owned();
        let _ = std::thread::Builder::new()
            .name(format!(
                "herse-kernel-{}",
                if is_err { "stderr" } else { "stdout" }
            ))
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            tail.lock().unwrap_or_else(|e| e.into_inner()).push(chunk);
                            for line in String::from_utf8_lossy(chunk).lines() {
                                if !line.trim().is_empty() {
                                    log::debug!("[{label}] {line}");
                                }
                            }
                        }
                    }
                }
                let _ = done.send(());
            });
    }
    drop(done_tx);

    let label = label.to_owned();
    let watcher = std::thread::Builder::new()
        .name("herse-kernel-wait".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let status = wait(&mut child);
            reaped(pid);
            // Let the pipe readers drain so the death reason includes the
            // final traceback (bounded: grandchildren may hold the pipes).
            let deadline = Instant::now() + Duration::from_millis(250);
            for _ in 0..2 {
                let left = deadline.saturating_duration_since(Instant::now());
                if done_rx.recv_timeout(left).is_err() {
                    break;
                }
            }
            log::debug!("[{label}] exited: {status}");
            on_exit(status);
        });
    if let Err(e) = watcher {
        // Nobody would ever notice this process exit: don't leave it running.
        reaped(pid);
        return Err(e).context("failed to start kernel watcher thread");
    }

    Ok(Spawned {
        pid,
        tail,
        spawn_time,
    })
}

fn wait(child: &mut Child) -> String {
    match child.wait() {
        Ok(status) => describe(status),
        Err(e) => format!("wait failed: {e}"),
    }
}

fn describe(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            let name = match sig {
                libc::SIGINT => " (SIGINT)",
                libc::SIGILL => " (SIGILL)",
                libc::SIGABRT => " (SIGABRT)",
                libc::SIGFPE => " (SIGFPE)",
                libc::SIGKILL => " (SIGKILL)",
                libc::SIGBUS => " (SIGBUS)",
                libc::SIGSEGV => " (SIGSEGV)",
                libc::SIGTERM => " (SIGTERM)",
                _ => "",
            };
            return format!("killed by signal {sig}{name}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}

/// Send `sig` to the kernel's process group (falls back to the pid alone if
/// the kernel left its group).
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, sig: i32) -> io::Result<()> {
    let pid = pid as libc::pid_t;
    // SAFETY: plain syscalls on integers.
    unsafe {
        if libc::killpg(pid, sig) == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) && libc::kill(pid, sig) == 0 {
            return Ok(());
        }
        Err(err)
    }
}

#[cfg(unix)]
pub(crate) fn interrupt(pid: u32) -> io::Result<()> {
    signal_group(pid, libc::SIGINT)
}

#[cfg(unix)]
pub(crate) fn kill(pid: u32) {
    let _ = signal_group(pid, libc::SIGKILL);
}

#[cfg(not(unix))]
pub(crate) fn interrupt(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "signal interrupts are not supported on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Registry of live kernel processes (atexit + watchdog)
// ---------------------------------------------------------------------------

struct Live {
    pid: u32,
    conn_file: PathBuf,
}

static LIVE: Mutex<Vec<Live>> = Mutex::new(Vec::new());
static ATEXIT: Once = Once::new();

fn register(pid: u32, conn_file: &Path) {
    ATEXIT.call_once(|| {
        #[cfg(unix)]
        // SAFETY: registering a plain extern "C" fn.
        unsafe {
            libc::atexit(at_exit);
        }
    });
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).push(Live {
        pid,
        conn_file: conn_file.to_path_buf(),
    });
    watchdog::send(&watchdog::add_line(pid, conn_file));
}

/// The process has been reaped: forget it, kill any children it left in its
/// group, and delete its connection file.
fn reaped(pid: u32) {
    kill(pid); // the group may still hold children (multiprocessing workers, `!cmd`)
    let entry = {
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        live.iter()
            .position(|l| l.pid == pid)
            .map(|i| live.swap_remove(i))
    };
    if let Some(entry) = entry {
        let _ = std::fs::remove_file(&entry.conn_file);
    }
    watchdog::send(&format!("-{pid}\n"));
}

/// SIGKILL every live kernel process group started by this process,
/// immediately. Prefer [`crate::shutdown_all`], which asks politely first.
pub fn kill_all() {
    let live = match LIVE.try_lock() {
        Ok(live) => live,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    for entry in live.iter() {
        kill(entry.pid);
        let _ = std::fs::remove_file(&entry.conn_file);
    }
}

#[cfg(unix)]
extern "C" fn at_exit() {
    kill_all();
}

pub(crate) fn live() -> Vec<(u32, PathBuf)> {
    LIVE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|l| (l.pid, l.conn_file.clone()))
        .collect()
}

#[cfg(unix)]
mod watchdog {
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::Mutex;

    /// Reads `+PGID CONNECTION_FILE` / `-PGID` lines. On EOF (the app is
    /// gone, however it died) it SIGKILLs every group still registered and
    /// deletes their connection files. Shell builtins only until then.
    const SCRIPT: &str = r#"trap '' INT HUP QUIT TSTP
nl='
'
e=''
while IFS= read -r l; do
  case "$l" in
    +*) e="$e${l#+}$nl" ;;
    -*) n=''
        while IFS= read -r r; do
          [ -n "$r" ] && [ "${r%% *}" != "${l#-}" ] && n="$n$r$nl"
        done <<EOF
$e
EOF
        e="$n" ;;
  esac
done
while IFS= read -r r; do
  [ -n "$r" ] || continue
  kill -s KILL -- "-${r%% *}" 2>/dev/null
  case "$r" in *' '*) rm -f -- "${r#* }" ;; esac
done <<EOF
$e
EOF
"#;

    pub(super) fn add_line(pid: u32, conn_file: &Path) -> String {
        format!("+{pid} {}\n", conn_file.display())
    }

    static WATCHDOG: Mutex<Option<(Child, ChildStdin)>> = Mutex::new(None);

    fn start() -> Option<(Child, ChildStdin)> {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sh")
            .args(["-c", SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| log::warn!("kernel watchdog unavailable: {e}"))
            .ok()?;
        let stdin = child.stdin.take()?;
        Some((child, stdin))
    }

    pub(super) fn send(line: &str) {
        let mut guard = WATCHDOG.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            if line.starts_with('-') {
                return;
            }
            *guard = start();
            // Re-register everything alive (the watchdog may have restarted).
            if let Some((_, stdin)) = guard.as_mut() {
                for (pid, file) in super::live() {
                    let _ = stdin.write_all(add_line(pid, &file).as_bytes());
                }
                return;
            }
        }
        if let Some((child, stdin)) = guard.as_mut()
            && stdin.write_all(line.as_bytes()).is_err()
        {
            let _ = child.try_wait();
            *guard = None;
        }
    }

    #[cfg(test)]
    pub(super) fn pid() -> Option<u32> {
        WATCHDOG.lock().unwrap().as_ref().map(|(c, _)| c.id())
    }
}

#[cfg(not(unix))]
mod watchdog {
    pub(super) fn send(_line: &str) {}
    pub(super) fn add_line(_pid: u32, _conn_file: &std::path::Path) -> String {
        String::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_last_lines() {
        let mut t = Tail::default();
        t.push(b"one\ntwo\n\nthree\n");
        assert_eq!(t.text(2), "two\nthree");
        t.push(&vec![b'x'; TAIL_BYTES * 2]);
        assert_eq!(t.buf.len(), TAIL_BYTES);
    }

    #[test]
    fn spawn_watch_and_group_kill() {
        let (tx, rx) = std::sync::mpsc::channel();
        let argv: Vec<String> = ["/bin/sh", "-c", "echo boom >&2; sleep 30 & wait"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let file = std::env::temp_dir().join(format!("herse-proc-test-{}", std::process::id()));
        std::fs::write(&file, "{}").unwrap();
        let spawned = spawn(&argv, &[], Path::new("/"), "test", &file, move |status| {
            tx.send(status).unwrap()
        })
        .unwrap();
        assert!(watchdog::pid().is_some());
        std::thread::sleep(Duration::from_millis(100));
        kill(spawned.pid);
        let status = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(status.contains("SIGKILL"), "{status}");
        assert!(spawned.tail.lock().unwrap().text(5).contains("boom"));
        assert!(!file.exists());
        assert!(!live().iter().any(|(pid, _)| *pid == spawned.pid));
    }
}
