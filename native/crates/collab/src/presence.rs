//! Presence: who is here, which cell they are in, where their cursor is.
//!
//! Presence travels as Yjs *awareness* state (not part of the document).
//! Each peer's state is JSON:
//!
//! ```json
//! {
//!   "user": { "name": "Ada", "display_name": "Ada", "initials": "A", "color": "#e5484d" },
//!   "herse": {
//!     "role": "editor",
//!     "cell": "a1b2c3d4",
//!     "anchor": { "item": { "client": 12, "clock": 345 }, "assoc": 0 },
//!     "head":   { "item": { "client": 12, "clock": 350 }, "assoc": 0 }
//!   }
//! }
//! ```
//!
//! `user` is the field JupyterLab's awareness uses, so Jupyter clients would
//! show the name and color. Cursor ends are Yjs relative positions (yrs
//! `StickyIndex`) into the cell's source text: they stay on the same
//! character while others type, and every peer resolves them against its
//! own copy of the document when asked ([`crate::Session::peers`]).
//!
//! Peers broadcast their state every `keepalive`; a peer not heard from for
//! `presence_timeout` (default 30 s, as in y-protocols) is dropped, and the
//! host drops a guest's presence immediately when its connection closes.

use serde::{Deserialize, Serialize};
use yrs::{Assoc, IndexedSequence, ReadTxn, StickyIndex};

use crate::doc::{self, Roots};

/// A participant's stable id within a session (their Yjs client id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub u64);

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who I am: shown to the other participants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Display name.
    pub name: String,
    /// CSS hex color (`#rrggbb`) for cursors and avatars.
    pub color: String,
}

impl Peer {
    /// A peer with a color picked from the name (stable across sessions).
    pub fn new(name: impl Into<String>) -> Peer {
        let name = name.into();
        let color = default_color(&name).to_owned();
        Peer { name, color }
    }

    pub fn with_color(name: impl Into<String>, color: impl Into<String>) -> Peer {
        Peer { name: name.into(), color: color.into() }
    }
}

/// A palette color chosen by hashing `name` (FNV-1a).
pub fn default_color(name: &str) -> &'static str {
    const PALETTE: [&str; 12] = [
        "#e5484d", "#f76b15", "#ffc53d", "#46a758", "#12a594", "#0090ff", "#3e63dd", "#8e4ec6", "#d6409f", "#ad7f58",
        "#00a2c7", "#5bb98b",
    ];
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

/// What a participant may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Runs the session (and the server). Can edit.
    Host,
    /// Joined with an edit link.
    Editor,
    /// Joined with a view-only link: sees everything, cannot edit or comment.
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Editor => "editor",
            Role::Viewer => "viewer",
        }
    }

    fn from_str_opt(s: &str) -> Option<Role> {
        match s {
            "host" => Some(Role::Host),
            "editor" => Some(Role::Editor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    pub fn can_edit(self) -> bool {
        self != Role::Viewer
    }
}

/// A text selection in a cell source: UTF-8 byte offsets. `anchor == head`
/// is a caret; `head` is where the cursor is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(at: usize) -> Selection {
        Selection { anchor: at, head: at }
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

/// My current focus, set with [`crate::Session::set_presence`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presence {
    /// The selected (or edited) cell.
    pub cell: Option<String>,
    /// Cursor/selection inside that cell's source, when editing it.
    pub selection: Option<Selection>,
}

/// Another participant, as currently known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerState {
    pub id: PeerId,
    pub name: String,
    pub color: String,
    pub role: Role,
    /// The cell they have selected, if any.
    pub cell: Option<String>,
    /// Their selection in that cell, resolved against this peer's copy of the
    /// document (so it reflects edits made since they moved).
    pub selection: Option<Selection>,
}

#[derive(Serialize, Deserialize, Default)]
struct UserJson {
    #[serde(default)]
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initials: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct HerseJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cell: Option<String>,
    // Kept as raw JSON so a malformed position from a foreign client only
    // loses the cursor, not the whole peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Default)]
struct StateJson {
    #[serde(default)]
    user: UserJson,
    #[serde(default)]
    herse: HerseJson,
}

fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().next()).take(2).flat_map(char::to_uppercase).collect()
}

/// My awareness state as JSON. The selection is turned into relative
/// positions against the current doc.
pub(crate) fn local_state<T: ReadTxn>(txn: &T, roots: &Roots, me: &Peer, role: Role, p: &Presence, hint: Option<usize>) -> String {
    let mut herse = HerseJson { role: Some(role.as_str().into()), cell: p.cell.clone(), ..Default::default() };
    if let (Some(cell), Some(sel)) = (&p.cell, p.selection)
        && let Some((_, map)) = doc::find_cell(txn, roots, cell, hint)
        && let Some(text) = doc::source_text(txn, &map)
    {
        let len = yrs::Text::len(&text, txn) as usize;
        let sticky = |at: usize| {
            let s = text.sticky_index(txn, at.min(len) as u32, Assoc::After)?;
            serde_json::to_value(s).ok()
        };
        herse.anchor = sticky(sel.anchor);
        herse.head = sticky(sel.head);
    }
    let state = StateJson {
        user: UserJson {
            name: me.name.clone(),
            display_name: Some(me.name.clone()),
            initials: Some(initials(&me.name)),
            color: Some(me.color.clone()),
        },
        herse,
    };
    serde_json::to_string(&state).unwrap_or_else(|_| "{}".into())
}

/// A remote peer's state resolved against the doc. `None` for states that
/// are not JSON objects.
pub(crate) fn resolve<T: ReadTxn>(txn: &T, roots: &Roots, id: PeerId, json: &str, hint: impl Fn(&str) -> Option<usize>) -> Option<PeerState> {
    let state: StateJson = serde_json::from_str(json).ok()?;
    let name = if state.user.name.is_empty() { state.user.display_name.clone().unwrap_or_default() } else { state.user.name };
    let color = state.user.color.filter(|c| !c.is_empty()).unwrap_or_else(|| default_color(&name).to_owned());
    let role = state.herse.role.as_deref().and_then(Role::from_str_opt).unwrap_or(Role::Editor);
    let cell = state.herse.cell;
    let selection = (|| {
        let cell = cell.as_deref()?;
        let (_, map) = doc::find_cell(txn, roots, cell, hint(cell))?;
        let text = doc::source_text(txn, &map)?;
        let branch = AsRef::<yrs::branch::Branch>::as_ref(&text).id();
        let at = |v: &serde_json::Value| -> Option<usize> {
            let s: StickyIndex = serde_json::from_value(v.clone()).ok()?;
            let off = s.get_offset(txn)?;
            (off.branch.id() == branch).then_some(off.index as usize)
        };
        let anchor = at(state.herse.anchor.as_ref()?)?;
        let head = at(state.herse.head.as_ref()?)?;
        Some(Selection { anchor, head })
    })();
    Some(PeerState { id, name, color, role, cell, selection })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_stable_and_valid() {
        assert_eq!(default_color("Ada"), default_color("Ada"));
        assert!(default_color("").starts_with('#'));
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("ümit"), "Ü");
    }
}
