//! The session handle: what the app talks to.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use herse_nb::{FormatHint, Notebook, Value};
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

use crate::changes::{self, DocChange, ORIGIN_INIT, ORIGIN_LOCAL, ORIGIN_REMOTE, ORIGIN_REPAIR, Tracker};
use crate::comments::{self, Anchor, Comment, Thread};
use crate::doc::{self, AssignedId, Roots};
use crate::error::{Error, Result};
use crate::link::{self, HostPort, JoinLink, SECRET_LEN};
use crate::net;
use crate::ops::{self, LocalOp};
use crate::presence::{self, Peer, PeerId, PeerState, Presence, Role};
use crate::util::{hex, random_bytes};

/// Tuning knobs. `Config::default()` is right for real use; tests shorten
/// the timeouts and bind to loopback.
#[derive(Clone, Debug)]
pub struct Config {
    /// Host: interface to listen on. Default: all interfaces (`0.0.0.0`),
    /// so LAN and Tailscale peers can connect.
    pub bind: IpAddr,
    /// Host: port to listen on; 0 picks a free one.
    pub port: u16,
    /// Host: addresses to put in join links. Default: this machine's
    /// Tailscale, LAN and `<hostname>.local` addresses.
    pub advertise: Option<Vec<String>>,
    /// How often peers send a heartbeat and refresh their presence.
    pub keepalive: Duration,
    /// A peer whose presence was not refreshed for this long is dropped.
    pub presence_timeout: Duration,
    /// A connection that received nothing for this long is closed (guests
    /// then reconnect).
    pub idle_timeout: Duration,
    /// Guests: time to connect and authenticate, per attempt.
    pub connect_timeout: Duration,
    /// Guests: longest wait between reconnection attempts.
    pub reconnect_max_delay: Duration,
    /// Largest message accepted (the initial sync of a big notebook is one
    /// message).
    pub max_message_size: usize,
    /// Messages queued for one connection before it is dropped as too slow
    /// (it then reconnects and catches up with a state diff).
    pub queue_len: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            advertise: None,
            keepalive: Duration::from_secs(15),
            presence_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(45),
            connect_timeout: Duration::from_secs(10),
            reconnect_max_delay: Duration::from_secs(10),
            max_message_size: 512 << 20,
            queue_len: 4096,
        }
    }
}

/// Connection state, reported with [`SessionEvent::Status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Host: serving, with this many guests connected.
    Hosting { guests: usize },
    /// Guest: connected to the host.
    Connected,
    /// Guest: lost the connection; retrying (`attempt` counts from 1).
    /// Local edits keep working and are sent after reconnecting.
    Reconnecting { attempt: u32 },
    /// The session is over.
    Ended,
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The host ended the session (for the host: [`Session::end`] was called).
    HostEnded,
    /// This guest left ([`Session::leave`], or every handle was dropped).
    Left,
    /// The host no longer accepts this link (it restarted the session).
    LinkRevoked,
    /// Something unrecoverable (listener failed, protocol error).
    Failed(String),
}

/// Something that happened, returned by [`Session::poll`] / [`Events`].
// Events are handled one by one, never stored in bulk: boxing the large
// variant would only cost an allocation per change.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// A document change; apply in order (see [`DocChange`]).
    Doc(DocChange),
    PeerJoined(PeerState),
    /// A peer moved their cursor, changed cell, or renamed themselves.
    PeerUpdated(PeerState),
    PeerLeft(PeerId),
    Status(Status),
    /// An app-defined message from another peer ([`Session::send_message`]).
    Message { from: PeerId, payload: Value },
    /// The session is over; no more events follow.
    Ended(EndReason),
}

/// A session this app is hosting.
#[derive(Debug)]
pub struct Hosted {
    pub session: Session,
    /// Edit link to share.
    pub link: JoinLink,
    /// View-only link to share.
    pub view_link: JoinLink,
    /// Cells that got an id (none before, or a duplicate). The app should
    /// adopt these (for nbformat < 4.5 notebooks, keep them alongside the
    /// cells: `Cell::id` must stay `None` there).
    pub assigned_ids: Vec<AssignedId>,
    /// The id of every cell, in order.
    pub cell_ids: Vec<String>,
}

