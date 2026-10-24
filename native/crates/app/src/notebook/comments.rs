//! Comments: threads about cells (or a quoted span of one), kept in the
//! notebook file under `metadata.herse.comments`, so they travel with it.
//!
//! ⌥⌘M comments on the selected cell, or on the selected text while editing.
//! Threads live in a panel on the page's right; a cell with open threads
//! shows their count in its margin.

use std::collections::HashMap;

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement, KeyBinding,
    MouseButton, ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, actions, div, prelude::FluentBuilder as _, px,
};
use herse_collab::comments::{self, Anchor, Thread};

use super::NotebookView;
use crate::actions::{AddComment, ToggleComments};

actions!(comments, [CancelComment]);

pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([KeyBinding::new("escape", CancelComment, Some("CommentComposer > Input"))]);
}

/// A comment being written: where it goes and its field.
struct Composer {
    cell_id: String,
    anchor: Option<Anchor>,
    input: Entity<InputState>,
    _subscription: Subscription,
}

#[derive(Default)]
pub(super) struct Comments {
    pub(super) threads: Vec<Thread>,
    pub(super) open: bool,
    show_resolved: bool,
    composer: Option<Composer>,
    /// Each open thread's reply field, made the first time it is drawn.
    replies: HashMap<String, (Entity<InputState>, Subscription)>,
    /// The thread a badge click asked to see.
    focused: Option<String>,
    scroll: ScrollHandle,
}

impl Comments {
    pub(super) fn load(nb: &herse_nb::Notebook) -> Self {
        Self { threads: comments::load(nb), ..Default::default() }
    }

    /// Open threads about the cell with this id.
    fn open_count(&self, cell_id: &str) -> usize {
        self.threads.iter().filter(|t| !t.resolved && t.cell_id == cell_id).count()
    }

    pub(super) fn open_total(&self) -> usize {
        self.threads.iter().filter(|t| !t.resolved).count()
    }
}

/// The name comments are signed with: the Mac account's full name.
pub(crate) fn my_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        std::process::Command::new("/usr/bin/id")
            .arg("-F")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "Me".into())
    })
}

