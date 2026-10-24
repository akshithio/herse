//! Extensions: programs that add to Herse, in any language. An extension is
//! a folder in `~/Library/Application Support/Herse/extensions/` holding a
//! `herse-extension.json` manifest and whatever it runs. Herse starts it
//! when first needed and speaks JSON-RPC 2.0 with it over stdin and stdout,
//! one message per line, in both directions:
//!
//! - Herse asks it to run its commands (listed in ⌘K), to draw output types
//!   it declares (as SVG, PNG, Markdown or text), and tells it about events
//!   (a notebook opened, saved, a cell ran).
//! - It asks Herse about the notebook in front and changes it: the same
//!   operations the assistant has (read, edit, insert, delete, run cells,
//!   evaluate code in the kernel), plus messages for the status line.
//!
//! There is no store: the folder is the install. Nothing in it runs until
//! you've allowed it once: Herse shows the command it would run and asks,
//! and remembers the answer by folder and fingerprint (the manifest plus the
//! files `run` names), so an extension that changes asks again. Allowed, it
//! runs as you. EXTENSIONS.md is the reference.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::oneshot;
use gpui_kit::{AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity};
use herse_nb::{CellType, MimeBundle, MimeData};
use serde_json::{Value, json};

use crate::notebook::NotebookView;

/// The protocol version Herse speaks.
pub const API_VERSION: u64 = 1;
const MANIFEST: &str = "herse-extension.json";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Run an extension's command (from ⌘K).
#[derive(gpui_kit::Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = herse, no_json)]
pub struct RunExtensionCommand {
    pub extension: String,
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
    pub run: Vec<String>,
    pub commands: Vec<(String, String)>,
    pub renderers: Vec<(String, String)>,
    pub events: Vec<String>,
    /// What an answer to "may it run?" holds for (see [`fingerprint`]).
    pub fingerprint: String,
}

impl Manifest {
    fn parse(dir: &Path, text: &str) -> Result<Manifest, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("{MANIFEST} isn't valid JSON: {e}"))?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let id = s("id").ok_or("it has no \"id\"")?;
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            return Err(format!("\"id\" must be letters, digits, '-', '_' or '.' (it's {id:?})"));
        }
        let api = v.get("api").and_then(Value::as_u64).unwrap_or(1);
        if api > API_VERSION {
            return Err(format!("it needs extension API {api}; this Herse speaks {API_VERSION}"));
        }
        let run: Vec<String> = v
            .get("run")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .filter(|r: &Vec<String>| !r.is_empty())
            .ok_or("it has no \"run\" (the command that starts it, e.g. [\"python3\", \"main.py\"])")?;
        let pairs = |k: &str, a: &str, b: &str| -> Vec<(String, String)> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| Some((i.get(a)?.as_str()?.to_string(), i.get(b)?.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(Manifest {
            fingerprint: fingerprint(dir, text.as_bytes(), &run),
            name: s("name").unwrap_or_else(|| id.clone()),
            version: s("version").unwrap_or_else(|| "0.0.0".into()),
            id,
            dir: dir.to_path_buf(),
            run,
            commands: pairs("commands", "id", "title"),
            renderers: pairs("renderers", "mime", "title"),
            events: v
                .get("events")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        })
    }

    fn load(dir: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(dir.join(MANIFEST)).map_err(|e| format!("{}: {e}", dir.display()))?;
        Manifest::parse(dir, &text).map_err(|why| format!("{}: {why}", dir.display()))
    }

    /// `run` as you'd type it: `python3 main.py`.
    pub fn command_line(&self) -> String {
        self.run
            .iter()
            .map(|a| {
                let plain = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
                if plain { a.clone() } else { format!("'{}'", a.replace('\'', "'\\''")) }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Its folder, with `~` for home.
    pub fn folder_label(&self) -> String {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match home.as_ref().and_then(|h| self.dir.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => self.dir.display().to_string(),
        }
    }

    /// The key its answer is remembered under in settings.json.
    fn trust_key(&self) -> String {
        std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone()).display().to_string()
    }
}

/// What an answer to "may it run?" holds for: the manifest, and each file in
/// the extension's folder that `run` names (`main.py` of `python3 main.py`,
/// `./server`). Other files it loads aren't covered.
fn fingerprint(dir: &Path, manifest: &[u8], run: &[String]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    let mut part = |name: &[u8], bytes: &[u8]| {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    part(MANIFEST.as_bytes(), manifest);
    for arg in run {
        // (Reading a folder fails, so only files count.)
        if Path::new(arg).is_relative()
            && let Ok(bytes) = std::fs::read(dir.join(arg))
        {
            part(arg.as_bytes(), &bytes);
        }
    }
    format!("{:x}", h.finalize())
}

/// Whether an extension may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Allowed, as it is now.
    Allowed,
    /// Told not to run (Don't Allow, or revoked in Settings).
    Denied,
    /// Never asked about.
    New,
    /// Answered for a different manifest or run file: asks again.
    Changed,
}

/// `m`'s standing, given the answers in settings.json.
fn trust_of(m: &Manifest, answers: &HashMap<String, (String, bool)>) -> Trust {
    match answers.get(&m.trust_key()) {
        Some((hash, true)) if *hash == m.fingerprint => Trust::Allowed,
        Some((hash, false)) if *hash == m.fingerprint => Trust::Denied,
        Some(_) => Trust::Changed,
        None => Trust::New,
    }
}

/// An extension waiting to hear whether it may run, and the ⌘K command to
/// run once it may.
#[derive(Clone)]
pub struct Ask {
    pub extension: String,
    pub command: Option<String>,
}

/// The question above the page: "Notebook Tools wants to run: …".
pub struct AskView {
    pub extension: String,
    pub name: String,
    pub command_line: String,
    pub folder: PathBuf,
    pub folder_label: String,
    /// It was answered before, for a different manifest or run file.
    pub changed: bool,
    /// The ⌘K command waiting on the answer.
    pub command: Option<String>,
}

/// An installed extension, for Settings ▸ Extensions….
pub struct Listed {
    pub id: String,
    pub name: String,
    pub version: String,
    pub command_line: String,
    pub trust: Trust,
}

/// Where extensions live (`HERSE_EXTENSIONS_DIR` overrides, for tests).
pub fn folder() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HERSE_EXTENSIONS_DIR") {
        return Some(dir.into());
    }
    if std::env::var_os("HERSE_NO_RECENTS").is_some() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Herse/extensions"))
}

