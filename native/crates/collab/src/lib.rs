//! # herse-collab
//!
//! Real-time collaboration for Herse notebooks without a server: one Herse
//! instance hosts a session and shares a join link; others open the link and
//! edit the notebook together live, with presence (who is where, cursors)
//! and comment threads. Peers connect directly over the LAN or a Tailscale
//! tailnet; every frame is end-to-end encrypted with keys derived from the
//! link's secret.
//!
//! ```no_run
//! use herse_collab::{LocalOp, Peer, Session, SessionEvent};
//! # fn main() -> herse_collab::Result<()> {
//! let nb = herse_nb::parse(&std::fs::read("analysis.ipynb")?).unwrap();
//!
//! // Host: start serving, share `hosted.link` (or `hosted.view_link`).
//! let hosted = Session::host(&nb, Peer::new("Ada"))?;
//! println!("{}", hosted.link); // herse://join/100.101.1.2:52345,192.168.1.20:52345,ada-mbp.local:52345/3f9c...#Qm9i...
//!
//! // Guest (another machine): join and get the notebook.
//! let joined = Session::join_blocking(&hosted.link, Peer::new("Grace"))?;
//! let mut model = joined.notebook;
//!
//! // Local edit (typing "x" at byte 0 of the first cell): change the model,
//! // tell the session.
//! let id = joined.cell_ids[0].clone();
//! model.cells[0].source.insert(0, 'x');
//! joined.session.apply_local(LocalOp::EditSource { id, range: 0..0, text: "x".into() })?;
//!
//! // Remote changes: integrate and apply in one go.
//! for event in joined.session.poll() {
//!     if let SessionEvent::Doc(change) = event {
//!         change.apply_to(&mut model);
//!     }
//! }
//!
//! // Host saving: the snapshot is complete (comments included) and keeps
//! // the file's formatting.
//! let text = herse_nb::to_string(&hosted.session.snapshot());
//! # let _ = text;
//! # Ok(()) }
//! ```
//!
//! ## Pieces
//!
//! * [`doc`] — the shared document (a [yrs] / Yjs CRDT in JupyterLab's RTC
//!   schema) and lossless [`Notebook`](herse_nb::Notebook) <-> doc conversion.
//! * [`LocalOp`] — local edits: cells inserted, deleted, moved, retyped,
//!   updated; source byte-range edits; outputs (push, replace, append to a
//!   stream, set); metadata. [`Session::sync_notebook`] diffs a whole
//!   notebook in when computing ops is a chore.
//! * [`DocChange`] — remote changes as typed, in-order events with byte
//!   ranges; [`DocChange::apply_to`] is the reference way to apply them.
//! * [`presence`] — names, colors, selected cell and cursor/selection.
//! * [`comments`] — threads anchored to cells; works offline (load/store in
//!   notebook metadata) and live (merged through the CRDT).
//! * [`link`] — join links and this machine's reachable addresses.
//! * [`crypto`] — handshake, key derivation and frame format.
//!
//! ## Threading contract
//!
//! Each session runs its network I/O on its own thread with a private smol
//! executor; nothing needs tokio or a caller-provided runtime, and
//! [`Session`] methods never block on the network ([`Session::join`] is a
//! runtime-agnostic future).
//!
//! Remote document updates are **not** applied by the network thread: they
//! are queued and integrated on the thread that calls [`Session::poll`] (or
//! [`Events::recv`] / [`Events::try_recv`]), which returns the resulting
//! changes. Local edits ([`Session::apply_local`]) are applied immediately on
//! the calling thread. So if the app applies each returned batch to its model
//! right away, on the thread that also issues local edits, the model and the
//! doc never disagree: byte offsets in local ops refer to the text the doc
//! has, offsets in events to the text the model has, and no operational
//! transformation is needed. In GPUI:
//!
//! ```ignore
//! let events = session.events();
//! cx.spawn(async move |this, cx| {
//!     while let Some(batch) = events.recv().await {
//!         this.update(cx, |this, cx| this.apply_remote(batch, cx)).ok();
//!     }
//! }).detach();
//! ```
//!
//! ## Applying remote changes
//!
//! * A batch applies in order; indices refer to the cell list after the
//!   previous changes, [`TextEdit`] ranges to the text after the previous
//!   edits. Every cell-level change carries the cell id.
//! * [`DocChange::CellMoved`] / [`DocChange::CellReplaced`] carry the full
//!   cell: replace your copy's content (keep UI state by id). Moves are a
//!   delete plus a re-insert in Yjs, so a concurrent edit to a moved cell can
//!   be lost; the event makes sure the model loses it too.
//! * [`DocChange::Reset`] replaces the whole model; it only happens if
//!   updates arrive out of causal order, which this transport never does.
//! * Two cells can briefly share an id after two peers move the same cell at
//!   once; edits address the first copy, and the host repairs the duplicate
//!   within one round trip (removing an identical copy or re-iding it), which
//!   arrives as ordinary changes.
//! * If [`DocChange::apply_to`] ever returns false, the model diverged (a
//!   bug): replace it with [`Session::snapshot`] and [`Session::cell_ids`].
//!
//! Presence is not part of the doc; [`Session::peers`] resolves everyone's
//! cursors against the doc as of the last poll.
//!
//! ## Transport and security
//!
//! The host serves WebSocket on all interfaces (port chosen by the OS);
//! guests connect to `ws://<addr>:<port>/herse/<session-id>`, trying every
//! address in the link (staggered, first to answer wins) and reconnecting
//! with exponential backoff (250 ms .. 10 s) after a drop. Topology is a star:
//! the host relays document updates, presence and app messages between
//! guests. On top runs the standard y-sync protocol (sync step 1/2, updates,
//! awareness) with Herse control messages in its custom-message slot.
//!
//! Every message after the handshake is sealed with XChaCha20-Poly1305 under
//! per-connection, per-direction keys derived with HKDF-SHA256 from the link
//! secret and both sides' handshake nonces, with a random nonce per frame and
//! the frame's sequence number as associated data: wrong secrets, tampered,
//! replayed, reordered or dropped frames close the connection, and the secret
//! never crosses the wire (see [`crypto`]). A view-only link carries a second
//! secret that only grants viewing; the host drops document updates from
//! viewers, and their sessions refuse local edits and comments.
//! [`Session::end`] tells every guest and closes the server (the links stop
//! working).
//!
//! Limits of the model: anyone with an edit link can change or wipe the
//! notebook (the link is the capability; share view links when in doubt);
//! keys are not forward-secret (someone who records traffic and later gets
//! the link can decrypt it); authenticated peers are trusted not to send
//! resource-exhausting messages (obviously bogus element counts are rejected
//! before decoding, and yrs panics on crafted updates are caught).
//!
//! ## Features
//!
//! * `session` — the live engine (networking and crypto). Without it the
//!   crate is the document model, events and comments only (no network
//!   dependencies).
//!
//! [yrs]: https://docs.rs/yrs

