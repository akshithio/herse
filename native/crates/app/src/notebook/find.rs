//! Find in notebook: counts and navigates occurrences across cell sources and
//! text outputs; highlights are recomputed only for what's on screen.

use std::ops::Range;

use gpui_kit::{App, HighlightStyle, Hsla, KeyBinding, actions};
use herse_nb::{CellType, MimeData, Notebook, Output};

actions!(find, [FindNext, FindPrev, CloseFind, ToggleFindCase, ToggleReplace, ReplaceOne, ReplaceAll]);

pub const FIND_BAR: &str = "FindBar";
/// The replace row, so its input's Enter replaces instead of finding.
pub const FIND_REPLACE: &str = "FindReplace";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("FindBar > Input");
    cx.bind_keys([
        KeyBinding::new("enter", FindNext, ctx),
        KeyBinding::new("shift-enter", FindPrev, ctx),
        KeyBinding::new("escape", CloseFind, ctx),
        KeyBinding::new("alt-c", ToggleFindCase, ctx),
        KeyBinding::new("cmd-alt-f", ToggleReplace, ctx),
        KeyBinding::new("cmd-g", FindNext, Some(crate::actions::NOTEBOOK_COMMAND)),
        KeyBinding::new("cmd-shift-g", FindPrev, Some(crate::actions::NOTEBOOK_COMMAND)),
        KeyBinding::new("cmd-alt-f", ToggleReplace, Some(crate::actions::NOTEBOOK_COMMAND)),
    ]);
    let replace = Some("FindReplace > Input");
    cx.bind_keys([
        KeyBinding::new("enter", ReplaceOne, replace),
        KeyBinding::new("cmd-enter", ReplaceAll, replace),
        KeyBinding::new("escape", CloseFind, replace),
        KeyBinding::new("alt-c", ToggleFindCase, replace),
        KeyBinding::new("cmd-alt-f", ToggleReplace, replace),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Source,
    Outputs,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub cell: usize,
    pub target: Target,
    /// Byte range in the cell source (only meaningful for `Target::Source`).
    pub range: Range<usize>,
}

pub struct Needle {
    pub text: String,
    pub case_sensitive: bool,
}

impl Needle {
    /// Smart case: any uppercase in the query makes it case-sensitive.
    pub fn new(text: &str, force_case: bool) -> Self {
        Self {
            text: text.to_string(),
            case_sensitive: force_case || text.chars().any(char::is_uppercase),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// All match ranges in `haystack`.
    pub fn find_all(&self, haystack: &str) -> Vec<Range<usize>> {
        if self.text.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.case_sensitive {
            let mut start = 0;
            while let Some(pos) = haystack[start..].find(&self.text) {
                let s = start + pos;
                out.push(s..s + self.text.len());
                start = s + self.text.len().max(1);
                if start > haystack.len() {
                    break;
                }
            }
        } else if self.text.is_ascii() {
            // ASCII needle: compare bytes case-insensitively, no allocation.
            let needle = self.text.as_bytes();
            let hay = haystack.as_bytes();
            let n = needle.len();
            let first = needle[0].to_ascii_lowercase();
            let mut i = 0;
            while i + n <= hay.len() {
                if hay[i].to_ascii_lowercase() == first && hay[i..i + n].eq_ignore_ascii_case(needle) {
                    // Never split a UTF-8 sequence: ASCII matches start on char boundaries.
                    out.push(i..i + n);
                    i += n;
                } else {
                    i += 1;
                }
            }
        } else {
            let lower_needle = self.text.to_lowercase();
            let lower = haystack.to_lowercase();
            // Lowercasing can change byte lengths; only trust it when it didn't.
            if lower.len() == haystack.len() {
                let mut start = 0;
                while let Some(pos) = lower[start..].find(&lower_needle) {
                    let s = start + pos;
                    out.push(s..s + lower_needle.len());
                    start = s + lower_needle.len().max(1);
                }
            }
        }
        out
    }
}

/// Every occurrence, in document order: a cell's source hits, then a single
/// entry per output hit (outputs are matched on their stored text).
pub fn search(nb: &Notebook, needle: &Needle) -> Vec<Hit> {
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    for (ix, cell) in nb.cells.iter().enumerate() {
        for range in needle.find_all(&cell.source) {
            hits.push(Hit {
                cell: ix,
                target: Target::Source,
                range,
            });
        }
        if cell.cell_type != CellType::Code {
            continue;
        }
        for output in &cell.outputs {
            let count = match output {
                Output::Stream { text, .. } => needle.find_all(text).len(),
                Output::Error {
                    ename,
                    evalue,
                    traceback,
                } => {
                    needle.find_all(ename).len()
                        + needle.find_all(evalue).len()
                        + traceback.iter().map(|t| needle.find_all(t).len()).sum::<usize>()
                }
                Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
                    match data.get("text/plain") {
                        Some(MimeData::Text(t)) => needle.find_all(t).len(),
                        _ => 0,
                    }
                }
                Output::Unknown(_) => 0,
            };
            for _ in 0..count {
                hits.push(Hit {
                    cell: ix,
                    target: Target::Outputs,
                    range: 0..0,
                });
            }
        }
    }
    hits
}

pub fn match_style(current: bool, dark: bool) -> HighlightStyle {
    HighlightStyle {
        background_color: Some(match_background(current, dark)),
        // The current hit is solid dew, so its text turns to paper to stay legible.
        color: current.then(|| if dark { gpui_kit::rgb(0x0F1A19).into() } else { gpui_kit::rgb(0xFFFFFF).into() }),
        ..Default::default()
    }
}

/// Dew, the accent, marks where you are: a tint on every hit, solid on the
/// current one (the theme's `primary`, which this must match).
pub fn match_background(current: bool, dark: bool) -> Hsla {
    match (current, dark) {
        (true, false) => gpui_kit::rgb(0x0B7A75).into(),
        (false, false) => gpui_kit::hsla(177. / 360., 0.60, 0.45, 0.22),
        (true, true) => gpui_kit::rgb(0x5CC8BE).into(),
        (false, true) => gpui_kit::hsla(174. / 360., 0.50, 0.57, 0.28),
    }
}
