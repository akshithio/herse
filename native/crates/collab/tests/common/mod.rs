//! Helpers for multi-peer tests over real localhost sockets.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use herse_collab::{Config, DocChange, Hosted, JoinLink, Joined, LocalOp, Peer, PeerState, Session, SessionEvent};
use herse_nb::{Cell, CellType, FormatHint, Notebook};

/// Loopback-only, fast timeouts.
pub fn config() -> Config {
    Config {
        bind: "127.0.0.1".parse().unwrap(),
        advertise: Some(vec!["127.0.0.1".into()]),
        keepalive: Duration::from_millis(200),
        presence_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(10),
        connect_timeout: Duration::from_secs(5),
        reconnect_max_delay: Duration::from_millis(300),
        ..Config::default()
    }
}

/// A participant: its session, and the app-side model kept in sync the way
/// an app would (local edits mirrored by hand, remote changes via events).
pub struct P {
    pub name: String,
    pub session: Session,
    pub model: Notebook,
    pub ids: Vec<String>,
    pub events: Vec<SessionEvent>,
}

impl P {
    pub fn host(nb: &Notebook, name: &str) -> (P, Hosted) {
        P::host_with(nb, name, config())
    }

    pub fn host_with(nb: &Notebook, name: &str, cfg: Config) -> (P, Hosted) {
        let hosted = Session::host_with(nb, Peer::new(name), cfg).expect("host");
        let mut model = nb.clone();
        let strip = nb.nbformat == 4 && nb.nbformat_minor <= 4;
        if !strip {
            for a in &hosted.assigned_ids {
                model.cells[a.index].id = Some(a.id.clone());
            }
        }
        let p = P {
            name: name.into(),
            session: hosted.session.clone(),
            model,
            ids: hosted.cell_ids.clone(),
            events: Vec::new(),
        };
        (p, hosted)
    }

    pub fn join(link: &JoinLink, name: &str) -> P {
        P::join_with(link, name, config())
    }

    pub fn join_with(link: &JoinLink, name: &str, cfg: Config) -> P {
        let joined: Joined = smol::block_on(Session::join_with(link, Peer::new(name), cfg)).expect("join");
        P { name: name.into(), session: joined.session, model: joined.notebook, ids: joined.cell_ids, events: Vec::new() }
    }

    /// Poll and apply everything to the model.
    pub fn pump(&mut self) {
        for ev in self.session.poll() {
            if let SessionEvent::Doc(change) = &ev {
                assert!(change.apply_to(&mut self.model), "{}: change did not apply: {change:?}", self.name);
                self.track_ids(change);
            }
            self.events.push(ev);
        }
    }

    fn track_ids(&mut self, change: &DocChange) {
        match change {
            DocChange::CellInserted { index, id, .. } => self.ids.insert(*index, id.clone()),
            DocChange::CellRemoved { index, .. } => {
                self.ids.remove(*index);
            }
            DocChange::CellMoved { from, to, id, .. } => {
                self.ids.remove(*from);
                self.ids.insert(*to, id.clone());
            }
            DocChange::CellReplaced { index, id, .. } => self.ids[*index] = id.clone(),
            DocChange::Reset { cell_ids, .. } => self.ids = cell_ids.clone(),
            _ => {}
        }
    }

    /// Apply a local op to the session and mirror it onto the model.
    pub fn edit(&mut self, op: LocalOp) {
        let assigned = self.session.apply_local(op.clone()).unwrap_or_else(|e| panic!("{}: {op:?}: {e}", self.name));
        mirror(&mut self.model, &mut self.ids, &op, assigned);
    }

    pub fn snapshot(&self) -> Notebook {
        normalized(self.session.snapshot())
    }

    /// The model must equal what the doc says.
    pub fn check_model(&self) {
        let snap = self.snapshot();
        let model = normalized(self.model.clone());
        if model != snap {
            for (i, (a, b)) in model.cells.iter().zip(&snap.cells).enumerate() {
                if a != b {
                    panic!("{}: model cell {i} differs:\n model {a:?}\n doc   {b:?}", self.name);
                }
            }
            panic!("{}: model differs from the doc ({} vs {} cells)", self.name, model.cells.len(), snap.cells.len());
        }
        assert_eq!(self.ids, self.session.cell_ids(), "{}: cell ids", self.name);
    }

    pub fn peers(&self) -> Vec<PeerState> {
        self.session.peers()
    }
}

pub fn normalized(mut nb: Notebook) -> Notebook {
    nb.format = FormatHint::jupyter();
    nb
}

