//! Transport: WebSocket server (host) and client (guest), the encrypted
//! channel, and the y-sync protocol on top.
//!
//! Each session runs one thread (`herse-collab-host` / `herse-collab-guest`)
//! with a private single-threaded smol executor, so the caller needs no
//! runtime. The host serves `ws://<addr>:<port>/herse/<session-id>`; any other
//! path gets a 404 before any Herse byte is exchanged. Topology is a star:
//! guests talk only to the host, which relays document updates, presence and
//! app messages between them (dropping document updates from viewers).
//!
//! After the handshake (see [`crate::crypto`]) both sides send sync step 1
//! (their state vector) and their presence; each answers a step 1 with step 2
//! (the missing updates); afterwards updates and presence flow as they
//! happen. Herse control messages use y-sync's custom message slot (tags
//! 100-104: auth, welcome, end, ping, app message).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use async_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use async_tungstenite::tungstenite::{self, Message as WsMessage, http};
use async_tungstenite::{WebSocketReceiver, WebSocketSender};
use herse_nb::{FormatHint, Value};
use serde_json::json;
use smol::net::{TcpListener, TcpStream};
use smol::stream::StreamExt;
use smol::{LocalExecutor, Timer};
use yrs::sync::{Awareness, AwarenessUpdate, Message, SyncMessage};
use yrs::sync::awareness::AwarenessUpdateEntry;
use yrs::updates::decoder::Decode;
use yrs::{ClientID, Update};

use crate::crypto::{self, Channel, HandshakeMsg, Half};
use crate::doc;
use crate::error::{Error, Result};
use crate::link::{HostPort, JoinLink};
use crate::presence::{Peer, PeerId, Role};
use crate::session::{self, Config, Core, EndReason, Secrets, SessionEvent, Status};

pub(crate) const TAG_AUTH: u8 = 100;
pub(crate) const TAG_WELCOME: u8 = 101;
pub(crate) const TAG_END: u8 = 102;
pub(crate) const TAG_PING: u8 = 103;
pub(crate) const TAG_APP: u8 = 104;

type Tx = WebSocketSender<TcpStream>;
type Rx = WebSocketReceiver<TcpStream>;

pub(crate) fn custom(tag: u8, data: &[u8]) -> Vec<u8> {
    session::encode(&Message::Custom(tag, data.to_vec()))
}

fn update_message(update: Vec<u8>) -> Vec<u8> {
    session::encode(&Message::Sync(SyncMessage::Update(update)))
}

fn awareness_message(update: AwarenessUpdate) -> Vec<u8> {
    session::encode(&Message::Awareness(update))
}

/// `Some(output)` unless `d` elapses first.
async fn timeout<T>(d: Duration, f: impl Future<Output = T>) -> Option<T> {
    smol::future::or(async { Some(f.await) }, async {
        Timer::after(d).await;
        None
    })
    .await
}

fn ws_config(config: &Config) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(config.max_message_size))
        .max_frame_size(Some(config.max_message_size))
}

/// Next binary WebSocket message. `None`: closed. `Some(Err)`: a text
/// message (not our protocol).
async fn next_binary(rx: &mut Rx) -> Option<std::result::Result<Vec<u8>, ()>> {
    loop {
        match rx.next().await? {
            Ok(WsMessage::Binary(b)) => return Some(Ok(b.to_vec())),
            Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_)) => continue,
            Ok(WsMessage::Text(_)) => return Some(Err(())),
            Ok(WsMessage::Close(_)) | Err(_) => return None,
        }
    }
}

async fn send_binary(tx: &mut Tx, data: Vec<u8>) -> bool {
    tx.send(WsMessage::Binary(data.into())).await.is_ok()
}

// ---------------------------------------------------------------------------
// Connection registry

struct Conn {
    /// Plaintext messages to seal and send.
    tx: Sender<Vec<u8>>,
    /// Ask the reader to stop.
    kill: Sender<()>,
    /// Host: the guest asked for our state (sync step 1); broadcasts go to it.
    synced: Cell<bool>,
    /// Awareness client ids this connection speaks for.
    clients: RefCell<HashSet<u64>>,
}

struct Net {
    core: Arc<Core>,
    is_host: bool,
    conns: RefCell<HashMap<u64, Rc<Conn>>>,
    next_id: Cell<u64>,
    /// Host: awareness client id -> connection that owns it.
    owners: RefCell<HashMap<u64, u64>>,
    writers: Cell<usize>,
    /// Guest: the first sync step 2 was applied.
    initial_synced: Cell<bool>,
    ready: RefCell<Option<Sender<Result<Arc<Core>>>>>,
}

impl Net {
    fn new(core: Arc<Core>, is_host: bool) -> Net {
        Net {
            core,
            is_host,
            conns: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            owners: RefCell::new(HashMap::new()),
            writers: Cell::new(0),
            initial_synced: Cell::new(false),
            ready: RefCell::new(None),
        }
    }

