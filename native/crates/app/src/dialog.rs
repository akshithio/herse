//! Herse's own dialogs, in place of the Mac's alerts: a question or a note
//! as a sheet over the window, in the app's type and colours. The text is
//! selectable; Return takes the default choice, Escape (or a click outside)
//! cancels.
//!
//! Open one with [`crate::workspace::Workspace::ask`], which answers with
//! the index of the choice taken, as `window.prompt` does.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, actions, div,
    prelude::FluentBuilder as _, px,
};

actions!(dialog, [ConfirmDialog, CancelDialog]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ConfirmDialog, Some("Dialog")),
        KeyBinding::new("escape", CancelDialog, Some("Dialog")),
    ]);
}

/// What a choice does, for its look and its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Return takes it; drawn in dew.
    Default,
    /// Loses something (Don't Save, Move to Trash): drawn in red. Return
    /// takes it when there's no `Default`.
    Destructive,
    /// Escape takes it.
    Cancel,
    Plain,
}

/// What leads the title.
#[derive(Clone, Copy)]
pub enum Icon {
    /// Herse's own mark (About).
    Mark,
    Svg(&'static str),
}

pub struct Dialog {
    icon: Option<Icon>,
    title: SharedString,
    detail: Option<SharedString>,
    choices: Vec<(SharedString, Role)>,
    answer: Option<smol::channel::Sender<usize>>,
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for Dialog {}

impl Focusable for Dialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Dialog {
    pub fn new(
        icon: Option<Icon>,
        title: impl Into<SharedString>,
        detail: Option<SharedString>,
        choices: Vec<(SharedString, Role)>,
        answer: smol::channel::Sender<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self { icon, title: title.into(), detail, choices, answer: Some(answer), focus }
    }

    fn choose(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(answer) = self.answer.take() {
            let _ = answer.try_send(ix);
        }
        cx.emit(DismissEvent);
    }

    /// Return: the default choice, else the destructive one, else the only one.
    fn default_choice(&self) -> Option<usize> {
        let find = |role| self.choices.iter().position(|(_, r)| *r == role);
        find(Role::Default).or_else(|| find(Role::Destructive)).or_else(|| (self.choices.len() == 1).then_some(0))
    }

    fn on_confirm(&mut self, _: &ConfirmDialog, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.default_choice() {
            self.choose(ix, cx);
        }
    }

    /// Escape: Cancel, or with only an OK, OK. A question without either
    /// closes unanswered (the asker reads that as cancel).
    fn on_cancel(&mut self, _: &CancelDialog, _: &mut Window, cx: &mut Context<Self>) {
        match self.choices.iter().position(|(_, r)| *r == Role::Cancel) {
            Some(ix) => self.choose(ix, cx),
            None if self.choices.len() == 1 => self.choose(0, cx),
            None => cx.emit(DismissEvent),
        }
    }
}

impl Render for Dialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accent = crate::theme::accent(cx);
        let default = self.default_choice();
        let buttons = self.choices.iter().enumerate().map(|(ix, (label, role))| {
            let id = SharedString::from(format!("dialog-choice-{ix}"));
            let main = Some(ix) == default;
            let button = match role {
                Role::Destructive if main => crate::ui::primary_button(id, label.clone(), true, cx).bg(theme.danger),
                Role::Destructive => crate::ui::button(id, label.clone(), cx).text_color(theme.danger),
                _ if main => crate::ui::primary_button(id, label.clone(), true, cx),
                Role::Cancel => crate::ui::button(id, label.clone(), cx),
                _ => crate::ui::button(id, label.clone(), cx),
            };
            button.h(px(28.)).on_click(cx.listener(move |this, _, _, cx| this.choose(ix, cx))).into_any_element()
        });
        // The Mac's order: what loses something on the left, Cancel then the
        // default on the right.
        let left: Vec<usize> = (0..self.choices.len())
            .filter(|&ix| self.choices[ix].1 != Role::Cancel && Some(ix) != default)
            .collect();
        let mut buttons: Vec<Option<gpui_kit::AnyElement>> = buttons.map(Some).collect();
        let take = |ix: usize, buttons: &mut Vec<Option<gpui_kit::AnyElement>>| buttons[ix].take();
        let left_buttons: Vec<gpui_kit::AnyElement> = left.iter().filter_map(|&ix| take(ix, &mut buttons)).collect();
        let cancel = self.choices.iter().position(|(_, r)| *r == Role::Cancel).and_then(|ix| take(ix, &mut buttons));
        let main = default.and_then(|ix| take(ix, &mut buttons));
        div()
            .key_context("Dialog")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_cancel))
            .w(px(420.))
            .max_w_full()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .font_family(crate::theme::UI_FONT)
            .text_size(px(13.))
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(12.))
                    .when_some(self.icon, |d, icon| {
                        d.child(match icon {
                            Icon::Mark => crate::ui::mark(px(36.), cx).into_any_element(),
                            Icon::Svg(path) => div()
                                .size(px(28.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(8.))
                                .bg(accent.opacity(0.12))
                                .child(gpui_kit::svg().path(path).size(px(15.)).text_color(accent))
                                .into_any_element(),
                        })
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(5.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(gpui_kit::base::SelectableText::new("dialog-title", self.title.clone())),
                            )
                            .when_some(self.detail.clone(), |d, detail| {
                                d.child(
                                    div()
                                        .text_size(px(12.5))
                                        .line_height(px(18.))
                                        .text_color(theme.muted_foreground)
                                        .child(gpui_kit::base::SelectableText::new("dialog-detail", detail)),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .id("dialog-buttons")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .children(left_buttons)
                    .child(div().flex_1())
                    .children(cancel)
                    .children(main),
            )
    }
}