/// Mirror a local op onto an app model (what the app's own editing code does).
pub fn mirror(nb: &mut Notebook, ids: &mut Vec<String>, op: &LocalOp, assigned: Option<String>) {
    let ix = |ids: &Vec<String>, id: &str| ids.iter().position(|x| x == id).expect("id");
    match op {
        LocalOp::InsertCell { index, cell } => {
            let mut cell = cell.clone();
            let id = assigned.expect("insert returns the id");
            // nbformat < 4.5 cells have no ids in the model.
            cell.id = (!(nb.nbformat == 4 && nb.nbformat_minor <= 4)).then(|| id.clone());
            nb.cells.insert(*index, cell);
            ids.insert(*index, id);
        }
        LocalOp::DeleteCell { id } => {
            let i = ix(ids, id);
            nb.cells.remove(i);
            ids.remove(i);
        }
        LocalOp::MoveCell { id, to } => {
            let i = ix(ids, id);
            let c = nb.cells.remove(i);
            let x = ids.remove(i);
            nb.cells.insert(*to, c);
            ids.insert(*to, x);
        }
        LocalOp::UpdateCell { cell } => {
            let i = ix(ids, cell.id.as_deref().unwrap());
            nb.cells[i] = cell.clone();
        }
        LocalOp::EditSource { id, range, text } => {
            let i = ix(ids, id);
            nb.cells[i].source.replace_range(range.clone(), text);
        }
        LocalOp::SetSource { id, source } => {
            let i = ix(ids, id);
            nb.cells[i].source = source.clone();
        }
        LocalOp::SetCellType { id, cell_type } => {
            let i = ix(ids, id);
            let c = &mut nb.cells[i];
            if c.cell_type != *cell_type {
                c.cell_type = *cell_type;
                c.outputs.clear();
                c.execution_count = None;
                if *cell_type == CellType::Code {
                    c.extra.shift_remove("outputs");
                    c.extra.shift_remove("execution_count");
                }
            }
        }
        LocalOp::SetCellMetadata { id, metadata } => {
            let i = ix(ids, id);
            nb.cells[i].metadata = metadata.clone();
        }
        LocalOp::SetAttachments { id, attachments } => {
            let i = ix(ids, id);
            nb.cells[i].attachments = attachments.clone();
        }
        LocalOp::SetOutputs { id, outputs } => {
            let i = ix(ids, id);
            nb.cells[i].outputs = outputs.clone();
        }
        LocalOp::PushOutput { id, output } => {
            let i = ix(ids, id);
            nb.cells[i].outputs.push(output.clone());
        }
        LocalOp::ReplaceOutput { id, index, output } => {
            let i = ix(ids, id);
            nb.cells[i].outputs[*index] = output.clone();
        }
        LocalOp::AppendStreamText { id, index, text } => {
            let i = ix(ids, id);
            if let herse_nb::Output::Stream { text: t, .. } = &mut nb.cells[i].outputs[*index] {
                t.push_str(text);
            }
        }
        LocalOp::SetExecutionCount { id, execution_count } => {
            let i = ix(ids, id);
            nb.cells[i].execution_count = *execution_count;
        }
        LocalOp::SetExecutionState { .. } => {}
        LocalOp::SetNotebookMetadata { metadata } => {
            // Comments are not changed by metadata ops.
            let comments = herse_collab::comments::load(nb);
            nb.metadata = metadata.clone();
            herse_collab::comments::store(nb, &comments);
        }
    }
}

/// Poll everyone until `done` holds (or panic after `secs`).
pub fn settle(peers: &mut [&mut P], secs: u64, mut done: impl FnMut(&mut [&mut P]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        for p in peers.iter_mut() {
            p.pump();
        }
        if done(peers) {
            return;
        }
        if Instant::now() > deadline {
            let snaps: Vec<String> = peers.iter().map(|p| format!("{}: {:?}", p.name, p.snapshot().cells.iter().map(|c| &c.source).collect::<Vec<_>>())).collect();
            panic!("peers did not settle:\n{}", snaps.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Poll until all snapshots are equal, then check every model.
pub fn converge(peers: &mut [&mut P]) {
    settle(peers, 15, |ps| {
        let first = ps[0].snapshot();
        ps.iter().all(|p| p.snapshot() == first)
    });
    // One more round so nothing is in flight, then compare models.
    std::thread::sleep(Duration::from_millis(50));
    for p in peers.iter_mut() {
        p.pump();
    }
    let first = peers[0].snapshot();
    for p in peers.iter() {
        assert_eq!(p.snapshot(), first, "{} diverged after settling", p.name);
        p.check_model();
    }
}

pub fn notebook(sources: &[&str]) -> Notebook {
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    nb.cells.clear();
    for (i, s) in sources.iter().enumerate() {
        let mut c = Cell::new(if i % 3 == 2 { CellType::Markdown } else { CellType::Code }).with_source(*s);
        c.id = Some(format!("cell{i}"));
        nb.cells.push(c);
    }
    nb
}

pub fn wait_for(p: &mut P, secs: u64, what: &str, mut f: impl FnMut(&mut P) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        p.pump();
        if f(p) {
            return;
        }
        if Instant::now() > deadline {
            panic!("{}: timed out waiting for {what}", p.name);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