    fn register(&self, conn: Conn) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.conns.borrow_mut().insert(id, Rc::new(conn));
        if self.is_host {
            self.hosting_status();
        }
        id
    }

    fn hosting_status(&self) {
        let guests = self.conns.borrow().len();
        self.core.set_status(Status::Hosting { guests });
    }

    fn conn(&self, id: u64) -> Option<Rc<Conn>> {
        self.conns.borrow().get(&id).cloned()
    }

    fn send_to(&self, id: u64, msg: Vec<u8>) {
        if let Some(c) = self.conn(id)
            && c.tx.try_send(msg).is_err()
        {
            // Too slow (or closing): drop it; it reconnects and catches up.
            log::warn!("herse-collab: dropping a connection whose send queue is full");
            kill(&c);
        }
    }

    /// Send to every synced connection except `except`.
    fn broadcast(&self, msg: &[u8], except: Option<u64>) {
        let targets: Vec<(u64, Rc<Conn>)> = self.conns.borrow().iter().map(|(k, v)| (*k, v.clone())).collect();
        for (id, c) in targets {
            if Some(id) != except && c.synced.get() {
                self.send_to(id, msg.to_vec());
            }
        }
    }

    fn close_all(&self) {
        for c in self.conns.borrow().values() {
            c.tx.close();
            let _ = c.kill.try_send(());
        }
    }

    /// Forget a closed connection and the presence it carried.
    fn unregister(&self, id: u64) {
        let Some(conn) = self.conns.borrow_mut().remove(&id) else { return };
        conn.tx.close();
        if self.is_host {
            let clients: Vec<u64> = conn.clients.borrow().iter().copied().collect();
            let mut owners = self.owners.borrow_mut();
            for c in &clients {
                owners.remove(c);
            }
            drop(owners);
            if let Some(update) = remove_presence(&self.core, &clients) {
                self.broadcast(&awareness_message(update), None);
            }
            self.hosting_status();
        } else {
            // Peers are only reachable through the host: forget them all
            // (they come back with the host's state after reconnecting).
            reset_remote_presence(&self.core);
        }
        self.core.wake();
    }
}

fn kill(c: &Conn) {
    c.tx.close();
    let _ = c.kill.try_send(());
}

/// Remove remote presence entries without bumping their clocks (as
/// y-protocols does), so the peer's next refresh is accepted everywhere.
/// Returns the removal to relay.
fn remove_presence(core: &Core, clients: &[u64]) -> Option<AwarenessUpdate> {
    let mut inner = core.lock();
    let me = inner.awareness.client_id().get();
    let mut entries = HashMap::new();
    for &c in clients {
        if c == me {
            continue;
        }
        let id = ClientID::new(c);
        if let Some((clock, _)) = inner.awareness.meta(id)
            && inner.awareness.iter().any(|(k, s)| k == id && s.data.is_some())
        {
            entries.insert(id, AwarenessUpdateEntry { clock, json: "null".into() });
        }
    }
    if entries.is_empty() {
        return None;
    }
    let update = AwarenessUpdate { clients: entries };
    let _ = inner.awareness.apply_update(update.clone());
    Some(update)
}

/// Guest lost the host: start a fresh awareness that only knows me (same
/// clock, so my next refresh is accepted).
fn reset_remote_presence(core: &Core) {
    let mut inner = core.lock();
    let me = inner.awareness.client_id();
    let Some((clock, _)) = inner.awareness.meta(me) else { return };
    let json = inner.awareness.local_state_raw().unwrap_or_else(|| "null".into());
    let mut fresh = Awareness::new(core.doc.clone());
    let mut clients = HashMap::new();
    clients.insert(me, AwarenessUpdateEntry { clock, json });
    let _ = fresh.apply_update(AwarenessUpdate { clients });
    inner.awareness = fresh;
}

/// Timed-out presence entries (not refreshed for `presence_timeout`).
fn expire_presence(core: &Core) -> Vec<u64> {
    let inner = core.lock();
    let me = inner.awareness.client_id();
    let now = crate::util::now_ms().max(0) as u64;
    let limit = core.config.presence_timeout.as_millis() as u64;
    inner
        .awareness
        .iter()
        .filter(|(id, s)| *id != me && s.data.is_some() && now.saturating_sub(s.last_updated) >= limit)
        .map(|(id, _)| id.get())
        .collect()
}

/// Seal and send queued messages for one connection.
async fn writer(net: Rc<Net>, rx: Receiver<Vec<u8>>, mut tx: Tx, mut half: Half) {
    net.writers.set(net.writers.get() + 1);
    while let Ok(msg) = rx.recv().await {
        let frame = half.seal(&msg);
        if !send_binary(&mut tx, frame).await {
            break;
        }
    }
    let _ = tx.close(None).await;
    net.writers.set(net.writers.get() - 1);
}

async fn wait_writers(net: &Net, max: Duration) {
    let deadline = Instant::now() + max;
    while net.writers.get() > 0 && Instant::now() < deadline {
        Timer::after(Duration::from_millis(5)).await;
    }
}

enum Next {
    Frame(Option<Vec<u8>>),
    Kill,
}

