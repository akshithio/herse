//! Share (the tab strip's share button, File ▸ Share…, ⌘K): publish the
//! notebook to a link that opens in any browser, and read and answer the
//! comments people leave on it.
//!
//! Publishing goes through a GitHub gist; the link people get is its
//! nbviewer page. The account is the GitHub CLI's when it is signed in, else
//! a token pasted here once and kept in the Keychain. Every network and
//! Keychain call runs off the main thread; the sheet fills in as they land.

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use herse_share as share;

use crate::notebook::NotebookView;

actions!(share_sheet, [CloseShare]);

pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([KeyBinding::new("escape", CloseShare, Some("ShareSheet"))]);
}

/// Where the GitHub account stands.
enum Account {
    Checking,
    SignedOut,
    SignedIn { client: share::GistClient, login: String, via: share::TokenSource },
}

pub struct ShareSheet {
    notebook: WeakEntity<NotebookView>,
    name: SharedString,
    focus: FocusHandle,
    account: Account,
    token_input: Entity<InputState>,
    comment_input: Entity<InputState>,
    public: bool,
    strip_outputs: bool,
    /// What the scan found, worded for people.
    warnings: Vec<SharedString>,
    info: Option<share::ShareInfo>,
    /// The work under way ("Publishing…"), if any.
    busy: Option<&'static str>,
    error: Option<SharedString>,
    /// Publishing hit a link of the other visibility: offer a new one.
    offer_new_link: bool,
    comments: Option<Vec<share::Comment>>,
    copied: bool,
    /// Which live link was just copied.
    copied_live: Option<&'static str>,
    _subscriptions: Vec<Subscription>,
}

/// The live session, as the sheet shows it.
struct LiveView {
    host: bool,
    link: Option<String>,
    view_link: Option<String>,
    people: SharedString,
    status: SharedString,
}

impl EventEmitter<DismissEvent> for ShareSheet {}

