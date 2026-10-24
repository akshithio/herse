//! A tiny HTTP/1.1 server on 127.0.0.1 for tests (nothing leaves the
//! machine), plus a stateful fake of the GitHub gist API built on it.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Path and query, e.g. `/gists/abc/comments?per_page=100`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }

    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        let query = self.path.split_once('?')?.1;
        query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == key).then_some(v)
        })
    }
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Option<Duration>,
}

impl Reply {
    pub fn json(status: u16, value: Value) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: value.to_string(),
            delay: None,
        }
    }

    pub fn raw(status: u16, body: &str) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: body.to_string(),
            delay: None,
        }
    }

    pub fn empty(status: u16) -> Reply {
        Reply::raw(status, "")
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn delay(mut self, delay: Duration) -> Reply {
        self.delay = Some(delay);
        self
    }
}

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

pub struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (requests, handler) = (requests.clone(), handler.clone());
                    std::thread::spawn(move || serve(stream, &requests, &*handler));
                }
            })
        };
        MockServer {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// `METHOD /path` of every request so far.
    pub fn log(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path_only()))
            .collect()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(stream: TcpStream, requests: &Mutex<Vec<Request>>, handler: &Handler) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return; // the wake-up connection from Drop
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            return;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if let Some(len) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        reader.read_exact(&mut body).unwrap();
    } else if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    }
    let request = Request {
        method,
        path,
        headers,
        body: String::from_utf8(body).unwrap(),
    };
    requests.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    if let Some(delay) = reply.delay {
        std::thread::sleep(delay);
    }
    let mut out = format!("HTTP/1.1 {} Mock\r\n", reply.status);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !reply.body.is_empty() {
        out.push_str("Content-Type: application/json; charset=utf-8\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    out.push_str(&reply.body);
    let mut stream = stream;
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// A fake GitHub: users, gists with revisions, comments with pagination.

#[derive(Clone, Debug)]
pub struct FakeGist {
    pub owner: String,
    pub public: bool,
    pub description: String,
    pub files: BTreeMap<String, String>,
    pub revisions: usize,
    pub comments: Vec<FakeComment>,
}

#[derive(Clone, Debug)]
pub struct FakeComment {
    pub id: u64,
    pub author: String,
    pub body: String,
}

#[derive(Debug)]
pub struct State {
    /// The login of the token's user.
    pub login: String,
    /// `X-OAuth-Scopes`; `None` = fine-grained token (no header).
    pub scopes: Option<String>,
    pub gists: BTreeMap<String, FakeGist>,
    pub next_id: u64,
}

pub struct FakeGitHub {
    pub state: Arc<Mutex<State>>,
    pub server: MockServer,
}

pub const FAKE_TOKEN: &str = "gho_herseTestToken0123456789abcdefABCDEF";

impl FakeGitHub {
    pub fn start(login: &str) -> FakeGitHub {
        let state = Arc::new(Mutex::new(State {
            login: login.to_string(),
            scopes: Some("gist, read:org, repo".into()),
            gists: BTreeMap::new(),
            next_id: 1,
        }));
        let shared = state.clone();
        let server = MockServer::start(move |req| handle(&mut shared.lock().unwrap(), req));
        FakeGitHub { state, server }
    }

    pub fn client(&self) -> herse_share::GistClient {
        herse_share::GistClient::with_base_url(
            herse_share::Token::new(FAKE_TOKEN).unwrap(),
            &self.server.url(),
        )
    }

    pub fn gist(&self, id: &str) -> Option<FakeGist> {
        self.state.lock().unwrap().gists.get(id).cloned()
    }

    /// Insert a gist directly (e.g. owned by someone else).
    pub fn insert(&self, owner: &str, public: bool, files: &[(&str, &str)]) -> String {
        let mut st = self.state.lock().unwrap();
        let id = format!("{:032x}", st.next_id);
        st.next_id += 1;
        st.gists.insert(
            id.clone(),
            FakeGist {
                owner: owner.to_string(),
                public,
                description: String::new(),
                files: files
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                revisions: 1,
                comments: vec![],
            },
        );
        id
    }
}

pub fn gist_json(id: &str, g: &FakeGist) -> Value {
    let files: serde_json::Map<String, Value> = g
        .files
        .iter()
        .map(|(name, content)| {
            (
                name.clone(),
                json!({
                    "filename": name,
                    "type": "text/plain",
                    "language": "Jupyter Notebook",
                    "raw_url": format!("https://gist.githubusercontent.com/{}/{id}/raw/0123abcd/{name}", g.owner),
                    "size": content.len(),
                    "truncated": false,
                    "content": content,
                }),
            )
        })
        .collect();
    let history: Vec<Value> = (0..g.revisions)
        .map(|i| json!({"version": format!("{i:040x}"), "committed_at": "2026-10-16T09:00:00Z", "change_status": {}}))
        .collect();
    json!({
        "url": format!("https://api.github.com/gists/{id}"),
        "id": id,
        "html_url": format!("https://gist.github.com/{id}"),
        "files": files,
        "public": g.public,
        "created_at": "2026-10-16T09:00:00Z",
        "updated_at": format!("2026-10-16T09:{:02}:00Z", g.revisions.min(59)),
        "description": g.description,
        "comments": g.comments.len(),
        "user": null,
        "owner": {"login": g.owner, "id": 1, "avatar_url": "https://avatars.example/u/1"},
        "truncated": false,
        "history": history,
    })
}

fn comment_json(c: &FakeComment) -> Value {
    json!({
        "id": c.id,
        "url": format!("https://api.github.com/gists/x/comments/{}", c.id),
        "body": c.body,
        "user": {"login": c.author, "id": 2, "avatar_url": format!("https://avatars.example/{}", c.author)},
        "author_association": if c.author == "octocat" { "OWNER" } else { "NONE" },
        "created_at": "2026-10-16T10:00:00Z",
        "updated_at": "2026-10-16T10:00:00Z",
    })
}

fn not_found() -> Reply {
    Reply::json(
        404,
        json!({"message": "Not Found", "documentation_url": "https://docs.github.com/rest"}),
    )
}

fn handle(st: &mut State, req: &Request) -> Reply {
    if req.header("authorization") != Some(&format!("Bearer {FAKE_TOKEN}")) {
        return Reply::json(401, json!({"message": "Bad credentials"}));
    }
    let segments: Vec<&str> = req.path_only().trim_start_matches('/').split('/').collect();
    let scopes = st.scopes.clone();
    let with_scopes = |r: Reply| match &scopes {
        Some(s) => r.header("X-OAuth-Scopes", s),
        None => r,
    };
    let reply = match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["user"]) => Reply::json(
            200,
            json!({"login": st.login, "id": 1, "name": "The Octocat", "avatar_url": "https://avatars.example/u/1", "html_url": format!("https://github.com/{}", st.login)}),
        ),
        ("POST", ["gists"]) => {
            let body = req.json();
            let files: BTreeMap<String, String> = body["files"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v["content"].as_str().unwrap().to_string()))
                .collect();
            if files.values().any(|c| c.is_empty()) {
                return with_scopes(Reply::json(
                    422,
                    json!({"message": "Validation Failed", "errors": [{"resource": "Gist", "code": "missing_field", "field": "files"}]}),
                ));
            }
            let id = format!("{:032x}", st.next_id);
            st.next_id += 1;
            let gist = FakeGist {
                owner: st.login.clone(),
                public: body["public"].as_bool().unwrap(),
                description: body["description"].as_str().unwrap_or("").to_string(),
                files,
                revisions: 1,
                comments: vec![],
            };
            let reply = Reply::json(201, gist_json(&id, &gist));
            st.gists.insert(id, gist);
            reply
        }
        ("GET", ["gists", id]) => match st.gists.get(*id) {
            Some(g) => Reply::json(200, gist_json(id, g)),
            None => not_found(),
        },
        ("PATCH", ["gists", id]) => {
            let login = st.login.clone();
            match st.gists.get_mut(*id) {
                Some(g) if g.owner == login => {
                    let body = req.json();
                    if let Some(d) = body["description"].as_str() {
                        g.description = d.to_string();
                    }
                    for (key, file) in body["files"].as_object().unwrap() {
                        let content = file["content"].as_str().map(str::to_string);
                        let old = g.files.remove(key);
                        let name = file["filename"].as_str().unwrap_or(key).to_string();
                        g.files.insert(name, content.or(old).unwrap_or_default());
                    }
                    g.revisions += 1;
                    Reply::json(200, gist_json(id, g))
                }
                _ => not_found(),
            }
        }
        ("DELETE", ["gists", id]) => {
            let login = st.login.clone();
            match st.gists.get(*id) {
                Some(g) if g.owner == login => {
                    st.gists.remove(*id);
                    Reply::empty(204)
                }
                _ => not_found(),
            }
        }
        ("GET", ["gists", id, "comments"]) => match st.gists.get(*id) {
            Some(g) => {
                let per_page: usize = req
                    .query("per_page")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30);
                let page: usize = req.query("page").and_then(|v| v.parse().ok()).unwrap_or(1);
                let items: Vec<Value> = g
                    .comments
                    .iter()
                    .skip((page - 1) * per_page)
                    .take(per_page)
                    .map(comment_json)
                    .collect();
                let mut reply = Reply::json(200, Value::Array(items));
                let last = g.comments.len().div_ceil(per_page).max(1);
                if page < last {
                    let base =
                        format!("http://{}/gists/{id}/comments", req.header("host").unwrap());
                    reply = reply.header(
                        "Link",
                        &format!(
                            "<{base}?per_page={per_page}&page={}>; rel=\"next\", <{base}?per_page={per_page}&page={last}>; rel=\"last\"",
                            page + 1
                        ),
                    );
                }
                reply
            }
            None => not_found(),
        },
        ("POST", ["gists", id, "comments"]) => {
            let login = st.login.clone();
            let next = st.next_id;
            match st.gists.get_mut(*id) {
                Some(g) => {
                    let c = FakeComment {
                        id: next,
                        author: login,
                        body: req.json()["body"].as_str().unwrap().into(),
                    };
                    g.comments.push(c.clone());
                    st.next_id += 1;
                    Reply::json(201, comment_json(&c))
                }
                None => not_found(),
            }
        }
        ("PATCH", ["gists", id, "comments", cid]) => {
            let login = st.login.clone();
            let cid: u64 = cid.parse().unwrap();
            match st
                .gists
                .get_mut(*id)
                .and_then(|g| g.comments.iter_mut().find(|c| c.id == cid))
            {
                Some(c) if c.author == login => {
                    c.body = req.json()["body"].as_str().unwrap().into();
                    Reply::json(200, comment_json(c))
                }
                _ => not_found(),
            }
        }
        ("DELETE", ["gists", id, "comments", cid]) => {
            let login = st.login.clone();
            let cid: u64 = cid.parse().unwrap();
            match st.gists.get_mut(*id) {
                Some(g)
                    if g.comments
                        .iter()
                        .any(|c| c.id == cid && (c.author == login || g.owner == login)) =>
                {
                    g.comments.retain(|c| c.id != cid);
                    Reply::empty(204)
                }
                _ => not_found(),
            }
        }
        _ => not_found(),
    };
    with_scopes(reply)
}

/// A minimal notebook with a code cell and an output.
pub fn sample_notebook() -> herse_nb::Notebook {
    let text = r##"{
 "cells": [
  {"cell_type": "markdown", "id": "m1", "metadata": {}, "source": ["# Results"]},
  {"cell_type": "code", "execution_count": 1, "id": "c1", "metadata": {},
   "outputs": [{"name": "stdout", "output_type": "stream", "text": ["42\n"]}],
   "source": ["print(6 * 7)"]}
 ],
 "metadata": {"kernelspec": {"display_name": "Python 3", "language": "python", "name": "python3"}},
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;
    herse_nb::parse(text.as_bytes()).unwrap()
}