/// Read and decrypt frames until the connection ends; `handle` returns
/// false to close it.
async fn read_loop(net: &Net, mut rx: Rx, mut half: Half, kill: Receiver<()>, mut handle: impl FnMut(&[u8]) -> bool) {
    let idle = net.core.config.idle_timeout;
    loop {
        let next = smol::future::or(
            async {
                Next::Frame(match timeout(idle, next_binary(&mut rx)).await {
                    Some(Some(Ok(f))) => Some(f),
                    _ => None,
                })
            },
            async {
                let _ = kill.recv().await;
                Next::Kill
            },
        )
        .await;
        let frame = match next {
            Next::Frame(Some(f)) => f,
            Next::Frame(None) | Next::Kill => return,
        };
        let Some(plain) = half.open(&frame) else {
            log::warn!("herse-collab: closing a connection after a frame failed authentication");
            return;
        };
        let keep = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&plain))).unwrap_or(false);
        if !keep {
            return;
        }
    }
}

/// Work queued by the app for the network thread.
async fn kick_loop(net: &Net) {
    let core = &net.core;
    while core.kick_rx.recv().await.is_ok() {
        let (outbound, awareness, messages, stop, drop_all) = {
            let mut inner = core.lock();
            let outbound = std::mem::take(&mut inner.outbound);
            let awareness = if inner.awareness_dirty && !inner.frozen {
                inner.awareness_dirty = false;
                core.my_awareness_message(&inner)
            } else {
                None
            };
            let messages = std::mem::take(&mut inner.messages_out);
            let drop_all = std::mem::take(&mut inner.debug_drop);
            (outbound, awareness, messages, inner.stop, drop_all)
        };
        if !outbound.is_empty() {
            let update = if outbound.len() == 1 {
                outbound.into_iter().next().unwrap_or_default()
            } else {
                yrs::merge_updates_v1(outbound.iter()).unwrap_or_default()
            };
            if !update.is_empty() {
                net.broadcast(&update_message(update), None);
            }
        }
        if let Some(a) = awareness {
            net.broadcast(&a, None);
        }
        for payload in messages {
            let envelope = json!({"from": core.me_id.0, "payload": payload}).to_string();
            net.broadcast(&custom(TAG_APP, envelope.as_bytes()), None);
        }
        if drop_all {
            for c in net.conns.borrow().values() {
                kill(c);
            }
        }
        if stop.is_some() {
            if !net.is_host {
                let leave = {
                    let mut inner = core.lock();
                    inner.awareness.clean_local_state();
                    core.my_awareness_message(&inner)
                };
                if let Some(leave) = leave {
                    net.broadcast(&leave, None);
                }
            }
            return;
        }
    }
}

/// Heartbeats, presence refresh and expiry.
async fn timer_loop(net: Rc<Net>) {
    let core = net.core.clone();
    let cfg = &core.config;
    let tick = cfg.keepalive.min(cfg.presence_timeout / 4).max(Duration::from_millis(10));
    let mut last_refresh = Instant::now();
    loop {
        Timer::after(tick).await;
        let frozen = core.lock().frozen;
        if !frozen && last_refresh.elapsed() >= cfg.keepalive {
            last_refresh = Instant::now();
            let msg = {
                let mut inner = core.lock();
                if let Some(json) = inner.awareness.local_state_raw() {
                    inner.awareness.set_local_state_raw(json);
                }
                core.my_awareness_message(&inner)
            };
            if let Some(msg) = msg {
                net.broadcast(&msg, None);
            }
            net.broadcast(&custom(TAG_PING, b""), None);
        }
        let expired = expire_presence(&core);
        if !expired.is_empty() {
            if let Some(update) = remove_presence(&core, &expired)
                && net.is_host
            {
                net.broadcast(&awareness_message(update), None);
            }
            core.wake();
        }
    }
}

/// lib0 varuint at `buf[*pos..]`.
fn read_varuint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut n: u64 = 0;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        n |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(n);
        }
    }
    None
}

/// Cheap plausibility check of the element counts a y-sync message
/// declares, before yrs decodes it: yrs reserves memory for declared counts,
/// so a few crafted bytes could otherwise make it allocate gigabytes. Each
/// state-vector entry, update client and awareness entry needs at least two
/// bytes, so a count above half the remaining bytes is a lie.
fn plausible(plain: &[u8]) -> bool {
    let mut pos = 0;
    let Some(tag) = read_varuint(plain, &mut pos) else { return false };
    let counted = |pos: &mut usize| -> bool {
        // varbuf: length, then the bytes; their first varuint is a count.
        let Some(len) = read_varuint(plain, pos) else { return false };
        let Some(body) = plain.get(*pos..).filter(|b| b.len() as u64 >= len) else { return false };
        let body = &body[..len as usize];
        let mut p = 0;
        match read_varuint(body, &mut p) {
            Some(count) => count <= (body.len() - p) as u64 / 2 + 1,
            None => body.is_empty(),
        }
    };
    match tag {
        0 => {
            let Some(_sub) = read_varuint(plain, &mut pos) else { return false };
            counted(&mut pos)
        }
        1 => counted(&mut pos),
        _ => true,
    }
}