/// The extensions in the folder, and why any couldn't load.
pub fn discover() -> (Vec<Manifest>, Vec<String>) {
    let (mut found, mut problems) = (Vec::new(), Vec::new());
    let Some(root) = folder() else { return (found, problems) };
    let Ok(entries) = std::fs::read_dir(&root) else { return (found, problems) };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let file = dir.join(MANIFEST);
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        match Manifest::parse(&dir, &text) {
            Ok(m) if found.iter().any(|f: &Manifest| f.id == m.id) => {
                problems.push(format!("{}: another extension already has the id {:?}", dir.display(), m.id))
            }
            Ok(m) => found.push(m),
            Err(why) => problems.push(format!("{}: {why}", dir.display())),
        }
    }
    (found, problems)
}

/// A drawing an extension made of an output.
#[derive(Clone)]
pub enum Rendered {
    Svg(String),
    Png(Vec<u8>),
    Markdown(String),
    Text(String),
}

#[derive(Clone)]
pub enum RenderState {
    Pending,
    Done(Rendered),
    Failed(String),
    /// The extension that draws it hasn't been allowed to run: `command` is
    /// what it would run, and where.
    NeedsApproval { extension: String, command: String },
}

/// Answers awaited from an extension, by request id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A running extension.
struct Process {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    pending: Pending,
    next_id: u64,
}

impl Process {
    fn alive(&self) -> bool {
        self.child.lock().ok().is_some_and(|mut c| matches!(c.try_wait(), Ok(None)))
    }

    fn send(&self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().map_err(|_| "stdin poisoned".to_string())?;
        stdin.write_all(&line).and_then(|_| stdin.flush()).map_err(|e| format!("it stopped listening ({e})"))
    }