/// A session this app joined.
#[derive(Debug)]
pub struct Joined {
    pub session: Session,
    /// The notebook as of joining (format hint: nbformat's own).
    pub notebook: Notebook,
    /// The id of every cell, in order (also for nbformat < 4.5 notebooks,
    /// whose `Cell::id`s are `None`).
    pub cell_ids: Vec<String>,
}

/// Why the network thread should stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Host: end for everyone. Guest: leave.
    End,
}

/// State shared by the handle and the network thread.
pub(crate) struct Inner {
    pub awareness: Awareness,
    pub me: Peer,
    pub presence: Presence,
    pub format: FormatHint,
    /// Remote doc updates waiting for the app's next `poll`.
    pub inbound: Vec<Vec<u8>>,
    /// Local doc updates waiting to be sent.
    pub outbound: Vec<Vec<u8>>,
    /// My presence changed; send it.
    pub awareness_dirty: bool,
    /// App messages waiting to be sent.
    pub messages_out: Vec<Value>,
    /// Non-document events waiting for `poll`.
    pub events: Vec<SessionEvent>,
    /// Peers as last reported to the app.
    pub reported: BTreeMap<PeerId, PeerState>,
    pub status: Status,
    pub stop: Option<Stop>,
    pub ended: Option<EndReason>,
    pub ended_reported: bool,
    /// Test hooks.
    pub debug_drop: bool,
    pub frozen: bool,
}

pub(crate) struct Core {
    pub inner: Mutex<Inner>,
    pub doc: Doc,
    pub roots: Roots,
    pub tracker: Arc<Mutex<Tracker>>,
    pub role: Role,
    pub session_id: String,
    pub me_id: PeerId,
    pub config: Config,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    kick_tx: Sender<()>,
    pub kick_rx: Receiver<()>,
}