/// Validate a document update before queueing or relaying it.
fn valid_update(u: &[u8]) -> bool {
    std::panic::catch_unwind(|| Update::decode_v1(u).is_ok()).unwrap_or(false)
}

fn app_message(core: &Core, data: &[u8], from_override: Option<u64>) -> Option<(PeerId, Value)> {
    let v: Value = serde_json::from_slice(data).ok()?;
    let from = from_override.or_else(|| v.get("from")?.as_u64())?;
    let payload = v.get("payload").cloned().unwrap_or(Value::Null);
    if from == core.me_id.0 {
        return None;
    }
    Some((PeerId(from), payload))
}

// ---------------------------------------------------------------------------
// Host

pub(crate) fn spawn_host(core: Arc<Core>, listener: std::net::TcpListener, secrets: Secrets) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("herse-collab-host".into()).spawn(move || {
        let ex = Rc::new(LocalExecutor::new());
        smol::block_on(ex.run(host_main(ex.clone(), core, listener, secrets)));
    })
}

async fn host_main(ex: Rc<LocalExecutor<'static>>, core: Arc<Core>, listener: std::net::TcpListener, secrets: Secrets) {
    let listener = match TcpListener::try_from(listener) {
        Ok(l) => l,
        Err(e) => {
            core.end(EndReason::Failed(format!("could not listen: {e}")));
            return;
        }
    };
    let net = Rc::new(Net::new(core.clone(), true));
    let secrets = Rc::new(secrets);
    let accept = {
        let net = net.clone();
        let ex2 = ex.clone();
        ex.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => ex2.spawn(host_conn(ex2.clone(), net.clone(), stream, secrets.clone())).detach(),
                    Err(e) => {
                        log::warn!("herse-collab: accept failed: {e}");
                        Timer::after(Duration::from_millis(100)).await;
                    }
                }
            }
        })
    };
    let timer = ex.spawn(timer_loop(net.clone()));
    kick_loop(&net).await;
    drop(accept);
    drop(timer);
    // Tell everyone, let the writers flush, close.
    let end = custom(TAG_END, json!({"reason": "host ended the session"}).to_string().as_bytes());
    for c in net.conns.borrow().values() {
        let _ = c.tx.try_send(end.clone());
    }
    net.close_all();
    wait_writers(&net, Duration::from_secs(1)).await;
    core.end(EndReason::HostEnded);
}

// The handshake callback's error type is tungstenite's.
#[allow(clippy::result_large_err)]
async fn host_conn(ex: Rc<LocalExecutor<'static>>, net: Rc<Net>, stream: TcpStream, secrets: Rc<Secrets>) {
    let _ = stream.set_nodelay(true);
    let core = net.core.clone();
    let cfg = core.config.clone();
    let hs = cfg.connect_timeout;
    let path = format!("/herse/{}", core.session_id);
    let check = move |req: &Request, resp: Response| -> std::result::Result<Response, ErrorResponse> {
        if req.uri().path() == path {
            Ok(resp)
        } else {
            let mut r = ErrorResponse::new(Some("not found".into()));
            *r.status_mut() = http::StatusCode::NOT_FOUND;
            Err(r)
        }
    };
    let ws = match timeout(hs, async_tungstenite::accept_hdr_async_with_config(stream, check, Some(ws_config(&cfg)))).await {
        Some(Ok(ws)) => ws,
        _ => return,
    };
    let (mut tx, mut rx) = ws.split();

    let client_nonce = match timeout(hs, next_binary(&mut rx)).await {
        Some(Some(Ok(m))) => match crypto::parse_handshake(&m) {
            Some(HandshakeMsg::Hello(n)) => n,
            Some(HandshakeMsg::WrongVersion(_)) => {
                let _ = send_binary(&mut tx, crypto::reject_message(crypto::REJECT_VERSION)).await;
                let _ = tx.close(None).await;
                return;
            }
            _ => return,
        },
        _ => return,
    };
    let server_nonce = crypto::new_nonce();
    if !send_binary(&mut tx, crypto::hello_message(crypto::CHALLENGE, &server_nonce)).await {
        return;
    }
    let frame0 = match timeout(hs, next_binary(&mut rx)).await {
        Some(Some(Ok(f))) => f,
        _ => return,
    };
    let mut chosen = None;
    for (secret, role) in [(&secrets.edit, Role::Editor), (&secrets.view, Role::Viewer)] {
        let keys = crypto::derive_keys(secret, &core.session_id, &client_nonce, &server_nonce);
        let mut ch = Channel::server(&keys);
        if let Some(plain) = ch.recv.open(&frame0) {
            chosen = Some((ch, role, plain));
            break;
        }
    }
    let Some((mut channel, role, plain)) = chosen else {
        log::info!("herse-collab: rejected a peer that could not authenticate");
        let _ = send_binary(&mut tx, crypto::reject_message(crypto::REJECT_AUTH)).await;
        let _ = tx.close(None).await;
        return;
    };
    if !matches!(Message::decode_v1(&plain), Ok(Message::Custom(TAG_AUTH, _))) {
        return;
    }
    let welcome = json!({"role": role.as_str(), "session": core.session_id, "host": core.me_id.0}).to_string();
    if !send_binary(&mut tx, channel.send.seal(&custom(TAG_WELCOME, welcome.as_bytes()))).await {
        return;
    }

    let (out_tx, out_rx) = async_channel::bounded(cfg.queue_len.max(16));
    let (kill_tx, kill_rx) = async_channel::bounded(1);
    let id = net.register(Conn {
        tx: out_tx,
        kill: kill_tx,
        synced: Cell::new(false),
        clients: RefCell::new(HashSet::new()),
    });
    let Channel { send, recv } = channel;
    ex.spawn(writer(net.clone(), out_rx, tx, send)).detach();
    net.send_to(id, session::sync_step1(core.state_vector()));
    if let Some(a) = core.full_awareness_message() {
        net.send_to(id, a);
    }
    read_loop(&net, rx, recv, kill_rx, |plain| handle_from_guest(&net, id, role, plain)).await;
    net.unregister(id);
}