impl Focusable for ShareSheet {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ShareSheet {
    pub fn new(notebook: Entity<NotebookView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (nb, name) = {
            let view = notebook.read(cx);
            (view.doc.nb.clone(), view.title())
        };
        let info = share::read_share_info(&nb);
        let token_input = cx.new(|cx| InputState::new(window, cx).placeholder("ghp_… or github_pat_…").masked(true));
        let comment_input = cx.new(|cx| InputState::new(window, cx).placeholder("Reply on the link…"));
        let subscriptions = vec![
            // People coming and going, the session starting and ending.
            cx.observe(&notebook, |_, _, cx| cx.notify()),
            cx.subscribe_in(&token_input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.connect(cx);
                }
            }),
            cx.subscribe_in(&comment_input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.post_comment(window, cx);
                }
            }),
        ];
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut sheet = Self {
            notebook: notebook.downgrade(),
            name: name.into(),
            focus,
            account: Account::Checking,
            token_input,
            comment_input,
            public: info.as_ref().is_some_and(|i| i.public),
            strip_outputs: false,
            warnings: Vec::new(),
            info,
            busy: None,
            error: None,
            offer_new_link: false,
            comments: None,
            copied: false,
            copied_live: None,
            _subscriptions: subscriptions,
        };
        sheet.find_account(cx);
        sheet.scan(cx);
        sheet
    }

    fn snapshot(&self, cx: &gpui_kit::App) -> Option<herse_nb::Notebook> {
        self.notebook.upgrade().map(|nb| nb.read(cx).doc.nb.clone())
    }

    /// The GitHub CLI's account, else the Keychain's token.
    fn find_account(&mut self, cx: &mut Context<Self>) {
        // The snapshot tool and tests never reach for a real account.
        if std::env::var_os("HERSE_NO_ACCOUNTS").is_some() {
            self.account = Account::SignedOut;
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let (token, via) = share::discover_token().ok().flatten()?;
                    let client = share::GistClient::new(token);
                    let login = client.login().ok()?;
                    Some((client, login, via))
                })
                .await;
            this.update(cx, |this, cx| {
                this.account = match found {
                    Some((client, login, via)) => Account::SignedIn { client, login, via },
                    None => Account::SignedOut,
                };
                cx.notify();
                this.load_comments(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Look for what shouldn't go public: secrets, home paths, huge outputs.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(nb) = self.snapshot(cx) else { return };
        let include_outputs = !self.strip_outputs;
        cx.spawn(async move |this, cx| {
            let warnings = cx
                .background_spawn(async move {
                    let options = share::ScanOptions { include_outputs, ..Default::default() };
                    share::scan(&nb, &options).warnings.iter().map(|w| SharedString::from(w.message())).collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.warnings = warnings;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A pasted token: check it with GitHub, then keep it in the Keychain.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let raw = self.token_input.read(cx).value().to_string();
        let token = match share::Token::new(raw.trim()) {
            Ok(token) => token,
            Err(err) => {
                self.error = Some(err.to_string().into());
                cx.notify();
                return;
            }
        };
        self.busy = Some("Checking the token…");
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let client = share::GistClient::new(token.clone());
                    let identity = client.whoami()?;
                    if identity.can_write_gists() == Some(false) {
                        return Err(share::ShareError::MissingGistScope { scopes: identity.scopes.clone() });
                    }
                    share::Keychain::default().store(&identity.login, &token)?;
                    Ok((client, identity.login))
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok((client, login)) => {
                        this.account = Account::SignedIn { client, login, via: share::TokenSource::Keychain };
                        this.load_comments(cx);
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn publish(&mut self, new_link: bool, cx: &mut Context<Self>) {
        let Account::SignedIn { client, .. } = &self.account else { return };
        let Some(mut nb) = self.snapshot(cx) else { return };
        let client = client.clone();
        let mut options = share::PublishOptions::new(self.name.to_string(), self.public);
        options.strip_outputs = self.strip_outputs;
        options.new_gist = new_link;
        self.busy = Some(if self.info.is_some() && !new_link { "Updating the link…" } else { "Publishing…" });
        self.error = None;
        self.offer_new_link = false;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { share::publish(&client, &mut nb, &options) }).await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(published) => {
                        this.info = Some(published.info.clone());
                        if let Some(nb) = this.notebook.upgrade() {
                            nb.update(cx, |nb, cx| nb.set_share_info(Some(&published.info), cx));
                        }
                        this.load_comments(cx);
                    }
                    Err(share::ShareError::VisibilityMismatch { gist_public }) => {
                        this.error = Some(
                            format!(
                                "This notebook's link is {}; GitHub can't change that. Make a new link to share it {}.",
                                if gist_public { "public" } else { "unlisted" },
                                if gist_public { "unlisted" } else { "publicly" },
                            )
                            .into(),
                        );
                        this.offer_new_link = true;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn unpublish(&mut self, cx: &mut Context<Self>) {
        let Account::SignedIn { client, .. } = &self.account else { return };
        let Some(mut nb) = self.snapshot(cx) else { return };
        let client = client.clone();
        self.busy = Some("Taking the link down…");
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { share::unpublish(&client, &mut nb) }).await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(_) => {
                        this.info = None;
                        this.comments = None;
                        if let Some(nb) = this.notebook.upgrade() {
                            nb.update(cx, |nb, cx| nb.set_share_info(None, cx));
                        }
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_comments(&mut self, cx: &mut Context<Self>) {
        let (Account::SignedIn { client, .. }, Some(info)) = (&self.account, &self.info) else { return };
        let (client, id) = (client.clone(), info.gist_id.clone());
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { client.list_comments(&id) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(comments) => this.comments = Some(comments),
                    Err(share::ShareError::NotFound) => {}
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn post_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Account::SignedIn { client, .. }, Some(info)) = (&self.account, &self.info) else { return };
        let body = self.comment_input.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        let (client, id) = (client.clone(), info.gist_id.clone());
        self.comment_input.update(cx, |input, cx| input.set_value("", window, cx));
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { client.create_comment(&id, &body) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(comment) => this.comments.get_or_insert_default().push(comment),
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn live_view(&self, cx: &gpui_kit::App) -> Option<LiveView> {
        let nb = self.notebook.upgrade()?;
        let live = nb.read(cx).live()?;
        Some(LiveView {
            host: live.role == herse_collab::Role::Host,
            link: live.link.as_ref().map(ToString::to_string),
            view_link: live.view_link.as_ref().map(ToString::to_string),
            people: live.people(),
            status: match &live.status {
                herse_collab::Status::Reconnecting { .. } => "Reconnecting…".into(),
                herse_collab::Status::Ended => "Ended".into(),
                _ => "Live".into(),
            },
        })
    }

    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(nb) = self.notebook.upgrade() else { return };
        if let Err(err) = nb.update(cx, |nb, cx| nb.start_live(window, cx)) {
            self.error = Some(format!("Couldn't start the session: {err}").into());
        }
        cx.notify();
    }

    fn stop_live(&mut self, cx: &mut Context<Self>) {
        if let Some(nb) = self.notebook.upgrade() {
            nb.update(cx, |nb, cx| nb.stop_live(cx));
        }
        self.copied_live = None;
        cx.notify();
    }

    fn close(&mut self, _: &CloseShare, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// "3 min ago", "2 h ago", "Oct 16": when a comment was written.
fn ago(iso: &str) -> String {
    let Some(at) = share::parse_timestamp(iso) else { return String::new() };
    let secs = std::time::SystemTime::now().duration_since(at).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => iso.get(..10).unwrap_or(iso).to_string(),
    }
}

impl Render for ShareSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accent = crate::theme::accent(cx);
        let busy = self.busy.is_some();
        let signed_in = matches!(self.account, Account::SignedIn { .. });
        let section = |title: &'static str| {
            div()
                .pt(px(14.))
                .pb(px(6.))
                .text_size(px(11.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(title)
        };
        let muted = |text: SharedString| div().text_size(px(12.)).text_color(theme.muted_foreground).child(text);

        let account: gpui_kit::AnyElement = match &self.account {
            Account::Checking => muted("Looking for your GitHub account…".into()).into_any_element(),
            Account::SignedIn { login, via, .. } => div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.5))
                .child(div().size(px(6.)).rounded_full().bg(accent))
                .child(format!("GitHub · @{login}"))
                .child(muted(match via {
                    share::TokenSource::GhCli => "from the GitHub CLI".into(),
                    share::TokenSource::Keychain => "token in your Keychain".into(),
                }))
                .into_any_element(),
            Account::SignedOut => div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(muted(
                    "Links are published as GitHub gists. Sign in with the GitHub CLI (gh auth login), or paste a token with the gist scope:"
                        .into(),
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .h(px(28.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .rounded(crate::ui::CONTROL_RADIUS)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .child(Input::new(&self.token_input).appearance(false)),
                        )
                        .child(
                            crate::ui::button("share-connect", "Connect", cx)
                                .h(px(28.))
                                .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                        ),
                )
                .child(
                    div()
                        .id("share-new-token")
                        .text_size(px(12.))
                        .text_color(theme.link)
                        .cursor_pointer()
                        .child("Create a token on GitHub →")
                        .on_click(|_, _, cx| {
                            cx.open_url("https://github.com/settings/tokens/new?scopes=gist&description=Herse")
                        }),
                )
                .into_any_element(),
        };

        let link = self.info.as_ref().map(|info| {
            let url: SharedString = info.nbviewer_url().into();
            let gist: SharedString = info.html_url.clone().into();
            let (copy_url, open_url) = (url.clone(), url.clone());
            div()
                .mt(px(10.))
                .p(px(10.))
                .rounded(px(8.))
                .bg(theme.foreground.opacity(0.04))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(gpui_kit::svg().path("icons/link.svg").size(px(13.)).flex_none().text_color(theme.muted_foreground))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(crate::theme::mono_font())
                                .text_size(px(12.))
                                .child(url.clone()),
                        )
                        .child(
                            crate::ui::button("share-copy", if self.copied { "Copied" } else { "Copy Link" }, cx).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy_url.to_string()));
                                    this.copied = true;
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(crate::ui::button("share-open", "Open", cx).on_click(move |_, _, cx| cx.open_url(&open_url))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.))
                        .child(muted(
                            if info.public { "Public: listed on your GitHub profile." } else { "Unlisted: anyone with the link can open it." }.into(),
                        ))
                        .child(div().flex_1())
                        .child(crate::ui::ghost_button("share-gist", "Gist Page", cx).on_click(move |_, _, cx| cx.open_url(&gist)))
                        .child(
                            crate::ui::ghost_button("share-unpublish", "Take Down", cx)
                                .on_click(cx.listener(|this, _, _, cx| this.unpublish(cx))),
                        ),
                )
        });

        let comments = self.info.as_ref().map(|_| {
            let list = self.comments.clone().unwrap_or_default();
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .when(self.comments.is_some() && list.is_empty(), |d| {
                    d.child(muted("No comments on the link yet. People sign in to GitHub to leave one.".into()))
                })
                .children(list.into_iter().map(|c| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(6.))
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(c.author.clone().unwrap_or_else(|| "ghost".into())),
                                )
                                .child(div().text_color(theme.muted_foreground).child(ago(&c.created_at))),
                        )
                        .child(div().text_size(px(12.5)).child(c.body.clone()))
                }))
                .when(signed_in, |d| {
                    d.child(
                        div()
                            .h(px(28.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(crate::ui::CONTROL_RADIUS)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(Input::new(&self.comment_input).appearance(false)),
                    )
                })
        });

        let publish_label = match (self.busy, self.info.is_some()) {
            (Some(what), _) => what,
            (None, true) => "Update Link",
            (None, false) => "Publish Link",
        };
        let can_publish = signed_in && !busy;

        div()
            .key_context("ShareSheet")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .w(px(540.))
            .max_w_full()
            .max_h(px(640.))
            .flex()
            .flex_col()
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
                    .h(px(48.))
                    .px(px(18.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(gpui_kit::svg().path("icons/share-2.svg").size(px(14.)).text_color(theme.muted_foreground))
                    .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Share"))
                    .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(self.name.clone()))
                    .child(
                        crate::ui::icon_button("share-close", "icons/close.svg", false, cx)
                            .on_click(cx.listener(|this, _, window, cx| this.close(&CloseShare, window, cx))),
                    ),
            )
            .child(
                div()
                    .id("share-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(18.))
                    .pb(px(18.))
                    .child(section("WORK TOGETHER LIVE"))
                    .child(self.render_live(cx))
                    .child(section("PUBLISH A LINK"))
                    .child(account)
                    .when(signed_in, |d| {
                        d.child(
                            div()
                                .pt(px(12.))
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(
                                    crate::ui::check("share-unlisted", !self.public, "Unlisted — only people with the link", cx)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.public = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    crate::ui::check("share-public", self.public, "Public — listed on your GitHub profile", cx)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.public = true;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    crate::ui::check("share-strip", self.strip_outputs, "Leave out outputs", cx).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.strip_outputs = !this.strip_outputs;
                                            this.scan(cx);
                                            cx.notify();
                                        }),
                                    ),
                                ),
                        )
                    })
                    .when(!self.warnings.is_empty(), |d| {
                        let more = self.warnings.len().saturating_sub(6);
                        d.child(
                            div()
                                .mt(px(12.))
                                .p(px(10.))
                                .rounded(px(8.))
                                .bg(gpui_kit::hsla(40. / 360., 0.9, 0.55, 0.12))
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .text_size(px(12.))
                                .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Check before publishing"))
                                .children(self.warnings.iter().take(6).map(|w| div().child(format!("• {w}"))))
                                .when(more > 0, |d| d.child(div().text_color(theme.muted_foreground).child(format!("and {more} more")))),
                        )
                    })
                    .when_some(self.error.clone(), |d, error| {
                        d.child(div().mt(px(10.)).child(crate::ui::error_line("share-error", error, cx)))
                    })
                    .child(
                        div()
                            .pt(px(12.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(crate::ui::primary_button("share-publish", publish_label, can_publish, cx).when(
                                can_publish,
                                |b| b.on_click(cx.listener(|this, _, _, cx| this.publish(false, cx))),
                            ))
                            .when(self.offer_new_link && can_publish, |d| {
                                d.child(
                                    crate::ui::button("share-new-link", "Make a New Link", cx)
                                        .h(px(28.))
                                        .on_click(cx.listener(|this, _, _, cx| this.publish(true, cx))),
                                )
                            }),
                    )
                    .children(link)
                    .when(comments.is_some(), |d| d.child(section("COMMENTS ON THE LINK")))
                    .children(comments),
            )
    }
}

impl ShareSheet {
    fn render_live(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme().clone();
        let accent = crate::theme::accent(cx);
        let muted = |text: &'static str| div().text_size(px(12.)).text_color(theme.muted_foreground).child(text);
        let Some(live) = self.live_view(cx) else {
            return div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(muted(
                    "People on your network or Tailscale open the link and edit this notebook with you, live. The link is the key: share it only with them.",
                ))
                .child(
                    div().flex().flex_row().child(
                        crate::ui::button("live-start", "Start Live Session", cx)
                            .h(px(28.))
                            .on_click(cx.listener(|this, _, window, cx| this.start_live(window, cx))),
                    ),
                )
                .into_any_element();
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(12.5))
            .child(div().size(px(6.)).rounded_full().bg(accent))
            .child(div().font_weight(gpui_kit::FontWeight::MEDIUM).child(live.status.clone()))
            .child(div().text_color(theme.muted_foreground).child(live.people.clone()));
        let copied = self.copied_live;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(header)
            .when_some(live.link.clone(), |d, link| {
                let copy = link.clone();
                d.child(
                    div()
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(theme.foreground.opacity(0.04))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(gpui_kit::svg().path("icons/link.svg").size(px(13.)).flex_none().text_color(theme.muted_foreground))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(crate::theme::mono_font())
                                .text_size(px(12.))
                                .child(link),
                        )
                        .child(
                            crate::ui::button("live-copy", if copied == Some("edit") { "Copied" } else { "Copy Link" }, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    this.copied_live = Some("edit");
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(4.))
                    .child(div().flex_1())
                    .when_some(live.view_link.clone(), |d, view| {
                        d.child(
                            crate::ui::ghost_button(
                                "live-copy-view",
                                if copied == Some("view") { "Copied" } else { "Copy View-Only Link" },
                                cx,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(view.clone()));
                                this.copied_live = Some("view");
                                cx.notify();
                            })),
                        )
                    })
                    .child(
                        crate::ui::ghost_button("live-stop", if live.host { "End Session" } else { "Leave" }, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.stop_live(cx))),
                    ),
            )
            .into_any_element()
    }
}