    fn request(&mut self, method: &str, params: Value) -> oneshot::Receiver<Result<Value, String>> {
        let (tx, rx) = oneshot::channel();
        self.next_id += 1;
        let id = self.next_id;
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(why) = self.send(&message)
            && let Some(tx) = self.pending.lock().ok().and_then(|mut p| p.remove(&id))
        {
            let _ = tx.send(Err(why));
        }
        rx
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// A message an extension sent Herse.
struct Incoming {
    extension: String,
    id: Option<Value>,
    method: String,
    params: Value,
}

pub struct Host {
    manifests: Vec<Manifest>,
    pub problems: Vec<String>,
    /// Whether each extension may run, by id.
    trust: HashMap<String, Trust>,
    /// Extensions waiting for "may it run?", the one shown first.
    pub asks: Vec<Ask>,
    running: HashMap<String, Process>,
    /// The notebook requests act on: the one used last, in its window.
    target: Option<(AnyWindowHandle, WeakEntity<NotebookView>)>,
    renders: HashMap<u64, RenderState>,
    /// Bumped when a drawing arrives: notebooks rebuild outputs waiting on one.
    generation: u64,
    incoming: smol::channel::Sender<Incoming>,
    _incoming_task: Task<()>,
}

struct HostGlobal(Entity<Host>);
impl Global for HostGlobal {}

/// Something to show above the page that no one asked for: an extension
/// allowed from an output's note that then wouldn't start.
pub struct HostProblem {
    pub title: String,
    pub detail: String,
}

impl EventEmitter<HostProblem> for Host {}

/// Find the extensions and get ready to run them: at startup, and again
/// for Reload Extensions (which stops the running ones first).
pub fn init(cx: &mut App) {
    if let Some(host) = host(cx) {
        host.update(cx, |host, cx| host.reload(cx));
        return;
    }
    let host = cx.new(|cx| {
        let (tx, rx) = smol::channel::unbounded::<Incoming>();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                if this.update(cx, |host: &mut Host, cx| host.handle(message, cx)).is_err() {
                    break;
                }
            }
        });
        Host {
            manifests: Vec::new(),
            problems: Vec::new(),
            trust: HashMap::new(),
            asks: Vec::new(),
            running: HashMap::new(),
            target: None,
            renders: HashMap::new(),
            generation: 0,
            incoming: tx,
            _incoming_task: task,
        }
    });
    cx.set_global(HostGlobal(host.clone()));
    host.update(cx, |host, cx| host.reload(cx));
}

pub fn host(cx: &App) -> Option<Entity<Host>> {
    cx.try_global::<HostGlobal>().map(|g| g.0.clone())
}

/// ⌘K's rows for extension commands: "Word Count: Count Words", and
/// whether it may run (one that may not asks when chosen).
pub fn commands(cx: &App) -> Vec<(String, Box<dyn gpui_kit::Action>, bool)> {
    let Some(host) = host(cx) else { return Vec::new() };
    let host = host.read(cx);
    host.manifests
        .iter()
        .flat_map(|m| {
            let allowed = host.trust(&m.id) == Trust::Allowed;
            m.commands.iter().map(move |(id, title)| {
                (
                    format!("{}: {title}", m.name),
                    Box::new(RunExtensionCommand { extension: m.id.clone(), command: id.clone() }) as Box<dyn gpui_kit::Action>,
                    allowed,
                )
            })
        })
        .collect()
}

/// The extensions loaded, for Settings.
pub fn loaded(cx: &App) -> Vec<(String, String)> {
    host(cx).map_or_else(Vec::new, |h| h.read(cx).manifests.iter().map(|m| (m.name.clone(), m.version.clone())).collect())
}

/// Each extension and whether it may run, for Settings ▸ Extensions….
pub fn list(cx: &App) -> Vec<Listed> {
    let Some(host) = host(cx) else { return Vec::new() };
    let host = host.read(cx);
    host.manifests
        .iter()
        .map(|m| Listed {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            command_line: m.command_line(),
            trust: host.trust(&m.id),
        })
        .collect()
}

/// The question to show above the page, if one is waiting.
pub fn current_ask(cx: &App) -> Option<AskView> {
    let host = host(cx)?;
    let host = host.read(cx);
    let ask = host.asks.first()?;
    let m = host.manifest(&ask.extension)?;
    Some(AskView {
        extension: m.id.clone(),
        name: m.name.clone(),
        command_line: m.command_line(),
        folder: m.dir.clone(),
        folder_label: m.folder_label(),
        changed: host.trust(&m.id) == Trust::Changed,
        command: ask.command.clone(),
    })
}