/// "just now", "5 min ago", "3 h ago", "2 d ago": when, roughly.
fn ago(ms: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let secs = ((now - ms) / 1000).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

impl NotebookView {
    /// The cell's id, giving the notebook ids first if it is older than 4.5
    /// (as Jupyter does when it upgrades one).
    fn ensure_cell_id(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<String> {
        if self.doc.nb.cells.get(ix)?.id.is_none() {
            self.doc.nb.upgrade_to_4_5();
            self.mark_dirty(cx);
        }
        self.doc.nb.cells.get(ix)?.id.clone()
    }

    /// Threads from a live session: shown, and kept in the file's metadata
    /// (the session already has them).
    pub(super) fn set_threads(&mut self, threads: Vec<Thread>, cx: &mut Context<Self>) {
        self.comments.threads = threads;
        comments::store(&mut self.doc.nb, &self.comments.threads);
        self.quiet_dirty(cx);
        cx.emit(super::NotebookEvent::Changed);
        cx.notify();
    }

    /// Before a change to the threads: one undo step brings them back.
    fn remember_comments(&mut self) {
        self.push_step(super::Step::Comments(self.comments.threads.clone()));
    }

    /// Write the threads back into the notebook's metadata (saved with it).
    pub(super) fn store_comments(&mut self, cx: &mut Context<Self>) {
        comments::store(&mut self.doc.nb, &self.comments.threads);
        self.mark_dirty(cx);
        cx.emit(super::NotebookEvent::Changed);
        cx.notify();
    }

    /// ⌥⌘M: a comment on the selected cell, quoting the selected text when
    /// the editor has some.
    pub(super) fn on_add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected;
        let anchor = self.editing.as_ref().and_then(|editing| {
            let editor = editing.editor.read(cx);
            let range = editor.selected_range();
            let text = editor.value();
            (!range.is_empty() && range.end <= text.len()).then(|| Anchor {
                start: range.start,
                end: range.end,
                quote: text[range.clone()].to_string(),
            })
        });
        let Some(cell_id) = self.ensure_cell_id(ix, cx) else { return };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Comment…"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.post_comment(window, cx);
            }
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.comments.composer = Some(Composer { cell_id, anchor, input, _subscription: subscription });
        self.comments.open = true;
        self.comments.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn post_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self.comments.composer.take() else { return };
        let body = composer.input.read(cx).value().trim().to_string();
        if !body.is_empty()
            && let Some(session) = self.live_session()
        {
            // Live: through the session, so everyone's thread lists merge.
            if let Ok(id) = session.add_comment(&composer.cell_id, composer.anchor.clone(), &body) {
                self.comments.focused = Some(id);
            }
            self.set_threads(session.comments(), cx);
        } else if !body.is_empty() {
            let thread = Thread::new(&composer.cell_id, composer.anchor.clone(), my_name(), &body);
            self.remember_comments();
            self.comments.focused = Some(thread.id.clone());
            self.comments.threads.push(thread);
            self.store_comments(cx);
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn on_cancel_comment(&mut self, _: &CancelComment, window: &mut Window, cx: &mut Context<Self>) {
        self.comments.composer = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn on_toggle_comments(&mut self, _: &ToggleComments, _: &mut Window, cx: &mut Context<Self>) {
        self.comments.open = !self.comments.open;
        cx.notify();
    }

    fn reply(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.comments.replies.get(thread_id) else { return };
        let input = input.clone();
        let body = input.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        input.update(cx, |input, cx| input.set_value("", window, cx));
        if let Some(session) = self.live_session() {
            let _ = session.reply(thread_id, &body);
            self.set_threads(session.comments(), cx);
            return;
        }
        self.remember_comments();
        if let Some(thread) = self.comments.threads.iter_mut().find(|t| t.id == thread_id) {
            thread.reply(my_name(), &body);
        }
        self.store_comments(cx);
    }

    fn set_resolved(&mut self, thread_id: &str, resolved: bool, cx: &mut Context<Self>) {
        if let Some(session) = self.live_session() {
            let _ = session.resolve(thread_id, resolved);
            self.set_threads(session.comments(), cx);
            return;
        }
        self.remember_comments();
        if let Some(thread) = self.comments.threads.iter_mut().find(|t| t.id == thread_id) {
            thread.set_resolved(resolved, my_name());
        }
        self.store_comments(cx);
    }

    /// Delete one of my messages; a thread left with none goes too.
    fn delete_message(&mut self, thread_id: &str, message_id: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.live_session() {
            let _ = session.delete_comment(thread_id, message_id);
            self.set_threads(session.comments(), cx);
            return;
        }
        self.remember_comments();
        if let Some(thread) = self.comments.threads.iter_mut().find(|t| t.id == thread_id) {
            thread.delete(message_id);
        }
        self.comments.threads.retain(|t| !t.messages.is_empty());
        self.store_comments(cx);
    }

    /// The margin's count of open threads about cell `ix`, if any.
    pub(super) fn comment_badge(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let cell_id = self.doc.nb.cells.get(ix)?.id.clone()?;
        let count = self.comments.open_count(&cell_id);
        if count == 0 {
            return None;
        }
        let accent = crate::theme::accent(cx);
        let uid = self.cells.get(ix)?.uid;
        Some(
            div()
                .id(("comments", uid))
                .mt(px(4.))
                .h(px(18.))
                .px(px(5.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.))
                .rounded(px(5.))
                .cursor_pointer()
                .text_color(accent)
                .hover(|s| s.bg(accent.opacity(0.1)))
                .child(gpui_kit::svg().path("icons/message-square.svg").size(px(11.)).text_color(accent))
                .child(div().text_size(px(10.5)).child(count.to_string()))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.comments.open = true;
                    this.comments.focused =
                        this.comments.threads.iter().find(|t| !t.resolved && t.cell_id == cell_id).map(|t| t.id.clone());
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// A short name for the cell a thread is about: its heading, else its
    /// first line.
    fn cell_label(&self, cell_id: &str) -> Option<(usize, SharedString)> {
        let ix = self.doc.nb.cells.iter().position(|c| c.id.as_deref() == Some(cell_id))?;
        let first = self.doc.nb.cells[ix].source.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let first = first.trim_start_matches('#').trim();
        let short: String = first.chars().take(48).collect();
        Some((ix, format!("Cell {} · {}", ix + 1, if short.is_empty() { "empty" } else { &short }).into()))
    }

    /// The comments panel, on the page's right, when open.
    pub(super) fn render_comments_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.comments.open {
            return None;
        }
        // Reply fields for the threads on show.
        let show_resolved = self.comments.show_resolved;
        let shown: Vec<String> =
            self.comments.threads.iter().filter(|t| show_resolved || !t.resolved).map(|t| t.id.clone()).collect();
        for id in &shown {
            if !self.comments.replies.contains_key(id) {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Reply…"));
                let thread_id = id.clone();
                let sub = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.reply(&thread_id, window, cx);
                    }
                });
                self.comments.replies.insert(id.clone(), (input, sub));
            }
        }
        let theme = cx.theme().clone();
        let accent = crate::theme::accent(cx);
        // Threads in notebook order; those whose cell is gone, last.
        let mut threads: Vec<&Thread> = self.comments.threads.iter().filter(|t| shown.contains(&t.id)).collect();
        let order = |t: &Thread| {
            let ix = self.doc.nb.cells.iter().position(|c| c.id.as_deref() == Some(t.cell_id.as_str()));
            (ix.unwrap_or(usize::MAX), t.created_at())
        };
        threads.sort_by_key(|t| order(t));
        let me = my_name();
        let field = |input: &Entity<InputState>| {
            div()
                .h(px(28.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(crate::ui::CONTROL_RADIUS)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_size(px(12.5))
                .child(Input::new(input).appearance(false))
        };
        let quote = |text: &str| {
            div()
                .pl(px(8.))
                .border_l_2()
                .border_color(accent.opacity(0.6))
                .font_family(crate::theme::mono_font())
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .truncate()
                .child(text.lines().next().unwrap_or("").to_string())
        };
        let composer = self.comments.composer.as_ref().map(|c| {
            let label = self.cell_label(&c.cell_id).map(|(_, l)| l).unwrap_or_else(|| "This cell".into());
            div()
                .key_context("CommentComposer")
                .p(px(12.))
                .rounded(px(8.))
                .border_1()
                .border_color(accent.opacity(0.5))
                .bg(theme.background)
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().text_size(px(11.5)).text_color(theme.muted_foreground).child(label))
                .when_some(c.anchor.as_ref(), |d, a| d.child(quote(&a.quote)))
                .child(field(&c.input))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(6.))
                        .child(
                            crate::ui::ghost_button("comment-cancel", "Cancel", cx)
                                .on_click(cx.listener(|this, _, window, cx| this.on_cancel_comment(&CancelComment, window, cx))),
                        )
                        .child(
                            crate::ui::primary_button("comment-post", "Comment", true, cx)
                                .on_click(cx.listener(|this, _, window, cx| this.post_comment(window, cx))),
                        ),
                )
        });
        let cards: Vec<AnyElement> = threads
            .iter()
            .map(|t| {
                let label = self.cell_label(&t.cell_id);
                let jump = label.as_ref().map(|(ix, _)| *ix);
                let label = label.map(|(_, l)| l).unwrap_or_else(|| "A removed cell".into());
                let focused = self.comments.focused.as_deref() == Some(t.id.as_str());
                let thread_id = t.id.clone();
                let (resolve_id, reply) = (t.id.clone(), self.comments.replies.get(&t.id).map(|(i, _)| i.clone()));
                div()
                    .id(SharedString::from(format!("thread-{}", t.id)))
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if focused { accent.opacity(0.6) } else { theme.border })
                    .bg(theme.background)
                    .when(t.resolved, |d| d.opacity(0.6))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .id(SharedString::from(format!("thread-cell-{}", t.id)))
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .when(jump.is_some(), |d| d.cursor_pointer().hover(|s| s.text_color(theme.foreground)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(ix) = jump {
                                    this.jump_to_cell(ix, window, cx);
                                }
                            })),
                    )
                    .when_some(t.anchor.as_ref(), |d, a| d.child(quote(&a.quote)))
                    .children(t.messages.iter().map(|m| {
                        let (tid, mid) = (thread_id.clone(), m.id.clone());
                        let mine = m.author == me;
                        div()
                            .group("message")
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(px(12.))
                                    .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(m.author.clone()))
                                    .child(div().text_color(theme.muted_foreground).child(ago(m.created_at)))
                                    .child(div().flex_1())
                                    .when(mine, |d| {
                                        d.child(
                                            div()
                                                .id(SharedString::from(format!("del-{}", m.id)))
                                                .opacity(0.)
                                                .group_hover("message", |s| s.opacity(1.))
                                                .text_size(px(11.))
                                                .text_color(theme.muted_foreground)
                                                .cursor_pointer()
                                                .hover(|s| s.text_color(theme.danger))
                                                .child("Delete")
                                                .on_click(cx.listener(move |this, _, _, cx| this.delete_message(&tid, &mid, cx))),
                                        )
                                    }),
                            )
                            .child(div().text_size(px(12.5)).child(m.body.clone()))
                    }))
                    .when_some(reply.filter(|_| !t.resolved), |d, input| d.child(field(&input)))
                    .child(
                        div().flex().flex_row().justify_end().child(
                            crate::ui::ghost_button(
                                SharedString::from(format!("resolve-{}", t.id)),
                                if t.resolved { "Reopen" } else { "Resolve" },
                                cx,
                            )
                            .on_click({
                                let resolved = t.resolved;
                                cx.listener(move |this, _, _, cx| this.set_resolved(&resolve_id, !resolved, cx))
                            }),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = cards.is_empty() && composer.is_none();
        let open_total = self.comments.open_total();
        Some(
            div()
                .w(px(320.))
                .flex_none()
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.background.blend(theme.foreground.opacity(0.02)))
                .font_family(crate::theme::UI_FONT)
                .text_size(px(13.))
                .on_action(cx.listener(Self::on_cancel_comment))
                .child(
                    div()
                        .h(px(44.))
                        .px(px(14.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Comments"))
                        .when(open_total > 0, |d| d.child(div().text_color(theme.muted_foreground).child(open_total.to_string())))
                        .child(div().flex_1())
                        .child(crate::ui::check("comments-resolved", show_resolved, "Resolved", cx).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.comments.show_resolved = !this.comments.show_resolved;
                                cx.notify();
                            },
                        )))
                        .child(
                            crate::ui::icon_button("comments-close", "icons/close.svg", false, cx)
                                .on_click(cx.listener(|this, _, window, cx| this.on_toggle_comments(&ToggleComments, window, cx))),
                        ),
                )
                .child(
                    div()
                        .id("comments-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.comments.scroll)
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .children(composer)
                        .children(cards)
                        .when(empty, |d| {
                            d.child(
                                div()
                                    .pt(px(24.))
                                    .px(px(8.))
                                    .text_size(px(12.5))
                                    .text_color(theme.muted_foreground)
                                    .child("No comments yet. Select a cell, or some text in it, and press ⌥⌘M to start a thread."),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}
