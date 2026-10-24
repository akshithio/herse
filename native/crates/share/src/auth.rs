//! Where the GitHub token comes from: the GitHub CLI first, else a token the
//! user pasted once and Herse keeps in the macOS Keychain.
//!
//! Tokens never appear in logs, `Debug` output or error messages, and never
//! in a child process's argv (where `ps` would show them):
//!
//! * `gh auth token` prints the token on a pipe.
//! * Storing goes through `security -i`, which reads the
//!   `add-generic-password ... -w <token>` command line from stdin instead of
//!   argv. (`security add-generic-password -w <token>` would be simpler but
//!   briefly exposes the token to every local user via `ps`; `-w` without a
//!   value prompts on the controlling terminal, which a GUI app doesn't have.)
//! * Loading uses `find-generic-password -w`, which prints the token on a pipe.
//!
//! Items are created by `/usr/bin/security`, so that tool is on the item's
//! access list and reading it back never shows a Keychain prompt.

use std::ffi::OsStr;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Result, ShareError};

/// A GitHub token. Validated on construction (GitHub tokens are ASCII
/// letters, digits and `_`), redacted in `Debug`, and with no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

/// What kind of token this is, from its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// `ghp_`: classic personal access token (needs the `gist` scope).
    Classic,
    /// `github_pat_`: fine-grained token (needs "Gists: Read and write").
    FineGrained,
    /// `gho_`: OAuth token, which is what `gh auth token` returns.
    OAuth,
    /// `ghu_` / `ghs_` / `ghr_`: GitHub App user, installation or refresh token.
    App,
    /// Anything else (e.g. a pre-2021 40-hex classic token).
    Other,
}

impl Token {
    /// Validate a token the user pasted (surrounding whitespace is trimmed).
    pub fn new(raw: impl AsRef<str>) -> Result<Token> {
        let token = raw.as_ref().trim();
        if token.is_empty() {
            return Err(ShareError::InvalidInput("The token is empty.".into()));
        }
        if token.len() > 255
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(ShareError::InvalidInput(
                "That doesn't look like a GitHub token (they look like ghp_… or github_pat_…)."
                    .into(),
            ));
        }
        Ok(Token(token.to_string()))
    }

    /// The secret itself, for the `Authorization` header. Don't log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> TokenKind {
        let t = self.0.as_str();
        if t.starts_with("github_pat_") {
            TokenKind::FineGrained
        } else if t.starts_with("ghp_") {
            TokenKind::Classic
        } else if t.starts_with("gho_") {
            TokenKind::OAuth
        } else if t.starts_with("ghu_") || t.starts_with("ghs_") || t.starts_with("ghr_") {
            TokenKind::App
        } else {
            TokenKind::Other
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token({:?}, <redacted>)", self.kind())
    }
}

/// Where a token came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    /// `gh auth token` (the GitHub CLI's login).
    GhCli,
    /// The Keychain item Herse stored after the user pasted a token.
    Keychain,
}

/// Directories searched for `gh` after `PATH`: a Finder-launched app gets a
/// minimal `PATH` without Homebrew's bin directories.
pub const GH_EXTRA_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

/// How long `gh auth token` may take before it's killed (it normally takes
/// 50–150 ms; it reads its own Keychain item).
pub const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// The GitHub CLI binary, if installed: `PATH`, then [`GH_EXTRA_DIRS`].
pub fn find_gh() -> Option<PathBuf> {
    let extra: Vec<&Path> = GH_EXTRA_DIRS.iter().map(Path::new).collect();
    find_gh_in(std::env::var_os("PATH").as_deref(), &extra)
}