/// The answer to "may it run?" (from the question above the page, an
/// output's note, or Settings). Allowed, an extension that listens for events
/// starts now; refused, a running one stops.
pub fn answer(id: &str, allow: bool, cx: &mut App) {
    if let Some(host) = host(cx) {
        host.update(cx, |host, cx| host.set_trust(id, allow, cx));
    }
}

/// The notebook extension requests act on.
pub fn set_target(window: AnyWindowHandle, notebook: &Entity<NotebookView>, cx: &mut App) {
    if let Some(host) = host(cx) {
        let weak = notebook.downgrade();
        host.update(cx, |h, _| h.target = Some((window, weak)));
    }
}

/// Tell the extensions that asked for it about `name`
/// (`notebook.opened`, `notebook.saved`, `cell.executed`).
/// `notebook` is [`describe`]'s account of it.
pub fn notify(name: &str, notebook: Value, extra: Value, cx: &mut App) {
    let Some(host) = host(cx) else { return };
    let params = json!({ "name": name, "notebook": notebook, "detail": extra });
    host.update(cx, |host, cx| {
        // Only to those allowed to run: the others hear nothing.
        let listeners: Vec<String> = host
            .manifests
            .iter()
            .filter(|m| m.events.iter().any(|e| e == name) && host.trust(&m.id) == Trust::Allowed)
            .map(|m| m.id.clone())
            .collect();
        for id in listeners {
            if host.ensure_running(&id, cx).is_ok()
                && let Some(p) = host.running.get(&id)
            {
                let _ = p.send(&json!({ "jsonrpc": "2.0", "method": "event", "params": params }));
            }
        }
    });
}

/// Is a drawing of this output wanted from an extension, and how far along
/// is it? Starts the request the first time it's asked.
pub fn render_state(data: &MimeBundle, dark: bool, cx: &mut App) -> Option<(SharedString, RenderState)> {
    let host = host(cx)?;
    let (ext, name, mime) = {
        let h = host.read(cx);
        h.manifests.iter().find_map(|m| {
            m.renderers
                .iter()
                .find(|(mime, _)| data.contains(mime))
                .map(|(mime, _)| (m.id.clone(), m.name.clone(), mime.clone()))
        })?
    };
    let payload = match data.get(&mime)? {
        MimeData::Json(v) => v.clone(),
        MimeData::Text(t) => Value::String(t.clone()),
    };
    let key = {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&ext, &mime, payload.to_string(), dark).hash(&mut h);
        h.finish()
    };
    let state = host.update(cx, |host, cx| {
        let blocked = |host: &Host| RenderState::NeedsApproval {
            extension: ext.clone(),
            command: host.manifest(&ext).map(|m| format!("{} in {}", m.command_line(), m.folder_label())).unwrap_or_default(),
        };
        if host.trust(&ext) != Trust::Allowed {
            return blocked(host);
        }
        if let Some(state) = host.renders.get(&key) {
            return state.clone();
        }
        // Changed on disk since it was allowed: it asks again first.
        if !host.ready(&ext, cx) {
            return blocked(host);
        }
        host.renders.insert(key, RenderState::Pending);
        let params = json!({ "mime": mime, "data": payload, "dark": dark });
        match host.request(&ext, "render", params, cx) {
            Ok(rx) => {
                cx.spawn(async move |this, cx| {
                    let answer = wait(rx).await;
                    this.update(cx, |host, cx| {
                        let state = match answer.and_then(|v| parse_rendered(&v)) {
                            Ok(r) => RenderState::Done(r),
                            Err(why) => RenderState::Failed(why),
                        };
                        host.renders.insert(key, state);
                        host.generation += 1;
                        cx.refresh_windows();
                    })
                    .ok();
                })
                .detach();
                RenderState::Pending
            }
            Err(why) => {
                host.renders.insert(key, RenderState::Failed(why.clone()));
                RenderState::Failed(why)
            }
        }
    });
    Some((name.into(), state))
}

/// Changes whenever an extension finishes a drawing.
pub fn render_generation(cx: &App) -> u64 {
    host(cx).map_or(0, |h| h.read(cx).generation)
}

