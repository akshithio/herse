//! Tests that need to misbehave on the wire (raw protocol access).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use async_tungstenite::tungstenite::Message as WsMessage;
use herse_nb::{Cell, CellType, Notebook};
use smol::stream::StreamExt;
use yrs::sync::{Message, SyncMessage};
use yrs::updates::encoder::Encode;
use yrs::Transact;

use crate::crypto::{self, HandshakeMsg};
use crate::net::testing::{raw_guest, recv, send};
use crate::session::Config;
use crate::{Peer, Session, Status};

fn config() -> Config {
    Config {
        bind: "127.0.0.1".parse().unwrap(),
        advertise: Some(vec!["127.0.0.1".into()]),
        keepalive: Duration::from_millis(200),
        connect_timeout: Duration::from_secs(2),
        ..Config::default()
    }
}

fn notebook() -> Notebook {
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    nb.cells[0].source = "x = 1".into();
    nb
}

/// xorshift64*
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn wait(mut f: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A real guest can still join and sync.
fn host_still_works(host: &Session, link: &crate::JoinLink) {
    let joined = smol::block_on(Session::join_with(link, Peer::new("Check"), config())).expect("join after abuse");
    assert_eq!(joined.notebook.cells[0].source, host.snapshot().cells[0].source);
    joined.session.leave();
}

#[test]
fn random_bytes_on_the_socket_do_not_hurt() {
    let hosted = Session::host_with(&notebook(), Peer::new("Ada"), config()).unwrap();
    let port = hosted.link.hosts[0].port;
    let mut rng = Rng(42);
    for size in [1, 17, 512, 70_000] {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = s.write_all(&rng.bytes(size));
        let mut buf = [0u8; 4096];
        // The host answers garbage with an HTTP error or just closes.
        while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
    }
    // A request for another path never reaches the Herse handshake.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /herse/nope HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").unwrap();
    let mut reply = String::new();
    let _ = s.read_to_string(&mut reply);
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    host_still_works(&hosted.session, &hosted.link);
}

#[test]
fn random_websocket_frames_are_rejected() {
    let hosted = Session::host_with(&notebook(), Peer::new("Ada"), config()).unwrap();
    let link = hosted.link.clone();
    let url = format!("ws://127.0.0.1:{}/herse/{}", link.hosts[0].port, link.session_id);
    let mut rng = Rng(7);
    smol::block_on(async {
        for round in 0..20 {
            let stream = smol::net::TcpStream::connect(("127.0.0.1", link.hosts[0].port)).await.unwrap();
            let (ws, _) = async_tungstenite::client_async(url.as_str(), stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            if round % 2 == 1 {
                // A valid hello, then a random "authenticated" frame.
                let nonce = crypto::new_nonce();
                assert!(send(&mut tx, crypto::hello_message(crypto::HELLO, &nonce)).await);
                let challenge = recv(&mut rx).await.unwrap();
                assert!(matches!(crypto::parse_handshake(&challenge), Some(HandshakeMsg::Challenge(_))));
            }
            let n = 1 + (rng.next() % 3000) as usize;
            let _ = tx.send(WsMessage::Binary(rng.bytes(n).into())).await;
            // Rejected (auth) or dropped; never a Welcome.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "connection not closed");
                match rx.next().await {
                    Some(Ok(WsMessage::Binary(b))) => {
                        assert_eq!(crypto::parse_handshake(&b), Some(HandshakeMsg::Reject(crypto::REJECT_AUTH)));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    });
    assert_eq!(hosted.session.status(), Status::Hosting { guests: 0 });
    host_still_works(&hosted.session, &hosted.link);
}

#[test]
fn tampered_frame_closes_the_connection() {
    let hosted = Session::host_with(&notebook(), Peer::new("Ada"), config()).unwrap();
    let host = hosted.session.clone();
    smol::block_on(async {
        let (mut tx, mut rx, mut ch) = raw_guest(&hosted.link).await.unwrap();
        // A proper message first: we get sync step 1 / 2 back.
        let step1 = Message::Sync(SyncMessage::SyncStep1(Default::default())).encode_v1();
        assert!(send(&mut tx, ch.send.seal(&step1)).await);
        let first = recv(&mut rx).await.unwrap();
        assert!(ch.recv.open(&first).is_some());
        wait(|| host.status() == Status::Hosting { guests: 1 }, "guest registered");
        // Now a tampered one.
        let mut bad = ch.send.seal(&step1);
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(send(&mut tx, bad).await);
        let deadline = Instant::now() + Duration::from_secs(5);
        while recv(&mut rx).await.is_some() {
            assert!(Instant::now() < deadline, "connection stayed open");
        }
    });
    wait(|| host.status() == Status::Hosting { guests: 0 }, "guest dropped");
    host_still_works(&host, &hosted.link);
}

#[test]
fn viewers_cannot_write_even_with_a_modified_client() {
    let hosted = Session::host_with(&notebook(), Peer::new("Ada"), config()).unwrap();
    let host = hosted.session.clone();
    let before = host.snapshot();
    smol::block_on(async {
        let (mut tx, mut rx, mut ch) = raw_guest(&hosted.view_link).await.unwrap();
        // Craft an update that inserts a cell, from a doc of our own.
        let doc = crate::doc::new_doc();
        let roots = crate::doc::Roots::new(&doc);
        let update = {
            let mut txn = doc.transact_mut();
            crate::doc::insert_cell(&mut txn, &roots, 0, &Cell::new(CellType::Code).with_source("evil"), "evil");
            txn.encode_update_v1()
        };
        for msg in [
            Message::Sync(SyncMessage::Update(update.clone())),
            Message::Sync(SyncMessage::SyncStep2(update)),
        ] {
            assert!(send(&mut tx, ch.send.seal(&msg.encode_v1())).await);
        }
        // Connection stays up (viewers are fine), the edit goes nowhere.
        let ping = crate::net::custom(crate::net::TAG_PING, b"");
        assert!(send(&mut tx, ch.send.seal(&ping)).await);
        let _ = recv(&mut rx).await;
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = host.poll();
    assert_eq!(host.snapshot(), before);
    assert_eq!(host.snapshot().cells.len(), 1);
}

#[test]
fn wrong_protocol_version_is_reported() {
    let hosted = Session::host_with(&notebook(), Peer::new("Ada"), config()).unwrap();
    let link = hosted.link.clone();
    let url = format!("ws://127.0.0.1:{}/herse/{}", link.hosts[0].port, link.session_id);
    smol::block_on(async {
        let stream = smol::net::TcpStream::connect(("127.0.0.1", link.hosts[0].port)).await.unwrap();
        let (ws, _) = async_tungstenite::client_async(url.as_str(), stream).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        let mut hello = crypto::hello_message(crypto::HELLO, &crypto::new_nonce());
        hello[7] = 99;
        assert!(send(&mut tx, hello).await);
        let reply = recv(&mut rx).await.unwrap();
        assert_eq!(crypto::parse_handshake(&reply), Some(HandshakeMsg::Reject(crypto::REJECT_VERSION)));
    });
}

/// Deterministic multi-replica fuzzing without sockets: cores exchange their
/// updates through queues the test controls (random delays and batching).
mod mesh {
    use std::sync::Arc;

    use herse_nb::{Cell, CellType, FormatHint, Notebook, Output, StreamName, Value};
    use yrs::{StateVector, Transact};

    use crate::changes::{DocChange, ORIGIN_INIT};
    use crate::ops::{self, LocalOp};
    use crate::presence::{Peer, Role};
    use crate::session::{Config, Core, SessionEvent};

    struct Node {
        core: Arc<Core>,
        model: Notebook,
        ids: Vec<String>,
        resets: usize,
    }

    fn host(nb: &Notebook) -> Node {
        let doc = crate::doc::new_doc();
        let roots = crate::doc::Roots::new(&doc);
        let (ids, _) = crate::doc::assign_cell_ids(nb);
        crate::doc::write_notebook(&mut doc.transact_mut_with(ORIGIN_INIT), &roots, nb, &ids);
        let core = Core::new(doc, Role::Host, "s".into(), Peer::new("host"), nb.format.clone(), Config::default());
        let mut model = nb.clone();
        for (c, id) in model.cells.iter_mut().zip(&ids) {
            c.id = Some(id.clone());
        }
        Node { core, model, ids, resets: 0 }
    }

    fn guest(host: &Node, name: &str) -> Node {
        let core = Core::new(crate::doc::new_doc(), Role::Editor, "s".into(), Peer::new(name), FormatHint::jupyter(), Config::default());
        core.apply_initial(&host.core.sync_step2(&StateVector::default())).unwrap();
        let model = snapshot(&core);
        let ids = model.cells.iter().map(|c| c.id.clone().unwrap()).collect();
        Node { core, model, ids, resets: 0 }
    }

    fn snapshot(core: &Core) -> Notebook {
        let _inner = core.lock();
        let mut nb = crate::doc::read_notebook(&core.doc.transact(), &core.roots, FormatHint::jupyter());
        nb.format = FormatHint::jupyter();
        nb
    }

    impl Node {
        fn edit(&mut self, op: LocalOp) {
            let op_dbg = format!("{op:?}");
            let core = self.core.clone();
            let assigned = core.local(|txn| ops::apply(txn, &core.roots, &core.tracker, op.clone())).unwrap().unwrap();
            let ix = |ids: &Vec<String>, id: &str| ids.iter().position(|x| x == id).unwrap();
            let nb = &mut self.model;
            match op {
                LocalOp::InsertCell { index, mut cell } => {
                    let id = assigned.unwrap();
                    cell.id = Some(id.clone());
                    nb.cells.insert(index, cell);
                    self.ids.insert(index, id);
                }
                LocalOp::DeleteCell { id } => {
                    let i = ix(&self.ids, &id);
                    nb.cells.remove(i);
                    self.ids.remove(i);
                }
                LocalOp::MoveCell { id, to } => {
                    let i = ix(&self.ids, &id);
                    let c = nb.cells.remove(i);
                    let x = self.ids.remove(i);
                    nb.cells.insert(to, c);
                    self.ids.insert(to, x);
                }
                LocalOp::EditSource { id, range, text } => {
                    let i = ix(&self.ids, &id);
                    nb.cells[i].source.replace_range(range, &text);
                }
                LocalOp::SetCellType { id, cell_type } => {
                    let i = ix(&self.ids, &id);
                    let c = &mut nb.cells[i];
                    if c.cell_type != cell_type {
                        c.cell_type = cell_type;
                        c.outputs.clear();
                        c.execution_count = None;
                        if cell_type == CellType::Code {
                            c.extra.shift_remove("outputs");
                            c.extra.shift_remove("execution_count");
                        }
                    }
                }
                LocalOp::SetCellMetadata { id, metadata } => {
                    let i = ix(&self.ids, &id);
                    nb.cells[i].metadata = metadata;
                }
                LocalOp::PushOutput { id, output } => {
                    let i = ix(&self.ids, &id);
                    nb.cells[i].outputs.push(output);
                }
                LocalOp::AppendStreamText { id, index, text } => {
                    let i = ix(&self.ids, &id);
                    if let Output::Stream { text: t, .. } = &mut nb.cells[i].outputs[index] {
                        t.push_str(&text);
                    }
                }
                LocalOp::UpdateCell { cell } => {
                    let i = ix(&self.ids, cell.id.as_deref().unwrap());
                    nb.cells[i] = cell;
                }
                LocalOp::SetOutputs { id, outputs } => {
                    let i = ix(&self.ids, &id);
                    nb.cells[i].outputs = outputs;
                }
                LocalOp::ReplaceOutput { id, index, output } => {
                    let i = ix(&self.ids, &id);
                    nb.cells[i].outputs[index] = output;
                }
                LocalOp::SetNotebookMetadata { metadata } => {
                    let threads = crate::comments::load(nb);
                    nb.metadata = metadata;
                    crate::comments::store(nb, &threads);
                }
                other => panic!("not mirrored: {other:?}"),
            }
            let snap = snapshot(&self.core);
            let mut model = self.model.clone();
            model.format = FormatHint::jupyter();
            if model != snap {
                for (i, (a, b)) in model.cells.iter().zip(&snap.cells).enumerate() {
                    assert_eq!(a, b, "local op broke cell {i}: {op_dbg}");
                }
                assert_eq!(model, snap, "local op diverged: {op_dbg}");
            }
        }

        /// A random comment action, applied like `Session` does it.
        fn comment(&mut self, rng: &mut Rng) {
            let core = self.core.clone();
            let threads = crate::comments::read_threads(&core.doc.transact(), &core.roots.comments);
            let roll = rng.below(4);
            core.local(|txn| {
                let root = &core.roots.comments;
                match (roll, threads.get(rng.below(threads.len().max(1)))) {
                    (0, _) | (_, None) => {
                        let cell = self.ids.first().cloned().unwrap_or_default();
                        crate::comments::write_thread(txn, root, &crate::comments::Thread::new(&cell, None, "me", "hi"));
                    }
                    (1, Some(t)) => {
                        crate::comments::add_message(txn, root, &t.id, &crate::comments::Comment::new("me", "re"));
                    }
                    (2, Some(t)) => {
                        crate::comments::set_resolved(txn, root, &t.id, !t.resolved, "me");
                    }
                    (_, Some(t)) => {
                        crate::comments::delete_message(txn, root, &t.id, &t.messages[0].id);
                    }
                }
            })
            .unwrap();
            // Comment changes reach the model as events, even local ones.
            self.pump();
        }

        fn take_outbox(&self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.core.lock().outbound)
        }

        fn pump(&mut self) {
            for ev in self.core.poll() {
                if let SessionEvent::Doc(change) = ev {
                    assert!(change.apply_to(&mut self.model), "change did not apply: {change:?}");
                    match &change {
                        DocChange::CellInserted { index, id, .. } => self.ids.insert(*index, id.clone()),
                        DocChange::CellRemoved { index, .. } => {
                            self.ids.remove(*index);
                        }
                        DocChange::CellMoved { from, to, id, .. } => {
                            self.ids.remove(*from);
                            self.ids.insert(*to, id.clone());
                        }
                        DocChange::CellReplaced { index, id, .. } => self.ids[*index] = id.clone(),
                        DocChange::Reset { cell_ids, .. } => {
                            self.resets += 1;
                            self.ids = cell_ids.clone();
                        }
                        _ => {}
                    }
                }
            }
            let snap = snapshot(&self.core);
            let mut model = self.model.clone();
            model.format = FormatHint::jupyter();
            if model != snap {
                for (i, (a, b)) in model.cells.iter().zip(&snap.cells).enumerate() {
                    assert_eq!(a, b, "cell {i} diverged");
                }
                assert_eq!(model.cells.len(), snap.cells.len(), "cell count diverged");
                assert_eq!(model, snap, "notebook diverged");
            }
            let ids: Vec<String> = self.core.tracker.lock().unwrap().shadow.iter().map(|(id, _)| id.clone()).collect();
            assert_eq!(self.ids, ids);
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
        fn boundary(&mut self, s: &str) -> usize {
            let b: Vec<usize> = (0..=s.len()).filter(|&i| s.is_char_boundary(i)).collect();
            b[self.below(b.len())]
        }
    }

    const SNIPPETS: &[&str] = &["a", "bc", "é", "日本語", "🦀", "\n", "ü→✓", "def f():\n    pass\n"];

    fn random_op(rng: &mut Rng, n: &Node) -> LocalOp {
        let len = n.model.cells.len();
        let roll = rng.below(100);
        if len == 0 || roll < 8 {
            let t = if rng.below(2) == 0 { CellType::Code } else { CellType::Markdown };
            return LocalOp::InsertCell { index: rng.below(len + 1), cell: Cell::new(t).with_source(SNIPPETS[rng.below(SNIPPETS.len())]) };
        }
        let i = rng.below(len);
        let id = n.ids[i].clone();
        if n.ids.iter().position(|x| *x == id) != Some(i) {
            // A duplicate id (concurrent moves, before the host repairs):
            // ops address the first copy, so do not aim at this one.
            return LocalOp::InsertCell { index: rng.below(len + 1), cell: Cell::new(CellType::Raw).with_source("r") };
        }
        let cell = &n.model.cells[i];
        match roll {
            8..=12 if len > 1 => LocalOp::DeleteCell { id },
            13..=20 if len > 1 => LocalOp::MoveCell { id, to: rng.below(len) },
            21..=23 => LocalOp::SetCellType {
                id,
                cell_type: if cell.cell_type == CellType::Code { CellType::Markdown } else { CellType::Code },
            },
            24..=27 => {
                let mut m = cell.metadata.clone();
                m.insert(format!("k{}", rng.below(3)), Value::from(rng.below(5) as u64));
                LocalOp::SetCellMetadata { id, metadata: m }
            }
            32..=33 => {
                let mut c = cell.clone();
                c.source.push_str(SNIPPETS[rng.below(SNIPPETS.len())]);
                c.metadata.insert("u".into(), Value::from(rng.below(9) as u64));
                LocalOp::UpdateCell { cell: c }
            }
            34 if cell.cell_type == CellType::Code => {
                let mut outs = cell.outputs.clone();
                if rng.below(2) == 0 || outs.is_empty() {
                    outs.push(Output::Stream { name: StreamName::Stderr, text: "warn\n".into() });
                } else {
                    outs.pop();
                }
                LocalOp::SetOutputs { id, outputs: outs }
            }
            35 if cell.cell_type == CellType::Code && !cell.outputs.is_empty() => LocalOp::ReplaceOutput {
                id,
                index: rng.below(cell.outputs.len()),
                output: Output::Stream { name: StreamName::Stdout, text: format!("replaced {}\n", rng.below(9)) },
            },
            36 => {
                let mut m = n.model.metadata.clone();
                m.insert(format!("nb{}", rng.below(3)), Value::from(rng.below(9) as u64));
                LocalOp::SetNotebookMetadata { metadata: m }
            }
            28..=31 if cell.cell_type == CellType::Code => {
                if let Some(k) = cell.outputs.iter().position(Output::is_stream).filter(|_| rng.below(2) == 0) {
                    LocalOp::AppendStreamText { id, index: k, text: SNIPPETS[rng.below(SNIPPETS.len())].into() }
                } else {
                    LocalOp::PushOutput { id, output: Output::Stream { name: StreamName::Stdout, text: "out\n".into() } }
                }
            }
            _ => {
                let a = rng.boundary(&cell.source);
                let b = if rng.below(3) == 0 { rng.boundary(&cell.source) } else { a };
                let (a, b) = (a.min(b), a.max(b));
                let text = if a != b && rng.below(2) == 0 { "" } else { SNIPPETS[rng.below(SNIPPETS.len())] };
                LocalOp::EditSource { id, range: a..b, text: text.into() }
            }
        }
    }

    /// Star topology, like production: guests talk to the host over FIFO
    /// links; the host relays what it receives to the other guests.
    fn run(seed: u64, peers: usize, steps: usize) {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        nb.cells = (0..5).map(|i| Cell::with_new_id(CellType::Code).with_source(format!("cell {i} é"))).collect();
        let mut nodes = vec![host(&nb)];
        for k in 1..peers {
            let g = guest(&nodes[0], &format!("g{k}"));
            nodes.push(g);
        }
        let mut to_host: Vec<Vec<Vec<u8>>> = vec![Vec::new(); peers];
        let mut to_guest: Vec<Vec<Vec<u8>>> = vec![Vec::new(); peers];
        let route = |nodes: &Vec<Node>, to_host: &mut Vec<Vec<Vec<u8>>>, to_guest: &mut Vec<Vec<Vec<u8>>>| {
            for (k, node) in nodes.iter().enumerate() {
                for u in node.take_outbox() {
                    if k == 0 {
                        for q in to_guest.iter_mut().skip(1) {
                            q.push(u.clone());
                        }
                    } else {
                        to_host[k].push(u);
                    }
                }
            }
        };
        let deliver_to_host = |nodes: &Vec<Node>, to_host: &mut Vec<Vec<Vec<u8>>>, to_guest: &mut Vec<Vec<Vec<u8>>>, g: usize, n: usize| {
            for u in to_host[g].drain(..n) {
                for (h, q) in to_guest.iter_mut().enumerate().skip(1) {
                    if h != g {
                        q.push(u.clone());
                    }
                }
                nodes[0].core.queue_inbound(u);
            }
        };
        let mut rng = Rng(seed);
        for _ in 0..steps {
            let k = rng.below(peers);
            match rng.below(10) {
                0..=5 => {
                    if rng.below(20) == 0 {
                        nodes[k].comment(&mut rng);
                    } else {
                        let op = random_op(&mut rng, &nodes[k]);
                        nodes[k].edit(op);
                    }
                }
                6..=7 if k > 0 => {
                    if rng.below(2) == 0 {
                        let n = rng.below(to_host[k].len() + 1);
                        deliver_to_host(&nodes, &mut to_host, &mut to_guest, k, n);
                    } else {
                        let n = rng.below(to_guest[k].len() + 1);
                        for u in to_guest[k].drain(..n) {
                            nodes[k].core.queue_inbound(u);
                        }
                    }
                }
                _ => nodes[k].pump(),
            }
            route(&nodes, &mut to_host, &mut to_guest);
        }
        // Drain everything (a few rounds: the host's repairs go out after it polls).
        for _ in 0..4 {
            route(&nodes, &mut to_host, &mut to_guest);
            for g in 1..peers {
                let n = to_host[g].len();
                deliver_to_host(&nodes, &mut to_host, &mut to_guest, g, n);
            }
            nodes[0].pump();
            route(&nodes, &mut to_host, &mut to_guest);
            for g in 1..peers {
                for u in to_guest[g].drain(..) {
                    nodes[g].core.queue_inbound(u);
                }
            }
            for n in nodes.iter_mut() {
                n.pump();
            }
        }
        let first = snapshot(&nodes[0].core);
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(snapshot(&n.core), first, "replica {i} differs (seed {seed})");
            assert_eq!(n.resets, 0, "replica {i} needed a reset in the star topology (seed {seed})");
        }
        let unique: std::collections::HashSet<_> = nodes[0].ids.iter().collect();
        assert_eq!(unique.len(), nodes[0].ids.len(), "duplicate ids left (seed {seed})");
    }

    #[test]
    fn out_of_order_updates_reset_instead_of_diverging() {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        nb.cells[0].source = "abc".into();
        let mut a = host(&nb);
        let mut b = guest(&a, "b");
        let id = a.ids[0].clone();
        let mut updates = Vec::new();
        for (i, t) in ["1", "2", "3"].iter().enumerate() {
            a.edit(LocalOp::EditSource { id: id.clone(), range: i..i, text: (*t).into() });
            updates.extend(a.take_outbox());
        }
        a.edit(LocalOp::DeleteCell { id: id.clone() });
        updates.extend(a.take_outbox());
        // Deliver newest first, polling in between: yrs stashes what it
        // cannot integrate, and the session resets instead of guessing.
        for u in updates.into_iter().rev() {
            b.core.queue_inbound(u);
            b.pump();
        }
        assert!(b.resets > 0);
        assert_eq!(snapshot(&b.core), snapshot(&a.core));
        a.pump();
    }

    #[test]
    fn replicas_converge_and_events_track_the_doc() {
        let seeds: u64 = std::env::var("HERSE_COLLAB_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        for seed in 1..=seeds {
            let peers = 2 + (seed % 3) as usize;
            let r = std::panic::catch_unwind(|| run(seed * 0x9E37_79B9, peers, 400));
            if let Err(e) = r {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                panic!("seed {seed}: {}", msg.unwrap_or_default());
            }
        }
    }
}

