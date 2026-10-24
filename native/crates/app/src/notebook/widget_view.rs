//! Draws a saved ipywidget ([`OutputItem::Widget`]) the way JupyterLab lays
//! it out — 28 px rows, an 80 px description column, 300 px controls — but
//! read-only: there is no kernel on the other end, so nothing reacts.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::Button,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    text::TextView,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, Div, DispatchPhase, ElementId, Entity, Focusable as _,
    Hsla, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement as _, Pixels,
    SharedString, StatefulInteractiveElement as _, StyledImage as _, Styled as _, WeakEntity, Window, canvas, div,
    img, prelude::FluentBuilder as _, px, rgb, rgba,
};
use herse_widgets::{Direction, Kind, Length, Tone, Widget};
use serde_json::{Map, Value, json};

use super::{
    NotebookView, math,
    outputs::{WidgetHtml, WidgetUi},
};
use crate::theme;

const ROW: f32 = 28.;
const LABEL: f32 = 80.;
const CONTROL: f32 = 300.;
const FONT: f32 = 13.;
/// Index offset for an Output widget's own outputs, past any cell output index.
const NESTED_BASE: usize = 1 << 20;

/// Sizes scaled by content zoom.
#[derive(Clone, Copy)]
struct Z(f32);

impl Z {
    fn px(self, v: f32) -> Pixels {
        px(v * self.0)
    }
}

struct Palette {
    fg: Hsla,
    muted: Hsla,
    border: Hsla,
    field: Hsla,
    track: Hsla,
    button: Hsla,
    selection: Hsla,
    dark: bool,
}

impl Palette {
    fn tone(&self, tone: Tone) -> Option<Hsla> {
        // JupyterLab's --jp-brand/success/info/warn/error-color1.
        let (light, dark) = match tone {
            Tone::Default => return None,
            Tone::Primary => (0x1976d2, 0x42a5f5),
            Tone::Success => (0x388e3c, 0x66bb6a),
            Tone::Info => (0x0097a7, 0x26c6da),
            Tone::Warning => (0xf57c00, 0xffa726),
            Tone::Danger => (0xd32f2f, 0xef5350),
        };
        Some(rgb(if self.dark { dark } else { light }).into())
    }

    fn brand(&self) -> Hsla {
        self.tone(Tone::Primary).unwrap_or(self.fg)
    }
}

fn css_color(v: &str) -> Option<Hsla> {
    herse_table::parse_css_color(v).map(|c| rgba(c.to_u32()).into())
}

/// What a control may do when used: change its model (always for view-only
/// state like a Tab's page; for values only when the running kernel holds
/// the model) and send that to the kernel.
#[derive(Clone)]
struct Act {
    view: WeakEntity<NotebookView>,
    live: bool,
    /// Element-id prefix unique to this output.
    base: u64,
    /// The text field being typed in, when this widget has one open.
    input: Option<Entity<InputState>>,
}

impl Act {
    fn id(&self, key: u32, i: usize) -> ElementId {
        ("widget-ctl", self.base | ((key as u64 & 0xffff) << 8) | (i as u64 & 0xff)).into()
    }

    /// A click handler that applies `update` to the model.
    fn set(&self, model: &str, update: Value) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        let (view, model) = (self.view.clone(), model.to_string());
        let update = update.as_object().cloned().unwrap_or_default();
        move |_, _, cx| {
            view.update(cx, |nb, cx| nb.widget_input(&model, update.clone(), cx)).ok();
        }
    }
}

impl NotebookView {
    /// A change made to a widget here: applied at once, and sent to the
    /// kernel when it holds the model (it runs the widget's callbacks).
    fn widget_input(&mut self, model: &str, update: Map<String, Value>, cx: &mut Context<Self>) {
        let live = self.widget_state.as_ref().is_some_and(|s| s.is_live(model));
        self.widget_state_mut().set_local(model, &update);
        if live && let Some(session) = self.session.as_ref() {
            let msg = session.kernel.send_shell(
                "comm_msg",
                json!({"comm_id": model, "data": {"method": "update", "state": update, "buffer_paths": []}}),
            );
            self.widget_state_mut().note_sent(model, &update, &msg);
            self.widgets_dirty = true;
        }
        self.invalidate_widgets(&std::iter::once(model.to_string()).collect());
        cx.notify();
    }