fn parse_rendered(v: &Value) -> Result<Rendered, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    if let Some(svg) = s("svg") {
        return Ok(Rendered::Svg(svg));
    }
    if let Some(png) = s("png") {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(png.trim()).map_err(|e| format!("its PNG isn't base64: {e}"))?;
        return Ok(Rendered::Png(bytes));
    }
    if let Some(md) = s("markdown") {
        return Ok(Rendered::Markdown(md));
    }
    if let Some(text) = s("text") {
        return Ok(Rendered::Text(text));
    }
    Err("its answer has none of svg, png, markdown or text".into())
}

async fn wait(rx: oneshot::Receiver<Result<Value, String>>) -> Result<Value, String> {
    smol::future::or(async { rx.await.unwrap_or_else(|_| Err("it stopped before answering".into())) }, async {
        smol::Timer::after(TIMEOUT).await;
        Err(format!("it didn't answer within {} s", TIMEOUT.as_secs()))
    })
    .await
}

/// The notebook, as extensions see it.
pub fn describe(nb: &NotebookView) -> Value {
    json!({
        "path": nb.path().map(|p| p.display().to_string()),
        "title": nb.title(),
        "cells": nb.doc.nb.cells.len(),
    })
}

impl Host {
    fn manifest(&self, id: &str) -> Option<&Manifest> {
        self.manifests.iter().find(|m| m.id == id)
    }

    pub fn trust(&self, id: &str) -> Trust {
        self.trust.get(id).copied().unwrap_or(Trust::New)
    }