/// App messages kept for an app that does not poll.
const MAX_QUEUED_EVENTS: usize = 4096;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    /// Wrap a doc (already filled for hosts, empty for guests).
    pub fn new(doc: Doc, role: Role, session_id: String, me: Peer, format: FormatHint, config: Config) -> Arc<Core> {
        let roots = Roots::new(&doc);
        let tracker = Arc::new(Mutex::new(Tracker::default()));
        changes::observe(&roots, tracker.clone());
        changes::reset_shadow(&tracker, &doc.transact(), &roots);
        let awareness = Awareness::new(doc.clone());
        let me_id = PeerId(awareness.client_id().get());
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        let (kick_tx, kick_rx) = async_channel::bounded(1);
        let status = if role == Role::Host { Status::Hosting { guests: 0 } } else { Status::Connected };
        let core = Arc::new(Core {
            inner: Mutex::new(Inner {
                awareness,
                me,
                presence: Presence::default(),
                format,
                inbound: Vec::new(),
                outbound: Vec::new(),
                awareness_dirty: false,
                messages_out: Vec::new(),
                events: Vec::new(),
                reported: BTreeMap::new(),
                status,
                stop: None,
                ended: None,
                ended_reported: false,
                debug_drop: false,
                frozen: false,
            }),
            doc,
            roots,
            tracker,
            role,
            session_id,
            me_id,
            config,
            wake_tx,
            wake_rx,
            kick_tx,
            kick_rx,
        });
        core.refresh_local_presence(&mut core.lock());
        core
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// Tell the app there is something to poll.
    pub fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }

    /// Tell the network thread there is something to send / do.
    pub fn kick(&self) {
        let _ = self.kick_tx.try_send(());
    }

    pub fn hint(&self, id: &str) -> Option<usize> {
        lock(&self.tracker).index_of(id)
    }

    /// Recompute my awareness state from `inner.me` / `inner.presence`.
    pub fn refresh_local_presence(&self, inner: &mut Inner) {
        let json = {
            let txn = self.doc.transact();
            let hint = inner.presence.cell.as_deref().and_then(|c| self.hint(c));
            presence::local_state(&txn, &self.roots, &inner.me, self.role, &inner.presence, hint)
        };
        inner.awareness.set_local_state_raw(json);
        inner.awareness_dirty = true;
    }

    /// Queue a validated remote update for the app's next poll.
    pub fn queue_inbound(&self, update: Vec<u8>) {
        let mut inner = self.lock();
        inner.inbound.push(update);
        // Keep the queue small if the app is not polling (e.g. UI busy).
        if inner.inbound.len() > 256
            && let Ok(merged) = yrs::merge_updates_v1(inner.inbound.iter())
        {
            inner.inbound = vec![merged];
        }
        drop(inner);
        self.wake();
    }

    /// Sync step 2 for a peer at `sv`: what our doc has that they lack, plus
    /// updates received but not yet integrated (so relayed changes are
    /// never missed by a peer that connects in between).
    pub fn sync_step2(&self, sv: &StateVector) -> Vec<u8> {
        let inner = self.lock();
        let diff = self.doc.transact().encode_state_as_update_v1(sv);
        if inner.inbound.is_empty() {
            return diff;
        }
        let mut all = vec![diff];
        all.extend(inner.inbound.iter().cloned());
        yrs::merge_updates_v1(all.iter()).unwrap_or_else(|_| all.swap_remove(0))
    }

    pub fn state_vector(&self) -> StateVector {
        let _inner = self.lock();
        self.doc.transact().state_vector()
    }

    /// Apply a guest's initial sync directly (the app gets a snapshot, not
    /// events, for it).
    pub fn apply_initial(&self, update: &[u8]) -> Result<()> {
        let _inner = self.lock();
        let update = Update::decode_v1(update).map_err(|e| Error::Protocol(format!("bad initial sync: {e}")))?;
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.doc.transact_mut_with(ORIGIN_INIT).apply_update(update)
        }));
        match applied {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Error::Protocol(format!("bad initial sync: {e}"))),
            Err(_) => return Err(Error::Protocol("bad initial sync".into())),
        }
        changes::reset_shadow(&self.tracker, &self.doc.transact(), &self.roots);
        let mut t = lock(&self.tracker);
        t.pending.clear();
        t.comments_dirty = false;
        t.structure_changed = false;
        Ok(())
    }

    /// Encoded awareness message carrying only my state.
    pub fn my_awareness_message(&self, inner: &Inner) -> Option<Vec<u8>> {
        let update = inner.awareness.update_with_clients([inner.awareness.client_id()]).ok()?;
        Some(Message::Awareness(update).encode_v1())
    }

    /// Encoded awareness message with every known state.
    pub fn full_awareness_message(&self) -> Option<Vec<u8>> {
        let inner = self.lock();
        let update = inner.awareness.update().ok()?;
        Some(Message::Awareness(update).encode_v1())
    }

    pub fn set_status(&self, status: Status) {
        let mut inner = self.lock();
        if inner.status != status && inner.status != Status::Ended {
            inner.status = status.clone();
            inner.events.push(SessionEvent::Status(status));
            drop(inner);
            self.wake();
        }
    }

    pub fn push_event(&self, event: SessionEvent) {
        let mut inner = self.lock();
        // Bounded even if the app stops polling while a peer spams messages.
        if inner.events.len() >= MAX_QUEUED_EVENTS
            && let Some(i) = inner.events.iter().position(|e| matches!(e, SessionEvent::Message { .. }))
        {
            inner.events.remove(i);
        }
        inner.events.push(event);
        drop(inner);
        self.wake();
    }

    /// Mark the session over (once).
    pub fn end(&self, reason: EndReason) {
        let mut inner = self.lock();
        if inner.ended.is_none() {
            inner.ended = Some(reason);
            inner.status = Status::Ended;
            inner.events.push(SessionEvent::Status(Status::Ended));
        }
        drop(inner);
        self.wake();
        // Wakes any receiver waiting forever.
        self.wake_tx.close();
    }

    /// Whether yrs holds updates it could not integrate yet (missing
    /// predecessors). Events for transactions touching such updates are not
    /// reliable, so those batches become a `Reset`.
    fn has_pending(&self) -> bool {
        let txn = self.doc.transact();
        let store = txn.store();
        store.pending_update().is_some() || store.pending_ds().is_some()
    }

    /// Integrate queued remote updates (app thread).
    fn integrate(&self, inner: &mut Inner) {
        if inner.inbound.is_empty() {
            return;
        }
        let had_pending = self.has_pending();
        let changes_before = lock(&self.tracker).pending.len();
        self.integrate_updates(inner);
        if had_pending || self.has_pending() {
            log::warn!("herse-collab: updates arrived out of order; resetting the model");
            changes::reset_shadow(&self.tracker, &self.doc.transact(), &self.roots);
            let notebook = doc::read_notebook(&self.doc.transact(), &self.roots, FormatHint::jupyter());
            let mut t = lock(&self.tracker);
            t.pending.truncate(changes_before);
            t.structure_changed = true;
            let cell_ids = t.shadow.iter().map(|(id, _)| id.clone()).collect();
            t.pending.push(DocChange::Reset { notebook, cell_ids });
        }
    }

    fn integrate_updates(&self, inner: &mut Inner) {
        let updates = std::mem::take(&mut inner.inbound);
        let batches: Vec<Vec<u8>> = match yrs::merge_updates_v1(updates.iter()) {
            Ok(m) => vec![m],
            Err(_) => updates,
        };
        for u in batches {
            let Ok(update) = Update::decode_v1(&u) else {
                log::warn!("herse-collab: dropping an undecodable update");
                continue;
            };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.doc.transact_mut_with(ORIGIN_REMOTE).apply_update(update)
            }));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::warn!("herse-collab: failed to apply an update: {e}"),
                Err(_) => log::error!("herse-collab: applying an update panicked; it was dropped"),
            }
        }
    }

    /// Host only: concurrent edits can leave shapes no single edit makes.
    /// After concurrent moves two cells can share an id: keep the first, drop
    /// later identical copies, re-id the others. After a type change races
    /// an output update, a non-code cell can carry code-cell keys (or a code
    /// cell lack them): fix the cell's shape. The repair is an ordinary
    /// change, sent to everyone.
    fn repair(&self, inner: &mut Inner) {
        self.repair_duplicates(inner);
        let check = std::mem::take(&mut lock(&self.tracker).check_cells);
        if check.is_empty() || self.role != Role::Host {
            return;
        }
        let update = {
            let mut txn = self.doc.transact_mut_with(ORIGIN_REPAIR);
            for id in check {
                use yrs::{Map, Out};
                let Some((_, map)) = doc::find_cell(&txn, &self.roots, &id, self.hint(&id)) else { continue };
                if doc::cell_type_of(&txn, &map) == herse_nb::CellType::Code {
                    if !matches!(map.get(&txn, "outputs"), Some(Out::YArray(_))) {
                        map.insert(&mut txn, "outputs", yrs::ArrayPrelim::default());
                    }
                    if !map.contains_key(&txn, "execution_count") {
                        map.insert(&mut txn, "execution_count", yrs::Any::Null);
                    }
                } else if matches!(map.get(&txn, "outputs"), Some(Out::YArray(_))) {
                    // Remnants of the code-cell schema (a file's unknown keys
                    // are plain JSON, never a Y.Array).
                    for key in ["outputs", "execution_count", "execution_state"] {
                        if map.contains_key(&txn, key) {
                            map.remove(&mut txn, key);
                        }
                    }
                }
            }
            txn.encode_update_v1()
        };
        if update.as_slice() != Update::EMPTY_V1 {
            inner.outbound.push(update);
            self.kick();
        }
    }

    fn repair_duplicates(&self, inner: &mut Inner) {
        let changed = std::mem::take(&mut lock(&self.tracker).structure_changed);
        if !changed || self.role != Role::Host {
            return;
        }
        let ids: Vec<String> = lock(&self.tracker).shadow.iter().map(|(id, _)| id.clone()).collect();
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut dups: Vec<(usize, usize)> = Vec::new(); // (first, later)
        for (i, id) in ids.iter().enumerate() {
            match seen.get(id.as_str()) {
                Some(&first) => dups.push((first, i)),
                None => {
                    seen.insert(id, i);
                }
            }
        }
        if dups.is_empty() {
            return;
        }
        let update = {
            let mut txn = self.doc.transact_mut_with(ORIGIN_REPAIR);
            for &(first, later) in dups.iter().rev() {
                use yrs::{Array, Map, Out};
                let (Some(Out::YMap(a)), Some(Out::YMap(b))) =
                    (self.roots.cells.get(&txn, first as u32), self.roots.cells.get(&txn, later as u32))
                else {
                    continue;
                };
                let (ca, _) = doc::read_cell(&txn, &self.roots, &a, false);
                let (cb, _) = doc::read_cell(&txn, &self.roots, &b, false);
                if ca == cb {
                    self.roots.cells.remove(&mut txn, later as u32);
                } else {
                    b.insert(&mut txn, "id", crate::util::new_uuid());
                }
            }
            txn.encode_update_v1()
        };
        inner.outbound.push(update);
        self.kick();
    }

    /// Everything that happened since the last poll (app thread).
    pub fn poll(&self) -> Vec<SessionEvent> {
        let mut inner = self.lock();
        let mut events = std::mem::take(&mut inner.events);
        self.integrate(&mut inner);
        self.repair(&mut inner);
        let (pending, comments_dirty) = {
            let mut t = lock(&self.tracker);
            (std::mem::take(&mut t.pending), std::mem::take(&mut t.comments_dirty))
        };
        events.extend(pending.into_iter().map(SessionEvent::Doc));
        if comments_dirty {
            let threads = comments::read_threads(&self.doc.transact(), &self.roots.comments);
            events.push(SessionEvent::Doc(DocChange::CommentsChanged { threads }));
        }
        // Presence: report the difference to what the app was last told.
        let now = self.peer_states(&inner);
        let mut current: BTreeMap<PeerId, PeerState> = now.into_iter().map(|p| (p.id, p)).collect();
        if inner.ended.is_some() {
            current.clear();
        }
        for id in inner.reported.keys() {
            if !current.contains_key(id) {
                events.push(SessionEvent::PeerLeft(*id));
            }
        }
        for (id, p) in &current {
            match inner.reported.get(id) {
                None => events.push(SessionEvent::PeerJoined(p.clone())),
                Some(old) if old != p => events.push(SessionEvent::PeerUpdated(p.clone())),
                _ => {}
            }
        }
        inner.reported = current;
        if let Some(reason) = inner.ended.clone()
            && !inner.ended_reported
        {
            inner.ended_reported = true;
            events.push(SessionEvent::Ended(reason));
        }
        events
    }

    pub fn peer_states(&self, inner: &Inner) -> Vec<PeerState> {
        let txn = self.doc.transact();
        let me = inner.awareness.client_id();
        let mut out: Vec<PeerState> = inner
            .awareness
            .iter()
            .filter(|(id, state)| *id != me && state.data.is_some())
            .filter_map(|(id, state)| {
                let json = state.data?;
                presence::resolve(&txn, &self.roots, PeerId(id.get()), &json, |cell| self.hint(cell))
            })
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    /// Run a local transaction; queue its update for sending.
    pub(crate) fn local<R>(&self, f: impl FnOnce(&mut yrs::TransactionMut) -> R) -> Result<R> {
        if !self.role.can_edit() {
            return Err(Error::ReadOnly);
        }
        let mut inner = self.lock();
        if inner.ended.is_some() {
            return Err(Error::Closed);
        }
        let (result, update) = {
            let mut txn = self.doc.transact_mut_with(ORIGIN_LOCAL);
            let r = f(&mut txn);
            (r, txn.encode_update_v1())
        };
        if update.as_slice() != Update::EMPTY_V1 {
            inner.outbound.push(update);
            drop(inner);
            self.kick();
        }
        Ok(result)
    }
}

