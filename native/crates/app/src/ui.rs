//! Herse's controls. Every button-like thing in the app is one of these, so
//! they share one height, radius, icon size and hover (see DESIGN.md).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Div, ElementId, InteractiveElement as _, ParentElement as _, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, svg,
};

/// Control height and radius: one size for every small control.
pub const CONTROL: gpui_kit::Pixels = px(24.);
pub const CONTROL_RADIUS: gpui_kit::Pixels = px(6.);
/// Icons are Lucide outlines at one size.
pub const ICON: gpui_kit::Pixels = px(14.);

/// A square button holding one icon, dim until hovered; `on` marks a toggle
/// that is on.
pub fn icon_button(id: impl Into<ElementId>, icon: &'static str, on: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let ink = if on { theme.foreground } else { theme.muted_foreground };
    div()
        .id(id)
        .size(CONTROL)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(CONTROL_RADIUS)
        .cursor_pointer()
        .when(on, |d| d.bg(theme.foreground.opacity(0.08)))
        .hover(|s| s.bg(theme.foreground.opacity(0.06)))
        .active(|s| s.bg(theme.foreground.opacity(0.1)))
        .child(svg().path(icon).size(ICON).text_color(ink))
}

/// A text button: a hairline-bordered chip on the page colour.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .h(CONTROL)
        .flex_none()
        .flex()
        .items_center()
        .px(px(9.))
        .rounded(CONTROL_RADIUS)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .text_size(px(12.))
        .text_color(theme.foreground)
        .cursor_pointer()
        .hover(|s| s.bg(theme.foreground.opacity(0.04)))
        .active(|s| s.bg(theme.foreground.opacity(0.08)))
        .child(label.into())
}

/// The one action a sheet is for: a chip filled with the accent.
pub fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>, enabled: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let accent = crate::theme::accent(cx);
    div()
        .id(id)
        .h(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(12.))
        .rounded(CONTROL_RADIUS)
        .bg(if enabled { accent } else { theme.foreground.opacity(0.12) })
        .text_size(px(12.5))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(if enabled { theme.background } else { theme.muted_foreground })
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.opacity(0.92)).active(|s| s.opacity(0.85)))
        .child(label.into())
}

/// A choice that is on or off: a small box with a check, then its label.
pub fn check(id: impl Into<ElementId>, on: bool, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let accent = crate::theme::accent(cx);
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .text_size(px(12.5))
        .child(
            div()
                .size(px(14.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(if on { accent } else { theme.border })
                .bg(if on { accent } else { theme.background })
                .when(on, |d| d.child(svg().path("icons/check.svg").size(px(10.)).text_color(theme.background))),
        )
        .child(label.into())
}

/// A quiet text button: pencil text, no border, a tint on hover. For
/// secondary actions that sit on the page (adding a cell).
pub fn ghost_button(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .h(CONTROL)
        .flex_none()
        .flex()
        .items_center()
        .px(px(9.))
        .rounded(CONTROL_RADIUS)
        .text_size(px(12.))
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|s| s.bg(theme.foreground.opacity(0.05)).text_color(theme.foreground))
        .active(|s| s.bg(theme.foreground.opacity(0.08)))
        .child(label.into())
}

/// The logo's blue (`brand/herse-logo.svg`). It's the brand's colour, kept
/// in both themes; inside the app, dew stays the one accent.
pub const LOGO_BLUE: u32 = 0x0682FE;

/// Herse's mark: the logo's swash, in the logo's blue.
pub fn mark(size: gpui_kit::Pixels) -> Div {
    div().size(size).flex_none().child(svg().size(size).path("icons/herse-mark.svg").text_color(gpui_kit::rgb(LOGO_BLUE)))
}

/// A failure the reader has to see and may want to copy (a kernel that died,
/// a notebook that wouldn't open): a red-tinted card whose title and detail
/// are selectable text, with Copy (both, as plain text), any `actions`, and
/// a close button. It stays until closed; nothing about it times out.
#[allow(clippy::too_many_arguments)]
pub fn problem_banner(
    id: &'static str,
    title: SharedString,
    detail: SharedString,
    actions: Vec<gpui_kit::AnyElement>,
    on_copied: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
    on_close: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    banner(Tone::Problem, id, title, detail, actions, on_copied, on_close, cx)
}

/// How a [`banner`] reads: something failed (red), something to know
/// before it fails (amber, such as packages a notebook needs), or a question
/// only the reader can answer (amber with a shield: may an extension run?).
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Problem,
    Notice,
    Permission,
}

