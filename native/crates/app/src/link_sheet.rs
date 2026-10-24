//! Open Link: paste a link and Herse opens what it points at. A notebook on
//! the web (GitHub, a gist, nbviewer, Colab, GitLab, Hugging Face, or any URL
//! serving the JSON) opens in a new tab; a live session link someone shared
//! (or a click on a `herse://join/…` link) joins their session; a path opens
//! the file.

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px,
};
use herse_collab::{JoinLink, Peer, Session};

use crate::workspace::Workspace;

actions!(link_sheet, [CloseLinkSheet]);

pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([KeyBinding::new("escape", CloseLinkSheet, Some("LinkSheet"))]);
}

pub struct LinkSheet {
    workspace: WeakEntity<Workspace>,
    input: Entity<InputState>,
    focus: FocusHandle,
    /// What it's doing while the link opens ("Joining…", "Downloading…").
    busy: Option<SharedString>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for LinkSheet {}

impl Focusable for LinkSheet {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LinkSheet {
    pub fn new(workspace: WeakEntity<Workspace>, link: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A link on the clipboard is most likely the one to open.
        let link = link.or_else(|| {
            let text = cx.read_from_clipboard()?.text()?;
            let text = text.trim();
            (text.len() < 4096 && !text.contains('\n') && (text.starts_with("herse://") || crate::remote::is_web_link(text)))
                .then(|| text.to_string())
        });
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("github.com/…/notebook.ipynb or herse://join/…")
                .default_value(link.unwrap_or_default())
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.open(window, cx),
                InputEvent::Change if this.error.take().is_some() => cx.notify(),
                _ => {}
            }
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        Self { workspace, input, focus: cx.focus_handle(), busy: None, error: None, _subscription: subscription }
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with("herse://") {
            self.join(&text, window, cx);
        } else if crate::remote::is_web_link(&text) {
            self.download(text, window, cx);
        } else if let Some(path) = local_path(&text) {
            cx.emit(DismissEvent);
            let workspace = self.workspace.clone();
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |ws, cx| ws.open_path(&path, window, cx)).ok();
            });
        } else {
            self.error = Some(
                "Paste a link to a notebook (on GitHub, nbviewer, Colab…) or a live session link (herse://…).".into(),
            );
            cx.notify();
        }
    }

    fn join(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let link = match JoinLink::parse(text) {
            Ok(link) => link,
            Err(err) => {
                self.error = Some(err.to_string().into());
                cx.notify();
                return;
            }
        };
        self.busy = Some("Joining…".into());
        self.error = None;
        cx.notify();
        let me = Peer::new(crate::notebook::comments::my_name());
        cx.spawn_in(window, async move |this, cx| {
            let joined = cx.background_spawn(Session::join(&link, me)).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match joined {
                    Ok(joined) => this.finish(window, cx, move |ws, window, cx| ws.open_live(joined, window, cx)),
                    Err(err) => {
                        this.error = Some(
                            format!("Couldn't join: {err}. Check that you're on the same network or tailnet as the host.")
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn download(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = crate::remote::raw_url(&text) {
            self.error = Some(err.into());
            cx.notify();
            return;
        }
        self.busy = Some("Downloading…".into());
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let fetched = cx.background_spawn(async move { crate::remote::fetch(&text) }).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match fetched {
                    Ok(fetched) => this.finish(window, cx, move |ws, window, cx| ws.open_fetched(fetched, window, cx)),
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Close, then open: closing hands focus back to where it was, and the
    /// new tab must take it after that.
    fn finish(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        open: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        cx.emit(DismissEvent);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |ws, cx| open(ws, window, cx)).ok();
        });
    }

    fn close(&mut self, _: &CloseLinkSheet, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// A pasted path to a notebook on this Mac (`~/` expanded), if it is one.
fn local_path(text: &str) -> Option<std::path::PathBuf> {
    let text = text.strip_prefix("file://").unwrap_or(text);
    let path = match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(std::path::PathBuf::from)?.join(rest),
        None => std::path::PathBuf::from(text),
    };
    (path.is_absolute() && path.extension().is_some_and(|e| e == "ipynb") && path.is_file()).then_some(path)
}

impl Render for LinkSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.busy.clone();
        div()
            .key_context("LinkSheet")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .w(px(520.))
            .max_w_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(18.))
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
                    .items_center()
                    .gap(px(8.))
                    .child(gpui_kit::svg().path("icons/link.svg").size(px(14.)).text_color(theme.muted_foreground))
                    .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Open Link")),
            )
            .child(div().text_size(px(12.)).text_color(theme.muted_foreground).child(
                "A notebook on GitHub, a gist, nbviewer, Colab, GitLab or Hugging Face opens in a new tab. A live session link joins that session.",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(28.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(crate::ui::CONTROL_RADIUS)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .font_family(crate::theme::mono_font())
                            .text_size(px(12.))
                            .child(Input::new(&self.input).appearance(false)),
                    )
                    .child(
                        crate::ui::primary_button("link-open", busy.clone().unwrap_or_else(|| "Open".into()), busy.is_none(), cx)
                            .when(busy.is_none(), |b| b.on_click(cx.listener(|this, _, window, cx| this.open(window, cx)))),
                    ),
            )
            .when_some(self.error.clone(), |d, error| d.child(crate::ui::error_line("link-error", error, cx)))
    }
}
