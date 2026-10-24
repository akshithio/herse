//! Herse's tools for coding agents, over MCP.
//!
//! An agent CLI (Claude Code, Codex) starts each MCP server as a subprocess
//! that speaks JSON-RPC on stdin and stdout. Herse's server has to live in
//! the app, beside the open notebook, so that subprocess is Herse itself in
//! bridge mode (`herse --mcp-bridge <socket>`): it copies stdio to and from a
//! Unix socket the app listens on, and the app answers. The socket is the
//! user's alone (mode 0600, in their temporary folder) and lives as long as
//! the conversation.

use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::oneshot;
use serde_json::{Value, json};

/// A tool call for the app to answer: the reply is the text the agent reads,
/// or an error it reads as one.
pub struct Call {
    pub tool: String,
    pub args: Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// The app's end of the socket. Dropping it stops listening and removes the
/// socket.
pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Listen for bridges; each `tools/call` arrives on `calls`.
    pub fn start(tools: Value, instructions: &'static str, calls: smol::channel::Sender<Call>) -> io::Result<Server> {
        let dir = std::env::temp_dir();
        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ (std::process::id() as u64) << 32;
        let path = dir.join(format!("herse-agent-{nonce:x}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let tools = Arc::new(tools);
        let stopped = stop.clone();
        std::thread::Builder::new().name("herse-mcp".into()).spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (tools, calls) = (tools.clone(), calls.clone());
                std::thread::spawn(move || serve(stream, &tools, instructions, &calls));
            }
        })?;
        Ok(Server { path, stop })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it sees the flag.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One bridge's session: newline-delimited JSON-RPC until it hangs up.
fn serve(stream: UnixStream, tools: &Value, instructions: &str, calls: &smol::channel::Sender<Call>) {
    let Ok(read) = stream.try_clone() else { return };
    let mut write = stream;
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Option<Result<Value, (i64, String)>> = match method {
            "initialize" => Some(Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "herse", "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions,
            }))),
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(Ok(json!({ "tools": tools }))),
            "tools/call" => {
                let tool = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (reply, answer) = oneshot::channel();
                let answer = if calls.send_blocking(Call { tool, args, reply }).is_ok() {
                    futures::executor::block_on(answer).unwrap_or_else(|_| Err("Herse closed the notebook.".into()))
                } else {
                    Err("Herse closed the notebook.".into())
                };
                let (text, is_error) = match answer {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                Some(Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })))
            }
            m if m.starts_with("notifications/") => None,
            _ => id.is_some().then(|| Err((-32601, format!("Herse doesn't handle {method}")))),
        };
        let (Some(id), Some(result)) = (id, result) else { continue };
        let response = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        };
        let mut out = serde_json::to_vec(&response).unwrap_or_default();
        out.push(b'\n');
        if write.write_all(&out).is_err() {
            break;
        }
    }
}

/// `herse --mcp-bridge <socket>`: what the agent CLI starts. Copies stdin to
/// the socket and the socket to stdout until either side closes.
pub fn bridge(socket: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    let mut from_app = stream.try_clone()?;
    let out = std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        let _ = io::copy(&mut from_app, &mut stdout);
        let _ = stdout.flush();
    });
    let mut to_app = &stream;
    io::copy(&mut io::stdin().lock(), &mut to_app)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = out.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_json_rpc_over_the_socket() {
        let (tx, rx) = smol::channel::unbounded::<Call>();
        let tools = json!([{ "name": "echo", "description": "Echo", "inputSchema": { "type": "object" } }]);
        let server = Server::start(tools, "Be brief.", tx).unwrap();
        std::thread::spawn(move || {
            while let Ok(call) = rx.recv_blocking() {
                let _ = call.reply.send(Ok(format!("{}:{}", call.tool, call.args)));
            }
        });
        let stream = UnixStream::connect(server.path()).unwrap();
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        fn ask(w: &mut UnixStream, r: &mut BufReader<UnixStream>, msg: Value) -> Value {
            w.write_all(format!("{msg}\n").as_bytes()).unwrap();
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap()
        }
        let init = ask(&mut w, &mut r, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } }));
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["instructions"], "Be brief.");
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").unwrap();
        let list = ask(&mut w, &mut r, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
        let call = ask(&mut w, &mut r, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "echo", "arguments": { "x": 1 } } }));
        assert_eq!(call["id"], 3);
        assert_eq!(call["result"]["content"][0]["text"], "echo:{\"x\":1}");
        assert_eq!(call["result"]["isError"], false);
        let unknown = ask(&mut w, &mut r, json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }));
        assert_eq!(unknown["error"]["code"], -32601);
        let path = server.path().to_path_buf();
        drop(server);
        assert!(!path.exists());
    }
}