    /// Stop what's running, find the extensions again, and start the
    /// allowed ones that listen for events; a listener never asked about
    /// asks now. The others start on first use (and ask then).
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.running.clear();
        let (manifests, problems) = discover();
        for p in &problems {
            eprintln!("extension: {p}");
        }
        let answers = crate::settings::extension_trust();
        self.trust = manifests.iter().map(|m| (m.id.clone(), trust_of(m, &answers))).collect();
        self.manifests = manifests;
        self.problems = problems;
        self.asks.clear();
        self.renders.clear();
        self.generation += 1;
        let listeners: Vec<String> = self.manifests.iter().filter(|m| !m.events.is_empty()).map(|m| m.id.clone()).collect();
        for id in listeners {
            match self.trust(&id) {
                Trust::Allowed => {
                    if let Err(why) = self.ensure_running(&id, cx) {
                        self.problems.push(why);
                    }
                }
                Trust::Denied => {}
                Trust::New | Trust::Changed => self.ask(&id, None, cx),
            }
        }
        cx.notify();
        cx.refresh_windows();
    }

    /// May `id` run now? Allowed, and (when it isn't running already) still
    /// what was allowed: one whose manifest or run file changed on disk
    /// since is read again and asks.
    pub fn ready(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if self.trust(id) != Trust::Allowed {
            return false;
        }
        if self.running.get(id).is_some_and(Process::alive) {
            return true;
        }
        let Some(m) = self.manifest(id) else { return false };
        let now = Manifest::load(&m.dir);
        if now.as_ref().is_ok_and(|now| now.fingerprint == m.fingerprint) {
            return true;
        }
        match now {
            Ok(now) if now.id == id => {
                let trust = trust_of(&now, &crate::settings::extension_trust());
                if let Some(slot) = self.manifests.iter_mut().find(|m| m.id == id) {
                    *slot = now;
                }
                self.trust.insert(id.to_string(), trust);
                if trust == Trust::Allowed {
                    // Changed back to a version that was allowed.
                    return true;
                }
                if trust != Trust::Denied {
                    self.ask(id, None, cx);
                }
            }
            // Broken now, or another extension: it waits for Reload Extensions.
            other => {
                self.trust.insert(id.to_string(), Trust::Changed);
                let name = self.manifest(id).map_or_else(|| id.to_string(), |m| m.name.clone());
                let why = match other {
                    Ok(now) => format!("Its id is {:?} now.", now.id),
                    Err(why) => why,
                };
                cx.emit(HostProblem {
                    title: format!("{name} changed and can't load"),
                    detail: format!("{why}\n⌘K ▸ Reload Extensions loads it again."),
                });
            }
        }
        self.generation += 1;
        cx.notify();
        cx.refresh_windows();
        false
    }

    /// Ask whether `id` may run, above the page; `command` runs once it may.
    /// A question with a command goes first: the reader just chose it.
    pub fn ask(&mut self, id: &str, command: Option<String>, cx: &mut Context<Self>) {
        let old = self.asks.iter().position(|a| a.extension == id).map(|ix| self.asks.remove(ix));
        let ask = Ask { extension: id.to_string(), command: command.clone().or(old.and_then(|a| a.command)) };
        if command.is_some() {
            self.asks.insert(0, ask);
        } else {
            self.asks.push(ask);
        }
        cx.notify();
    }

    /// Put the question away unanswered: it asks again next time it's needed.
    pub fn dismiss_ask(&mut self, id: &str, cx: &mut Context<Self>) {
        self.asks.retain(|a| a.extension != id);
        cx.notify();
    }

    /// Remember whether `id` may run, for this manifest and run file.
    fn set_trust(&mut self, id: &str, allow: bool, cx: &mut Context<Self>) {
        let Some(m) = self.manifest(id).cloned() else { return };
        crate::settings::set_extension_trust(&m.trust_key(), &m.fingerprint, allow);
        self.trust.insert(id.to_string(), if allow { Trust::Allowed } else { Trust::Denied });
        self.asks.retain(|a| a.extension != id);
        if allow {
            // Drawings that failed get another try.
            self.renders.retain(|_, state| !matches!(state, RenderState::Failed(_)));
            if !m.events.is_empty()
                && let Err(why) = self.ensure_running(id, cx)
            {
                cx.emit(HostProblem { title: format!("{} couldn't start", m.name), detail: why });
            }
        } else {
            // Refused (or revoked): it stops now.
            self.running.remove(id);
        }
        // Outputs it draws, or would, are built again.
        self.generation += 1;
        cx.notify();
        cx.refresh_windows();
    }

    /// Start the extension if it isn't running. Only one allowed to run
    /// starts (see [`Host::ready`]).
    fn ensure_running(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if !self.ready(id, cx) {
            let name = self.manifest(id).map_or(id, |m| m.name.as_str());
            return Err(format!("{name} isn't allowed to run"));
        }
        if self.running.get(id).is_some_and(Process::alive) {
            return Ok(());
        }
        self.running.remove(id);
        let m = self.manifest(id).cloned().ok_or_else(|| format!("no extension called {id}"))?;
        let program = {
            let p = Path::new(&m.run[0]);
            if p.is_relative() && m.dir.join(p).exists() { m.dir.join(p) } else { p.to_path_buf() }
        };
        let mut cmd = Command::new(&program);
        if let Some(env) = herse_kernel::shell_env::get() {
            cmd.envs(env);
        }
        cmd.args(&m.run[1..])
            .current_dir(&m.dir)
            .env("HERSE_EXTENSION_API", API_VERSION.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("{}: couldn't start {}: {e}", m.name, program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr_pipe = child.stderr.take();
        let pending: Pending = Default::default();
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = stderr_pipe {
            let tail = stderr.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut t) = tail.lock() {
                        t.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if t.len() > 8000 {
                            let cut = t.len() - 8000;
                            let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(cut);
                            t.drain(..cut);
                        }
                    }
                }
            });
        }
        {
            let (pending, incoming, ext, stderr) = (pending.clone(), self.incoming.clone(), m.id.clone(), stderr.clone());
            std::thread::Builder::new()
                .name(format!("herse-ext-{}", m.id))
                .spawn(move || {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                        if let Some(method) = msg.get("method").and_then(Value::as_str) {
                            let _ = incoming.send_blocking(Incoming {
                                extension: ext.clone(),
                                id: msg.get("id").cloned(),
                                method: method.to_string(),
                                params: msg.get("params").cloned().unwrap_or(Value::Null),
                            });
                        } else if let Some(id) = msg.get("id").and_then(Value::as_u64)
                            && let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id))
                        {
                            let answer = match msg.get("error") {
                                Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("it failed").to_string()),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(answer);
                        }
                    }
                    // It exited: everything still waiting fails, with its last words.
                    let why = stderr.lock().map(|t| t.trim().lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")).unwrap_or_default();
                    if let Ok(mut p) = pending.lock() {
                        for (_, tx) in p.drain() {
                            let _ = tx.send(Err(if why.is_empty() { "it exited".into() } else { format!("it exited:\n{why}") }));
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        let mut process = Process {
            stdin: Arc::new(Mutex::new(stdin)),
            child: Arc::new(Mutex::new(child)),
            pending,
            next_id: 0,
        };
        // Its first message: who's calling. The answer isn't waited for.
        drop(process.request(
            "initialize",
            json!({ "api": API_VERSION, "herse": env!("CARGO_PKG_VERSION"), "extension": { "id": m.id, "folder": m.dir } }),
        ));
        self.running.insert(m.id.clone(), process);
        Ok(())
    }

    fn request(
        &mut self,
        id: &str,
        method: &str,
        params: Value,
        cx: &mut Context<Self>,
    ) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
        self.ensure_running(id, cx)?;
        let p = self.running.get_mut(id).ok_or("it isn't running")?;
        Ok(p.request(method, params))
    }

    /// Run `command` of extension `id`: the status line gets what it says
    /// back (`{"message": "…"}`).
    pub fn run_command(&mut self, id: &str, command: &str, cx: &mut Context<Self>) -> Task<Result<Option<String>, String>> {
        let notebook = self.target.as_ref().and_then(|(_, nb)| nb.upgrade()).map(|nb| describe(nb.read(cx)));
        let name = self.manifest(id).map_or_else(|| id.to_string(), |m| m.name.clone());
        match self.request(id, "command.run", json!({ "command": command, "notebook": notebook }), cx) {
            Ok(rx) => cx.background_spawn(async move {
                let v = wait(rx).await.map_err(|why| format!("{name}: {why}"))?;
                Ok(v.get("message").and_then(Value::as_str).map(String::from))
            }),
            Err(why) => Task::ready(Err(why)),
        }
    }

    /// Send an extension the answer to its request `id` (notifications
    /// have none and get none).
    fn respond(&self, extension: &str, id: Option<Value>, answer: Result<Value, String>) {
        let Some(id) = id else { return };
        let message = match answer {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(why) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": why } }),
        };
        if let Some(p) = self.running.get(extension) {
            let _ = p.send(&message);
        }
    }

    /// Answer a message an extension sent.
    fn handle(&mut self, message: Incoming, cx: &mut Context<Self>) {
        let Incoming { extension, id, method, params } = message;
        let reply = {
            let id = id.clone();
            let ext = extension.clone();
            let this = cx.entity().downgrade();
            move |answer: Result<Value, String>, cx: &mut App| {
                let Some(id) = id else { return };
                let message = match answer {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(why) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": why } }),
                };
                if let Some(host) = this.upgrade() {
                    host.update(cx, |h, _| {
                        if let Some(p) = h.running.get(&ext) {
                            let _ = p.send(&message);
                        }
                    });
                }
            }
        };
        let target = self.target.clone();
        let who: SharedString = self.manifest(&extension).map_or_else(|| extension.clone(), |m| m.name.clone()).into();
        // Answers known now go straight back (this runs inside the host's
        // own update); the notebook operations answer when they finish.
        let tool = match method.as_str() {
            "notebook.get" => {
                let answer = target
                    .as_ref()
                    .and_then(|(_, nb)| nb.upgrade())
                    .map(|nb| notebook_json(nb.read(cx), params.get("outputs").and_then(Value::as_bool).unwrap_or(false)))
                    .ok_or_else(|| "No notebook is open.".to_string());
                self.respond(&extension, id, answer);
                return;
            }
            "ui.message" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(nb) = target.as_ref().and_then(|(_, nb)| nb.upgrade()) {
                    nb.update(cx, |nb, cx| nb.flash(format!("{who}: {text}"), cx));
                }
                self.respond(&extension, id, Ok(Value::Null));
                return;
            }
            "notebook.readCells" => "read_cells",
            "notebook.editCell" => "edit_cell",
            "notebook.insertCell" => "insert_cell",
            "notebook.deleteCell" => "delete_cell",
            "notebook.runCells" => "run_cells",
            "kernel.evaluate" => "evaluate",
            other => {
                self.respond(&extension, id, Err(format!("Herse has no method {other}")));
                return;
            }
        };
        let Some((window, nb)) = target else {
            self.respond(&extension, id, Err("No notebook is open.".into()));
            return;
        };
        let tool = tool.to_string();
        let spawned = window.update(cx, |_, window, cx| {
            window
                .spawn(cx, async move |cx| {
                    // An extension has no Stop: what it starts is its own each call.
                    let started = Default::default();
                    let answer = crate::notebook::agent_tools::call(nb, tool, params, who, started, cx).await;
                    cx.update(|_, cx| reply(answer.map(|text| json!({ "text": text })), cx)).ok();
                })
                .detach();
        });
        if spawned.is_err() {
            // The window closed: nothing to act on. (The reply was moved; the
            // extension's request times out on its side.)
        }
    }
}

