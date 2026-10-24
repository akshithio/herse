//! What changed since the last commit. A notebook in a git repository is
//! compared, cell by cell, with its copy at HEAD: the margin marks the cells
//! added and changed since, and where cells were removed, and a changed
//! cell can show its lines as they were. Cells pair up by their nbformat ids
//! when both copies have them, so an edited cell stays the same cell; by
//! their content otherwise.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use herse_nb::{Cell, CellType};
use similar::{Algorithm, ChangeTag, DiffOp, TextDiff, capture_diff_slices_deadline};

/// A cell as it was at HEAD.
pub struct OldCell {
    pub id: Option<String>,
    pub kind: CellType,
    pub source: String,
}

/// The notebook at HEAD.
pub struct Baseline {
    pub cells: Vec<OldCell>,
    /// HEAD's short hash, for "since a1b2c3d".
    pub commit: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    Same,
    Added,
    /// Changed from the old cell at this index.
    Changed(usize),
}

/// The comparison: a mark for every cell, and the old cells removed above
/// each place (`removed` holds `(at, old indices)`, sorted by `at`; `at` is
/// the index of the cell they were above, or the cell count for the end).
#[derive(Default)]
pub struct Changes {
    pub marks: Vec<Mark>,
    pub removed: Vec<(usize, Vec<usize>)>,
}

impl Changes {
    /// The old cells removed just above cell `ix`.
    pub fn removed_at(&self, ix: usize) -> Option<&[usize]> {
        self.removed.iter().find(|(at, _)| *at == ix).map(|(_, old)| old.as_slice())
    }

    /// Where the changes are, in order: each added or changed cell, and each
    /// place cells were removed (a removal at the end counts as the last
    /// cell's), one stop per cell.
    pub fn stops(&self) -> Vec<usize> {
        let last = self.marks.len().saturating_sub(1);
        let mut stops: Vec<usize> = self
            .marks
            .iter()
            .enumerate()
            .filter(|(_, m)| **m != Mark::Same)
            .map(|(ix, _)| ix)
            .chain(self.removed.iter().map(|(at, _)| (*at).min(last)))
            .collect();
        stops.sort_unstable();
        stops.dedup();
        stops
    }

    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.marks.iter().all(|m| *m == Mark::Same)
    }
}

/// The notebook at `path` as of HEAD, if it is in a git repository and
/// committed. Blocking (it runs git): call it off the UI thread.
pub fn load(path: &Path) -> Option<Baseline> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_str()?;
    // Only where there is a repository: running git elsewhere costs a
    // process, and on a Mac without developer tools it offers to install them.
    if !dir.ancestors().any(|d| d.join(".git").exists()) {
        return None;
    }
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    };
    let commit = String::from_utf8(git(&["rev-parse", "--short", "HEAD"])?).ok()?.trim().to_string();
    // `./` makes the path relative to `dir`, not the repository's root.
    let bytes = git(&["show", &format!("HEAD:./{name}")])?;
    let nb = herse_nb::parse(&bytes).ok()?;
    let cells = nb.cells.into_iter().map(|c| OldCell { id: c.id, kind: c.cell_type, source: c.source }).collect();
    Some(Baseline { cells, commit })
}

/// Compare `cells` with the baseline.
pub fn compare(base: &Baseline, cells: &[Cell]) -> Changes {
    let old = &base.cells;
    let mut marks = vec![Mark::Same; cells.len()];
    let mut removed: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut remove = |at: usize, range: std::ops::Range<usize>| match removed.last_mut() {
        Some((last, ixs)) if *last == at => ixs.extend(range),
        _ => removed.push((at, range.collect())),
    };
    let by_id = !old.is_empty() && old.iter().all(|c| c.id.is_some()) && cells.iter().all(|c| c.id.is_some());
    // A notebook of thousands of cells still answers in a frame.
    let deadline = Some(Instant::now() + Duration::from_millis(30));
    let ops = if by_id {
        let a: Vec<&str> = old.iter().map(|c| c.id.as_deref().unwrap_or("")).collect();
        let b: Vec<&str> = cells.iter().map(|c| c.id.as_deref().unwrap_or("")).collect();
        capture_diff_slices_deadline(Algorithm::Myers, &a, &b, deadline)
    } else {
        let a: Vec<(u8, &str)> = old.iter().map(|c| (c.kind as u8, c.source.as_str())).collect();
        let b: Vec<(u8, &str)> = cells.iter().map(|c| (c.cell_type as u8, c.source.as_str())).collect();
        capture_diff_slices_deadline(Algorithm::Myers, &a, &b, deadline)
    };
    for op in ops {
        match op {
            DiffOp::Equal { old_index, new_index, len } => {
                for k in 0..len {
                    let (o, n) = (&old[old_index + k], &cells[new_index + k]);
                    if o.source != n.source || o.kind != n.cell_type {
                        marks[new_index + k] = Mark::Changed(old_index + k);
                    }
                }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                marks[new_index..new_index + new_len].fill(Mark::Added);
            }
            DiffOp::Delete { old_index, old_len, new_index } => remove(new_index, old_index..old_index + old_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                // By content, a replaced run is cells edited in place: pair
                // them up in order. By id, they are different cells.
                let paired = if by_id { 0 } else { old_len.min(new_len) };
                for k in 0..paired {
                    marks[new_index + k] = Mark::Changed(old_index + k);
                }
                if old_len > paired {
                    remove(new_index + paired, old_index + paired..old_index + old_len);
                }
                marks[new_index + paired..new_index + new_len].fill(Mark::Added);
            }
        }
    }
    Changes { marks, removed }
}