fn handle_from_guest(net: &Net, id: u64, role: Role, plain: &[u8]) -> bool {
    let core = &net.core;
    if !plausible(plain) {
        return false;
    }
    let Ok(msg) = Message::decode_v1(plain) else { return false };
    match msg {
        Message::Sync(SyncMessage::SyncStep1(sv)) => {
            let step2 = core.sync_step2(&sv);
            net.send_to(id, session::encode(&Message::Sync(SyncMessage::SyncStep2(step2))));
            if let Some(c) = net.conn(id) {
                c.synced.set(true);
            }
        }
        Message::Sync(SyncMessage::SyncStep2(u) | SyncMessage::Update(u)) => {
            if !role.can_edit() {
                // View-only guests' edits are not accepted (their session
                // refuses local edits, so this only happens with a modified
                // client).
                return true;
            }
            if !valid_update(&u) {
                return false;
            }
            if u.as_slice() == Update::EMPTY_V1 {
                return true;
            }
            net.broadcast(&update_message(u.clone()), Some(id));
            core.queue_inbound(u);
        }
        Message::Awareness(mut update) => {
            let me = core.me_id.0;
            {
                let mut owners = net.owners.borrow_mut();
                update.clients.retain(|cid, _| {
                    let cid = cid.get();
                    cid != me && owners.get(&cid).is_none_or(|o| *o == id)
                });
                if let Some(c) = net.conn(id) {
                    for cid in update.clients.keys() {
                        owners.insert(cid.get(), id);
                        c.clients.borrow_mut().insert(cid.get());
                    }
                }
            }
            if update.clients.is_empty() {
                return true;
            }
            let _ = core.lock().awareness.apply_update(update.clone());
            net.broadcast(&awareness_message(update), Some(id));
            core.wake();
        }
        Message::AwarenessQuery => {
            if let Some(a) = core.full_awareness_message() {
                net.send_to(id, a);
            }
        }
        Message::Custom(TAG_APP, data) => {
            let owner = net.conn(id).and_then(|c| c.clients.borrow().iter().next().copied());
            if let Some((from, payload)) = app_message(core, &data, owner) {
                let envelope = json!({"from": from.0, "payload": payload}).to_string();
                net.broadcast(&custom(TAG_APP, envelope.as_bytes()), Some(id));
                core.push_event(SessionEvent::Message { from, payload });
            }
        }
        // Pings keep the idle timer happy; other tags are ignored.
        Message::Custom(..) | Message::Auth(_) => {}
    }
    true
}

// ---------------------------------------------------------------------------
// Guest

pub(crate) fn spawn_guest(link: JoinLink, me: Peer, config: Config, ready: Sender<Result<Arc<Core>>>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("herse-collab-guest".into()).spawn(move || {
        let ex = Rc::new(LocalExecutor::new());
        smol::block_on(ex.run(guest_main(ex.clone(), link, me, config, ready)));
    })
}

struct Established {
    tx: Tx,
    rx: Rx,
    channel: Channel,
    role: Role,
    addr: SocketAddr,
}

enum ConnEnd {
    /// Connection dropped; reconnect.
    Lost,
    /// The host ended the session.
    HostEnded,
    /// The initial sync failed.
    Failed(Error),
}

fn backoff(attempt: u32, max: Duration) -> Duration {
    let base = Duration::from_millis(250).saturating_mul(1u32 << attempt.min(8).saturating_sub(1));
    let jitter = Duration::from_millis(u64::from(crate::util::random_bytes::<1>()[0]));
    base.min(max) + jitter
}

