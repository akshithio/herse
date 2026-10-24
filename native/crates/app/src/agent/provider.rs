//! The agent CLIs people already pay for, Claude Code and Codex, run
//! headless with Herse's tools: their sign-in, their plan, their models.
//! What they print (JSON lines) becomes [`Event`]s.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    /// Who runs the model, and so receives what the agent reads.
    pub fn company(self) -> &'static str {
        match self {
            Provider::Claude => "Anthropic",
            Provider::Codex => "OpenAI",
        }
    }

    /// The command line tool's name, as people know it.
    pub fn tool_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
        }
    }

    /// Said once, before the first question: what leaves the machine.
    pub fn privacy_notice(self, sees: Sees) -> String {
        let what = match sees {
            Sees::Notebook => "this notebook's code and outputs, data included",
            Sees::Table => "the rows it reads from this table",
            Sees::Folder => "the files it reads in this folder",
        };
        let (name, company, tool) = (self.name(), self.company(), self.tool_name());
        let mut notice = format!("{name} sees what you ask and {what}: they go to {company} through your {tool} sign-in.");
        // Only a notebook's assistant can run code.
        if sees == Sees::Notebook {
            notice.push_str(&format!(" Herse asks before {name} runs any code."));
        }
        notice
    }

    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id() == id)
    }

    /// The CLI, where the user installed it: their shell's PATH, else the
    /// places installers put it.
    pub fn binary(self) -> Option<PathBuf> {
        let name = self.id();
        if let Some(path) = herse_kernel::shell_env::var_os("PATH")
            && let Some(found) = std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
        {
            return Some(found);
        }
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let places: &[&str] = match self {
            Provider::Claude => &[".local/bin/claude", ".claude/local/claude", ".npm-global/bin/claude"],
            Provider::Codex => &[".volta/bin/codex", ".npm-global/bin/codex", ".local/bin/codex"],
        };
        places
            .iter()
            .map(|p| home.join(p))
            .chain(["/opt/homebrew/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name)))
            .find(|p| p.is_file())
    }

    /// The installed providers, the preferred one first.
    pub fn installed(preferred: Option<Provider>) -> Vec<Provider> {
        let mut all: Vec<Provider> = Provider::ALL.into_iter().filter(|p| p.binary().is_some()).collect();
        if let Some(p) = preferred
            && let Some(i) = all.iter().position(|x| *x == p)
        {
            all.swap(0, i);
        }
        all
    }
}

/// What a run reports, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The conversation's id, to continue it with the next question.
    Session(String),
    /// Something the agent said to the user.
    Say(String),
    /// A step outside Herse's tools (reading a file, a shell command).
    Activity(String),
    /// It finished, with an error if it failed.
    Done(Option<String>),
}

/// How Herse speaks to the agent, on top of its own instructions: about a
/// notebook ([`NOTEBOOK`]), a table ([`TABLE`]), or nothing open ([`FOLDER`]).
pub const NOTEBOOK: &str = "You are working inside Herse, a notebook app, on the notebook the user has open. \
The user watches the notebook while you work: each edit or run you make with the herse tools scrolls \
their view to that cell. Your messages appear as short texts in a corner, like a chat on a phone, so \
write one or two plain sentences at a time, no headings, no tables. Before a change, say what you are \
about to do (\"Let me fix the import in cell 3.\"); after it, say what happened. Use the herse tools \
to read, edit, insert, delete and run cells, and evaluate to look at data in the kernel. Never edit the \
.ipynb file directly. Refer to cells by number (\"cell 3\") as the tools do.";

pub const TABLE: &str = "You are working inside Herse, a notebook app, where the user has a table open \
(a data file such as a CSV, or a DataFrame from a notebook) and is looking at it. Your messages appear \
as short texts in a corner, like a chat on a phone, so write one or two plain sentences at a time, no \
headings; a small list of numbers is fine. Answer from the data: use the herse tools (describe_table, \
then read_rows, find_rows, group_rows) rather than guessing, and say how you got a number (\"312 of \
2,000 orders were returned\"). Don't change the file.";

pub const FOLDER: &str = "You are working inside Herse, a notebook app. The user has nothing open, only \
this folder in the sidebar; it is your working directory. Your messages appear as short texts in a \
corner, like a chat on a phone, so write one or two plain sentences at a time, no headings. Answer \
questions about its files (notebooks, data, code) by reading them; don't change anything.";

/// What an assistant gets to see, for the notice before a first question.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sees {
    Notebook,
    Table,
    Folder,
}

pub struct Request {
    pub prompt: String,
    /// What Herse tells the agent about where it is ([`NOTEBOOK`], [`TABLE`], [`FOLDER`]).
    pub system: &'static str,
    /// The conversation to continue.
    pub resume: Option<String>,
    pub cwd: PathBuf,
    /// The app's MCP socket, and the executable that bridges to it.
    pub socket: PathBuf,
    pub bridge: PathBuf,
}

/// A running agent. Dropping it doesn't stop it; [`Run::stop`] does.
pub struct Run {
    child: Arc<Mutex<Option<Child>>>,
}

