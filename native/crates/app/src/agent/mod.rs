//! Ask an agent about what's open (a notebook, a table, or else the folder),
//! with the subscription you already
//! have (Claude Code or Codex). Not a chat window: you ask in a box in the
//! middle of the app (⌘I), the agent's replies arrive as texts in the bottom
//! right corner, and the notebook follows its work, scrolling to each cell
//! it reads, edits or runs (see [`crate::notebook::agent_tools`]). Running
//! code waits for the reader's yes, asked in the conversation; Stop also
//! interrupts the code the agent started.

pub mod ask;
pub mod mcp;
pub mod provider;
pub mod table_tools;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window,
    div, prelude::FluentBuilder as _, px, svg,
};
use serde_json::Value;

use crate::frame_view::FrameView;
use crate::notebook::{NotebookView, agent_tools};
use crate::theme;
pub use provider::Provider;

/// What a conversation is about: what was in front when it began.
#[derive(Clone, PartialEq)]
pub enum Subject {
    /// Read, edit and run its cells.
    Notebook(WeakEntity<NotebookView>),
    /// Read the table: a data file, or a DataFrame opened full size.
    Table(WeakEntity<FrameView>),
    /// Nothing open: the folder in the sidebar, or home.
    Folder(PathBuf),
}

impl Subject {
    /// What the assistant gets to see, for the notice.
    pub fn sees(&self) -> provider::Sees {
        match self {
            Subject::Notebook(_) => provider::Sees::Notebook,
            Subject::Table(_) => provider::Sees::Table,
            Subject::Folder(_) => provider::Sees::Folder,
        }
    }

    fn system(&self) -> &'static str {
        match self {
            Subject::Notebook(_) => provider::NOTEBOOK,
            Subject::Table(_) => provider::TABLE,
            Subject::Folder(_) => provider::FOLDER,
        }
    }
}

/// Who a line of the conversation is from.
#[derive(Clone, Copy, PartialEq)]
enum Who {
    You,
    Agent,
    /// What the agent is doing ("Editing cell 4"): a quiet line, the latest
    /// of a run of them.
    Step,
    /// What went wrong, to read and copy.
    Problem,
    /// A run waiting for the reader's answer: what it runs, then Allow,
    /// Allow for this conversation, Deny.
    Ask,
    /// The answer to an `Ask`, as a quiet line.
    Answered,
    /// What goes to the provider, before a first question asked from
    /// outside the Ask box (which says it itself).
    Notice,
}

struct Message {
    who: Who,
    text: SharedString,
    /// Steps folded into this one.
    steps: usize,
    /// For an `Ask`: which one, and the code it runs by cell number
    /// (`None`: code outside any cell).
    ask: Option<u64>,
    code: Vec<(Option<usize>, SharedString)>,
}

/// The reader's answer to a run.
#[derive(Clone, Copy)]
enum Answer {
    Once,
    Always,
    Deny,
}

/// What the agent reads when the reader says no.
const DECLINED: &str = "The user didn't allow this run. Don't run it again or work around it (with evaluate \
or another cell): tell them in a sentence what it would do, so they can run it themselves or ask you to.";

/// The conversation is over (closed with ×).
pub struct Closed;

pub struct Conversation {
    subject: Subject,
    pub provider: Provider,
    title: SharedString,
    messages: Vec<Message>,
    /// The provider's id for the conversation, to continue it.
    session: Option<String>,
    run: Option<provider::Run>,
    working: bool,
    server: Option<mcp::Server>,
    /// What the agent's calls started in the kernel, for Stop.
    started: Rc<RefCell<agent_tools::Started>>,
    /// Runs waiting for the reader's answer, by ask id.
    asks: Vec<(u64, mcp::Call)>,
    next_ask: u64,
    /// "Allow for this conversation": runs no longer ask.
    allow_runs: bool,
    /// A question waiting for the reader to read the notice.
    held: Option<String>,
    reply: Entity<InputState>,
    scroll: ScrollHandle,
    minimized: bool,
    /// The pointer is over the messages: show who, minimize and close.
    pointer_over: bool,
    _calls: Option<Task<()>>,
    _events: Option<Task<()>>,
    _subscription: Subscription,
    _release: Subscription,
}