/// Wake-up channel plus poll, for an app event loop. Cloneable.
#[derive(Clone)]
pub struct Events {
    core: Arc<Core>,
}

impl Events {
    /// Wait until something happens, then integrate it and return it all.
    /// Returns `None` once the session has ended and its last events were
    /// returned.
    ///
    /// Remote document changes are integrated into the doc *here*, on the
    /// calling thread; apply the returned batch to your model before running
    /// any code that calls [`Session::apply_local`] (in GPUI: await this in a
    /// foreground task and apply the batch in the same `update` call).
    pub async fn recv(&self) -> Option<Vec<SessionEvent>> {
        loop {
            let events = self.core.poll();
            if !events.is_empty() {
                return Some(events);
            }
            if self.core.wake_rx.recv().await.is_err() {
                let events = self.core.poll();
                return (!events.is_empty()).then_some(events);
            }
        }
    }

    /// Non-blocking [`Events::recv`].
    pub fn try_recv(&self) -> Vec<SessionEvent> {
        self.core.poll()
    }
}

/// Ends the session when the last [`Session`] clone is dropped.
struct Handle {
    core: Arc<Core>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    links: Option<(JoinLink, JoinLink)>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let mut inner = self.core.lock();
        if inner.stop.is_none() {
            inner.stop = Some(Stop::End);
        }
        drop(inner);
        self.core.kick();
    }
}