    /// A button press: a `custom` message, as ipywidgets' frontend sends.
    fn widget_click(&mut self, model: &str) {
        if self.widget_state.as_ref().is_some_and(|s| s.is_live(model))
            && let Some(session) = self.session.as_ref()
        {
            session.kernel.send_shell(
                "comm_msg",
                json!({"comm_id": model, "data": {"method": "custom", "content": {"event": "click"}}}),
            );
        }
    }

    /// Clicked a live text field: type in it. Changes go to the kernel as
    /// they're made when the model asks for `continuous_update`, else on
    /// Enter or when the field loses focus (which also closes it).
    fn start_widget_text(&mut self, model: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.widget_state.clone() else { return };
        let name = state.model_name(model).unwrap_or_default().to_string();
        let value = match state.get(model, "value") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let continuous = state.get(model, "continuous_update").and_then(Value::as_bool).unwrap_or(false);
        let placeholder = state.get(model, "placeholder").and_then(Value::as_str).unwrap_or_default().to_string();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(value)
                .placeholder(placeholder.trim_start_matches('\u{200b}').to_string())
                .masked(name == "PasswordModel")
        });
        let model_id = model.to_string();
        let subscription = cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, _, cx| {
            let send = match event {
                InputEvent::Change => continuous,
                InputEvent::PressEnter { .. } | InputEvent::Blur => true,
                InputEvent::Focus => false,
            };
            if send {
                let text = input.read(cx).value().to_string();
                let value = match name.as_str() {
                    "IntTextModel" | "BoundedIntTextModel" => text.trim().parse::<i64>().ok().map(Value::from),
                    "FloatTextModel" | "BoundedFloatTextModel" => {
                        text.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number)
                    }
                    _ => Some(Value::String(text)),
                };
                let unchanged = this.widget_state.as_ref().and_then(|s| s.get(&model_id, "value")) == value.as_ref();
                if let Some(value) = value.filter(|_| !unchanged) {
                    let mut update = Map::new();
                    update.insert("value".into(), value);
                    this.widget_input(&model_id, update, cx);
                }
            }
            if matches!(event, InputEvent::Blur) {
                this.widget_inputs.remove(&model_id);
                cx.notify();
            }
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.widget_inputs.insert(model.to_string(), (input, subscription));
        cx.notify();
    }

    fn widget_slide(&mut self, model: &str, fraction: f64, cx: &mut Context<Self>) {
        let update = self.widget_state.as_ref().and_then(|s| s.slider_update(model, fraction));
        if let Some(update) = update {
            self.widget_input(model, update, cx);
        }
    }
}