async fn guest_main(ex: Rc<LocalExecutor<'static>>, link: JoinLink, me: Peer, config: Config, ready: Sender<Result<Arc<Core>>>) {
    let mut net: Option<Rc<Net>> = None;
    let mut background = Vec::new();
    let mut attempt = 0u32;
    let mut prefer: Option<SocketAddr> = None;
    loop {
        match connect(&ex, &link, &config, prefer).await {
            Ok(est) => {
                prefer = Some(est.addr);
                let n = match &net {
                    Some(n) => n.clone(),
                    None => {
                        let core = Core::new(doc::new_doc(), est.role, link.session_id.clone(), me.clone(), FormatHint::jupyter(), config.clone());
                        let n = Rc::new(Net::new(core, false));
                        *n.ready.borrow_mut() = Some(ready.clone());
                        background.push(ex.spawn(timer_loop(n.clone())));
                        let kn = n.clone();
                        background.push(ex.spawn(async move {
                            kick_loop(&kn).await;
                            kn.close_all();
                        }));
                        net = Some(n.clone());
                        n
                    }
                };
                if attempt > 0 {
                    n.core.set_status(Status::Connected);
                }
                attempt = 0;
                match run_guest_conn(&ex, &n, est).await {
                    ConnEnd::HostEnded => {
                        n.core.end(EndReason::HostEnded);
                        break;
                    }
                    ConnEnd::Failed(e) => {
                        if let Some(r) = n.ready.borrow_mut().take() {
                            let _ = r.try_send(Err(e));
                        }
                        n.core.end(EndReason::Failed("initial sync failed".into()));
                        break;
                    }
                    ConnEnd::Lost => {}
                }
            }
            Err(e) => {
                let Some(n) = &net else {
                    let _ = ready.send(Err(e)).await;
                    return;
                };
                if matches!(e, Error::AuthFailed) {
                    n.core.end(EndReason::LinkRevoked);
                    break;
                }
                log::info!("herse-collab: reconnect failed: {e}");
            }
        }
        let Some(n) = &net else { return };
        if !n.initial_synced.get() {
            // Connected but never synced (e.g. dropped mid-sync): report.
            if let Some(r) = n.ready.borrow_mut().take() {
                let _ = r.try_send(Err(Error::Unreachable("connection lost during the initial sync".into())));
            }
            n.core.end(EndReason::Failed("connection lost during the initial sync".into()));
            break;
        }
        if n.core.lock().stop.is_some() {
            n.core.end(EndReason::Left);
            break;
        }
        attempt += 1;
        n.core.set_status(Status::Reconnecting { attempt });
        let deadline = Instant::now() + backoff(attempt, config.reconnect_max_delay);
        while Instant::now() < deadline && n.core.lock().stop.is_none() {
            Timer::after(Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now()))).await;
        }
        if n.core.lock().stop.is_some() {
            n.core.end(EndReason::Left);
            break;
        }
    }
    if let Some(n) = &net {
        n.close_all();
        wait_writers(n, Duration::from_secs(1)).await;
    }
    drop(background);
}

async fn run_guest_conn(ex: &Rc<LocalExecutor<'static>>, net: &Rc<Net>, est: Established) -> ConnEnd {
    let core = net.core.clone();
    let Established { tx, rx, channel, .. } = est;
    let (out_tx, out_rx) = async_channel::bounded(core.config.queue_len.max(16));
    let (kill_tx, kill_rx) = async_channel::bounded(1);
    let id = net.register(Conn {
        tx: out_tx,
        kill: kill_tx,
        synced: Cell::new(true),
        clients: RefCell::new(HashSet::new()),
    });
    let Channel { send, recv } = channel;
    ex.spawn(writer(net.clone(), out_rx, tx, send)).detach();
    net.send_to(id, session::sync_step1(core.state_vector()));
    // Bump my presence clock so the host accepts it even if it remembers
    // the state from before a reconnect.
    let mine = {
        let mut inner = core.lock();
        core.refresh_local_presence(&mut inner);
        inner.awareness_dirty = false;
        core.my_awareness_message(&inner)
    };
    if let Some(a) = mine {
        net.send_to(id, a);
    }
    let mut outcome = ConnEnd::Lost;
    read_loop(net, rx, recv, kill_rx, |plain| match handle_from_host(net, id, plain) {
        Ok(true) => true,
        Ok(false) => {
            outcome = ConnEnd::HostEnded;
            false
        }
        Err(e) => {
            outcome = ConnEnd::Failed(e);
            false
        }
    })
    .await;
    net.unregister(id);
    outcome
}

/// `Ok(false)`: the host ended the session.
fn handle_from_host(net: &Net, id: u64, plain: &[u8]) -> Result<bool> {
    let core = &net.core;
    if !plausible(plain) {
        return Err(Error::Protocol("implausible message".into()));
    }
    let Ok(msg) = Message::decode_v1(plain) else { return Err(Error::Protocol("undecodable message".into())) };
    match msg {
        Message::Sync(SyncMessage::SyncStep1(sv)) => {
            let step2 = core.sync_step2(&sv);
            net.send_to(id, session::encode(&Message::Sync(SyncMessage::SyncStep2(step2))));
        }
        Message::Sync(SyncMessage::SyncStep2(u)) => receive_update(net, u, true)?,
        Message::Sync(SyncMessage::Update(u)) => receive_update(net, u, false)?,
        Message::Awareness(update) => {
            let _ = core.lock().awareness.apply_update(update);
            core.wake();
        }
        Message::AwarenessQuery => {
            let mine = {
                let inner = core.lock();
                core.my_awareness_message(&inner)
            };
            if let Some(a) = mine {
                net.send_to(id, a);
            }
        }
        Message::Custom(TAG_END, _) => return Ok(false),
        Message::Custom(TAG_APP, data) => {
            if let Some((from, payload)) = app_message(core, &data, None) {
                core.push_event(SessionEvent::Message { from, payload });
            }
        }
        Message::Custom(..) | Message::Auth(_) => {}
    }
    Ok(true)
}

