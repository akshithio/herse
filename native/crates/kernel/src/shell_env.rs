//! The user's login-shell environment, for an app started from Finder.
//!
//! A GUI launch inherits launchd's environment: `PATH` is
//! `/usr/bin:/bin:/usr/sbin:/sbin` and no conda or venv is active. A
//! kernelspec whose argv is a bare `python` (what `pip install --user
//! ipykernel` writes) then fails to start, and `!pip` in a kernel that does
//! start finds the wrong interpreter. Like VS Code and Zed, ask the login
//! shell for its environment once, off the main thread at startup
//! ([`preload`]), and use it for discovery and for every kernel.
//!
//! Launched from a terminal, the process already has that environment (its
//! `PATH` is more than launchd's system default) and nothing is spawned.

use std::{
    collections::HashMap,
    ffi::OsString,
    io::Read as _,
    process::{Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

/// A shell whose rc files hang must not hold kernels back for long.
const TIMEOUT: Duration = Duration::from_secs(5);
const MARK: &str = "__HERSE_SHELL_ENV__";
/// Printed after the environment: reading stops there, so a background job
/// an rc file started (which keeps the pipe open) can't hold it up.
const END: &str = "__HERSE_SHELL_ENV_END__";

static SHELL_ENV: LazyLock<Option<HashMap<String, String>>> = LazyLock::new(load);

/// Start reading the login shell's environment in the background. Call once
/// at startup; [`get`] waits for it.
pub fn preload() {
    if needed() {
        std::thread::Builder::new()
            .name("shell-env".into())
            .spawn(|| {
                LazyLock::force(&SHELL_ENV);
            })
            .ok();
    }
}

/// The login shell's environment when the app was started without one (from
/// Finder or the Dock), else `None`: the process environment is already right.
/// Blocks until [`preload`]'s read finishes (at most a few seconds, once).
pub fn get() -> Option<&'static HashMap<String, String>> {
    if !needed() {
        return None;
    }
    SHELL_ENV.as_ref()
}

/// An environment variable as a kernel would see it from the user's shell.
pub fn var_os(key: &str) -> Option<OsString> {
    match get() {
        Some(env) => env.get(key).map(OsString::from),
        None => std::env::var_os(key),
    }
}

/// Started by launchd (Finder, the Dock, `open`), the process has launchd's
/// bare `PATH`; anything richer came from a shell and is already right.
fn needed() -> bool {
    cfg!(unix) && std::env::var_os("HERSE_NO_SHELL_ENV").is_none() && is_bare_path(std::env::var_os("PATH").as_deref())
}

fn is_bare_path(path: Option<&std::ffi::OsStr>) -> bool {
    const SYSTEM: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
    path.is_none_or(|p| {
        std::env::split_paths(p).all(|d| d.as_os_str().is_empty() || SYSTEM.iter().any(|s| d == std::path::Path::new(s)))
    })
}

fn load() -> Option<HashMap<String, String>> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut cmd = Command::new(&shell);
    // Login and interactive: conda's and pyenv's hooks usually live in the
    // interactive rc file (`.zshrc`, `.bashrc`). The marker skips anything
    // the rc files print first.
    cmd.args(["-l", "-i", "-c", &format!("printf '%s' {MARK}; /usr/bin/env -0; printf '%s' {END}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    let mut child = cmd.spawn().map_err(|e| log::warn!("shell environment: {shell}: {e}")).ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
            if out.windows(END.len()).any(|w| w == END.as_bytes()) {
                break;
            }
        }
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(TIMEOUT) {
        Ok(out) => out,
        Err(_) => {
            log::warn!("shell environment: {shell} took longer than {TIMEOUT:?}; using the app's");
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    // Done reading; the shell has exited or is about to. Don't wait on it.
    let _ = child.kill();
    let _ = child.wait();
    let env = parse(&out)?;
    // Without a PATH this is not an environment worth switching to.
    env.contains_key("PATH").then_some(env)
}

fn parse(out: &[u8]) -> Option<HashMap<String, String>> {
    let text = String::from_utf8_lossy(out);
    let (_, vars) = text.split_once(MARK)?;
    let vars = vars.split_once(END).map_or(vars, |(vars, _)| vars);
    let env: HashMap<String, String> = vars
        .split('\0')
        .filter_map(|entry| entry.split_once('='))
        .filter(|(k, _)| !k.is_empty() && !matches!(*k, "PWD" | "OLDPWD" | "SHLVL" | "_"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (!env.is_empty()).then_some(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_after_the_marker() {
        let out = format!("conda says hi\n{MARK}PATH=/opt/x/bin:/usr/bin\0HOME=/Users/a\0SHLVL=2\0EQ=a=b\0");
        let env = parse(out.as_bytes()).unwrap();
        assert_eq!(env["PATH"], "/opt/x/bin:/usr/bin");
        assert_eq!(env["EQ"], "a=b");
        assert!(!env.contains_key("SHLVL"));
    }

    #[test]
    fn only_launchds_path_counts_as_bare() {
        use std::ffi::OsStr;
        assert!(is_bare_path(None));
        assert!(is_bare_path(Some(OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin"))));
        assert!(!is_bare_path(Some(OsStr::new("/opt/homebrew/bin:/usr/bin:/bin"))));
    }

    #[test]
    fn stops_at_the_end_marker() {
        let out = format!("{MARK}PATH=/usr/bin\0{END}job output\0X=1\0");
        let env = parse(out.as_bytes()).unwrap();
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn no_marker_no_environment() {
        assert!(parse(b"PATH=/usr/bin\0").is_none());
    }
}
