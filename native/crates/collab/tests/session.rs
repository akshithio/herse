//! Live sessions between in-process peers over real localhost sockets.

mod common;

use std::time::{Duration, Instant};

use common::*;
use herse_collab::{
    Anchor, DocChange, EndReason, Error, LocalOp, Peer, Presence, Role, Selection, Session, SessionEvent, Status,
};
use herse_nb::{Cell, CellType, MimeBundle, MimeData, Output, StreamName, Value};

fn edit(id: &str, range: std::ops::Range<usize>, text: &str) -> LocalOp {
    LocalOp::EditSource { id: id.into(), range, text: text.into() }
}

fn source(p: &P, id: &str) -> String {
    let i = p.ids.iter().position(|x| x == id).unwrap();
    p.model.cells[i].source.clone()
}

#[test]
fn two_peers_concurrent_edits_in_one_cell_utf8() {
    let nb = notebook(&["héllo wörld", "print(1)", "# Title"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");
    guest.check_model();
    assert_eq!(guest.session.role(), Role::Editor);

    // Concurrent: neither has polled the other's edit yet.
    host.edit(edit("cell0", 0..0, "→ ")); // "→ héllo wörld"
    guest.edit(edit("cell0", 13..13, " ✓日本")); // end of "héllo wörld" (13 bytes)
    host.edit(edit("cell0", 5..7, "e")); // é -> e in host's text (after "→ " = 4 bytes)
    guest.edit(edit("cell0", 6..13, "")); // " wörld" removed
    converge(&mut [&mut host, &mut guest]);
    let text = source(&host, "cell0");
    assert!(text.starts_with("→ he"), "{text}");
    assert!(text.ends_with(" ✓日本"), "{text}");
    assert!(!text.contains('é'), "{text}");
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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn char_boundary(&mut self, s: &str) -> usize {
        let bounds: Vec<usize> = (0..=s.len()).filter(|&i| s.is_char_boundary(i)).collect();
        bounds[self.below(bounds.len())]
    }
}

const SNIPPETS: &[&str] = &["a", "bc", "é", "日本語", "🦀", "\n", " ", "def f():\n    return 1\n", "ü→✓"];

/// One random valid op against this peer's model.
fn random_op(rng: &mut Rng, p: &P, structural: bool) -> LocalOp {
    let n = p.model.cells.len();
    let roll = rng.below(100);
    if n == 0 || (structural && roll < 8) {
        let kind = if rng.below(2) == 0 { CellType::Code } else { CellType::Markdown };
        let cell = Cell::new(kind).with_source(SNIPPETS[rng.below(SNIPPETS.len())]);
        return LocalOp::InsertCell { index: rng.below(n + 1), cell };
    }
    let i = rng.below(n);
    let id = p.ids[i].clone();
    if p.ids.iter().position(|x| *x == id) != Some(i) {
        // A duplicate id (concurrent moves, until the host repairs it): ops
        // address the first copy, so do not aim at this one.
        return LocalOp::InsertCell { index: rng.below(n + 1), cell: Cell::new(CellType::Raw).with_source("r") };
    }
    let src = &p.model.cells[i].source;
    match roll {
        _ if structural && roll < 12 && n > 1 => LocalOp::DeleteCell { id },
        _ if structural && roll < 17 && n > 1 => LocalOp::MoveCell { id, to: rng.below(n) },
        _ if structural && roll < 19 => {
            let t = if p.model.cells[i].cell_type == CellType::Code { CellType::Markdown } else { CellType::Code };
            LocalOp::SetCellType { id, cell_type: t }
        }
        _ if roll < 22 => {
            let mut m = p.model.cells[i].metadata.clone();
            m.insert(format!("k{}", rng.below(3)), Value::from(rng.below(1000) as u64));
            LocalOp::SetCellMetadata { id, metadata: m }
        }
        _ if roll < 25 && p.model.cells[i].cell_type == CellType::Code => LocalOp::PushOutput {
            id,
            output: Output::Stream { name: StreamName::Stdout, text: format!("out {}\n", rng.below(100)) },
        },
        _ if roll < 60 && !src.is_empty() => {
            let a = rng.char_boundary(src);
            let b = rng.char_boundary(src);
            let (a, b) = (a.min(b), a.max(b));
            let text = if rng.below(2) == 0 { String::new() } else { SNIPPETS[rng.below(SNIPPETS.len())].into() };
            LocalOp::EditSource { id, range: a..b, text }
        }
        _ => {
            let at = rng.char_boundary(src);
            LocalOp::EditSource { id, range: at..at, text: SNIPPETS[rng.below(SNIPPETS.len())].into() }
        }
    }
}

fn fuzz_round(seed: u64, peers: &mut [&mut P], ops: usize, structural: bool) {
    let mut rng = Rng(seed);
    for _ in 0..ops {
        let k = rng.below(peers.len());
        if rng.below(4) == 0 {
            peers[k].pump();
        }
        let op = random_op(&mut rng, peers[k], structural);
        peers[k].edit(op);
    }
    converge(peers);
}

#[test]
fn two_peers_random_text_edits_converge() {
    let nb = notebook(&["héllo wörld", "print('日本')", "# Title 🦀"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");
    for seed in 1..=3 {
        fuzz_round(seed * 7919, &mut [&mut host, &mut guest], 300, false);
    }
}

#[test]
fn three_peers_random_structure_races_converge() {
    let nb = notebook(&["a = 1", "b = 2", "# notes", "c = a + b", "print(c)"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut g1 = P::join(&hosted.link, "Grace");
    let mut g2 = P::join(&hosted.link, "Linus");
    for seed in 1..=4 {
        fuzz_round(seed * 104_729, &mut [&mut host, &mut g1, &mut g2], 250, true);
    }
    // No duplicate ids survive concurrent moves.
    let ids = host.session.cell_ids();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn three_peers_targeted_races() {
    let nb = notebook(&["zero", "one", "two", "three"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut g1 = P::join(&hosted.link, "Grace");
    let mut g2 = P::join(&hosted.link, "Linus");

    // Same insert position from two guests; delete vs edit; move vs edit;
    // the same cell moved to two places at once.
    g1.edit(LocalOp::InsertCell { index: 1, cell: Cell::new(CellType::Code).with_source("from g1") });
    g2.edit(LocalOp::InsertCell { index: 1, cell: Cell::new(CellType::Code).with_source("from g2") });
    host.edit(LocalOp::DeleteCell { id: "cell2".into() });
    g1.edit(edit("cell2", 0..0, "lost edit "));
    g2.edit(LocalOp::MoveCell { id: "cell3".into(), to: 0 });
    host.edit(edit("cell3", 5..5, "!"));
    g1.edit(LocalOp::MoveCell { id: "cell0".into(), to: 4 });
    g2.edit(LocalOp::MoveCell { id: "cell0".into(), to: 2 });
    converge(&mut [&mut host, &mut g1, &mut g2]);

    let snap = host.snapshot();
    let sources: Vec<&str> = snap.cells.iter().map(|c| c.source.as_str()).collect();
    assert!(sources.contains(&"from g1") && sources.contains(&"from g2"), "{sources:?}");
    assert!(!sources.iter().any(|s| s.contains("two")), "deleted cell came back: {sources:?}");
    assert_eq!(sources.iter().filter(|s| **s == "zero").count(), 1, "{sources:?}");
    assert_eq!(snap.cells.len(), 5, "{sources:?}");
}

#[test]
fn outputs_metadata_and_types_sync() {
    let nb = notebook(&["print('hi')", "x", "# md"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");

    let mut data = MimeBundle::new();
    data.insert("text/plain", MimeData::Text("<Figure>".into()));
    data.insert("image/png", MimeData::Text("iVBORw0KGgo=".into()));
    let mut json = herse_nb::Map::new();
    json.insert("z".into(), Value::from(1));
    json.insert("a".into(), serde_json::json!({"big": 9_007_199_254_740_993_u64, "f": 1.0}));
    data.insert("application/json", MimeData::Json(Value::Object(json)));
    host.edit(LocalOp::SetExecutionState { id: "cell0".into(), state: "running".into() });
    host.edit(LocalOp::PushOutput { id: "cell0".into(), output: Output::Stream { name: StreamName::Stdout, text: "hi".into() } });
    converge(&mut [&mut host, &mut guest]);
    host.edit(LocalOp::AppendStreamText { id: "cell0".into(), index: 0, text: " there\n".into() });
    converge(&mut [&mut host, &mut guest]);
    // An append alone arrives as just the appended text.
    assert!(guest.events.iter().any(|e| matches!(e, SessionEvent::Doc(DocChange::OutputTextEdited { output_index: 0, edits, .. }) if edits[0].text == " there\n")));
    host.edit(LocalOp::PushOutput {
        id: "cell0".into(),
        output: Output::DisplayData { data: data.clone(), metadata: Default::default(), transient: None },
    });
    host.edit(LocalOp::SetExecutionCount { id: "cell0".into(), execution_count: Some(7) });
    host.edit(LocalOp::SetExecutionState { id: "cell0".into(), state: "idle".into() });
    let mut md = herse_nb::Map::new();
    md.insert("tags".into(), serde_json::json!(["hide-input"]));
    guest.edit(LocalOp::SetCellMetadata { id: "cell1".into(), metadata: md });
    guest.edit(LocalOp::SetCellType { id: "cell2".into(), cell_type: CellType::Code });
    converge(&mut [&mut host, &mut guest]);

    let cell = &guest.model.cells[0];
    assert_eq!(cell.execution_count, Some(7));
    assert_eq!(cell.outputs[0], Output::Stream { name: StreamName::Stdout, text: "hi there\n".into() });
    // Key order and big integers survive through fidelity records.
    let Output::DisplayData { data: got, .. } = &cell.outputs[1] else { panic!() };
    assert_eq!(serde_json::to_string(&got.get("application/json").unwrap().as_json()).unwrap(), serde_json::to_string(&data.get("application/json").unwrap().as_json()).unwrap());
    assert!(guest.events.iter().any(|e| matches!(e, SessionEvent::Doc(DocChange::ExecutionStateChanged { state, .. }) if state == "running")));
    assert_eq!(host.model.cells[2].cell_type, CellType::Code);
}

#[test]
fn sync_notebook_diffs_a_whole_notebook() {
    let nb = notebook(&["one", "two", "three", "four"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");
    let mut edited = guest.model.clone();
    edited.cells.swap(0, 3);
    edited.cells[1].source.push_str(" (edited)");
    edited.cells.remove(2);
    edited.cells.push(Cell::new(CellType::Markdown).with_source("new cell"));
    edited.metadata.insert("title".into(), "Shared".into());
    let assigned = guest.session.sync_notebook(&edited).unwrap();
    assert_eq!(assigned.len(), 1);
    edited.cells[3].id = Some(assigned[0].id.clone());
    guest.model = edited.clone();
    guest.ids = edited.cells.iter().map(|c| c.id.clone().unwrap()).collect();
    converge(&mut [&mut host, &mut guest]);
    assert_eq!(host.snapshot(), normalized(edited));
}

#[test]
fn presence_join_cursor_and_leave() {
    let nb = notebook(&["abcdef", "x"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace Hopper");
    wait_for(&mut host, 5, "guest presence", |h| h.peers().iter().any(|p| p.name == "Grace Hopper"));
    let gid = guest.session.peer_id();
    assert!(host.events.iter().any(|e| matches!(e, SessionEvent::PeerJoined(p) if p.id == gid)));
    let me = host.peers().into_iter().find(|p| p.id == gid).unwrap();
    assert_eq!(me.color, Peer::new("Grace Hopper").color);
    assert_eq!(me.role, Role::Editor);

    guest.session.set_presence(Presence { cell: Some("cell0".into()), selection: Some(Selection { anchor: 2, head: 4 }) });
    wait_for(&mut host, 5, "guest cursor", |h| {
        h.peers().iter().any(|p| p.id == gid && p.selection == Some(Selection { anchor: 2, head: 4 }))
    });
    // The cursor sticks to its characters while the host types before it.
    host.edit(edit("cell0", 0..0, "日本"));
    let sel = host.peers().into_iter().find(|p| p.id == gid).unwrap().selection.unwrap();
    assert_eq!(sel, Selection { anchor: 8, head: 10 });
    assert_eq!(&source(&host, "cell0")[sel.range()], "cd");

    // The guest sees the host too.
    wait_for(&mut guest, 5, "host presence", |g| g.peers().iter().any(|p| p.name == "Ada" && p.role == Role::Host));

    let t = Instant::now();
    guest.session.leave();
    wait_for(&mut host, 5, "guest left", |h| h.events.iter().any(|e| matches!(e, SessionEvent::PeerLeft(id) if *id == gid)));
    assert!(t.elapsed() < Duration::from_secs(2), "leave took {:?}", t.elapsed());
    wait_for(&mut guest, 5, "guest ended", |g| g.events.iter().any(|e| matches!(e, SessionEvent::Ended(EndReason::Left))));
    wait_for(&mut host, 5, "no guests", |h| h.session.status() == Status::Hosting { guests: 0 });
}

#[test]
fn presence_times_out_for_a_frozen_peer() {
    let mut cfg = config();
    cfg.keepalive = Duration::from_millis(100);
    cfg.presence_timeout = Duration::from_millis(700);
    let nb = notebook(&["x"]);
    let (mut host, hosted) = P::host_with(&nb, "Ada", cfg.clone());
    let mut g1 = P::join_with(&hosted.link, "Grace", cfg.clone());
    let mut g2 = P::join_with(&hosted.link, "Linus", cfg);
    let id2 = g2.session.peer_id();
    wait_for(&mut g1, 5, "both peers", |g| g.peers().len() == 2);
    g2.session.debug_freeze(true);
    let t = Instant::now();
    wait_for(&mut host, 5, "frozen peer dropped (host)", |h| !h.peers().iter().any(|p| p.id == id2));
    wait_for(&mut g1, 5, "frozen peer dropped (guest)", |g| !g.peers().iter().any(|p| p.id == id2));
    assert!(t.elapsed() >= Duration::from_millis(500), "dropped too early: {:?}", t.elapsed());
    // Still connected, just silent; it comes back when it speaks again.
    assert_eq!(host.session.status(), Status::Hosting { guests: 2 });
    g2.session.debug_freeze(false);
    wait_for(&mut g1, 5, "peer back", |g| g.peers().iter().any(|p| p.id == id2));
    g2.pump();
}

#[test]
fn concurrent_comments_both_survive() {
    let nb = notebook(&["df.groupby('k').sum()", "x"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut g1 = P::join(&hosted.link, "Grace");
    let mut g2 = P::join(&hosted.link, "Linus");

    let t1 = g1.session.add_comment("cell0", Some(Anchor { start: 3, end: 10, quote: "groupby".into() }), "Why group?").unwrap();
    let t2 = g2.session.add_comment("cell0", None, "Looks good").unwrap();
    converge(&mut [&mut host, &mut g1, &mut g2]);
    // Concurrent replies to the same thread, and a concurrent resolve.
    let r1 = g1.session.reply(&t2, "Agreed").unwrap();
    let r2 = host.session.reply(&t2, "Merging").unwrap();
    g2.session.resolve(&t1, true).unwrap();
    converge(&mut [&mut host, &mut g1, &mut g2]);

    for p in [&host, &g1, &g2] {
        let threads = p.session.comments();
        assert_eq!(threads.len(), 2, "{}", p.name);
        let a = threads.iter().find(|t| t.id == t1).unwrap();
        let b = threads.iter().find(|t| t.id == t2).unwrap();
        assert!(a.resolved && a.resolved_by.as_deref() == Some("Linus"));
        assert_eq!(a.anchor.as_ref().unwrap().quote, "groupby");
        let ids: Vec<&str> = b.messages.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&r1.as_str()) && ids.contains(&r2.as_str()) && b.messages.len() == 3);
        // Ready to save: the snapshot carries them in metadata.
        let snap = p.session.snapshot();
        assert_eq!(herse_collab::comments::load(&snap).len(), 2);
    }
    assert!(g1.events.iter().any(|e| matches!(e, SessionEvent::Doc(DocChange::CommentsChanged { .. }))));

    // Saving and re-hosting keeps them.
    let saved = herse_nb::to_string(&host.session.snapshot());
    let reloaded = herse_nb::parse(saved.as_bytes()).unwrap();
    let (again, _) = P::host(&reloaded, "Ada");
    assert_eq!(again.session.comments(), host.session.comments());
}

#[test]
fn wrong_secret_and_wrong_session_are_rejected() {
    let nb = notebook(&["x"]);
    let (_host, hosted) = P::host(&nb, "Ada");
    let good = hosted.link.to_string();
    let (head, secret) = good.split_once('#').unwrap();
    let mut bytes: Vec<char> = secret.chars().collect();
    bytes[5] = if bytes[5] == 'A' { 'B' } else { 'A' };
    let bad_secret: herse_collab::JoinLink = format!("{head}#{}", bytes.iter().collect::<String>()).parse().unwrap();
    let err = smol::block_on(Session::join_with(&bad_secret, Peer::new("Mallory"), config())).unwrap_err();
    assert!(matches!(err, Error::AuthFailed), "{err}");

    let other_session: herse_collab::JoinLink = good.replace(&hosted.link.session_id, "0123456789abcdef").parse().unwrap();
    let err = smol::block_on(Session::join_with(&other_session, Peer::new("Mallory"), config())).unwrap_err();
    assert!(matches!(err, Error::AuthFailed), "{err}");

    // The host is unaffected.
    let guest = P::join(&hosted.link, "Grace");
    guest.check_model();
}

#[test]
fn view_only_guests_see_but_cannot_edit() {
    let nb = notebook(&["x = 1"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    assert!(hosted.view_link.view_only && hosted.view_link.to_string().contains("#ro."));
    let mut viewer = P::join(&hosted.view_link, "Viewer");
    assert_eq!(viewer.session.role(), Role::Viewer);
    assert!(matches!(viewer.session.apply_local(edit("cell0", 0..0, "!")), Err(Error::ReadOnly)));
    assert!(matches!(viewer.session.add_comment("cell0", None, "hi"), Err(Error::ReadOnly)));
    host.edit(edit("cell0", 5..5, "0"));
    converge(&mut [&mut host, &mut viewer]);
    assert_eq!(source(&viewer, "cell0"), "x = 10");
    wait_for(&mut host, 5, "viewer presence", |h| h.peers().iter().any(|p| p.role == Role::Viewer));
}

#[test]
fn host_end_kicks_everyone() {
    let nb = notebook(&["x"]);
    let (host, hosted) = P::host(&nb, "Ada");
    let mut g1 = P::join(&hosted.link, "Grace");
    let mut g2 = P::join(&hosted.link, "Linus");
    host.session.end();
    for g in [&mut g1, &mut g2] {
        wait_for(g, 5, "ended", |g| g.events.iter().any(|e| matches!(e, SessionEvent::Ended(EndReason::HostEnded))));
        assert_eq!(g.session.status(), Status::Ended);
        assert!(matches!(g.session.apply_local(edit("cell0", 0..0, "!")), Err(Error::Closed)));
        assert!(g.session.peers().is_empty());
    }
    assert!(host.session.wait_closed(Duration::from_secs(5)));
    assert!(smol::block_on(Session::join_with(&hosted.link, Peer::new("Late"), config())).is_err());
}

#[test]
fn guest_reconnects_and_syncs_offline_edits() {
    let nb = notebook(&["shared", "other"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");
    host.session.debug_drop_connections();
    wait_for(&mut guest, 5, "reconnecting", |g| {
        g.events.iter().any(|e| matches!(e, SessionEvent::Status(Status::Reconnecting { .. })))
    });
    // Edits on both sides while apart.
    guest.edit(edit("cell0", 6..6, " by guest"));
    host.edit(edit("cell1", 0..0, "host: "));
    wait_for(&mut guest, 10, "reconnected", |g| g.session.status() == Status::Connected);
    converge(&mut [&mut host, &mut guest]);
    assert_eq!(source(&host, "cell0"), "shared by guest");
    assert_eq!(source(&guest, "cell1"), "host: other");
    wait_for(&mut host, 5, "presence back", |h| h.peers().len() == 1);
}

#[test]
fn app_messages_reach_other_peers() {
    let nb = notebook(&["x"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut g1 = P::join(&hosted.link, "Grace");
    let mut g2 = P::join(&hosted.link, "Linus");
    // Wait until g1's presence reached the host (its id is known there).
    let id1 = g1.session.peer_id();
    wait_for(&mut host, 5, "g1 presence", |h| h.peers().iter().any(|p| p.id == id1));
    g1.session.send_message(serde_json::json!({"run": "cell0"})).unwrap();
    for p in [&mut host, &mut g2] {
        wait_for(p, 5, "message", |p| {
            p.events.iter().any(|e| matches!(e, SessionEvent::Message { from, payload } if *from == id1 && payload["run"] == "cell0"))
        });
    }
    g1.pump();
    assert!(!g1.events.iter().any(|e| matches!(e, SessionEvent::Message { .. })));
}

#[test]
fn events_recv_works_on_any_executor() {
    let nb = notebook(&["x"]);
    let (host, hosted) = P::host(&nb, "Ada");
    let joined = Session::join_blocking(&hosted.link, Peer::new("Grace")).unwrap();
    let events = joined.session.events();
    host.session.apply_local(edit("cell0", 1..1, "y")).unwrap();
    let mut model = joined.notebook.clone();
    let got = smol::block_on(async {
        loop {
            let batch = events.recv().await.expect("session alive");
            for e in batch {
                if let SessionEvent::Doc(c) = e {
                    assert!(c.apply_to(&mut model));
                    return true;
                }
            }
        }
    });
    assert!(got);
    assert_eq!(model.cells[0].source, "xy");
    host.session.end();
    // After the end, recv drains and then returns None.
    let done = smol::block_on(async {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if events.recv().await.is_none() {
                return true;
            }
        }
        false
    });
    assert!(done);
}

#[test]
fn bench_1000_keystrokes_apply_and_sync_fast() {
    let nb = notebook(&["", "other"]);
    let (mut host, hosted) = P::host(&nb, "Ada");
    let mut guest = P::join(&hosted.link, "Grace");
    let text: String = "fn main() { println!(\"héllo, wörld 🦀\"); }\n".chars().cycle().take(1000).collect();
    let start = Instant::now();
    let mut at = 0;
    for ch in text.chars() {
        let s = ch.to_string();
        host.session.apply_local(edit("cell0", at..at, &s)).unwrap();
        at += s.len();
    }
    let applied = start.elapsed();
    wait_for(&mut guest, 10, "all keystrokes", |g| g.model.cells[0].source.len() == text.len());
    let total = start.elapsed();
    assert_eq!(guest.model.cells[0].source, text);
    eprintln!("1000 keystrokes: applied locally in {applied:?}, synced to the guest in {total:?}");
    assert!(total < Duration::from_millis(100), "too slow: {total:?}");
    host.pump();
    host.model.cells[0].source = text;
    host.check_model();
}

#[test]
fn old_nbformat_keeps_ids_out_of_the_model() {
    let mut nb = notebook(&["a", "b"]);
    nb.nbformat_minor = 4;
    for c in &mut nb.cells {
        c.id = None;
    }
    let (mut host, hosted) = P::host(&nb, "Ada");
    assert_eq!(hosted.assigned_ids.len(), 2);
    let mut guest = P::join(&hosted.link, "Grace");
    assert!(guest.model.cells.iter().all(|c| c.id.is_none()));
    assert_eq!(guest.ids, hosted.cell_ids);
    let first = guest.ids[0].clone();
    guest.edit(edit(&first, 1..1, "!"));
    host.edit(LocalOp::InsertCell { index: 1, cell: Cell::new(CellType::Code).with_source("new") });
    converge(&mut [&mut host, &mut guest]);
    assert!(guest.events.iter().any(|e| matches!(e, SessionEvent::Doc(DocChange::CellInserted { id, cell, .. }) if !id.is_empty() && cell.id.is_none())));
    assert!(herse_nb::to_string(&host.session.snapshot()).find("\"id\"").is_none());
}

#[test]
fn joining_a_dead_address_fails_fast() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let nb = notebook(&["x"]);
    let (_host, hosted) = P::host(&nb, "Ada");
    let dead = hosted.link.with_hosts(vec![herse_collab::HostPort { host: "127.0.0.1".into(), port }]);
    let t = Instant::now();
    let err = smol::block_on(Session::join_with(&dead, Peer::new("Grace"), config())).unwrap_err();
    assert!(matches!(err, Error::Unreachable(_)), "{err}");
    assert!(t.elapsed() < Duration::from_secs(2));
    // A link listing a dead address first still works.
    let mut hosts = dead.hosts.clone();
    hosts.extend(hosted.link.hosts.clone());
    let both = hosted.link.with_hosts(hosts);
    let guest = P::join(&both, "Grace");
    guest.check_model();
}