impl Run {
    pub fn start(provider: Provider, req: Request, events: smol::channel::Sender<Event>) -> Result<Run, String> {
        let binary = provider
            .binary()
            .ok_or_else(|| format!("Herse couldn't find the {} command line tool.", provider.id()))?;
        let mut cmd = Command::new(&binary);
        if let Some(env) = herse_kernel::shell_env::get() {
            cmd.envs(env);
        }
        // Herse started from inside another agent's session (a terminal in
        // Claude Code) must not hand that session's identity and endpoint to
        // the agent it starts: the CLI signs in as it does in Terminal.
        if std::env::var_os("CLAUDECODE").is_some() {
            for (key, _) in std::env::vars_os() {
                if key.to_str().is_some_and(host_session_var) {
                    cmd.env_remove(key);
                }
            }
        }
        let bridge = req.bridge.to_string_lossy().into_owned();
        let socket = req.socket.to_string_lossy().into_owned();
        let prompt = match provider {
            Provider::Claude => {
                let config = json!({
                    "mcpServers": { "herse": { "type": "stdio", "command": bridge, "args": ["--mcp-bridge", socket] } }
                });
                cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"])
                    .arg("--mcp-config")
                    .arg(config.to_string())
                    // Read-only looks around the folder, and Herse's tools.
                    .args(["--tools", "Read,Glob,Grep", "--allowedTools", "mcp__herse,Read,Glob,Grep"])
                    .arg("--append-system-prompt")
                    .arg(req.system);
                if let Some(id) = &req.resume {
                    cmd.arg("--resume").arg(id);
                }
                if let Some(model) = crate::settings::assistant_model("claude") {
                    cmd.arg("--model").arg(model);
                }
                // A run waits for the reader's answer: give the call a day.
                cmd.env("MCP_TOOL_TIMEOUT", "86400000");
                req.prompt
            }
            Provider::Codex => {
                cmd.args(["exec", "--json", "--skip-git-repo-check"])
                    .arg("-c")
                    .arg(format!("mcp_servers.herse.command={}", toml_string(&bridge)))
                    .arg("-c")
                    .arg(format!("mcp_servers.herse.args=[\"--mcp-bridge\", {}]", toml_string(&socket)))
                    // Herse's tools need no approval from Codex: Herse asks the reader
                    // itself before a run (edits are undoable). A call waits for that
                    // answer, so it may take as long as the reader does. Codex's own
                    // shell stays read-only.
                    .args(["-c", "mcp_servers.herse.default_tools_approval_mode=\"approve\""])
                    .args(["-c", "mcp_servers.herse.tool_timeout_sec=86400"])
                    .args(["-c", "approval_policy=\"never\"", "-c", "sandbox_mode=\"read-only\""]);
                if let Some(model) = crate::settings::assistant_model("codex") {
                    cmd.arg("-m").arg(model);
                }
                if let Some(id) = &req.resume {
                    cmd.arg("resume").arg(id);
                }
                cmd.arg("-");
                // Codex has no extra-instructions flag: the first question carries them.
                if req.resume.is_some() { req.prompt } else { format!("{}\n\n---\n\n{}", req.system, req.prompt) }
            }
        };
        cmd.current_dir(&req.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", binary.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(Some(child)));
        let waiter = child.clone();
        std::thread::Builder::new()
            .name(format!("herse-agent-{}", provider.id()))
            .spawn(move || {
                let err = std::thread::spawn(move || {
                    let mut text = String::new();
                    if let Some(mut s) = stderr {
                        let _ = s.read_to_string(&mut text);
                    }
                    text
                });
                let mut parser = Parser::new(provider);
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        for event in parser.line(&line) {
                            let _ = events.send_blocking(event);
                        }
                    }
                }
                let stderr = err.join().unwrap_or_default();
                let status = waiter.lock().ok().and_then(|mut c| c.take()).and_then(|mut c| c.wait().ok());
                if !parser.done {
                    let failed = status.is_none_or(|s| !s.success());
                    let why = parser.last_error.take().or_else(|| {
                        let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
                        (!tail.is_empty()).then(|| tail[tail.len().saturating_sub(12)..].join("\n"))
                    });
                    let _ = events.send_blocking(Event::Done(match (failed, why) {
                        (true, Some(why)) => Some(why),
                        (true, None) => Some("It stopped without saying why.".into()),
                        (false, _) => None,
                    }));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Run { child })
    }

    /// Stop the agent (its tool calls in flight are abandoned).
    pub fn stop(&self) {
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
    }
}

/// A variable an agent session sets for the processes it runs.
fn host_session_var(key: &str) -> bool {
    key == "CLAUDECODE"
        || key == "CLAUDE_PID"
        || key == "CLAUDE_EFFORT"
        || key == "ANTHROPIC_BASE_URL"
        || key.starts_with("CLAUDE_CODE_")
        || key.starts_with("CLAUDE_AGENT_SDK_")
        || key.starts_with("CLAUDE_PREVIEW_")
        || (key.starts_with("USE_") && key.ends_with("_OAUTH"))
}

fn toml_string(s: &str) -> String {
    // A JSON string is a TOML basic string.
    Value::String(s.to_string()).to_string()
}

/// Turns a CLI's JSON lines into events.
struct Parser {
    provider: Provider,
    done: bool,
    last_error: Option<String>,
}

impl Parser {
    fn new(provider: Provider) -> Parser {
        Parser { provider, done: false, last_error: None }
    }