impl NotebookView {
    pub(super) fn render_widget(
        &mut self,
        ix: usize,
        n: usize,
        ui: &Rc<WidgetUi>,
        dark: bool,
        pad: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = ui.root.clone();
        let act = Act {
            view: cx.entity().downgrade(),
            live: false,
            base: ((self.cells[ix].uid & 0xff_ffff) << 32) | ((n as u64 & 0xff) << 24),
            input: None,
        };
        let body = self.widget(ix, n, ui, &root, dark, Z(theme::zoom()), &act, cx);
        div().px(pad).max_w_full().overflow_hidden().child(body).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn widget(
        &mut self,
        ix: usize,
        n: usize,
        ui: &Rc<WidgetUi>,
        w: &Widget,
        dark: bool,
        z: Z,
        act: &Act,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if w.layout.hidden {
            return div().into_any_element();
        }
        let act = &Act {
            live: self.widget_state.as_ref().is_some_and(|s| s.is_live(&w.model_id)),
            input: self.widget_inputs.get(&w.model_id).map(|(i, _)| i.clone()),
            ..act.clone()
        };
        let t = cx.theme();
        let p = Palette {
            fg: t.foreground,
            muted: t.muted_foreground,
            border: t.border,
            field: if dark { t.foreground.opacity(0.06) } else { t.background },
            track: t.foreground.opacity(if dark { 0.16 } else { 0.1 }),
            button: t.foreground.opacity(if dark { 0.12 } else { 0.07 }),
            selection: t.selection,
            dark,
        };
        let el: AnyElement = match &w.kind {
            Kind::Box { direction, wrap, children } => {
                let mut row = match direction {
                    Direction::Row => div().flex().flex_row().items_center().gap(z.px(4.)).when(*wrap, |d| d.flex_wrap()),
                    Direction::Column => div().flex().flex_col().gap(z.px(4.)),
                    Direction::Grid => div()
                        .grid()
                        .grid_cols(w.layout.grid_columns.unwrap_or(1).clamp(1, 24) as u16)
                        .gap(z.px(4.)),
                };
                for child in children {
                    let flex = child.layout.flex;
                    let el = self.widget(ix, n, ui, child, dark, z, act, cx);
                    row = row.child(match flex {
                        Some(f) if f > 0. && *direction == Direction::Row => div().flex_grow(f).flex_basis(px(0.)).min_w(px(0.)).child(el),
                        _ => div().flex_shrink_0().child(el),
                    });
                }
                row.into_any_element()
            }
            Kind::Tabs { titles, selected, children } => {
                let mut header = div().flex().flex_row().border_b_1().border_color(p.border);
                for (i, title) in titles.iter().enumerate() {
                    let on = Some(i) == *selected;
                    header = header.child(
                        div()
                            .id(act.id(w.key, i))
                            .cursor_pointer()
                            .on_click(act.set(&w.model_id, json!({"selected_index": i})))
                            .px(z.px(12.))
                            .h(z.px(ROW))
                            .flex()
                            .items_center()
                            .text_color(if on { p.fg } else { p.muted })
                            .when(on, |d| {
                                d.border_t_2()
                                    .border_color(p.brand())
                                    .bg(p.field)
                                    .border_l_1()
                                    .border_r_1()
                            })
                            .child(if title.is_empty() { SharedString::from(format!("Tab {}", i + 1)) } else { title.clone().into() }),
                    );
                }
                let body = selected
                    .and_then(|i| children.get(i))
                    .map(|c| self.widget(ix, n, ui, c, dark, z, act, cx));
                div()
                    .flex()
                    .flex_col()
                    .child(header)
                    .when_some(body, |d, b| {
                        d.child(div().p(z.px(8.)).border_1().border_t_0().border_color(p.border).child(b))
                    })
                    .into_any_element()
            }
            Kind::Accordion { titles, selected, children } => {
                let mut col = div().flex().flex_col().border_1().border_color(p.border).rounded(px(2.));
                for (i, child) in children.iter().enumerate() {
                    let open = Some(i) == *selected;
                    let title = titles.get(i).filter(|t| !t.is_empty()).cloned().unwrap_or_else(|| format!("Section {}", i + 1));
                    col = col.child(
                        div()
                            .id(act.id(w.key, i))
                            .cursor_pointer()
                            .on_click(act.set(&w.model_id, json!({"selected_index": if open { Value::Null } else { Value::from(i) }})))
                            .h(z.px(ROW + 4.))
                            .px(z.px(10.))
                            .flex()
                            .items_center()
                            .gap(z.px(6.))
                            .bg(p.button)
                            .when(i > 0, |d| d.border_t_1().border_color(p.border))
                            .child(div().text_color(p.muted).child(if open { "▾" } else { "▸" }))
                            .child(title),
                    );
                    if open {
                        let body = self.widget(ix, n, ui, child, dark, z, act, cx);
                        col = col.child(div().p(z.px(8.)).border_t_1().border_color(p.border).child(body));
                    }
                }
                col.into_any_element()
            }
            Kind::Stack { child } => match child {
                Some(c) => self.widget(ix, n, ui, c, dark, z, act, cx),
                None => div().into_any_element(),
            },
            Kind::Output { .. } => {
                let items = ui.outputs.get(&w.key).map(|v| v.as_slice()).unwrap_or_default();
                let mut col = div().flex().flex_col().gap(z.px(4.)).min_w(px(0.));
                for (k, item) in items.iter().enumerate() {
                    let nested = NESTED_BASE + n * 4096 + (w.key as usize) * 64 + k;
                    col = col.child(self.render_output_item(ix, nested, item, dark, true, px(0.), cx));
                }
                col.when(w.layout.border, |d| d.border_1().border_color(p.border).p(z.px(4.)))
                    .into_any_element()
            }
            _ => {
                if let Some((image, _, _)) = ui.images.get(&w.key) {
                    crate::images::touch(image, cx);
                }
                leaf(w, ui, &p, z, act)
            }
        };
        let width = match w.layout.width {
            Some(Length::Px(v)) => Some(z.px(v)),
            _ => None,
        };
        let percent = matches!(w.layout.width, Some(Length::Percent(_)));
        div()
            .when_some(width, |d, wd| d.w(wd))
            .when(percent, |d| d.w_full())
            .when(w.disabled, |d| d.opacity(0.6))
            .child(el)
            .into_any_element()
    }
}

/// A control with no children.
fn leaf(w: &Widget, ui: &WidgetUi, p: &Palette, z: Z, act: &Act) -> AnyElement {
    let live = act.live;
    let label_w = match w.description_width {
        Some(Length::Px(v)) => z.px(v),
        _ => z.px(LABEL),
    };
    // The control's width when the layout doesn't set one: what's left of
    // the 300 px row after the description.
    let has_label = !w.description.is_empty();
    // A flex child or a percentage width fills what its box gives it.
    let stretch = w.layout.flex.is_some() || matches!(w.layout.width, Some(Length::Percent(_)));
    let control_w = match w.layout.width {
        Some(Length::Px(v)) => z.px(v) - if has_label { label_w + z.px(8.) } else { px(0.) },
        _ => z.px(CONTROL) - if has_label { label_w + z.px(8.) } else { px(0.) },
    }
    .max(z.px(40.));
    let label = || {
        div()
            .w(label_w)
            .flex_shrink_0()
            .h(z.px(ROW))
            .flex()
            .items_center()
            .justify_end()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_color(p.fg)
            .child(w.description.clone())
    };
    let row = |control: AnyElement| -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(z.px(8.))
            .text_size(z.px(FONT))
            .when(has_label, |d| d.child(label()))
            .map(|d| if stretch { d.child(div().flex_1().min_w(px(0.)).child(control)) } else { d.child(control) })
            .into_any_element()
    };
    let field = |content: Div| {
        content
            .h(z.px(ROW))
            .w(control_w)
            .px(z.px(6.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(p.field)
            .border_1()
            .border_color(p.border)
            .rounded(px(2.))
    };
    let button = |text: SharedString, icon: &str, fill: Option<Hsla>, pressed: bool, width: Option<Pixels>| {
        let glyph = icon_glyph(icon);
        div()
            .h(z.px(ROW))
            .when_some(width, |d, wd| d.w(wd))
            .when(width.is_none(), |d| d.px(z.px(12.)))
            .flex()
            .items_center()
            .justify_center()
            .gap(z.px(6.))
            .overflow_hidden()
            .whitespace_nowrap()
            .rounded(px(2.))
            .text_size(z.px(FONT))
            .map(|d| match fill {
                Some(c) => d
                    .bg(if pressed { c.blend(gpui_kit::black().opacity(0.18)) } else { c })
                    .text_color(readable_on(c)),
                None => d
                    .bg(if pressed { p.button.opacity(2.2) } else { p.button })
                    .text_color(p.fg),
            })
            .when(pressed, |d| d.border_1().border_color(p.fg.opacity(0.25)))
            .when_some(glyph, |d, g| d.child(g))
            .child(text)
    };
    match &w.kind {
        Kind::Progress { fraction, tone, color, vertical } => {
            let fill = color.as_deref().and_then(css_color).or_else(|| p.tone(*tone)).unwrap_or(p.brand());
            let bar = if *vertical {
                let h = z.px(120.);
                div()
                    .w(z.px(16.))
                    .h(h)
                    .flex()
                    .flex_col()
                    .justify_end()
                    .rounded(px(2.))
                    .bg(p.track)
                    .overflow_hidden()
                    .child(div().w_full().h(h * *fraction as f32).bg(fill))
            } else {
                div()
                    .map(|d| if stretch { d.w_full() } else { d.w(control_w) })
                    .h(z.px(18.))
                    .rounded(px(2.))
                    .bg(p.track)
                    .overflow_hidden()
                    .child(div().h_full().w(gpui_kit::relative(*fraction as f32)).bg(fill))
            };
            row(bar.into_any_element())
        }
        Kind::Slider { start, end, range, readout, vertical, color } => {
            let fill = color.as_deref().and_then(css_color).unwrap_or(p.brand());
            let knob = || {
                div()
                    .size(z.px(14.))
                    .rounded_full()
                    .bg(if p.dark { p.fg } else { gpui_kit::white() })
                    .border_1()
                    .border_color(p.fg.opacity(0.35))
            };
            let readout_el = readout.clone().map(|r| {
                div().text_color(p.fg).whitespace_nowrap().child(r)
            });
            if *vertical {
                let h = z.px(120.);
                let (lo, hi) = (if *range { *start } else { 0. }, *end);
                let track = div()
                    .relative()
                    .w(z.px(14.))
                    .h(h)
                    .child(div().absolute().left(z.px(5.)).top_0().w(z.px(4.)).h(h).rounded_full().bg(p.track))
                    .child(
                        div()
                            .absolute()
                            .left(z.px(5.))
                            .top(h * (1. - hi as f32))
                            .w(z.px(4.))
                            .h(h * (hi - lo) as f32)
                            .bg(fill),
                    )
                    .child(knob().absolute().left_0().top(h * (1. - hi as f32) - z.px(7.)))
                    .when(*range, |d| d.child(knob().absolute().left_0().top(h * (1. - lo as f32) - z.px(7.))));
                let col = div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(z.px(6.))
                    .text_size(z.px(FONT))
                    .when(!w.description.is_empty(), |d| d.child(div().text_color(p.fg).child(w.description.clone())))
                    .child(track)
                    .when_some(readout_el, |d, r| d.child(r));
                return col.into_any_element();
            }
            let readout_w = if readout.is_some() { z.px(if *range { 90. } else { 50. }) } else { px(0.) };
            let track_w = (control_w - readout_w).max(z.px(40.));
            let x = |f: f64| track_w * f as f32;
            let track = div()
                .id(act.id(w.key, 0))
                .relative()
                .w(track_w)
                .h(z.px(ROW))
                .when(live, |d| d.cursor_pointer().child(slider_drag(act, &w.model_id)))
                .child(div().absolute().top(z.px(12.)).left_0().w(track_w).h(z.px(4.)).rounded_full().bg(p.track))
                .child(
                    div()
                        .absolute()
                        .top(z.px(12.))
                        .left(x(*start))
                        .w(x(end - start))
                        .h(z.px(4.))
                        .bg(fill),
                )
                .when(*range, |d| d.child(knob().absolute().top(z.px(7.)).left(x(*start) - z.px(7.))))
                .child(knob().absolute().top(z.px(7.)).left(x(*end) - z.px(7.)));
            row(div()
                .flex()
                .flex_row()
                .items_center()
                .gap(z.px(10.))
                .child(div().px(z.px(7.)).child(track))
                .when_some(readout_el, |d, r| d.child(r))
                .into_any_element())
        }
        Kind::Text { multiline: false, .. } if act.input.is_some() => {
            let input = act.input.clone().expect("checked");
            row(div()
                .w(control_w)
                .h(z.px(ROW))
                .text_size(z.px(FONT))
                .child(Input::new(&input).small())
                .into_any_element())
        }
        Kind::Text { value, placeholder, multiline } => {
            let empty = value.is_empty();
            let shown: SharedString = if empty { placeholder.clone().into() } else { value.clone().into() };
            if *multiline {
                let lines = value.lines().count().clamp(2, 12) as f32;
                let el = div()
                    .w(control_w)
                    .min_h(z.px(20. * lines + 8.))
                    .px(z.px(6.))
                    .py(z.px(4.))
                    .bg(p.field)
                    .border_1()
                    .border_color(p.border)
                    .rounded(px(2.))
                    .text_color(if empty { p.muted } else { p.fg })
                    .child(shown);
                return row(el.into_any_element());
            }
            let (view, model) = (act.view.clone(), w.model_id.clone());
            row(field(div().text_color(if empty { p.muted } else { p.fg }).child(shown))
                .id(act.id(w.key, 0))
                .when(live, |d| {
                    d.cursor_text().on_click(move |_, window, cx| {
                        view.update(cx, |nb, cx| nb.start_widget_text(&model, window, cx)).ok();
                    })
                })
                .into_any_element())
        }
        Kind::Checkbox { value, indent } => {
            let bx = div()
                .size(z.px(14.))
                .flex_shrink_0()
                .rounded(px(2.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(z.px(11.))
                .map(|d| {
                    if *value {
                        d.bg(p.brand()).text_color(gpui_kit::white()).child("✓")
                    } else {
                        d.border_1().border_color(p.fg.opacity(0.4)).bg(p.field)
                    }
                });
            let toggle = div()
                .id(act.id(w.key, 0))
                .flex()
                .flex_row()
                .items_center()
                .gap(z.px(8.))
                .when(live, |d| d.cursor_pointer().on_click(act.set(&w.model_id, json!({"value": !*value}))))
                .child(bx)
                .child(div().text_color(p.fg).child(w.description.clone()));
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(z.px(8.))
                .h(z.px(ROW))
                .text_size(z.px(FONT))
                .when(*indent, |d| d.child(div().w(label_w).flex_shrink_0()))
                .child(toggle)
                .into_any_element()
        }
        // In a flex row, so a button is as wide as its label, not its column.
        Kind::ToggleButton { label, value, tone, icon } => div()
            .flex()
            .child(
                button(label.clone().into(), icon, p.tone(*tone), *value, None)
                    .id(act.id(w.key, 0))
                    .when(live, |d| d.cursor_pointer().on_click(act.set(&w.model_id, json!({"value": !*value})))),
            )
            .into_any_element(),
        Kind::Button { label, tone, icon, color } => {
            let fill = color.as_deref().and_then(css_color).or_else(|| p.tone(*tone));
            let (view, model) = (act.view.clone(), w.model_id.clone());
            div()
                .flex()
                .child(
                    button(label.clone().into(), icon, fill, false, None)
                        .id(act.id(w.key, 0))
                        .when(live, |d| {
                            d.cursor_pointer()
                                .active(|s| s.opacity(0.8))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |nb, _| nb.widget_click(&model)).ok();
                                })
                        }),
                )
                .into_any_element()
        }
        Kind::Valid { value, readout } => {
            let (glyph, color) = if *value {
                ("✓", p.tone(Tone::Success).unwrap_or(p.fg))
            } else {
                ("✗", p.tone(Tone::Danger).unwrap_or(p.fg))
            };
            row(div()
                .flex()
                .flex_row()
                .items_center()
                .gap(z.px(6.))
                .child(div().text_color(color).child(glyph))
                .when(!*value && !readout.is_empty(), |d| d.child(div().text_color(color).child(readout.clone())))
                .into_any_element())
        }
        Kind::Dropdown { options, selected } => {
            let label: SharedString = selected.and_then(|i| options.get(i)).cloned().unwrap_or_default().into();
            if !live {
                return row(field(
                    div()
                        .justify_between()
                        .child(div().text_color(p.fg).overflow_hidden().child(label))
                        .child(div().text_color(p.muted).child("▾")),
                )
                .into_any_element());
            }
            let (options, selected) = (options.clone(), *selected);
            let act = act.clone();
            let model = w.model_id.clone();
            let trigger = Button::new(act.id(w.key, 0))
                .outline()
                .compact()
                .w(control_w)
                .h(z.px(ROW))
                .text_size(z.px(FONT))
                // Label left, caret right, like a <select>.
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().overflow_hidden().whitespace_nowrap().child(label))
                        .child(div().text_color(p.muted).child("▾")),
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for (i, opt) in options.iter().enumerate() {
                        menu = menu.item(
                            PopupMenuItem::new(opt.clone())
                                .checked(selected == Some(i))
                                .on_click(act.set(&model, json!({"index": i}))),
                        );
                    }
                    menu
                });
            row(trigger.into_any_element())
        }
        Kind::Radio { options, selected } => {
            let mut col = div().flex().flex_col().gap(z.px(2.));
            for (i, opt) in options.iter().enumerate() {
                let on = Some(i) == *selected;
                col = col.child(
                    div()
                        .id(act.id(w.key, i))
                        .when(live, |d| d.cursor_pointer().on_click(act.set(&w.model_id, json!({"index": i}))))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(z.px(6.))
                        .h(z.px(20.))
                        .child(
                            div()
                                .size(z.px(13.))
                                .rounded_full()
                                .border_1()
                                .border_color(if on { p.brand() } else { p.fg.opacity(0.4) })
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(on, |d| d.child(div().size(z.px(7.)).rounded_full().bg(p.brand()))),
                        )
                        .child(div().text_color(p.fg).child(opt.clone())),
                );
            }
            div()
                .flex()
                .flex_row()
                .gap(z.px(8.))
                .text_size(z.px(FONT))
                .when(has_label, |d| d.child(label().h(z.px(20.))))
                .child(col)
                .into_any_element()
        }
        Kind::Select { options, selected, rows, multiple } => {
            let shown = (*rows).min(options.len().max(1));
            let mut list = div()
                .w(control_w)
                .flex()
                .flex_col()
                .py(z.px(2.))
                .bg(p.field)
                .border_1()
                .border_color(p.border)
                .rounded(px(2.))
                .overflow_hidden();
            for (i, opt) in options.iter().take(shown).enumerate() {
                let on = selected.contains(&i);
                let update = if *multiple {
                    let mut next: Vec<usize> = selected.iter().copied().filter(|&j| j != i).collect();
                    if !on {
                        next.push(i);
                        next.sort_unstable();
                    }
                    json!({"index": next})
                } else {
                    json!({"index": i})
                };
                list = list.child(
                    div()
                        .id(act.id(w.key, i))
                        .when(live, |d| d.cursor_pointer().on_click(act.set(&w.model_id, update)))
                        .h(z.px(20.))
                        .px(z.px(6.))
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .text_color(p.fg)
                        .when(on, |d| d.bg(p.selection))
                        .child(opt.clone()),
                );
            }
            if options.len() > shown {
                list = list.child(
                    div()
                        .px(z.px(6.))
                        .text_size(z.px(11.))
                        .text_color(p.muted)
                        .child(format!("… {} more", options.len() - shown)),
                );
            }
            div()
                .flex()
                .flex_row()
                .gap(z.px(8.))
                .text_size(z.px(FONT))
                .when(has_label, |d| d.child(label()))
                .child(list)
                .into_any_element()
        }
        Kind::ToggleButtons { options, selected, tone } => {
            let mut group = div().flex().flex_row().flex_wrap().gap(z.px(2.));
            for (i, opt) in options.iter().enumerate() {
                let on = Some(i) == *selected;
                group = group.child(
                    button(opt.clone().into(), "", p.tone(*tone), on, Some(z.px(96.)))
                        .id(act.id(w.key, i))
                        .when(live, |d| d.cursor_pointer().on_click(act.set(&w.model_id, json!({"index": i})))),
                );
            }
            row(group.into_any_element())
        }
        Kind::Label { text } => row(div()
            .min_h(z.px(ROW))
            .flex()
            .items_center()
            .text_color(p.fg)
            .child(text.clone())
            .into_any_element()),
        Kind::Html { .. } => {
            let content: AnyElement = match ui.html.get(&w.key) {
                Some(WidgetHtml::Plain(text)) => div()
                    .min_h(z.px(ROW))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .text_color(p.fg)
                    .child(text.clone())
                    .into_any_element(),
                Some(WidgetHtml::Rich(state)) => div()
                    .text_size(z.px(FONT))
                    .child(TextView::new(state).markdown_extensions(math::extensions()))
                    .into_any_element(),
                None => div().into_any_element(),
            };
            row(content)
        }
        Kind::Image { .. } => match ui.images.get(&w.key) {
            Some((image, iw, ih)) => img(image.clone())
                .w(z.px(*iw))
                .max_w_full()
                .aspect_ratio(iw / ih)
                .object_fit(ObjectFit::Contain)
                .with_fallback(super::broken_image(p.muted))
                .into_any_element(),
            None => div().into_any_element(),
        },
        Kind::ColorPicker { value, concise } => {
            let swatch = div()
                .size(z.px(ROW - 6.))
                .flex_shrink_0()
                .rounded(px(2.))
                .border_1()
                .border_color(p.border)
                .when_some(css_color(value), |d, c| d.bg(c));
            let el = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(z.px(4.))
                .when(!*concise, |d| {
                    d.child(field(div().text_color(p.fg).child(value.clone())).w(control_w - z.px(ROW)))
                })
                .child(swatch);
            row(el.into_any_element())
        }
        Kind::DateTime { value } => {
            let empty = value.is_none();
            row(field(
                div()
                    .text_color(if empty { p.muted } else { p.fg })
                    .child(value.clone().unwrap_or_else(|| "yyyy-mm-dd".into())),
            )
            .into_any_element())
        }
        Kind::Play { playing, .. } => {
            let mut group = div().flex().flex_row().gap(z.px(2.));
            for (glyph, on) in [("▶", *playing), ("❚❚", false), ("■", !*playing), ("↻", false)] {
                group = group.child(button(glyph.into(), "", None, on, Some(z.px(36.))));
            }
            row(group.into_any_element())
        }
        Kind::Unsupported { model, .. } => div()
            .text_size(z.px(12.))
            .italic()
            .text_color(p.muted)
            .child(format!("{} widget — shown live only in a browser", model.trim_end_matches("Model")))
            .into_any_element(),
        // Containers and Output are drawn by `NotebookView::widget`.
        _ => div().into_any_element(),
    }
}