// Without the engine, its doc-side internals have no callers (and the
// docs above link to items that do not exist).
#![cfg_attr(not(feature = "session"), allow(dead_code, unused_imports, rustdoc::broken_intra_doc_links))]

mod util;

pub mod changes;
pub mod comments;
pub mod doc;
mod json;
pub mod ops;
pub mod presence;

#[cfg(feature = "session")]
pub mod crypto;
mod error;
#[cfg(feature = "session")]
pub mod link;
#[cfg(feature = "session")]
mod net;
#[cfg(feature = "session")]
mod session;

pub use changes::{DocChange, TextEdit, apply_text_edits};
pub use comments::{Anchor, Comment, Thread};
pub use doc::{AssignedId, assign_cell_ids, notebook_from_doc, notebook_to_doc};
pub use error::{Error, Result};
pub use ops::LocalOp;
pub use presence::{Peer, PeerId, PeerState, Presence, Role, Selection, default_color};
pub use util::{format_rfc3339, parse_rfc3339};

#[cfg(feature = "session")]
pub use link::{AddressKind, HostPort, JoinLink, LocalAddress, local_addresses};
#[cfg(feature = "session")]
pub use session::{Config, EndReason, Events, Hosted, Joined, Session, SessionEvent, Status};

#[cfg(all(test, feature = "session"))]
mod tests;
