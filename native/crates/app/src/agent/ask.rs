//! The Ask box (⌘I): one line in the middle of the app, like Spotlight. Type
//! a question or a request about the open notebook and press Return; the
//! answer comes as texts in the corner while the notebook follows the work.
//! The first time, it says what goes to the provider.

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window, actions, div, prelude::FluentBuilder as _, px, svg,
};

use super::Provider;
use crate::workspace::Workspace;

actions!(ask_box, [CloseAskBox, NextAssistant]);

pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseAskBox, Some("AskBox")),
        KeyBinding::new("tab", NextAssistant, Some("AskBox")),
    ]);
}

pub struct AskBox {
    workspace: WeakEntity<Workspace>,
    input: Entity<InputState>,
    focus: FocusHandle,
    /// The installed assistants, the preferred one first.
    providers: Vec<Provider>,
    chosen: usize,
    /// What it's about (the notebook's name), and whether it continues a
    /// conversation already under way.
    about: SharedString,
    /// What it sends, for the notice.
    sees: super::provider::Sees,
    follow_up: bool,
    /// Say what the assistant sends where (until a question is asked with
    /// it showing).
    notice: bool,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for AskBox {}

impl Focusable for AskBox {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AskBox {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        about: SharedString,
        sees: super::provider::Sees,
        current: Option<Provider>,
        follow_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preferred = current.or_else(|| crate::settings::assistant().and_then(|id| Provider::from_id(&id)));
        let providers = Provider::installed(preferred);
        let input = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit(window, cx);
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let mut this = Self {
            workspace,
            input,
            focus: cx.focus_handle(),
            providers,
            chosen: 0,
            about,
            sees,
            follow_up,
            notice: !crate::settings::assistant_notice_seen(),
            _subscription: subscription,
        };
        this.update_placeholder(window, cx);
        this
    }

    fn provider(&self) -> Option<Provider> {
        self.providers.get(self.chosen).copied()
    }

    fn update_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match self.provider() {
            Some(p) if self.follow_up => format!("Reply to {}…", p.name()),
            Some(p) => format!("Ask {} about {}…", p.name(), self.about),
            None => "Ask about this notebook…".into(),
        };
        self.input.update(cx, |input, cx| input.set_placeholder(text, window, cx));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        let Some(provider) = self.provider() else { return };
        if text.is_empty() {
            return;
        }
        if self.notice {
            crate::settings::set_assistant_notice_seen(Some(provider.id()));
        } else {
            crate::settings::set_assistant(provider.id());
        }
        cx.emit(DismissEvent);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |ws, cx| ws.ask_agent(provider, text, window, cx)).ok();
        });
    }

    fn next(&mut self, _: &NextAssistant, window: &mut Window, cx: &mut Context<Self>) {
        if self.providers.len() > 1 {
            self.chosen = (self.chosen + 1) % self.providers.len();
            self.follow_up = false;
            self.update_placeholder(window, cx);
            cx.notify();
        }
    }

    fn close(&mut self, _: &CloseAskBox, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for AskBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accent = crate::theme::accent(cx);
        let provider = self.provider();
        let others = self.providers.len() > 1;
        let hint = match (provider, others) {
            (Some(_), true) => {
                let next = self.providers[(self.chosen + 1) % self.providers.len()];
                format!("Return to ask · Tab for {} · Esc to close", next.name())
            }
            (Some(_), false) => "Return to ask · Esc to close".to_string(),
            (None, _) => String::new(),
        };
        div()
            .key_context("AskBox")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::next))
            .w(px(640.))
            .max_w_full()
            .mt(px(96.))
            .flex()
            .flex_col()
            .rounded(px(14.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .overflow_hidden()
            .font_family(crate::theme::UI_FONT)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .h(px(56.))
                    .px(px(18.))
                    .child(svg().path("icons/sparkles.svg").flex_none().size(px(19.)).text_color(accent))
                    .child(div().flex_1().min_w_0().text_size(px(17.)).child(Input::new(&self.input).appearance(false)))
                    .when_some(provider, |d, p| {
                        d.child(
                            div()
                                .id("ask-provider")
                                .flex_none()
                                .h(px(24.))
                                .px(px(9.))
                                .flex()
                                .items_center()
                                .rounded_full()
                                .bg(accent.opacity(0.12))
                                .text_color(accent)
                                .text_size(px(12.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .when(others, |d| d.cursor_pointer())
                                .child(p.name())
                                .on_click(cx.listener(|this, _, window, cx| this.next(&NextAssistant, window, cx))),
                        )
                    }),
            )
            .when_some(provider.filter(|_| self.notice), |d, p| {
                d.child(
                    div()
                        .px(px(18.))
                        .py(px(10.))
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(12.))
                        .line_height(px(17.))
                        .text_color(theme.muted_foreground)
                        .child(gpui_kit::base::SelectableText::new("ask-notice", p.privacy_notice(self.sees))),
                )
            })
            .map(|d| match provider {
                Some(_) => d.child(
                    div()
                        .px(px(18.))
                        .py(px(8.))
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(11.5))
                        .text_color(theme.muted_foreground)
                        .child(hint),
                ),
                None => d.child(
                    div().px(px(18.)).py(px(12.)).border_t_1().border_color(theme.border).child(crate::ui::error_line(
                        "ask-none",
                        "Herse asks the coding agent you already use, and neither is installed here. Install \
                         Claude Code (curl -fsSL https://claude.ai/install.sh | bash) or Codex \
                         (npm install -g @openai/codex), sign in once in Terminal, then press ⌘I again."
                            .into(),
                        cx,
                    )),
                ),
            })
    }
}