/// [`find_gh`] with an explicit `PATH` value and fallback directories.
pub fn find_gh_in(path_var: Option<&OsStr>, extra_dirs: &[&Path]) -> Option<PathBuf> {
    let from_path = path_var.into_iter().flat_map(std::env::split_paths);
    from_path
        .chain(extra_dirs.iter().map(|d| d.to_path_buf()))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("gh"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// The GitHub CLI's token for github.com. `Ok(None)` when `gh` isn't
/// installed or isn't signed in. Blocks for the `gh` call (~50–150 ms, at
/// most [`GH_TIMEOUT`]).
pub fn gh_token() -> Result<Option<Token>> {
    match find_gh() {
        Some(gh) => gh_token_with(&gh, GH_TIMEOUT),
        None => Ok(None),
    }
}

/// [`gh_token`] with an explicit `gh` binary and timeout.
pub fn gh_token_with(gh: &Path, timeout: Duration) -> Result<Option<Token>> {
    let mut cmd = Command::new(gh);
    cmd.args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1");
    let out =
        run(cmd, None, timeout).map_err(|e| ShareError::GhCli(format!("couldn't run gh: {e}")))?;
    let Some(status) = out.status else {
        return Err(ShareError::GhCli(
            "`gh auth token` didn't finish in time".into(),
        ));
    };
    if !status.success() {
        // Signed out ("no oauth token found"), or a gh too old to have `auth token`.
        log::debug!(
            "gh auth token exited with {status}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match Token::new(stdout.trim()) {
        Ok(token) => Ok(Some(token)),
        Err(_) if stdout.trim().is_empty() => Ok(None),
        Err(_) => Err(ShareError::GhCli(
            "`gh auth token` printed something that isn't a token".into(),
        )),
    }
}

/// The token to use: the GitHub CLI's if it's signed in, else the one in the
/// Keychain. `Ok(None)` when neither exists. Blocks ~50–200 ms (one or two
/// subprocesses).
pub fn discover_token() -> Result<Option<(Token, TokenSource)>> {
    let gh_error = match gh_token() {
        Ok(Some(token)) => return Ok(Some((token, TokenSource::GhCli))),
        Ok(None) => None,
        Err(e) => {
            log::warn!("{e}");
            Some(e)
        }
    };
    match Keychain::default().load()? {
        Some(token) => Ok(Some((token, TokenSource::Keychain))),
        None => gh_error.map_or(Ok(None), Err),
    }
}

/// The Keychain item service name Herse stores the pasted token under.
pub const KEYCHAIN_SERVICE: &str = "Herse GitHub";

const SECURITY: &str = "/usr/bin/security";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(15);
/// `security`'s exit status for errSecItemNotFound.
const NOT_FOUND_STATUS: i32 = 44;

/// A generic-password Keychain item holding the pasted token, managed with
/// the `security` CLI. Each call blocks for one or two `security` runs
/// (~20–50 ms each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keychain {
    service: String,
}

impl Default for Keychain {
    fn default() -> Self {
        Keychain {
            service: KEYCHAIN_SERVICE.to_string(),
        }
    }
}

impl Keychain {
    /// A Keychain item under another service name (tests use their own).
    pub fn new(service: impl Into<String>) -> Keychain {
        Keychain {
            service: service.into(),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// The stored token, or `None` if there is no item.
    pub fn load(&self) -> Result<Option<Token>> {
        self.check_service()?;
        let mut cmd = Command::new(SECURITY);
        cmd.args(["find-generic-password", "-s", &self.service, "-w"]);
        let out = run_security(cmd, None)?;
        match out.code() {
            Some(0) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                Token::new(stdout.trim()).map(Some).map_err(|_| {
                    ShareError::Keychain("the stored GitHub token is malformed".into())
                })
            }
            Some(NOT_FOUND_STATUS) => Ok(None),
            _ => Err(out.error("reading the GitHub token", None)),
        }
    }

    /// Store `token` for GitHub user `login`, replacing any earlier item.
    pub fn store(&self, login: &str, token: &Token) -> Result<()> {
        self.check_service()?;
        if login.is_empty()
            || login.len() > 39
            || !login
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(ShareError::InvalidInput(format!(
                "{login:?} isn't a GitHub login"
            )));
        }
        // One item per service, so `load` can't pick up a stale account's token.
        self.delete()?;
        // `security -i` tokenizes like a shell with double quotes; the service is
        // checked for quotes/backslashes and the login and token are [A-Za-z0-9_-].
        let line = format!(
            "add-generic-password -U -s \"{}\" -a \"{login}\" -w \"{}\"\n",
            self.service,
            token.expose()
        );
        let mut cmd = Command::new(SECURITY);
        cmd.arg("-i");
        let out = run_security(cmd, Some(line.as_bytes()))?;
        match out.code() {
            Some(0) => Ok(()),
            _ => Err(out.error("storing the GitHub token", Some(token))),
        }
    }

    /// Delete every item under this service. Returns whether one existed.
    pub fn delete(&self) -> Result<bool> {
        self.check_service()?;
        let mut deleted = false;
        for _ in 0..32 {
            let mut cmd = Command::new(SECURITY);
            cmd.args(["delete-generic-password", "-s", &self.service]);
            let out = run_security(cmd, None)?;
            match out.code() {
                Some(0) => deleted = true,
                Some(NOT_FOUND_STATUS) => return Ok(deleted),
                _ => return Err(out.error("deleting the GitHub token", None)),
            }
        }
        Ok(deleted)
    }

    fn check_service(&self) -> Result<()> {
        let bad = |c: char| c == '"' || c == '\\' || c.is_control();
        if self.service.is_empty() || self.service.chars().any(bad) {
            return Err(ShareError::InvalidInput(format!(
                "bad Keychain service name {:?}",
                self.service
            )));
        }
        Ok(())
    }
}

fn run_security(cmd: Command, stdin: Option<&[u8]>) -> Result<ProcOutput> {
    let out = run(cmd, stdin, SECURITY_TIMEOUT)
        .map_err(|e| ShareError::Keychain(format!("couldn't run {SECURITY}: {e}")))?;
    if out.status.is_none() {
        return Err(ShareError::Keychain(
            "the security tool didn't finish in time".into(),
        ));
    }
    Ok(out)
}

/// A finished (or killed, `status: None`) child process.
struct ProcOutput {
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ProcOutput {
    fn code(&self) -> Option<i32> {
        self.status.and_then(|s| s.code())
    }

    fn error(&self, doing: &str, secret: Option<&Token>) -> ShareError {
        let mut detail = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if let Some(token) = secret {
            detail = detail.replace(token.expose(), "<redacted>");
        }
        if detail.is_empty() {
            detail = match self.code() {
                Some(code) => format!("exit status {code}"),
                None => "killed".into(),
            };
        }
        ShareError::Keychain(format!("{doing} failed: {detail}"))
    }
}

/// Run `cmd` with optional stdin, collecting stdout/stderr, killing it after
/// `timeout`.
fn run(mut cmd: Command, stdin: Option<&[u8]>, timeout: Duration) -> std::io::Result<ProcOutput> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Small (one command line), so it fits the pipe buffer; a child that
        // exits early just makes this fail, and its status says why.
        let _ = pipe.write_all(input);
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = stdout.join().unwrap_or_default();
            let stderr = stderr.join().unwrap_or_default();
            return Ok(ProcOutput {
                status: Some(status),
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Don't join the readers: a grandchild may still hold the pipes open.
            return Ok(ProcOutput {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