/// The whole notebook as JSON: cells numbered from 1, with outputs in
/// nbformat's shape when asked for.
fn notebook_json(nb: &NotebookView, outputs: bool) -> Value {
    let cells: Vec<Value> = nb
        .doc
        .nb
        .cells
        .iter()
        .enumerate()
        .map(|(ix, c)| {
            let mut cell = json!({
                "number": ix + 1,
                "type": match c.cell_type { CellType::Code => "code", CellType::Markdown => "markdown", CellType::Raw => "raw" },
                "source": c.source,
                "executionCount": c.execution_count,
            });
            if outputs && c.cell_type == CellType::Code {
                let text = herse_nb::to_string(&herse_nb::Notebook { cells: vec![c.clone()], ..nb.doc.nb.clone() });
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    cell["outputs"] = v.pointer("/cells/0/outputs").cloned().unwrap_or(Value::Null);
                }
            }
            cell
        })
        .collect();
    json!({
        "path": nb.path().map(|p| p.display().to_string()),
        "title": nb.title(),
        "kernel": nb.kernel_label(),
        "cells": cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_manifest() {
        let m = Manifest::parse(
            Path::new("/x"),
            r#"{"id": "word-count", "name": "Word Count", "run": ["python3", "main.py"],
                "commands": [{"id": "count", "title": "Count Words"}],
                "renderers": [{"mime": "application/vnd.demo+json", "title": "Demo"}],
                "events": ["notebook.saved"]}"#,
        )
        .unwrap();
        assert_eq!(m.commands, [("count".to_string(), "Count Words".to_string())]);
        assert_eq!(m.renderers[0].0, "application/vnd.demo+json");
        assert_eq!(m.events, ["notebook.saved"]);
        assert!(Manifest::parse(Path::new("/x"), r#"{"id": "a b", "run": ["x"]}"#).is_err());
        assert!(Manifest::parse(Path::new("/x"), r#"{"id": "a"}"#).unwrap_err().contains("run"));
        assert!(Manifest::parse(Path::new("/x"), r#"{"id": "a", "run": ["x"], "api": 99}"#).unwrap_err().contains("API 99"));
    }

    #[test]
    fn a_changed_manifest_or_run_file_asks_again() {
        let dir = std::env::temp_dir().join(format!("herse-ext-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = r#"{"id": "t", "name": "T", "run": ["python3", "main.py"]}"#;
        std::fs::write(dir.join(MANIFEST), manifest).unwrap();
        std::fs::write(dir.join("main.py"), "print(1)").unwrap();
        let m = Manifest::load(&dir).unwrap();
        let mut answers = HashMap::new();
        assert_eq!(trust_of(&m, &answers), Trust::New);
        answers.insert(m.trust_key(), (m.fingerprint.clone(), true));
        assert_eq!(trust_of(&m, &answers), Trust::Allowed);
        answers.insert(m.trust_key(), (m.fingerprint.clone(), false));
        assert_eq!(trust_of(&m, &answers), Trust::Denied);

        // Another file in the folder doesn't matter; the run file does.
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        assert_eq!(Manifest::load(&dir).unwrap().fingerprint, m.fingerprint);
        std::fs::write(dir.join("main.py"), "print(2)").unwrap();
        let edited = Manifest::load(&dir).unwrap();
        assert_ne!(edited.fingerprint, m.fingerprint);
        assert_eq!(trust_of(&edited, &answers), Trust::Changed);

        // So does the manifest.
        std::fs::write(dir.join("main.py"), "print(1)").unwrap();
        std::fs::write(dir.join(MANIFEST), manifest.replace("\"T\"", "\"T2\"")).unwrap();
        assert_ne!(Manifest::load(&dir).unwrap().fingerprint, m.fingerprint);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shows_run_as_typed() {
        let m = Manifest::parse(Path::new("/x"), r#"{"id": "a", "run": ["python3", "my tool.py", "--x=1", "it's"]}"#).unwrap();
        assert_eq!(m.command_line(), r"python3 'my tool.py' --x=1 'it'\''s'");
    }
}
