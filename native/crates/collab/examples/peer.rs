//! A scripted peer, for testing an app against a live session without a
//! second person: hosts a notebook, prints its join link on stdout, and when
//! a guest joins, edits the first cell, adds a cell and moves its cursor.
//! Every change it receives is logged on stderr.
//!
//! `cargo run -p herse-collab --features session --example peer -- notebook.ipynb [seconds]`

use std::io::Write as _;
use std::time::{Duration, Instant};

use herse_collab::{LocalOp, Peer, Presence, Session, SessionEvent};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: peer NOTEBOOK [SECONDS]");
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let nb = herse_nb::parse(&std::fs::read(&path).expect("read notebook")).expect("parse notebook");
    let hosted = Session::host(&nb, Peer::new("Robot Host")).expect("host");
    println!("{}", hosted.link);
    std::io::stdout().flush().ok();
    let session = hosted.session;
    let ids = hosted.cell_ids;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut edited = false;
    while Instant::now() < deadline {
        for event in session.poll() {
            match event {
                SessionEvent::PeerJoined(peer) => {
                    eprintln!("joined: {} ({:?})", peer.name, peer.role);
                    if !edited && !ids.is_empty() {
                        edited = true;
                        std::thread::sleep(Duration::from_millis(400));
                        let first = ids[0].clone();
                        session
                            .apply_local(LocalOp::EditSource { id: first.clone(), range: 0..0, text: "LIVE ✓ ".into() })
                            .expect("edit");
                        let mut cell = herse_nb::Notebook::new_empty("python3", "Python 3", "python").new_cell(herse_nb::CellType::Markdown);
                        cell.id = None;
                        cell.source = "## Added by Robot Host, live".into();
                        session.apply_local(LocalOp::InsertCell { index: 1, cell }).expect("insert");
                        session.set_presence(Presence { cell: Some(first), selection: None });
                        eprintln!("host: edited cell 0 and inserted a cell");
                    }
                }
                SessionEvent::PeerLeft(id) => eprintln!("left: {id:?}"),
                SessionEvent::Doc(change) => {
                    let text = format!("{change:?}");
                    eprintln!("remote: {}", &text[..text.len().min(160)]);
                }
                SessionEvent::Ended(reason) => {
                    eprintln!("ended: {reason:?}");
                    return;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    session.end();
}