/// A [`problem_banner`] in either tone.
#[allow(clippy::too_many_arguments)]
pub fn banner(
    tone: Tone,
    id: &'static str,
    title: SharedString,
    detail: SharedString,
    actions: Vec<gpui_kit::AnyElement>,
    on_copied: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
    on_close: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let dark = theme.is_dark();
    let (red, background, icon) = match tone {
        Tone::Problem => (crate::theme::error_color(dark), crate::theme::error_background(dark), "icons/circle-alert.svg"),
        Tone::Notice | Tone::Permission => {
            let amber = crate::theme::changed_color(dark);
            let icon = if tone == Tone::Notice { "icons/package.svg" } else { "icons/shield.svg" };
            (amber, amber.opacity(if dark { 0.10 } else { 0.08 }), icon)
        }
    };
    let copy = if detail.is_empty() { title.to_string() } else { format!("{title}\n{detail}") };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.))
        .p(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(red.opacity(0.3))
        .bg(background)
        .child(svg().path(icon).flex_none().mt(px(2.)).size(ICON).text_color(red))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("{id}-title")), title)),
                )
                .when(!detail.is_empty(), |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("{id}-detail-scroll")))
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .font_family(crate::theme::mono_font())
                            .text_size(crate::theme::mono_size())
                            .line_height(crate::theme::mono_line_height())
                            .text_color(theme.foreground.opacity(0.85))
                            .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("{id}-detail")), detail)),
                    )
                }),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(button(SharedString::from(format!("{id}-copy")), "Copy", cx).on_click(move |event, window, cx| {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(copy.clone()));
                    on_copied(event, window, cx);
                }))
                .children(actions)
                .child(icon_button(SharedString::from(format!("{id}-close")), "icons/close.svg", false, cx).on_click(on_close)),
        )
}

/// An error inside a sheet: red, selectable text with a Copy button beside
/// it. (A failure above a notebook is a [`problem_banner`].)
pub fn error_line(id: &'static str, text: SharedString, cx: &App) -> Div {
    let theme = cx.theme();
    let copy = text.to_string();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(theme.danger)
                .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("{id}-text")), text)),
        )
        .child(button(SharedString::from(format!("{id}-copy")), "Copy", cx).flex_none().on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(copy.clone()))
        }))
}

/// A quiet button that opens a menu on click (with
/// `gpui_kit::component::menu::DropdownMenu::dropdown_menu`): a hover
/// tint, and while its menu is open, the pressed tint.
#[derive(gpui_kit::IntoElement)]
pub struct MenuTrigger {
    base: Stateful<Div>,
    style: gpui_kit::StyleRefinement,
    children: Vec<gpui_kit::AnyElement>,
    selected: bool,
}

impl MenuTrigger {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self { base: div().id(id), style: Default::default(), children: Vec::new(), selected: false }
    }
}

impl gpui_kit::component::Selectable for MenuTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl gpui_kit::ParentElement for MenuTrigger {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui_kit::AnyElement>) {
        self.children.extend(elements);
    }
}

impl gpui_kit::Styled for MenuTrigger {
    fn style(&mut self) -> &mut gpui_kit::StyleRefinement {
        &mut self.style
    }
}

impl gpui_kit::InteractiveElement for MenuTrigger {
    fn interactivity(&mut self) -> &mut gpui_kit::Interactivity {
        self.base.interactivity()
    }
}

impl gpui_kit::StatefulInteractiveElement for MenuTrigger {}

impl gpui_kit::component::menu::DropdownMenu for MenuTrigger {}

impl gpui_kit::RenderOnce for MenuTrigger {
    fn render(self, _: &mut gpui_kit::Window, cx: &mut App) -> impl gpui_kit::IntoElement {
        let theme = cx.theme();
        let mut base = self.base;
        *base.style() = self.style;
        base.rounded(CONTROL_RADIUS)
            .cursor_pointer()
            .when(self.selected, |d| d.bg(theme.foreground.opacity(0.08)))
            .when(!self.selected, |d| d.hover(|s| s.bg(theme.foreground.opacity(0.05))))
            .children(self.children)
    }
}