/// A live collaboration session (hosted or joined). Cheap to clone and
/// `Send + Sync`; methods never block on the network. Dropping the last
/// clone leaves (guest) or ends (host) the session.
///
/// See the [crate docs](crate) for the threading contract.
#[derive(Clone)]
pub struct Session(Arc<Handle>);

/// Secrets of a hosted session.
#[derive(Clone)]
pub(crate) struct Secrets {
    pub edit: [u8; SECRET_LEN],
    pub view: [u8; SECRET_LEN],
}

impl Session {
    /// Host `nb`: start a server on a free port and return the join links.
    pub fn host(nb: &Notebook, me: Peer) -> Result<Hosted> {
        Session::host_with(nb, me, Config::default())
    }

    pub fn host_with(nb: &Notebook, me: Peer, config: Config) -> Result<Hosted> {
        let listener = std::net::TcpListener::bind((config.bind, config.port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();

        let doc = doc::new_doc();
        let roots = Roots::new(&doc);
        let (cell_ids, assigned_ids) = doc::assign_cell_ids(nb);
        {
            let mut txn = doc.transact_mut_with(ORIGIN_INIT);
            doc::write_notebook(&mut txn, &roots, nb, &cell_ids);
        }
        let session_id = hex(&random_bytes::<8>());
        let core = Core::new(doc, Role::Host, session_id.clone(), me, nb.format.clone(), config.clone());
        let secrets = Secrets { edit: random_bytes(), view: random_bytes() };

        let mut hosts: Vec<HostPort> = match &config.advertise {
            Some(list) => list.iter().map(|h| HostPort { host: h.clone(), port }).collect(),
            None => link::advertised(port),
        };
        if hosts.is_empty() {
            hosts.push(HostPort { host: "127.0.0.1".into(), port });
        }
        let link = JoinLink::new(hosts.clone(), session_id.clone(), secrets.edit, false);
        let view_link = JoinLink::new(hosts, session_id, secrets.view, true);
        let thread = net::spawn_host(core.clone(), listener, secrets)?;
        let session = Session(Arc::new(Handle {
            core,
            thread: Mutex::new(Some(thread)),
            links: Some((link.clone(), view_link.clone())),
        }));
        Ok(Hosted { session, link, view_link, assigned_ids, cell_ids })
    }

    /// Join the session behind `link`. Resolves once connected,
    /// authenticated and synced. Runtime-agnostic: await it on any executor.
    pub fn join(link: &JoinLink, me: Peer) -> impl Future<Output = Result<Joined>> + Send + 'static {
        Session::join_with(link, me, Config::default())
    }

    pub fn join_with(link: &JoinLink, me: Peer, config: Config) -> impl Future<Output = Result<Joined>> + Send + 'static {
        let link = link.clone();
        async move {
            let (ready_tx, ready_rx) = async_channel::bounded(1);
            let thread = net::spawn_guest(link, me, config, ready_tx)?;
            let core = match ready_rx.recv().await {
                Ok(r) => r?,
                Err(_) => return Err(Error::Closed),
            };
            let (notebook, cell_ids) = {
                let _inner = core.lock();
                let txn = core.doc.transact();
                let nb = doc::read_notebook(&txn, &core.roots, FormatHint::jupyter());
                let ids = lock(&core.tracker).shadow.iter().map(|(id, _)| id.clone()).collect();
                (nb, ids)
            };
            let session = Session(Arc::new(Handle { core, thread: Mutex::new(Some(thread)), links: None }));
            Ok(Joined { session, notebook, cell_ids })
        }
    }