/// Dragging a live slider: an overlay that records the track's bounds, starts
/// a drag on mouse down, and follows the mouse anywhere in the window until
/// it's released (as a browser range input does). The drag itself lives on
/// the view, since every move redraws this.
fn slider_drag(act: &Act, model: &str) -> AnyElement {
    let (view, model) = (act.view.clone(), model.to_string());
    let paint = move |b: Bounds<Pixels>, _: (), window: &mut Window, _: &mut App| {
        let fraction = move |x: Pixels| {
            if b.size.width <= px(0.) { 0. } else { ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64 }
        };
        let (v, m) = (view.clone(), model.clone());
        window.on_mouse_event(move |e: &MouseDownEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && b.contains(&e.position) {
                v.update(cx, |nb, cx| {
                    nb.widget_drag = Some(m.clone());
                    nb.widget_slide(&m, fraction(e.position.x), cx);
                })
                .ok();
            }
        });
        let (v, m) = (view.clone(), model.clone());
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            v.update(cx, |nb, cx| {
                if nb.widget_drag.as_deref() != Some(m.as_str()) {
                    return;
                }
                if e.pressed_button == Some(MouseButton::Left) {
                    nb.widget_slide(&m, fraction(e.position.x), cx);
                } else {
                    nb.widget_drag = None;
                }
            })
            .ok();
        });
        let (v, m) = (view.clone(), model.clone());
        window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
            v.update(cx, |nb, _| {
                if nb.widget_drag.as_deref() == Some(m.as_str()) {
                    nb.widget_drag = None;
                }
            })
            .ok();
        });
    };
    canvas(|_, _, _| {}, paint).absolute().size_full().into_any_element()
}