/// One line of a changed cell's comparison.
pub struct DiffLine {
    pub tag: ChangeTag,
    pub text: String,
}

/// A changed cell's lines against the old ones: the changed lines with two
/// lines of context around each run, `None` between runs that are far apart.
pub fn line_diff(old: &str, new: &str) -> Vec<Option<DiffLine>> {
    let diff = TextDiff::configure().algorithm(Algorithm::Patience).diff_lines(old, new);
    let mut out = Vec::new();
    for (i, group) in diff.grouped_ops(2).iter().enumerate() {
        if i > 0 {
            out.push(None);
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let text = change.value().trim_end_matches(['\n', '\r']).to_string();
                out.push(Some(DiffLine { tag: change.tag(), text }));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ the view

use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};

use super::{NotebookEvent, NotebookView};
use crate::{actions::*, theme};

impl NotebookView {
    /// Read the notebook as of the last commit, in the background: on open,
    /// and after a save or reload (a commit may have happened in between).
    pub(super) fn refresh_baseline(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.doc.path.clone() else { return };
        self.baseline_task = Some(cx.spawn(async move |this, cx| {
            let base = cx.background_spawn(async move { load(&path) }).await;
            this.update(cx, |this, cx| {
                this.baseline = base.map(Rc::new);
                this.changes_rev = None;
                cx.emit(NotebookEvent::Changed);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Bring the comparison up to date with the latest edits: once per
    /// change, at the next frame.
    pub(super) fn update_changes(&mut self) {
        let fresh = self.changes_rev == Some(self.doc_rev)
            && (self.changes.marks.is_empty() || self.changes.marks.len() == self.doc.nb.cells.len());
        if fresh {
            return;
        }
        self.changes_rev = Some(self.doc_rev);
        self.changes = match &self.baseline {
            Some(base) if self.show_changes => compare(base, &self.doc.nb.cells),
            _ => Changes::default(),
        };
    }

    /// For the status bar: how many cells changed since which commit.
    pub fn change_summary(&self) -> Option<(usize, SharedString)> {
        let base = self.baseline.as_ref().filter(|_| self.show_changes)?;
        let n = self.changes.marks.iter().filter(|m| **m != Mark::Same).count()
            + self.changes.removed.iter().map(|(_, old)| old.len()).sum::<usize>();
        Some((n, base.commit.clone().into()))
    }

    pub fn step_change(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.update_changes();
        let stops = self.changes.stops();
        let found = if forward {
            stops.iter().copied().find(|&i| i > self.selected)
        } else {
            stops.iter().rev().copied().find(|&i| i < self.selected)
        };
        match found {
            Some(ix) => {
                self.jump_to_cell(ix, window, cx);
                let at = stops.iter().position(|&s| s == ix).map_or(0, |p| p + 1);
                self.flash(format!("Change {at} of {}", stops.len()), cx);
            }
            None if self.baseline.is_none() => {
                self.flash("This notebook isn't in a git repository, or isn't committed", cx)
            }
            None if stops.is_empty() => self.flash("No changes since the last commit", cx),
            None => self.flash(
                format!("No more changes {} ({} in total)", if forward { "below" } else { "above" }, stops.len()),
                cx,
            ),
        }
    }

    pub(super) fn on_next_change(&mut self, _: &NextChange, window: &mut Window, cx: &mut Context<Self>) {
        self.step_change(true, window, cx);
    }

    pub(super) fn on_prev_change(&mut self, _: &PrevChange, window: &mut Window, cx: &mut Context<Self>) {
        self.step_change(false, window, cx);
    }

    pub(super) fn on_toggle_changes(&mut self, _: &ToggleChanges, _: &mut Window, cx: &mut Context<Self>) {
        self.show_changes = !self.show_changes;
        crate::settings::set_show_changes(self.show_changes);
        self.changes_rev = None;
        self.list.remeasure();
        self.flash(if self.show_changes { "Showing changes since the last commit" } else { "Changes hidden" }, cx);
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    /// The margin's mark for a cell added or changed since the last commit.
    /// A changed cell's mark shows how it was, on a click.
    pub(super) fn change_marker(&self, ix: usize, uid: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mark = *self.changes.marks.get(ix)?;
        let commit = self.baseline.as_ref()?.commit.clone();
        let dark = cx.theme().is_dark();
        let (color, tip): (_, SharedString) = match mark {
            Mark::Same => return None,
            Mark::Added => (theme::added_color(dark), format!("Added since {commit}").into()),
            Mark::Changed(_) => (theme::changed_color(dark), format!("Changed since {commit}: click to compare").into()),
        };
        let open = self.diff_open.contains(&uid);
        Some(
            // A wider target than the 3 px it shows.
            div()
                .id(("change", uid))
                .absolute()
                .top(px(1.))
                .bottom(px(1.))
                .left(px(0.))
                .w(px(9.))
                .flex()
                .flex_row()
                .when(matches!(mark, Mark::Changed(_)), |d| d.cursor_pointer())
                .child(div().h_full().w(px(3.)).rounded(px(1.5)).bg(color).when(open, |d| d.w(px(4.))))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !matches!(this.changes.marks.get(ix), Some(Mark::Changed(_))) {
                        return;
                    }
                    if !this.diff_open.remove(&uid) {
                        this.diff_open.insert(uid);
                    }
                    this.list.splice(ix..ix + 1, 1);
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// A changed cell's lines against the last commit's, under its source,
    /// while its margin mark is open.
    pub(super) fn cell_diff(&self, ix: usize, uid: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.diff_open.contains(&uid) {
            return None;
        }
        let Mark::Changed(old_ix) = *self.changes.marks.get(ix)? else { return None };
        let base = self.baseline.as_ref()?;
        let old = &base.cells.get(old_ix)?.source;
        let lines = line_diff(old, &self.doc.nb.cells[ix].source);
        let theme = cx.theme();
        let dark = theme.is_dark();
        let (added, removed) = (theme::added_color(dark), theme::error_color(dark));
        let muted = theme.muted_foreground;
        let commit = base.commit.clone();
        Some(
            div()
                .mt(px(6.))
                .flex()
                .flex_col()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(px(10.))
                        .h(px(26.))
                        .bg(theme.foreground.opacity(0.03))
                        .font_family(theme::UI_FONT)
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(format!("Changes since {commit}"))
                        .child(crate::ui::ghost_button(("diff-hide", uid), "Hide", cx).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.diff_open.remove(&uid);
                                this.list.splice(ix..ix + 1, 1);
                                cx.notify();
                            },
                        ))),
                )
                .child(
                    div()
                        .py(px(4.))
                        .font_family(theme::mono_font())
                        .text_size(px(12.))
                        .line_height(theme::mono_line_height())
                        .children(lines.into_iter().map(move |line| {
                            let row = div().px(px(10.)).flex().flex_row().gap(px(8.)).whitespace_nowrap().overflow_hidden();
                            match line {
                                None => row.text_color(muted).child("⋯"),
                                Some(line) => {
                                    let (sign, bg, fg) = match line.tag {
                                        ChangeTag::Delete => ("−", removed.opacity(0.10), removed),
                                        ChangeTag::Insert => ("+", added.opacity(0.12), added),
                                        ChangeTag::Equal => (" ", gpui_kit::transparent_black(), muted),
                                    };
                                    let text = if line.text.is_empty() { " ".to_string() } else { line.text };
                                    row.bg(bg).child(div().w(px(10.)).flex_none().text_color(fg).child(sign)).child(text)
                                }
                            }
                        })),
                )
                .into_any_element(),
        )
    }

    /// Above cell `ix` (or below the last, at the cell count): the cells
    /// removed there since the last commit, as a quiet note that opens to
    /// show them.
    pub(super) fn removed_note(&self, at: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let old = self.changes.removed_at(at)?;
        let base = self.baseline.as_ref()?;
        let theme = cx.theme();
        let color = theme::error_color(theme.is_dark());
        let open = self.removed_open.contains(&at);
        let label = match old.len() {
            1 => "1 cell removed".to_string(),
            n => format!("{n} cells removed"),
        };
        let sources: Vec<String> = if open {
            old.iter().filter_map(|&i| base.cells.get(i)).map(|c| c.source.clone()).collect()
        } else {
            Vec::new()
        };
        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id(("removed", at))
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(18.))
                        .cursor_pointer()
                        .child(div().w(theme::GUTTER_WIDTH).flex_none().child(div().w(px(9.)).h(px(2.)).rounded(px(1.)).bg(color)))
                        .child(
                            div()
                                .px(theme::CELL_PAD_X)
                                .font_family(theme::UI_FONT)
                                .text_size(px(11.))
                                .text_color(color.opacity(0.85))
                                .child(format!("{label} since {}{}", base.commit, if open { "" } else { " ›" })),
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.removed_open.remove(&at) {
                                this.removed_open.insert(at);
                            }
                            let n = this.doc.nb.cells.len();
                            let ix = at.min(n.saturating_sub(1));
                            this.list.splice(ix..ix + 1, 1);
                            cx.notify();
                        })),
                )
                .children(sources.into_iter().map(|source| {
                    div()
                        .ml(theme::GUTTER_WIDTH)
                        .mb(px(4.))
                        .px(theme::CELL_PAD_X)
                        .py(px(6.))
                        .rounded(px(6.))
                        .bg(color.opacity(0.06))
                        .font_family(theme::mono_font())
                        .text_size(px(12.))
                        .line_height(theme::mono_line_height())
                        .text_color(theme.muted_foreground)
                        .child(if source.is_empty() { " ".to_string() } else { source })
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(id: Option<&str>, kind: CellType, source: &str) -> Cell {
        let mut c = Cell::new(kind);
        c.id = id.map(str::to_string);
        c.source = source.to_string();
        c
    }

    fn base(cells: &[(Option<&str>, &str)]) -> Baseline {
        Baseline {
            cells: cells
                .iter()
                .map(|(id, s)| OldCell { id: id.map(str::to_string), kind: CellType::Code, source: s.to_string() })
                .collect(),
            commit: "abc1234".into(),
        }
    }

    #[test]
    fn ids_keep_an_edited_cell_the_same_cell() {
        let b = base(&[(Some("a"), "x = 1"), (Some("b"), "y = 2"), (Some("c"), "z = 3")]);
        let now = [
            cell(Some("a"), CellType::Code, "x = 1"),
            cell(Some("n"), CellType::Code, "new"),
            cell(Some("b"), CellType::Code, "y = 20"),
        ];
        let ch = compare(&b, &now);
        assert_eq!(ch.marks, vec![Mark::Same, Mark::Added, Mark::Changed(1)]);
        assert_eq!(ch.removed, vec![(3, vec![2])]);
        assert_eq!(ch.stops(), vec![1, 2]);
    }

    #[test]
    fn without_ids_content_pairs_cells() {
        let b = base(&[(None, "import os"), (None, "print(1)"), (None, "gone"), (None, "end")]);
        let now = [
            cell(None, CellType::Code, "import os"),
            cell(None, CellType::Code, "print(2)"),
            cell(None, CellType::Code, "end"),
        ];
        let ch = compare(&b, &now);
        assert_eq!(ch.marks, vec![Mark::Same, Mark::Changed(1), Mark::Same]);
        assert_eq!(ch.removed, vec![(2, vec![2])]);
        assert!(!ch.is_empty());
        let same = compare(&base(&[(None, "a")]), &[cell(None, CellType::Code, "a")]);
        assert!(same.is_empty());
    }

    #[test]
    fn a_changed_cell_shows_its_lines_in_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nC\nd\ne\nf\ng\nh\ni\n";
        let lines = line_diff(old, new);
        let render: Vec<String> = lines
            .iter()
            .map(|l| match l {
                None => "…".into(),
                Some(l) => format!("{}{}", match l.tag { ChangeTag::Delete => "-", ChangeTag::Insert => "+", ChangeTag::Equal => " " }, l.text),
            })
            .collect();
        assert_eq!(render, vec![" a", " b", "-c", "+C", " d", " e", "…", " g", " h", "+i"]);
    }
}
