//! Live sessions: one notebook edited by several people at once
//! (`herse-collab`). The host serves it over the network (the LAN or a
//! Tailscale tailnet) and shares a link; guests join with the link. Every
//! frame is end-to-end encrypted with keys from the link's secret.
//!
//! The rule that keeps the notebook and the shared document in step: remote
//! changes are applied to the model on this (the UI) thread as soon as they
//! arrive, and local edits are sent from the same thread, so byte offsets
//! always agree. Typing goes out as one source edit per change; anything
//! else that changes the notebook (cells added, outputs, metadata) is
//! diffed in whole from `mark_dirty`.

use gpui_kit::{Context, SharedString, Task, Window};
use herse_collab::{
    DocChange, EndReason, JoinLink, LocalOp, Peer, PeerState, Presence, Role, Selection, Session, SessionEvent, Status,
};

use super::{NotebookEvent, NotebookView, comments};

/// This notebook's live session.
pub struct Live {
    pub session: Session,
    pub role: Role,
    /// Host only: the edit link, and the view-only one.
    pub link: Option<JoinLink>,
    pub view_link: Option<JoinLink>,
    /// Everyone else here, as last heard.
    pub peers: Vec<PeerState>,
    pub status: Status,
    _events: Task<()>,
}

impl Live {
    /// Who is in the session, for a status line: "Ada, Grace and 2 others".
    pub fn people(&self) -> SharedString {
        let names: Vec<&str> = self.peers.iter().map(|p| p.name.as_str()).collect();
        match names.len() {
            0 => "Only you so far".into(),
            1 => names[0].to_string().into(),
            2 => format!("{} and {}", names[0], names[1]).into(),
            n => format!("{}, {} and {} others", names[0], names[1], n - 2).into(),
        }
    }
}

/// A peer's color ("#RRGGBB") as a GPUI color.
pub fn peer_color(color: &str) -> gpui_kit::Hsla {
    gpui_kit::Rgba::try_from(color).map(Into::into).unwrap_or_else(|_| gpui_kit::hsla(0.5, 0.5, 0.5, 1.))
}