    /// [`Session::join`] for callers without an executor (blocks).
    pub fn join_blocking(link: &JoinLink, me: Peer) -> Result<Joined> {
        smol::block_on(Session::join(link, me))
    }

    fn core(&self) -> &Core {
        &self.0.core
    }

    /// My role in this session.
    pub fn role(&self) -> Role {
        self.core().role
    }

    /// My id as other peers see it.
    pub fn peer_id(&self) -> PeerId {
        self.core().me_id
    }

    pub fn session_id(&self) -> &str {
        &self.core().session_id
    }

    /// Host only: the (edit, view-only) links.
    pub fn links(&self) -> Option<(JoinLink, JoinLink)> {
        self.0.links.clone()
    }

    /// Host only: the links rebuilt with this machine's current addresses
    /// (after a network change: new Wi-Fi, Tailscale up/down). Same session
    /// id, secrets and port.
    pub fn refresh_links(&self) -> Option<(JoinLink, JoinLink)> {
        let (edit, view) = self.0.links.as_ref()?;
        let port = edit.hosts.first()?.port;
        if self.core().config.advertise.is_some() {
            return Some((edit.clone(), view.clone()));
        }
        let mut hosts = link::advertised(port);
        if hosts.is_empty() {
            hosts.push(HostPort { host: "127.0.0.1".into(), port });
        }
        Some((edit.with_hosts(hosts.clone()), view.with_hosts(hosts)))
    }