fn receive_update(net: &Net, u: Vec<u8>, is_step2: bool) -> Result<()> {
    let core = &net.core;
    if !valid_update(&u) {
        return Err(Error::Protocol("invalid document update".into()));
    }
    if !net.initial_synced.get() {
        core.apply_initial(&u)?;
        if is_step2 {
            net.initial_synced.set(true);
            if let Some(r) = net.ready.borrow_mut().take()
                && r.try_send(Ok(core.clone())).is_err()
            {
                // Nobody is waiting for this join any more: leave.
                core.lock().stop = Some(session::Stop::End);
                core.kick();
            }
        }
    } else if u.as_slice() != Update::EMPTY_V1 {
        core.queue_inbound(u);
    }
    Ok(())
}

/// Resolve the link's addresses and connect to the first that completes the
/// handshake (connection attempts are staggered by 250 ms, happy-eyeballs
/// style). `prefer` (the address that worked last time) goes first.
async fn connect(ex: &Rc<LocalExecutor<'static>>, link: &JoinLink, config: &Config, prefer: Option<SocketAddr>) -> Result<Established> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for hp in &link.hosts {
        let target = hp.to_string();
        match timeout(config.connect_timeout, smol::net::resolve(target.as_str())).await {
            Some(Ok(list)) => {
                for a in list {
                    if !addrs.contains(&a) {
                        addrs.push(a);
                    }
                }
            }
            Some(Err(e)) => errors.push(format!("{target}: {e}")),
            None => errors.push(format!("{target}: name lookup timed out")),
        }
    }
    if let Some(p) = prefer
        && let Some(i) = addrs.iter().position(|a| *a == p)
    {
        let a = addrs.remove(i);
        addrs.insert(0, a);
    }
    if addrs.is_empty() {
        return Err(Error::Unreachable(errors.join("; ")));
    }
    let per_addr = config.connect_timeout.min(Duration::from_secs(5));
    let (res_tx, res_rx) = async_channel::unbounded::<(SocketAddr, std::io::Result<TcpStream>)>();
    let mut attempts = Vec::new();
    for (i, addr) in addrs.iter().copied().enumerate() {
        let res_tx = res_tx.clone();
        attempts.push(ex.spawn(async move {
            Timer::after(Duration::from_millis(250) * i as u32).await;
            let r = match timeout(per_addr, TcpStream::connect(addr)).await {
                Some(r) => r,
                None => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out")),
            };
            let _ = res_tx.send((addr, r)).await;
        }));
    }
    drop(res_tx);
    let deadline = Instant::now() + config.connect_timeout + Duration::from_millis(250) * addrs.len() as u32;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(Ok((addr, r))) = timeout(left, res_rx.recv()).await else { break };
        match r {
            Ok(stream) => match handshake(stream, addr, link, config).await {
                Ok(est) => return Ok(est),
                Err(e @ (Error::AuthFailed | Error::Protocol(_))) => return Err(e),
                Err(e) => errors.push(format!("{addr}: {e}")),
            },
            Err(e) => errors.push(format!("{addr}: {e}")),
        }
    }
    drop(attempts);
    Err(Error::Unreachable(if errors.is_empty() { "timed out".into() } else { errors.join("; ") }))
}