impl EventEmitter<Closed> for Conversation {}

impl Drop for Conversation {
    fn drop(&mut self) {
        if let Some(run) = &self.run {
            run.stop();
        }
    }
}

impl Conversation {
    pub fn new(subject: Subject, title: SharedString, provider: Provider, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let reply = cx.new(|cx| InputState::new(window, cx).placeholder("Reply…"));
        let subscription = cx.subscribe_in(&reply, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: false, .. } = event {
                let text = input.read(cx).value().trim().to_string();
                if !text.is_empty() && this.can_ask() {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    this.ask(text, window, cx);
                }
            }
        });
        // Replaced or closed mid-run: its code stops with it.
        let release = cx.on_release(|this, cx| {
            if this.working {
                let _ = halt_runs(&this.subject, &this.started, cx);
            }
        });
        Self {
            subject,
            provider,
            title,
            messages: Vec::new(),
            session: None,
            run: None,
            working: false,
            server: None,
            started: Rc::default(),
            asks: Vec::new(),
            next_ask: 0,
            allow_runs: false,
            held: None,
            reply,
            scroll: ScrollHandle::new(),
            minimized: false,
            pointer_over: false,
            _calls: None,
            _events: None,
            _subscription: subscription,
            _release: release,
        }
    }

    /// Whether this conversation is about `subject`.
    pub fn is_about(&self, subject: &Subject) -> bool {
        self.subject == *subject
    }

    pub fn is_working(&self) -> bool {
        self.working
    }

    fn can_ask(&self) -> bool {
        !self.working && self.held.is_none()
    }

    fn push(&mut self, who: Who, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        match self.messages.last_mut() {
            Some(last) if who == Who::Step && last.who == Who::Step => {
                last.text = text;
                last.steps += 1;
            }
            _ => self.messages.push(Message { who, text, steps: 1, ask: None, code: Vec::new() }),
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Ask (or follow up): starts the agent with Herse's tools.
    pub fn ask(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.working {
            return;
        }
        // A question still waiting on the notice gives way to this one.
        if self.held.take().is_some() {
            self.messages.retain(|m| m.who != Who::Notice);
        }
        self.minimized = false;
        self.push(Who::You, prompt.clone(), cx);
        // Asked from ⌘K before the notice was read: it waits for it.
        if !crate::settings::assistant_notice_seen() {
            self.held = Some(prompt);
            self.push(Who::Notice, self.provider.privacy_notice(self.subject.sees()), cx);
            return;
        }
        self.start(prompt, window, cx);
    }

    /// The reader read the notice: ask, or don't.
    fn answer_notice(&mut self, ask: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.held.take() else { return };
        if ask {
            crate::settings::set_assistant_notice_seen(None);
            self.messages.retain(|m| m.who != Who::Notice);
            self.start(prompt, window, cx);
        } else if let Some(m) = self.messages.iter_mut().rev().find(|m| m.who == Who::Notice) {
            m.who = Who::Answered;
            m.text = "Not asked".into();
        }
        cx.notify();
    }

    fn start(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.ensure_server(window, cx) {
            self.push(Who::Problem, format!("Herse couldn't open its tools for the agent: {err}"), cx);
            return;
        }
        let Some(socket) = self.server.as_ref().map(|s| s.path().to_path_buf()) else { return };
        let bridge = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.push(Who::Problem, format!("Herse couldn't find itself to bridge the agent: {err}"), cx);
                return;
            }
        };
        let cwd = match &self.subject {
            Subject::Notebook(nb) => nb.read_with(cx, |nb, _| nb.doc.directory().map(|d| d.to_path_buf())).ok().flatten(),
            Subject::Table(table) => {
                table.read_with(cx, |t, _| t.path().and_then(|p| p.parent()).map(|d| d.to_path_buf())).ok().flatten()
            }
            Subject::Folder(dir) => Some(dir.clone()),
        }
        .or_else(|| std::env::var_os("HOME").map(Into::into))
        .unwrap_or_else(|| ".".into());
        let (tx, rx) = smol::channel::unbounded();
        let system = self.subject.system();
        let request = provider::Request { prompt, system, resume: self.session.clone(), cwd, socket, bridge };
        match provider::Run::start(self.provider, request, tx) {
            Ok(run) => {
                self.run = Some(run);
                self.working = true;
                cx.notify();
            }
            Err(err) => {
                self.push(Who::Problem, err, cx);
                return;
            }
        }
        self._events = Some(cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let alive = this.update(cx, |this, cx| this.on_event(event, cx));
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn on_event(&mut self, event: provider::Event, cx: &mut Context<Self>) {
        // A stopped run's last words (its exit) aren't news.
        if !self.working {
            return;
        }
        match event {
            provider::Event::Session(id) => self.session = Some(id),
            provider::Event::Say(text) => self.push(Who::Agent, text, cx),
            provider::Event::Activity(text) => self.push(Who::Step, text, cx),
            provider::Event::Done(error) => {
                self.working = false;
                self.run = None;
                // It stopped waiting for an answer (it gave up, or failed).
                self.withdraw_asks(cx);
                if let Some(error) = error {
                    // A CLI often says its error as a message too: show it once, as the error.
                    let error = match hint(self.provider, &error) {
                        Some(hint) => format!("{}\n{hint}", error.trim()),
                        None => error,
                    };
                    match self.messages.last_mut() {
                        Some(last) if last.who == Who::Agent && error.starts_with(last.text.trim()) => {
                            last.who = Who::Problem;
                            last.text = error.into();
                        }
                        _ => self.push(Who::Problem, error, cx),
                    }
                }
                cx.notify();
            }
        }
    }

    /// The MCP socket the agent's bridge connects to, and the loop that
    /// answers its tool calls against the notebook.
    fn ensure_server(&mut self, window: &mut Window, cx: &mut Context<Self>) -> std::io::Result<()> {
        if self.server.is_some() {
            return Ok(());
        }
        let (tx, rx) = smol::channel::unbounded::<mcp::Call>();
        let (tools, instructions) = match &self.subject {
            Subject::Notebook(_) => (agent_tools::definitions(), agent_tools::INSTRUCTIONS),
            Subject::Table(_) => (table_tools::definitions(), table_tools::INSTRUCTIONS),
            // Its own read tools are enough for a folder.
            Subject::Folder(_) => (serde_json::json!([]), "Herse has no tools for a folder; read its files."),
        };
        self.server = Some(mcp::Server::start(tools, instructions, tx)?);
        self._calls = Some(cx.spawn_in(window, async move |this, cx| {
            while let Ok(call) = rx.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_call(call, window, cx)).is_err() {
                    break;
                }
            }
        }));
        Ok(())
    }

    /// A tool call: running code waits for the reader's answer (until they
    /// allow it for the conversation); everything else goes through.
    fn on_call(&mut self, call: mcp::Call, window: &mut Window, cx: &mut Context<Self>) {
        if !self.working {
            let _ = call.reply.send(Err("The user stopped you.".into()));
            return;
        }
        if let Subject::Notebook(notebook) = &self.subject
            && !self.allow_runs
            && agent_tools::runs_code(&call.tool)
        {
            let who = self.provider.name();
            let preview = notebook
                .update(cx, |nb, cx| {
                    let preview = nb.agent_run_preview(&call.tool, &call.args)?;
                    if let Some(ix) = preview.first {
                        nb.agent_follow(ix, format!("{who} wants to run this"), cx);
                    }
                    Some(preview)
                })
                .ok()
                .flatten();
            if let Some(preview) = preview {
                let id = self.next_ask;
                self.next_ask += 1;
                self.messages.push(Message {
                    who: Who::Ask,
                    text: format!("{who} wants to run {}", run_what(&call.tool, &call.args)).into(),
                    steps: 1,
                    ask: Some(id),
                    code: preview.code.into_iter().map(|(n, code)| (n, code.trim_end().to_string().into())).collect(),
                });
                self.asks.push((id, call));
                self.minimized = false;
                self.scroll.scroll_to_bottom();
                cx.notify();
                return;
            }
        }
        self.dispatch(call, window, cx);
    }

    /// Answer a call against what the conversation is about, showing the step.
    fn dispatch(&mut self, call: mcp::Call, window: &mut Window, cx: &mut Context<Self>) {
        self.push(Who::Step, describe(&call.tool, &call.args), cx);
        let (subject, started) = (self.subject.clone(), self.started.clone());
        let who: SharedString = self.provider.name().into();
        cx.spawn_in(window, async move |_, cx| {
            let answer = match subject {
                Subject::Notebook(notebook) => agent_tools::call(notebook, call.tool, call.args, who, started, cx).await,
                Subject::Table(table) => table_tools::call(table, call.tool, call.args, cx).await,
                Subject::Folder(_) => Err(format!("Herse has no tool called {}.", call.tool)),
            };
            let _ = call.reply.send(answer);
        })
        .detach();
    }

    fn answer(&mut self, id: u64, answer: Answer, window: &mut Window, cx: &mut Context<Self>) {
        let Some(at) = self.asks.iter().position(|(i, _)| *i == id) else { return };
        let (_, call) = self.asks.remove(at);
        let what = run_what(&call.tool, &call.args);
        match answer {
            Answer::Once => {
                self.settle_ask(id, format!("You allowed running {what}"));
                self.dispatch(call, window, cx);
            }
            Answer::Always => {
                self.allow_runs = true;
                self.settle_ask(id, "You allowed running code for this conversation".into());
                self.dispatch(call, window, cx);
                // The others waiting are allowed too.
                for (id, call) in std::mem::take(&mut self.asks) {
                    self.settle_ask(id, format!("Allowed running {}", run_what(&call.tool, &call.args)));
                    self.dispatch(call, window, cx);
                }
            }
            Answer::Deny => {
                self.settle_ask(id, format!("You didn't allow running {what}"));
                let _ = call.reply.send(Err(DECLINED.into()));
            }
        }
        cx.notify();
    }

    /// An ask's card becomes the line saying how it ended.
    fn settle_ask(&mut self, id: u64, text: String) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.ask == Some(id)) {
            m.who = Who::Answered;
            m.text = text.into();
            m.ask = None;
            m.code.clear();
        }
    }

    /// The runs still waiting for an answer won't happen.
    fn withdraw_asks(&mut self, cx: &mut Context<Self>) {
        for (id, call) in std::mem::take(&mut self.asks) {
            self.settle_ask(id, format!("Didn't run {}", run_what(&call.tool, &call.args)));
            let _ = call.reply.send(Err("The user stopped you.".into()));
        }
        cx.notify();
    }

    /// Stop the agent, and what it's running in the kernel.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.run.take() {
            run.stop();
        }
        if !self.working {
            return;
        }
        self.working = false;
        self.withdraw_asks(cx);
        match halt_runs(&self.subject, &self.started, cx) {
            Ok(true) => self.push(Who::Step, "Stopped, and interrupted the code it was running", cx),
            Ok(false) => self.push(Who::Step, "Stopped", cx),
            Err(err) => {
                self.push(Who::Step, "Stopped", cx);
                let name = self.provider.name();
                self.push(
                    Who::Problem,
                    format!("Herse couldn't interrupt the kernel, so the code {name} started may still be running: {err}"),
                    cx,
                );
            }
        }
    }

    fn render_message(&self, ix: usize, message: &Message, cx: &Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let accent = theme::accent(cx);
        let text = match message.who {
            Who::Agent => {
                let (plain, runs) = inline_markdown(&message.text, theme.foreground.opacity(0.07));
                gpui_kit::base::SelectableText::new(("agent-message", ix), plain).with_highlights(runs)
            }
            _ => gpui_kit::base::SelectableText::new(("agent-message", ix), message.text.clone()),
        };
        // Each message is its own bubble on the page, with no card behind:
        // solid, edged and lifted so it reads over code, and a click on it
        // stays with it (the gaps between bubbles reach the notebook).
        let bubble = || {
            div()
                .min_w_0()
                .rounded(px(16.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_sm()
                .block_mouse_except_scroll()
        };
        let step = |dot: gpui_kit::Hsla| {
            bubble()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .py(px(4.))
                .px(px(10.))
                .rounded(px(12.))
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .child(div().size(px(5.)).flex_none().rounded_full().bg(dot))
        };
        match message.who {
            Who::You => div()
                .w_full()
                .flex()
                .flex_row()
                .justify_end()
                .pl(px(40.))
                .child(
                    div()
                        .min_w_0()
                        .px(px(12.))
                        .py(px(7.))
                        .rounded(px(16.))
                        .bg(accent)
                        .shadow_sm()
                        .block_mouse_except_scroll()
                        .text_color(gpui_kit::white())
                        .child(text),
                )
                .into_any_element(),
            Who::Agent => {
                // Who's talking, on its first bubble after each of yours.
                let first = self.messages[..ix]
                    .iter()
                    .rev()
                    .take_while(|m| m.who != Who::You)
                    .all(|m| m.who != Who::Agent);
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .pr(px(28.))
                    .child(
                        bubble()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px(px(12.))
                            .py(px(7.))
                            .when(first, |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(4.))
                                        .text_size(px(11.))
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(accent)
                                        .child(svg().path("icons/sparkles.svg").flex_none().size(px(10.)).text_color(accent))
                                        .child(self.provider.name()),
                                )
                            })
                            .child(text),
                    )
                    .into_any_element()
            }
            Who::Step => step(accent.opacity(0.7))
                .child(div().min_w_0().child(text))
                .when(message.steps > 1, |d| {
                    d.child(div().flex_none().opacity(0.7).child(format!("· {} steps", message.steps)))
                })
                .into_any_element(),
            Who::Problem => div()
                .w_full()
                .pr(px(28.))
                .child(
                    bubble()
                        .px(px(12.))
                        .py(px(8.))
                        .child(crate::ui::error_line("agent-problem", message.text.clone(), cx)),
                )
                .into_any_element(),
            Who::Answered => step(theme.muted_foreground.opacity(0.6))
                .child(div().min_w_0().child(text))
                .into_any_element(),
            Who::Ask => self.render_ask(ix, message, text, cx),
            Who::Notice => div()
                .w_full()
                .pr(px(28.))
                .child(
                    bubble()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(10.))
                        .rounded(px(12.))
                        .child(div().px(px(2.)).text_size(px(12.)).line_height(px(17.)).child(text))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    crate::ui::primary_button(
                                        "agent-notice-ask",
                                        format!("Ask {}", self.provider.name()),
                                        true,
                                        cx,
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| this.answer_notice(true, window, cx))),
                                )
                                .child(
                                    crate::ui::ghost_button("agent-notice-cancel", "Cancel", cx)
                                        .on_click(cx.listener(|this, _, window, cx| this.answer_notice(false, window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
        }
    }

    /// A run waiting for the reader: what it runs, as code to read, then
    /// the answers.
    fn render_ask(
        &self,
        ix: usize,
        message: &Message,
        title: gpui_kit::base::SelectableText,
        cx: &Context<Self>,
    ) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let id = message.ask.unwrap_or_default();
        let block = theme.foreground.opacity(if theme.is_dark() { 0.055 } else { 0.035 });
        let labelled = message.code.len() > 1;
        let code = message.code.iter().enumerate().map(|(n, (cell, code))| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .when_some(cell.filter(|_| labelled), |d, cell| {
                    d.child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(format!("Cell {cell}")))
                })
                .child(
                    div()
                        .id(SharedString::from(format!("agent-ask-code-{ix}-{n}")))
                        .max_h(px(168.))
                        .overflow_y_scroll()
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .bg(block)
                        .font_family(theme::mono_font())
                        .text_size(px(11.5))
                        .line_height(px(17.))
                        .child(gpui_kit::base::SelectableText::new(
                            SharedString::from(format!("agent-ask-text-{ix}-{n}")),
                            code.clone(),
                        )),
                )
        });
        div()
            .w_full()
            .pr(px(28.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_sm()
                    .block_mouse_except_scroll()
                    .child(div().px(px(2.)).child(title))
                    .children(code)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                crate::ui::primary_button(SharedString::from(format!("agent-allow-{id}")), "Allow", true, cx)
                                    .on_click(cx.listener(move |this, _, window, cx| this.answer(id, Answer::Once, window, cx))),
                            )
                            .child(
                                crate::ui::button(
                                    SharedString::from(format!("agent-allow-all-{id}")),
                                    "Allow for this conversation",
                                    cx,
                                )
                                .on_click(cx.listener(move |this, _, window, cx| this.answer(id, Answer::Always, window, cx))),
                            )
                            .child(
                                crate::ui::ghost_button(SharedString::from(format!("agent-deny-{id}")), "Deny", cx)
                                    .on_click(cx.listener(move |this, _, window, cx| this.answer(id, Answer::Deny, window, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// Interrupt what the agent started that's still in the kernel, and end
/// its calls' waiting. Whether there was anything to interrupt.
fn halt_runs(subject: &Subject, started: &Rc<RefCell<agent_tools::Started>>, cx: &mut gpui_kit::App) -> Result<bool, String> {
    started.borrow_mut().stop();
    // Only a notebook's conversation runs code.
    let Subject::Notebook(notebook) = subject else { return Ok(false) };
    notebook.update(cx, |nb, cx| nb.agent_interrupt(&mut started.borrow_mut(), cx)).unwrap_or(Ok(false))
}

/// What a run_cells or evaluate call runs: "cell 4", "cells 2–5".
fn run_what(tool: &str, args: &Value) -> String {
    let n = |k: &str| args.get(k).and_then(Value::as_u64);
    match (tool, n("from"), n("to")) {
        ("run_cells", Some(a), Some(b)) if b > a => format!("cells {a}–{b}"),
        ("run_cells", Some(a), _) => format!("cell {a}"),
        _ => "code in the kernel".into(),
    }
}

/// What to do about a failure the user can fix.
fn hint(provider: Provider, error: &str) -> Option<&'static str> {
    let e = error.to_lowercase();
    if e.contains("401") || e.contains("authenticat") || e.contains("not logged in") || e.contains("log in") {
        return Some(match provider {
            Provider::Claude => "Sign in again: run claude in Terminal and type /login.",
            Provider::Codex => "Sign in again: run codex login in Terminal.",
        });
    }
    if e.contains("model") && (e.contains("not supported") || e.contains("does not exist") || e.contains("not found")) {
        return Some("Its configured model isn't available on your plan: change \"model\" in its config, or set one for Herse under \"assistant_models\" in Herse's settings.json.");
    }
    if e.contains("usage limit") || e.contains("rate limit") || e.contains("429") {
        return Some("Your plan's limit was reached; it resets on its own, or ask the other assistant (Tab in the Ask box).");
    }
    None
}

/// An agent's text as it reads: `code` on a tint and **bold** in bold, the
/// marks themselves dropped.
fn inline_markdown(text: &str, code_bg: gpui_kit::Hsla) -> (SharedString, Vec<(std::ops::Range<usize>, gpui_kit::HighlightStyle)>) {
    let mut plain = String::with_capacity(text.len());
    let mut runs = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = [("`", "`"), ("**", "**")]
            .iter()
            .filter_map(|(open, close)| {
                let start = rest.find(open)?;
                let end = rest[start + open.len()..].find(close)? + start + open.len();
                (end > start + open.len()).then_some((start, end, *open, *close))
            })
            .min_by_key(|(start, ..)| *start);
        let Some((start, end, open, close)) = next else {
            plain.push_str(rest);
            break;
        };
        plain.push_str(&rest[..start]);
        let at = plain.len();
        plain.push_str(&rest[start + open.len()..end]);
        let style = if open == "`" {
            gpui_kit::HighlightStyle { background_color: Some(code_bg), ..Default::default() }
        } else {
            gpui_kit::HighlightStyle { font_weight: Some(gpui_kit::FontWeight::SEMIBOLD), ..Default::default() }
        };
        runs.push((at..plain.len(), style));
        rest = &rest[end + close.len()..];
    }
    (plain.into(), runs)
}

/// What a tool call is doing, as a line in the conversation.
fn describe(tool: &str, args: &Value) -> String {
    let n = |k: &str| args.get(k).and_then(Value::as_u64);
    let cells = || match (n("from"), n("to")) {
        (Some(a), Some(b)) if b > a => format!("cells {a}–{b}"),
        (Some(a), _) => format!("cell {a}"),
        _ => "cells".into(),
    };
    match tool {
        "overview" => "Looking over the notebook".into(),
        "describe_table" => "Looking over the table".into(),
        "read_rows" => match (n("from"), n("to")) {
            (Some(a), Some(b)) if b > a => format!("Reading rows {a}–{b}"),
            (Some(a), _) => format!("Reading row {a}"),
            _ => "Reading rows".into(),
        },
        "find_rows" => {
            let conditions: Vec<String> = args
                .get("where")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| Some(format!("{} {}", c.get("column")?.as_str()?, c.get("condition")?.as_str()?)))
                .collect();
            match (conditions.is_empty(), args.get("search").and_then(Value::as_str)) {
                (false, _) => format!("Finding rows where {}", conditions.join(", ")),
                (true, Some(text)) => format!("Searching for “{text}”"),
                _ => "Finding rows".into(),
            }
        }
        "group_rows" => {
            let by = args.get("by").and_then(Value::as_str).unwrap_or("a column");
            match args.get("value").and_then(Value::as_str) {
                Some(value) => format!("Adding up {value} by {by}"),
                None => format!("Counting rows by {by}"),
            }
        }
        "read_cells" => format!("Reading {}", cells()),
        "edit_cell" => format!("Editing cell {}", n("cell").unwrap_or(0)),
        "insert_cell" => format!("Adding cell {}", n("at").unwrap_or(0)),
        "delete_cell" => format!("Deleting cell {}", n("cell").unwrap_or(0)),
        "run_cells" => format!("Running {}", cells()),
        "evaluate" => {
            let code = args.get("code").and_then(Value::as_str).unwrap_or("");
            let first = code.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            let first: String = first.chars().take(48).collect();
            format!("Looking at the data: {first}")
        }
        other => format!("Using {other}"),
    }
}

impl Render for Conversation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accent = theme::accent(cx);
        let name = self.provider.name();
        if self.minimized {
            let last = self
                .messages
                .iter()
                .rev()
                .find(|m| m.who != Who::You)
                .map_or_else(|| SharedString::from("…"), |m| m.text.clone());
            return div()
                .id("agent-pill")
                .max_w(px(360.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .h(px(34.))
                .px(px(12.))
                .rounded_full()
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_lg()
                .cursor_pointer()
                .text_size(px(12.5))
                .child(svg().path("icons/sparkles.svg").flex_none().size(px(14.)).text_color(accent))
                .child(div().flex_none().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(name))
                .child(div().min_w_0().truncate().text_color(theme.muted_foreground).child(last))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.minimized = false;
                    this.scroll.scroll_to_bottom();
                    cx.notify();
                }))
                .into_any_element();
        }
        let max_h = (window.viewport_size().height * 0.62).max(px(240.));
        let messages: Vec<gpui_kit::AnyElement> =
            self.messages.iter().enumerate().map(|(ix, m)| self.render_message(ix, m, cx)).collect();
        let thinking = self.working && self.asks.is_empty() && self.messages.last().is_none_or(|m| m.who != Who::Agent);
        // No card: the messages float on the page by themselves. The
        // padding is room for their shadows inside the scroll's clip.
        let pad = px(10.);
        let lifted = |d: gpui_kit::Stateful<gpui_kit::Div>| {
            d.border_1().border_color(theme.border).bg(theme.popover).shadow_md().block_mouse_except_scroll()
        };
        // Whether the pointer is over the messages, from the bounds: the
        // bubbles block the mouse, so a hover on the column never registers.
        let over = self.pointer_over;
        let this = cx.entity().downgrade();
        let pointer = gpui_kit::canvas(
            |bounds, _, _| bounds,
            move |_, bounds, window, _| {
                window.on_mouse_event(move |ev: &gpui_kit::MouseMoveEvent, phase, _, cx| {
                    let inside = bounds.contains(&ev.position);
                    if phase == gpui_kit::DispatchPhase::Capture && inside != over {
                        let _ = this.update(cx, |this, cx| {
                            this.pointer_over = inside;
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        div()
            .id("agent-conversation")
            .relative()
            .w(px(380.) + pad * 2.)
            .flex()
            .flex_col()
            .text_size(px(13.))
            .text_color(theme.foreground)
            .font_family(theme::UI_FONT)
            .child(pointer)
            // Who and what it's about, minimize and close: there when the
            // pointer is over the messages, out of the way otherwise.
            .child(
                div()
                    .id("agent-controls")
                    .flex()
                    .flex_row()
                    .justify_end()
                    .px(pad)
                    .items_center()
                    .gap(px(4.))
                    .when(!over, |d| d.invisible())
                    .child(
                        lifted(div().id("agent-controls-pill"))
                            .flex_none()
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(5.))
                            .rounded_full()
                            .shadow_sm()
                            .text_size(px(12.))
                            .child(svg().path("icons/sparkles.svg").flex_none().size(px(12.)).text_color(accent))
                            .child(div().flex_none().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(name))
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(200.))
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(self.title.clone()),
                            ),
                    )
                    .child(
                        lifted(crate::ui::icon_button("agent-minimize", "icons/minus.svg", false, cx))
                            .size(px(26.))
                            .rounded_full()
                            .shadow_sm()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.minimized = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        lifted(crate::ui::icon_button("agent-close", "icons/close.svg", false, cx))
                            .size(px(26.))
                            .rounded_full()
                            .shadow_sm()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stop(cx);
                                cx.emit(Closed);
                            })),
                    ),
            )
            .child(
                div()
                    .id("agent-messages")
                    .max_h(max_h)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(px(6.))
                            .p(pad)
                            .children(messages)
                            .when(thinking, |d| {
                                d.child(
                                    lifted(div().id("agent-thinking"))
                                        .px(px(12.))
                                        .py(px(7.))
                                        .rounded(px(16.))
                                        .shadow_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("•••"),
                                )
                            }),
                    ),
            )
            // At the foot: Stop while it works, a reply field once it's done.
            .child(if self.working {
                div().flex().flex_row().justify_end().px(pad).child(
                    lifted(crate::ui::button("agent-stop", "Stop", cx))
                        .rounded_full()
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            } else {
                div().px(pad).child(
                    lifted(div().id("agent-reply"))
                        .h(px(36.))
                        .pl(px(14.))
                        .pr(px(6.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .rounded_full()
                        .child(div().flex_1().min_w_0().child(Input::new(&self.reply).appearance(false)))
                        .child(
                            div()
                                .id("agent-send")
                                .flex_none()
                                .size(px(24.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if self.can_ask() { accent } else { theme.foreground.opacity(0.12) })
                                .cursor_pointer()
                                .child(svg().path("icons/arrow-up.svg").size(px(13.)).text_color(gpui_kit::white()))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let text = this.reply.read(cx).value().trim().to_string();
                                    if !text.is_empty() && this.can_ask() {
                                        this.reply.update(cx, |input, cx| input.set_value("", window, cx));
                                        this.ask(text, window, cx);
                                    }
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_marks_become_runs() {
        let (plain, runs) = inline_markdown("Cell 4 uses `pd` and **needs** an import, `x", gpui_kit::black());
        assert_eq!(plain.as_ref(), "Cell 4 uses pd and needs an import, `x");
        assert_eq!(runs.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), [12..14, 19..24]);
    }

    #[test]
    fn only_runs_ask_first() {
        for tool in ["run_cells", "evaluate"] {
            assert!(agent_tools::runs_code(tool), "{tool}");
        }
        for tool in ["overview", "read_cells", "edit_cell", "insert_cell", "delete_cell"] {
            assert!(!agent_tools::runs_code(tool), "{tool}");
        }
    }

    #[test]
    fn says_what_a_run_runs() {
        assert_eq!(run_what("run_cells", &serde_json::json!({ "from": 4 })), "cell 4");
        assert_eq!(run_what("run_cells", &serde_json::json!({ "from": 2, "to": 5 })), "cells 2–5");
        assert_eq!(run_what("run_cells", &serde_json::json!({ "from": 3, "to": 3 })), "cell 3");
        assert_eq!(run_what("evaluate", &serde_json::json!({ "code": "df.head()" })), "code in the kernel");
    }

    #[test]
    fn notice_names_who_gets_the_data() {
        let claude = Provider::Claude.privacy_notice(provider::Sees::Notebook);
        assert!(claude.contains("Anthropic") && claude.contains("Claude Code") && claude.contains("data included"));
        let codex = Provider::Codex.privacy_notice(provider::Sees::Notebook);
        assert!(codex.contains("OpenAI") && codex.contains("asks before Codex runs"));
        // A table's assistant reads rows and runs nothing.
        let table = Provider::Claude.privacy_notice(provider::Sees::Table);
        assert!(table.contains("rows it reads from this table") && !table.contains("runs any code"));
    }
}