    pub fn status(&self) -> Status {
        self.core().lock().status.clone()
    }

    /// Apply a local edit; returns the cell id for [`LocalOp::InsertCell`].
    /// Fails with [`Error::ReadOnly`] for viewers, [`Error::InvalidOp`] if
    /// the op does not fit the document (nothing is changed then).
    pub fn apply_local(&self, op: LocalOp) -> Result<Option<String>> {
        let core = self.core();
        core.local(|txn| ops::apply(txn, &core.roots, &core.tracker, op))?
    }

    /// Apply several edits as one change (one message to peers). Stops at the
    /// first failing op; the ones before it stay applied.
    pub fn apply_local_batch(&self, ops: Vec<LocalOp>) -> Result<Vec<Option<String>>> {
        let core = self.core();
        core.local(|txn| ops.into_iter().map(|op| ops::apply(txn, &core.roots, &core.tracker, op)).collect())?
    }

    /// Make the shared notebook equal `nb` with the fewest changes (cells
    /// matched by id). Handy after structural edits (paste, undo, reorder)
    /// where computing fine-grained ops is a chore; costs a read of the
    /// whole doc, so keep using [`LocalOp::EditSource`] while typing.
    /// `herse.comments` in the metadata is ignored. Returns ids given to
    /// cells that had none (or duplicates).
    pub fn sync_notebook(&self, nb: &Notebook) -> Result<Vec<AssignedId>> {
        let core = self.core();
        core.local(|txn| ops::sync_notebook(txn, &core.roots, nb))
    }

    /// The handle to wait for events with.
    pub fn events(&self) -> Events {
        Events { core: self.0.core.clone() }
    }

    /// Integrate pending remote changes (on this thread) and return what
    /// happened since the last call, in order. See [`Events::recv`].
    pub fn poll(&self) -> Vec<SessionEvent> {
        self.core().poll()
    }

    /// The notebook as of the last poll (complete: comments are stored in
    /// `metadata.herse.comments`). The host's snapshot keeps the original
    /// file's formatting hint, so saving it reproduces the file's style.
    pub fn snapshot(&self) -> Notebook {
        let core = self.core();
        let inner = core.lock();
        let txn = core.doc.transact();
        doc::read_notebook(&txn, &core.roots, inner.format.clone())
    }