    fn line(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        let mut out = Vec::new();
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match self.provider {
            Provider::Claude => match kind {
                "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                    if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                        out.push(Event::Session(id.into()));
                    }
                    let herse = v
                        .get("mcp_servers")
                        .and_then(Value::as_array)
                        .and_then(|s| s.iter().find(|s| s.get("name").and_then(Value::as_str) == Some("herse")));
                    if let Some(status) = herse.and_then(|s| s.get("status")).and_then(Value::as_str)
                        && status != "connected"
                    {
                        out.push(Event::Activity(format!("Herse's tools didn't connect ({status}).")));
                    }
                }
                "assistant" => {
                    let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let text = block.get("text").and_then(Value::as_str).unwrap_or("").trim();
                                if !text.is_empty() {
                                    out.push(Event::Say(text.into()));
                                }
                            }
                            Some("tool_use") => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                                if !name.starts_with("mcp__herse__") {
                                    out.push(Event::Activity(describe_builtin(name, block.get("input"))));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "result" => {
                    self.done = true;
                    let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                        || v.get("subtype").and_then(Value::as_str).is_some_and(|s| s != "success");
                    let why = v
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| v.get("subtype").and_then(Value::as_str))
                        .unwrap_or("It failed.")
                        .to_string();
                    out.push(Event::Done(failed.then_some(why)));
                }
                _ => {}
            },
            Provider::Codex => match kind {
                "thread.started" => {
                    if let Some(id) = v.get("thread_id").and_then(Value::as_str) {
                        out.push(Event::Session(id.into()));
                    }
                }
                "item.completed" | "item.started" => {
                    let item = v.get("item").cloned().unwrap_or(Value::Null);
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    match (kind, item_type) {
                        ("item.completed", "agent_message") => {
                            let text = item.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !text.is_empty() {
                                out.push(Event::Say(text.into()));
                            }
                        }
                        ("item.started", "command_execution") => {
                            let command = item.get("command").and_then(Value::as_str).unwrap_or("a command");
                            out.push(Event::Activity(format!("Running {}", short(command, 60))));
                        }
                        ("item.completed", "error") => {
                            if let Some(m) = item.get("message").and_then(Value::as_str) {
                                self.last_error = Some(m.into());
                            }
                        }
                        _ => {}
                    }
                }
                "turn.completed" => {
                    self.done = true;
                    out.push(Event::Done(None));
                }
                "turn.failed" => {
                    self.done = true;
                    let why = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("It failed.");
                    out.push(Event::Done(Some(plain_error(why))));
                }
                "error" => {
                    if let Some(m) = v.get("message").and_then(Value::as_str) {
                        self.last_error = Some(plain_error(m));
                    }
                }
                _ => {}
            },
        }
        out
    }
}

/// An API error's message, when the CLI passes the whole JSON body along.
fn plain_error(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| message.to_string())
}

/// "Reading data.csv" for a built-in tool call.
fn describe_builtin(name: &str, input: Option<&Value>) -> String {
    let arg = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("");
    let file = |k: &str| Path::new(arg(k)).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    match name {
        "Read" => format!("Reading {}", file("file_path")),
        "Glob" => format!("Looking for {}", short(arg("pattern"), 50)),
        "Grep" => format!("Searching for {}", short(arg("pattern"), 50)),
        other => format!("Using {other}"),
    }
}

fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_stream_json() {
        let mut p = Parser::new(Provider::Claude);
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"abc","mcp_servers":[{"name":"herse","status":"connected"}]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"mcp__herse__overview","input":{}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/a/data.csv"}}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done."}"#,
        ];
        let events: Vec<Event> = lines.iter().flat_map(|l| p.line(l)).collect();
        assert_eq!(
            events,
            [
                Event::Session("abc".into()),
                Event::Say("Let me look.".into()),
                Event::Activity("Reading data.csv".into()),
                Event::Done(None),
            ]
        );
    }

    #[test]
    fn reads_codex_json() {
        let mut p = Parser::new(Provider::Codex);
        let lines = [
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"item.started","item":{"id":"1","type":"command_execution","command":"ls"}}"#,
            r#"{"type":"item.completed","item":{"id":"2","type":"agent_message","text":"Fixed it."}}"#,
            r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#,
        ];
        let events: Vec<Event> = lines.iter().flat_map(|l| p.line(l)).collect();
        assert_eq!(
            events,
            [
                Event::Session("t1".into()),
                Event::Activity("Running ls".into()),
                Event::Say("Fixed it.".into()),
                Event::Done(Some("usage limit".into())),
            ]
        );
    }
}