/// Black or white text, whichever contrasts more with `bg` (WCAG luminance).
fn readable_on(bg: Hsla) -> Hsla {
    let c = gpui_kit::Rgba::from(bg);
    let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let luminance = 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
    // Jupyter puts white text on its mid-tone buttons, so switch to black
    // only for clearly light fills (lightgreen, the dark theme's orange).
    if luminance > 0.4 { gpui_kit::black() } else { gpui_kit::white() }
}

/// The handful of Font Awesome icon names widgets commonly use.
fn icon_glyph(name: &str) -> Option<&'static str> {
    let name = name.trim_start_matches("fa-");
    Some(match name {
        "" => return None,
        "check" | "check-circle" => "✓",
        "times" | "close" | "xmark" | "remove" => "✕",
        "plus" | "plus-circle" => "+",
        "minus" => "−",
        "play" => "▶",
        "pause" => "❚❚",
        "stop" => "■",
        "refresh" | "redo" | "repeat" | "sync" | "rotate-right" => "↻",
        "upload" => "⤒",
        "download" => "⤓",
        "search" => "⌕",
        "arrow-right" => "→",
        "arrow-left" => "←",
        "arrow-up" => "↑",
        "arrow-down" => "↓",
        "star" => "★",
        "heart" => "♥",
        "save" | "floppy-disk" => "💾",
        "trash" | "trash-alt" | "trash-can" => "🗑",
        "cog" | "gear" => "⚙",
        "info" | "info-circle" => "ⓘ",
        "warning" | "exclamation-triangle" => "⚠",
        _ => return None,
    })
}