async fn handshake(stream: TcpStream, addr: SocketAddr, link: &JoinLink, config: &Config) -> Result<Established> {
    let _ = stream.set_nodelay(true);
    let t = config.connect_timeout;
    let hp = HostPort { host: addr.ip().to_string(), port: addr.port() };
    let url = format!("ws://{hp}/herse/{}", link.session_id);
    let ws = match timeout(t, async_tungstenite::client_async_with_config(url, stream, Some(ws_config(config)))).await {
        Some(Ok((ws, _))) => ws,
        Some(Err(tungstenite::Error::Http(resp))) if resp.status() == http::StatusCode::NOT_FOUND => {
            // A Herse host without this session (it ended).
            return Err(Error::AuthFailed);
        }
        Some(Err(e)) => return Err(Error::Unreachable(format!("WebSocket handshake failed: {e}"))),
        None => return Err(Error::Unreachable("WebSocket handshake timed out".into())),
    };
    let (mut tx, mut rx) = ws.split();
    let client_nonce = crypto::new_nonce();
    if !send_binary(&mut tx, crypto::hello_message(crypto::HELLO, &client_nonce)).await {
        return Err(Error::Unreachable("connection closed".into()));
    }
    let server_nonce = match timeout(t, next_binary(&mut rx)).await {
        Some(Some(Ok(m))) => match crypto::parse_handshake(&m) {
            Some(HandshakeMsg::Challenge(n)) => n,
            Some(HandshakeMsg::Reject(crypto::REJECT_AUTH)) => return Err(Error::AuthFailed),
            Some(HandshakeMsg::Reject(_) | HandshakeMsg::WrongVersion(_)) => {
                return Err(Error::Protocol("the host runs another protocol version".into()));
            }
            _ => return Err(Error::Protocol("not a Herse host".into())),
        },
        _ => return Err(Error::Unreachable("no handshake reply".into())),
    };
    let keys = crypto::derive_keys(link.secret(), &link.session_id, &client_nonce, &server_nonce);
    let mut channel = Channel::client(&keys);
    if !send_binary(&mut tx, channel.send.seal(&custom(TAG_AUTH, b"{}"))).await {
        return Err(Error::Unreachable("connection closed".into()));
    }
    let reply = match timeout(t, next_binary(&mut rx)).await {
        Some(Some(Ok(m))) => m,
        _ => return Err(Error::AuthFailed),
    };
    if let Some(HandshakeMsg::Reject(reason)) = crypto::parse_handshake(&reply) {
        return Err(if reason == crypto::REJECT_AUTH {
            Error::AuthFailed
        } else {
            Error::Protocol("the host runs another protocol version".into())
        });
    }
    let Some(plain) = channel.recv.open(&reply) else {
        return Err(Error::Protocol("the host could not prove it knows the link secret".into()));
    };
    let role = match Message::decode_v1(&plain) {
        Ok(Message::Custom(TAG_WELCOME, data)) => {
            let v: Value = serde_json::from_slice(&data).map_err(|_| Error::Protocol("bad welcome".into()))?;
            match v.get("role").and_then(Value::as_str) {
                Some("viewer") => Role::Viewer,
                Some("editor") => Role::Editor,
                _ => return Err(Error::Protocol("bad welcome".into())),
            }
        }
        _ => return Err(Error::Protocol("bad welcome".into())),
    };
    Ok(Established { tx, rx, channel, role, addr })
}

#[cfg(test)]
pub(crate) mod testing {
    //! Raw protocol access for tests that need to misbehave.
    use super::*;

    /// Connect and authenticate like a guest, returning the raw halves.
    pub(crate) async fn raw_guest(link: &JoinLink) -> Result<(Tx, Rx, Channel)> {
        let addr: SocketAddr = format!("{}:{}", link.hosts[0].host, link.hosts[0].port)
            .parse()
            .map_err(|_| Error::BadLink("test links use IP addresses".into()))?;
        let stream = TcpStream::connect(addr).await?;
        let est = handshake(stream, addr, link, &Config::default()).await?;
        Ok((est.tx, est.rx, est.channel))
    }

    pub(crate) async fn send(tx: &mut Tx, data: Vec<u8>) -> bool {
        send_binary(tx, data).await
    }

    pub(crate) async fn recv(rx: &mut Rx) -> Option<Vec<u8>> {
        match next_binary(rx).await {
            Some(Ok(v)) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handlers_survive_random_plaintext() {
        let cfg = Config::default();
        let host = Core::new(doc::new_doc(), Role::Host, "s".into(), Peer::new("h"), FormatHint::jupyter(), cfg.clone());
        let hnet = Net::new(host.clone(), true);
        let guest = Core::new(doc::new_doc(), Role::Editor, "s".into(), Peer::new("g"), FormatHint::jupyter(), cfg);
        let gnet = Net::new(guest.clone(), false);
        gnet.initial_synced.set(true);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        // yrs panics on some crafted updates in debug builds (integer
        // overflow); the handlers catch that. Keep the output readable.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        for i in 0..20_000u64 {
            let len = (next() % 160) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Steer half of the inputs into the y-sync decoders: a valid
            // message tag (sync / awareness / auth / query / custom) and
            // sync sub-tag.
            if i % 3 == 0 && len >= 2 {
                bytes[0] = [0, 1, 2, 3, 100, 102, 104][(i / 3 % 7) as usize];
                bytes[1] = (i / 21 % 3) as u8;
            } else if i % 3 == 1 {
                // Correctly framed sync / awareness message with a random body.
                let mut framed = if i % 2 == 0 { vec![0, (i / 6 % 3) as u8] } else { vec![1] };
                framed.push(len as u8 & 0x7f);
                framed.extend(bytes.iter().take((len as u8 & 0x7f) as usize));
                if let Some(b) = framed.get_mut(if i % 2 == 0 { 3 } else { 2 }) {
                    *b %= 8; // small declared counts get past the plausibility check
                }
                bytes = framed;
            }
            let t = Instant::now();
            let _ = handle_from_guest(&hnet, 1, Role::Editor, &bytes);
            let _ = handle_from_host(&gnet, 1, &bytes);
            assert!(t.elapsed() < Duration::from_millis(500), "input {i} took {:?}: {bytes:?}", t.elapsed());
        }
        std::panic::set_hook(hook);
        let _ = host.poll();
        let _ = guest.poll();
    }
}