    /// Cell ids in order, as of the last poll.
    pub fn cell_ids(&self) -> Vec<String> {
        lock(&self.core().tracker).shadow.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Publish my focus (selected cell, cursor/selection).
    pub fn set_presence(&self, presence: Presence) {
        let core = self.core();
        let mut inner = core.lock();
        if inner.presence == presence {
            return;
        }
        inner.presence = presence;
        core.refresh_local_presence(&mut inner);
        drop(inner);
        core.kick();
    }

    /// Change my name or color.
    pub fn set_me(&self, me: Peer) {
        let core = self.core();
        let mut inner = core.lock();
        inner.me = me;
        core.refresh_local_presence(&mut inner);
        drop(inner);
        core.kick();
    }

    /// The other participants, with cursors resolved against the doc.
    pub fn peers(&self) -> Vec<PeerState> {
        let core = self.core();
        let inner = core.lock();
        if inner.ended.is_some() {
            return Vec::new();
        }
        core.peer_states(&inner)
    }

    /// All comment threads, oldest first.
    pub fn comments(&self) -> Vec<Thread> {
        let core = self.core();
        let _inner = core.lock();
        comments::read_threads(&core.doc.transact(), &core.roots.comments)
    }

    fn comment_op(&self, f: impl FnOnce(&mut yrs::TransactionMut, &yrs::MapRef) -> bool, what: &str) -> Result<()> {
        let core = self.core();
        let ok = core.local(|txn| f(txn, &core.roots.comments))?;
        core.wake();
        if ok { Ok(()) } else { Err(Error::UnknownComment(what.to_owned())) }
    }

    fn my_name(&self) -> String {
        self.core().lock().me.name.clone()
    }

    /// Start a thread on a cell; returns the thread id.
    pub fn add_comment(&self, cell_id: &str, anchor: Option<Anchor>, body: &str) -> Result<String> {
        let thread = Thread::new(cell_id, anchor, &self.my_name(), body);
        let id = thread.id.clone();
        self.comment_op(
            |txn, root| {
                comments::write_thread(txn, root, &thread);
                true
            },
            &id,
        )?;
        Ok(id)
    }

    /// Reply to a thread; returns the message id.
    pub fn reply(&self, thread_id: &str, body: &str) -> Result<String> {
        let c = Comment::new(&self.my_name(), body);
        let id = c.id.clone();
        self.comment_op(|txn, root| comments::add_message(txn, root, thread_id, &c), thread_id)?;
        Ok(id)
    }

    pub fn edit_comment(&self, thread_id: &str, message_id: &str, body: &str) -> Result<()> {
        self.comment_op(|txn, root| comments::edit_message(txn, root, thread_id, message_id, body), message_id)
    }

    /// Delete a message (the thread too, if it was the last one).
    pub fn delete_comment(&self, thread_id: &str, message_id: &str) -> Result<()> {
        self.comment_op(|txn, root| comments::delete_message(txn, root, thread_id, message_id), message_id)
    }

    /// Resolve or reopen a thread.
    pub fn resolve(&self, thread_id: &str, resolved: bool) -> Result<()> {
        let by = self.my_name();
        self.comment_op(|txn, root| comments::set_resolved(txn, root, thread_id, resolved, &by), thread_id)
    }

    pub fn delete_thread(&self, thread_id: &str) -> Result<()> {
        self.comment_op(|txn, root| comments::delete_thread(txn, root, thread_id), thread_id)
    }

    /// Send an app-defined JSON message to every other peer (delivered as
    /// [`SessionEvent::Message`]). Not persisted; viewers may send too, so
    /// the host should check the sender's role before acting on requests.
    pub fn send_message(&self, payload: Value) -> Result<()> {
        let core = self.core();
        let mut inner = core.lock();
        if inner.ended.is_some() {
            return Err(Error::Closed);
        }
        inner.messages_out.push(payload);
        drop(inner);
        core.kick();
        Ok(())
    }

    /// Guest: leave the session (others see you go immediately).
    pub fn leave(&self) {
        self.stop();
    }

    /// Host: end the session for everyone (guests get
    /// [`EndReason::HostEnded`]; the links stop working).
    pub fn end(&self) {
        self.stop();
    }

    fn stop(&self) {
        let core = self.core();
        let mut inner = core.lock();
        if inner.stop.is_none() {
            inner.stop = Some(Stop::End);
        }
        drop(inner);
        core.kick();
    }

    /// Wait (blocking) until the network thread has shut down after
    /// [`Session::end`] / [`Session::leave`]. Returns false on timeout.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let finished = lock(&self.0.thread).as_ref().is_none_or(|t| t.is_finished());
            if finished {
                if let Some(t) = lock(&self.0.thread).take() {
                    let _ = t.join();
                }
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[doc(hidden)]
    /// Test hook: drop every network connection abruptly (guests reconnect).
    pub fn debug_drop_connections(&self) {
        self.core().lock().debug_drop = true;
        self.core().kick();
    }

    #[doc(hidden)]
    /// Test hook: stop sending heartbeats and presence (simulates a frozen
    /// peer whose socket stays open).
    pub fn debug_freeze(&self, frozen: bool) {
        self.core().lock().frozen = frozen;
        self.core().kick();
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.core().session_id)
            .field("role", &self.core().role)
            .field("peer_id", &self.core().me_id)
            .finish()
    }
}

/// Encode a y-sync message.
pub(crate) fn encode(msg: &Message) -> Vec<u8> {
    msg.encode_v1()
}

pub(crate) fn sync_step1(sv: StateVector) -> Vec<u8> {
    encode(&Message::Sync(SyncMessage::SyncStep1(sv)))
}

#[cfg(test)]
#[test]
fn handles_are_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Session>();
    check::<Events>();
}
