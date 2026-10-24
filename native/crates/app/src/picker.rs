//! A keyboard-first overlay list with a query field: Go to File, the command
//! palette, the kernel picker and the outline all use it. Matching is
//! synchronous on every keystroke, so delegates must stay well under a frame.

use std::ops::Range;

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, StyledText,
    Subscription, UniformListScrollHandle, Window, actions, div, prelude::FluentBuilder as _, px,
    uniform_list,
};

use crate::theme;

actions!(picker, [PickerUp, PickerDown, PickerConfirm, PickerCancel]);

pub const PICKER: &str = "Picker";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Picker > Input");
    cx.bind_keys([
        gpui_kit::KeyBinding::new("up", PickerUp, ctx),
        gpui_kit::KeyBinding::new("ctrl-p", PickerUp, ctx),
        gpui_kit::KeyBinding::new("down", PickerDown, ctx),
        gpui_kit::KeyBinding::new("ctrl-n", PickerDown, ctx),
        gpui_kit::KeyBinding::new("enter", PickerConfirm, ctx),
        gpui_kit::KeyBinding::new("escape", PickerCancel, ctx),
    ]);
}

pub struct Match {
    pub label: SharedString,
    /// Byte ranges of `label` to emphasize (fuzzy-match positions).
    pub highlights: Vec<Range<usize>>,
    pub detail: Option<SharedString>,
    /// Right-aligned hint, e.g. a key binding.
    pub hint: Option<SharedString>,
    /// A leading icon telling kinds apart in a mixed list (⌘K).
    pub icon: Option<&'static str>,
}

pub trait PickerDelegate: Sized + 'static {
    fn placeholder(&self) -> SharedString;
    /// Recompute matches. Slow delegates (workspace search) spawn a task on
    /// `cx` and fill `matches` in as results stream back.
    fn update_matches(&mut self, query: &str, cx: &mut Context<Picker<Self>>);
    fn matches(&self) -> &[Match];
    /// Handle the chosen match. Return `true` to close the picker.
    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool;
    fn empty_text(&self) -> SharedString {
        "No matches".into()
    }
    /// A line under the results, e.g. timing and counts.
    fn status(&self) -> Option<SharedString> {
        None
    }
    /// What Return does, for the hint row ("to open", "to run").
    fn confirm_verb(&self) -> &'static str {
        "to open"
    }
}

pub struct Picker<D: PickerDelegate> {
    pub delegate: D,
    query: Entity<InputState>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl<D: PickerDelegate> EventEmitter<DismissEvent> for Picker<D> {}

impl<D: PickerDelegate> Picker<D> {
    pub fn new(mut delegate: D, initial_query: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = delegate.placeholder();
        let initial = initial_query.unwrap_or("").to_string();
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(initial.clone()));
        let subscription = cx.subscribe_in(&query, window, |this, state, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                let q = state.read(cx).value().to_string();
                this.delegate.update_matches(&q, cx);
                this.selected = 0;
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
        });
        delegate.update_matches(&initial, cx);
        let handle = query.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        Self {
            delegate,
            query,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.delegate.matches().len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn on_up(&mut self, _: &PickerUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, cx);
    }

    fn on_down(&mut self, _: &PickerDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, cx);
    }

    fn on_confirm(&mut self, _: &PickerConfirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, window, cx);
    }

    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.delegate.matches().len() {
            return;
        }
        if self.delegate.confirm(ix, window, cx) {
            cx.emit(DismissEvent);
        }
    }

    fn on_cancel(&mut self, _: &PickerCancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let selected = self.selected;
        range
            .filter_map(|ix| self.delegate.matches().get(ix).map(|m| (ix, m)))
            .map(|(ix, m)| {
                let emphasis = HighlightStyle {
                    font_weight: Some(gpui_kit::FontWeight::BOLD),
                    color: Some(theme.foreground),
                    ..Default::default()
                };
                let label = StyledText::new(m.label.clone())
                    .with_highlights(m.highlights.iter().map(|r| (r.clone(), emphasis)));
                div()
                    .id(("pick", ix))
                    .w_full()
                    .h(px(30.))
                    .px(px(6.))
                    .child(
                div()
                    // An id: GPUI redraws on a hover change only for elements with one.
                    .id(("pick-bg", ix))
                    .size_full()
                    .px(px(10.))
                    .rounded(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .when(ix == selected, |d| d.bg(theme.foreground.opacity(0.07)))
                    .when(ix != selected, |d| d.hover(|s| s.bg(theme.foreground.opacity(0.04))))
                    .when_some(m.icon, |d, icon| {
                        d.child(gpui_kit::svg().path(icon).size(px(14.)).flex_none().text_color(theme.muted_foreground))
                    })
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(400.))
                            .truncate()
                            .text_color(if ix == selected { theme.foreground } else { theme.foreground.opacity(0.85) })
                            .child(label),
                    )
                    .when_some(m.detail.clone(), |d, detail| {
                        d.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                    })
                    .when(m.detail.is_none(), |d| d.child(div().flex_1()))
                    .when_some(m.hint.clone(), |d, hint| d.child(keycaps(&hint, cx)))
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm(ix, window, cx)))
                    .into_any_element()
            })
            .collect()
    }
}

fn hint(keys: &'static str, what: &'static str, cx: &App) -> gpui_kit::Div {
    div().flex().flex_row().items_center().gap(px(5.)).child(keycaps(keys, cx)).child(what)
}

/// Keystrokes as chips: "⌘K ⌘T" is two.
pub fn keycaps(keys: &str, cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    let mut row = div().flex_none().flex().flex_row().gap(px(3.));
    for stroke in keys.split(' ').filter(|k| !k.is_empty()) {
        row = row.child(
            div()
                .h(px(18.))
                .min_w(px(18.))
                .px(px(5.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .bg(theme.foreground.opacity(0.05))
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(stroke.to_string()),
        );
    }
    row
}

impl<D: PickerDelegate> Focusable for Picker<D> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

impl<D: PickerDelegate> Render for Picker<D> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let count = self.delegate.matches().len();
        let list_height = px((count.min(14) as f32) * 30. + 8.);
        div()
            .key_context(PICKER)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_cancel))
            .w(px(600.))
            .max_w_full()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .font_family(theme::UI_FONT)
            .text_size(px(13.))
            .overflow_hidden()
            .child(
                div()
                    .h(px(46.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(15.))
                    .child(Input::new(&self.query).appearance(false)),
            )
            .when(count == 0, |d| {
                d.child(
                    div()
                        .p(px(14.))
                        .text_color(theme.muted_foreground)
                        .child(self.delegate.empty_text()),
                )
            })
            .when(count > 0, |d| {
                d.child(
                    uniform_list(
                        "picker-results",
                        count,
                        cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
                    )
                    .track_scroll(&self.scroll)
                    .w_full()
                    .py(px(4.))
                    .h(list_height),
                )
            })
            .child(
                // The keys, said once (Superhuman, Raycast), beside any status.
                div()
                    .h(px(30.))
                    .px(px(14.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(div().truncate().children(self.delegate.status()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(12.))
                            .child(hint("↑↓", "to move", cx))
                            .child(hint("↩", self.delegate.confirm_verb(), cx))
                            .child(hint("esc", "to close", cx)),
                    ),
            )
    }
}