/// The bytes of `old` that changed to become `new`: the range replaced and
/// what replaced it (common prefix and suffix kept, on char boundaries).
fn source_diff(old: &str, new: &str) -> Option<(std::ops::Range<usize>, String)> {
    if old == new {
        return None;
    }
    let mut prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old.bytes().rev().zip(new.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    Some((prefix..old.len() - suffix, new[prefix..new.len() - suffix].to_string()))
}

impl NotebookView {
    pub fn live(&self) -> Option<&Live> {
        self.live.as_ref()
    }

    /// The session, when this notebook is live and may be edited here.
    pub(super) fn live_session(&self) -> Option<Session> {
        self.live.as_ref().filter(|l| l.role != Role::Viewer).map(|l| l.session.clone())
    }

    /// Whether this is a view-only guest's copy.
    pub(super) fn live_view_only(&self) -> bool {
        self.live.as_ref().is_some_and(|l| l.role == Role::Viewer)
    }

    /// A change of our own that the session already has (or mustn't get):
    /// dirty, without diffing the notebook into the session.
    pub(super) fn quiet_dirty(&mut self, cx: &mut Context<Self>) {
        self.live_quiet = true;
        self.mark_dirty(cx);
        self.live_quiet = false;
    }

    /// Host this notebook: serve it and make its links. Older notebooks get
    /// cell ids first (nbformat 4.5), as Jupyter does when it upgrades one.
    pub fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if self.live.is_some() {
            return Ok(());
        }
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        }
        if self.doc.nb.nbformat == 4 && self.doc.nb.nbformat_minor < 5 {
            self.doc.nb.upgrade_to_4_5();
            self.quiet_dirty(cx);
        }
        let hosted = Session::host(&self.doc.nb, Peer::new(comments::my_name())).map_err(|e| e.to_string())?;
        for (ix, id) in hosted.cell_ids.iter().enumerate() {
            if let Some(cell) = self.doc.nb.cells.get_mut(ix)
                && cell.id.as_deref() != Some(id.as_str())
            {
                cell.id = Some(id.clone());
            }
        }
        let events = self.listen(&hosted.session, window, cx);
        self.live = Some(Live {
            session: hosted.session,
            role: Role::Host,
            link: Some(hosted.link),
            view_link: Some(hosted.view_link),
            peers: Vec::new(),
            status: Status::Hosting { guests: 0 },
            _events: events,
        });
        self.send_presence(cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// A guest's copy, joined with a link.
    pub fn attach_live(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let role = session.role();
        let events = self.listen(&session, window, cx);
        self.live = Some(Live {
            peers: session.peers(),
            session,
            role,
            link: None,
            view_link: None,
            status: Status::Connected,
            _events: events,
        });
        self.send_presence(cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// End the session (host: for everyone), or leave it (guest: the copy
    /// stays, as an untitled notebook).
    pub fn stop_live(&mut self, cx: &mut Context<Self>) {
        if let Some(live) = self.live.take() {
            match live.role {
                Role::Host => live.session.end(),
                _ => live.session.leave(),
            }
        }
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    fn listen(&mut self, session: &Session, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let events = session.events();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(batch) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.apply_remote(batch, window, cx)).is_err() {
                    break;
                }
            }
        })
    }

    /// Diff the whole notebook into the session (from `mark_dirty`).
    pub(super) fn sync_live(&mut self) {
        let Some(session) = self.live_session() else { return };
        match session.sync_notebook(&self.doc.nb) {
            Ok(assigned) => {
                for a in assigned {
                    if let Some(cell) = self.doc.nb.cells.get_mut(a.index) {
                        cell.id = Some(a.id);
                    }
                }
            }
            Err(err) => eprintln!("live: sync failed: {err}"),
        }
    }

    /// Typing: send just what changed in cell `ix` (its source was `old`).
    pub(super) fn send_source_edit(&mut self, ix: usize, old: &str) {
        let Some(session) = self.live_session() else { return };
        let Some(cell) = self.doc.nb.cells.get(ix) else { return };
        let Some(id) = cell.id.clone() else { return };
        if let Some((range, text)) = source_diff(old, &cell.source)
            && let Err(err) = session.apply_local(LocalOp::EditSource { id, range, text })
        {
            eprintln!("live: edit not sent ({err}); syncing the notebook");
            self.sync_live();
        }
    }

    /// Where I am: the selected cell, and my selection in it while editing.
    pub(super) fn send_presence(&mut self, cx: &mut Context<Self>) {
        let Some(live) = &self.live else { return };
        let cell = self.doc.nb.cells.get(self.selected).and_then(|c| c.id.clone());
        let selection = self
            .editing
            .as_ref()
            .filter(|e| self.cells.get(self.selected).is_some_and(|ui| ui.uid == e.uid))
            .map(|e| {
                let range = e.editor.read(cx).selected_range();
                Selection { anchor: range.start, head: range.end }
            });
        live.session.set_presence(Presence { cell, selection });
    }

    /// Peers whose selected cell is cell `ix`.
    pub(super) fn peers_at(&self, ix: usize) -> Vec<&PeerState> {
        let (Some(live), Some(id)) = (&self.live, self.doc.nb.cells.get(ix).and_then(|c| c.id.as_deref())) else {
            return Vec::new();
        };
        live.peers.iter().filter(|p| p.cell.as_deref() == Some(id)).collect()
    }

    /// Another peer's changes: apply each to the notebook, in order, and
    /// keep the view in step without disturbing the cell being edited here.
    fn apply_remote(&mut self, batch: Vec<SessionEvent>, window: &mut Window, cx: &mut Context<Self>) {
        let mut doc_changed = false;
        let mut peers_changed = false;
        for event in batch {
            match event {
                SessionEvent::Doc(change) => {
                    doc_changed = true;
                    if !self.apply_change(&change, window, cx) {
                        // Our copy diverged: start over from the session's.
                        if let Some(live) = &self.live {
                            let nb = live.session.snapshot();
                            self.replace_notebook(nb, cx);
                        }
                    }
                }
                SessionEvent::PeerJoined(_) => {
                    peers_changed = true;
                    // Guests learn the notebook's name from the host.
                    if let Some(live) = self.live.as_ref().filter(|l| l.role == Role::Host) {
                        let title = self.title();
                        if let Err(err) = live.session.send_message(serde_json::json!({ "herse": "title", "name": title })) {
                            eprintln!("live: title not sent: {err}");
                        }
                    }
                }
                SessionEvent::PeerUpdated(_) | SessionEvent::PeerLeft(_) => peers_changed = true,
                SessionEvent::Status(status) => {
                    if let Some(live) = &mut self.live {
                        live.status = status;
                    }
                    cx.emit(NotebookEvent::Changed);
                }
                SessionEvent::Message { payload, .. } => {
                    if payload.get("herse").and_then(|v| v.as_str()) == Some("title")
                        && let Some(name) = payload.get("name").and_then(|v| v.as_str())
                    {
                        self.name_hint = Some(name.to_string());
                        cx.emit(NotebookEvent::Changed);
                    }
                }
                SessionEvent::Ended(reason) => {
                    let what = match reason {
                        EndReason::HostEnded => "The host ended the live session. This copy stays open.",
                        EndReason::LinkRevoked => "The host no longer accepts this link.",
                        EndReason::Left => "You left the live session.",
                        EndReason::Failed(_) => "The live session stopped.",
                    };
                    self.live = None;
                    self.flash(what, cx);
                    cx.emit(NotebookEvent::Changed);
                }
            }
        }
        if peers_changed && let Some(live) = &mut self.live {
            live.peers = live.session.peers();
        }
        if doc_changed {
            self.hidden_dirty = true;
            self.schedule_highlights(cx);
            self.quiet_dirty(cx);
        }
        cx.notify();
    }

    /// One remote change, mirrored in the view. `false` when it doesn't fit
    /// our copy.
    fn apply_change(&mut self, change: &DocChange, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // The source as it was, for an edit to the cell being edited here.
        let editing_ix = self.editing.as_ref().and_then(|e| self.cells.iter().position(|ui| ui.uid == e.uid));
        if !change.apply_to(&mut self.doc.nb) {
            return false;
        }
        match change {
            DocChange::CellInserted { index, .. } => {
                let ui = self.new_cell_ui();
                self.cells.insert(*index, ui);
                self.list.splice(*index..*index, 1);
                if self.selected >= *index && self.doc.nb.cells.len() > 1 {
                    self.selected += 1;
                }
            }
            DocChange::CellRemoved { index, .. } => {
                if editing_ix == Some(*index) {
                    self.editing = None;
                }
                self.cells.remove(*index);
                self.list.splice(*index..*index + 1, 0);
                if self.selected > *index || self.selected >= self.doc.nb.cells.len() {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            DocChange::CellMoved { from, to, .. } => {
                let ui = self.cells.remove(*from);
                self.cells.insert(*to, ui);
                self.list.splice(*from..*from + 1, 0);
                self.list.splice(*to..*to, 1);
            }
            DocChange::CellReplaced { index, .. } => {
                if editing_ix == Some(*index)
                    && let Some(editing) = &self.editing
                {
                    let source = self.doc.nb.cells[*index].source.clone();
                    editing.editor.update(cx, |e, cx| e.set_value(source, window, cx));
                }
                self.refresh_cell(*index, cx);
            }
            DocChange::SourceEdited { index, edits, .. } => {
                if editing_ix == Some(*index)
                    && let Some(editing) = &self.editing
                {
                    // Into the open editor, keeping my selection on its text.
                    editing.editor.update(cx, |e, cx| {
                        let mut sel = e.selected_range();
                        for edit in edits {
                            e.set_selected_range(edit.range.clone(), cx);
                            e.replace(edit.text.clone(), window, cx);
                            let delta = edit.text.len() as isize - edit.range.len() as isize;
                            let shift = |at: usize| {
                                if at >= edit.range.end {
                                    (at as isize + delta).max(0) as usize
                                } else if at > edit.range.start {
                                    edit.range.start + edit.text.len()
                                } else {
                                    at
                                }
                            };
                            sel = shift(sel.start)..shift(sel.end);
                        }
                        e.set_selected_range(sel, cx);
                    });
                }
                if let Some(ui) = self.cells.get_mut(*index) {
                    ui.rev += 1;
                }
                self.refresh_markdown(*index, cx);
            }
            DocChange::OutputsChanged { index, .. }
            | DocChange::OutputChanged { index, .. }
            | DocChange::OutputTextEdited { index, .. }
            | DocChange::ExecutionCountChanged { index, .. } => self.invalidate_outputs(*index),
            DocChange::CommentsChanged { threads } => self.set_threads(threads.clone(), cx),
            DocChange::Reset { notebook, .. } => self.replace_notebook(notebook.clone(), cx),
            _ => {}
        }
        true
    }

    /// A cell's content was replaced: draw it afresh.
    fn refresh_cell(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(ui) = self.cells.get_mut(ix) {
            ui.rev += 1;
            ui.markdown = None;
        }
        self.invalidate_outputs(ix);
        let _ = cx;
    }

    /// The whole notebook, replaced (a reset, or our copy diverged).
    fn replace_notebook(&mut self, nb: herse_nb::Notebook, cx: &mut Context<Self>) {
        self.doc.nb = nb;
        self.editing = None;
        self.anchor = None;
        let count = self.doc.nb.cells.len();
        self.cells.clear();
        for _ in 0..count {
            let ui = self.new_cell_ui();
            self.cells.push(ui);
        }
        self.selected = self.selected.min(count.saturating_sub(1));
        self.list.reset(count);
        self.hidden_dirty = true;
        self.comments = comments::Comments::load(&self.doc.nb);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::source_diff;

    #[test]
    fn diffs_sources() {
        assert_eq!(source_diff("abc", "abc"), None);
        assert_eq!(source_diff("abc", "abXc"), Some((2..2, "X".into())));
        assert_eq!(source_diff("abc", "ac"), Some((1..2, String::new())));
        assert_eq!(source_diff("", "hi"), Some((0..0, "hi".into())));
        assert_eq!(source_diff("aaa", "aa"), Some((2..3, String::new())));
        // Never splits a character.
        assert_eq!(source_diff("é", "è"), Some((0..2, "è".into())));
        assert_eq!(source_diff("xé", "xéé"), Some((3..3, "é".into())));
    }
}
