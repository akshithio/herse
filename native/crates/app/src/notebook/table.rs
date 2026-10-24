//! DataFrames drawn natively: one canvas paints the whole grid, so a
//! 60×50 frame is a few quads and shaped lines rather than three thousand
//! laid-out elements. Rows outside the visible area are skipped at paint
//! time, which keeps a 10,000-row table as cheap as a short one.

use std::sync::Arc;

use gpui_kit::{
    AnyElement, App, Bounds, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, SharedString, StatefulInteractiveElement as _, Styled as _,
    TextAlign, TextRun, Window, canvas, div, fill, font, point, prelude::FluentBuilder as _, px,
    rgba, size,
};
use herse_table::{Align, CellKind, Table};

use crate::theme;

const FONT_SIZE: f32 = 12.5;
const ROW_HEIGHT: f32 = 24.;
const CELL_PAD: f32 = 8.;
/// Average advance of the UI font at FONT_SIZE, for sizing columns from
/// character counts without measuring text.
const CHAR_WIDTH: f32 = 7.6;
const MAX_COLUMN_CHARS: u16 = 40;

/// A parsed table with its column geometry, built once per output.
pub struct TableOutput {
    pub table: Arc<Table>,
    pub column_x: Arc<[f32]>,
    pub width: f32,
}

impl TableOutput {
    pub fn new(table: Table) -> Self {
        let chars = table.column_widths_chars_capped(MAX_COLUMN_CHARS);
        let mut x = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0.;
        x.push(0.);
        for c in chars {
            // A little slack: headers are bold and some glyphs run wider.
            acc += c.max(3) as f32 * CHAR_WIDTH + CELL_PAD * 2. + 6.;
            x.push(acc);
        }
        Self {
            table: Arc::new(table),
            column_x: x.into(),
            width: acc,
        }
    }
}

struct Colors {
    fg: Hsla,
    muted: Hsla,
    header_bg: Hsla,
    stripe: Hsla,
    rule: Hsla,
    strong_rule: Hsla,
}

pub fn render(
    output: &TableOutput,
    id: impl Into<gpui_kit::ElementId>,
    available: Pixels,
    scroll: &gpui_kit::ScrollHandle,
    cx: &App,
) -> AnyElement {
    let theme = gpui_kit::component::ActiveTheme::theme(cx);
    let colors = Colors {
        fg: theme.foreground,
        muted: theme.muted_foreground,
        header_bg: theme.foreground.opacity(0.035),
        stripe: theme.foreground.opacity(0.02),
        rule: theme.foreground.opacity(0.07),
        strong_rule: theme.foreground.opacity(0.18),
    };
    let dark = theme.is_dark();
    let page = theme.background;
    let table = output.table.clone();
    let xs = output.column_x.clone();
    // polars', R's and Julia's column types: a muted row under the header.
    let dtype_rows = usize::from(table.dtypes.is_some());
    let rows = table.header.len() + dtype_rows + table.body.len();
    // Geometry is computed at zoom 1 and scaled here.
    let z = theme::zoom();
    let width = px(output.width.max(1.) * z);
    let height = px(ROW_HEIGHT * z) * rows as f32;
    let header_rows = table.header.len();

    let grid = canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| paint(&table, &xs, z, header_rows, dtype_rows, bounds, &colors, dark, window, cx),
    )
    .w(width)
    .h(height);

    let t = &output.table;
    let caption: Option<SharedString> = t.caption.clone().map(Into::into);
    let footer: Option<SharedString> = t.footer.clone().map(Into::into);
    // Julia prints its "3×2 DataFrame" line above the table; the others below.
    let footer_above = t.flavor == herse_table::Flavor::Julia;
    let (above, footer) = if footer_above { (footer, None) } else { (None, footer) };
    let fits = width <= available;
    let body: AnyElement = if fits {
        grid.into_any_element()
    } else {
        // Wide frames scroll sideways inside the cell; only these pay for a
        // scroll container, and they show a scrollbar so the hidden columns
        // are discoverable.
        div()
            .relative()
            .max_w(available)
            .pb(px(10.))
            .child(
                div()
                    .id(id)
                    .max_w(available)
                    .overflow_x_scroll()
                    .track_scroll(scroll)
                    .child(grid),
            )
            // Overlay scrollbars hide until used; a fade at the right edge
            // says there is more table out of view.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom(px(10.))
                    .right_0()
                    .w(px(36.))
                    .bg(gpui_kit::linear_gradient(
                        90.,
                        gpui_kit::linear_color_stop(page.opacity(0.), 0.),
                        gpui_kit::linear_color_stop(page, 1.),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(10.))
                    .child(gpui_kit::component::scroll::Scrollbar::horizontal(scroll)),
            )
            .into_any_element()
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .when_some(caption, |d, c| {
            d.child(div().text_size(px(12.)).text_color(theme.muted_foreground).child(c))
        })
        .when_some(above, |d, f| {
            d.child(div().text_size(px(11.5)).text_color(theme.muted_foreground).child(f))
        })
        .child(body)
        .when_some(footer, |d, f| {
            d.child(div().text_size(px(11.5)).text_color(theme.muted_foreground).child(f))
        })
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn paint(
    table: &Table,
    xs: &[f32],
    z: f32,
    header_rows: usize,
    dtype_rows: usize,
    bounds: Bounds<Pixels>,
    colors: &Colors,
    dark: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let visible = window.content_mask().bounds;
    let origin = bounds.origin;
    let row_h = px(ROW_HEIGHT * z);
    let pad = px(CELL_PAD * z);
    let font_size = px(FONT_SIZE * z);
    let total_rows = header_rows + dtype_rows + table.body.len();
    let first = (((visible.top() - origin.y) / row_h).floor().max(0.)) as usize;
    let last = ((((visible.bottom() - origin.y) / row_h).ceil()) as usize).min(total_rows);
    let regular = font(theme::UI_FONT);
    let strong = gpui_kit::Font {
        weight: FontWeight::SEMIBOLD,
        ..regular.clone()
    };
    let text_system = window.text_system().clone();

    // Header band and rules.
    let head = header_rows + dtype_rows;
    if head > 0 {
        window.paint_quad(fill(
            Bounds::new(origin, size(bounds.size.width, row_h * head as f32)),
            colors.header_bg,
        ));
    }
    for r in first..last {
        let y = origin.y + row_h * r as f32;
        let rule = if r + 1 == head { colors.strong_rule } else { colors.rule };
        window.paint_quad(fill(
            Bounds::new(point(origin.x, y + row_h - px(1.)), size(bounds.size.width, px(1.))),
            rule,
        ));
        if r >= header_rows && r < head {
            // The dtype row: small and muted, under each column's name.
            let Some(dtypes) = &table.dtypes else { continue };
            let small = px(FONT_SIZE * 0.85 * z);
            for (c, dtype) in dtypes.iter().enumerate() {
                if dtype.is_empty() || c + 1 >= xs.len() {
                    continue;
                }
                let x0 = origin.x + px(xs[c] * z);
                let x1 = origin.x + px(xs[c + 1] * z);
                let text: SharedString = dtype.clone().into();
                let run = TextRun {
                    len: text.len(),
                    font: regular.clone(),
                    color: colors.muted,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let line = text_system.shape_line(text, small, &[run], None);
                let clip = Bounds::new(point(x0 + pad, y), size((x1 - x0 - pad * 2.).max(px(0.)), row_h));
                window.with_content_mask(Some(gpui_kit::ContentMask { bounds: clip }), |window| {
                    let _ = line.paint(point(x0 + pad, y + (row_h - small) / 2. - px(1.)), small * 1.3, TextAlign::Left, None, window, cx);
                });
            }
            continue;
        }
        let is_header = r < header_rows;
        let row = if is_header { &table.header[r] } else { &table.body[r - head] };
        let body_ix = r.saturating_sub(head);
        if !is_header && body_ix % 2 == 1 {
            window.paint_quad(fill(Bounds::new(point(origin.x, y), size(bounds.size.width, row_h)), colors.stripe));
        }
        let ellipsis_row = !is_header && table.is_ellipsis_row(body_ix);
        for (c, cell) in row.iter().enumerate() {
            if cell.colspan == 0 || c + 1 >= xs.len() {
                continue; // covered by a spanning cell
            }
            let span = (cell.colspan.max(1) as usize).min(xs.len() - 1 - c);
            let x0 = origin.x + px(xs[c] * z);
            let x1 = origin.x + px(xs[c + span] * z);
            let cell_bounds = Bounds::new(point(x0, y), size(x1 - x0, row_h - px(1.)));
            if !cell_bounds.intersects(&visible) {
                continue;
            }
            let style = &cell.style;
            if let Some(bg) = style.bg {
                window.paint_quad(fill(cell_bounds, rgba(bg.to_u32())));
            }
            if let Some(bar) = &style.bar {
                let w = (x1 - x0).as_f32();
                let bar_bounds = Bounds::new(
                    point(x0 + px(w * bar.start), y + px(4.)),
                    size(px(w * (bar.end - bar.start).max(0.)), row_h - px(9.)),
                );
                window.paint_quad(fill(bar_bounds, rgba(bar.color.to_u32())));
            }
            if cell.text.is_empty() {
                continue;
            }
            let color = match (style.fg, style.bg) {
                (Some(fg), _) => rgba(fg.to_u32()).into(),
                // A coloured background without a text colour: pick contrast.
                (None, Some(bg)) => {
                    if bg.luminance() > 0.5 { gpui_kit::black() } else { gpui_kit::white() }
                }
                _ if cell.is_null() || cell.kind == CellKind::Ellipsis || ellipsis_row => colors.muted,
                _ if is_header => colors.muted,
                _ => colors.fg,
            };
            let _ = dark;
            let bold = style.bold || (is_header && !cell.is_index) || cell.is_index;
            let text: SharedString = first_line(&cell.text).into();
            let run = TextRun {
                len: text.len(),
                font: if bold { strong.clone() } else { regular.clone() },
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let mut line = text_system.shape_line(text.clone(), font_size, std::slice::from_ref(&run), None);
            let inner = (x1 - x0) - pad * 2.;
            // Wider than its column (a long header, a long string): end it in
            // an ellipsis rather than cut a letter in half.
            if line.width() > inner && inner > px(0.) {
                let ellipsis = text_system.shape_line("…".into(), font_size, &[TextRun { len: "…".len(), ..run.clone() }], None);
                let keep = line.index_for_x(inner - ellipsis.width()).unwrap_or(0);
                let mut cut = keep.min(text.len());
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let short: SharedString = format!("{}…", text[..cut].trim_end()).into();
                line = text_system.shape_line(short.clone(), font_size, &[TextRun { len: short.len(), ..run }], None);
            }
            let text_x = match cell.align {
                Align::Right => x0 + pad + (inner - line.width()).max(px(0.)),
                Align::Center => x0 + pad + ((inner - line.width()) / 2.).max(px(0.)),
                Align::Left => x0 + pad,
            };
            let clip = Bounds::new(point(x0 + pad, y), size(inner.max(px(0.)), row_h));
            window.with_content_mask(Some(gpui_kit::ContentMask { bounds: clip }), |window| {
                let _ = line.paint(point(text_x, y), row_h, TextAlign::Left, None, window, cx);
            });
        }
    }
}

fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or("")
}

/// A DataFrame's Open chip, beside its Copy chip (top-right, on hover):
/// the frame full size, sortable and searchable (see `crate::frame_view`).
pub fn open_chip(
    id: u64,
    group: SharedString,
    open: super::OpenFrame,
    cx: &gpui_kit::Context<super::NotebookView>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = gpui_kit::component::ActiveTheme::theme(&**cx);
    let open = std::rc::Rc::new(open);
    crate::ui::icon_button(("open-frame", id), "icons/maximize-2.svg", false, cx)
        .absolute()
        .top(px(2.))
        .right(px(32.))
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .opacity(0.)
        .group_hover(group, |s| s.opacity(1.))
        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Open Full Size").build(window, cx))
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.stop_propagation();
            cx.emit(super::NotebookEvent::OpenFrame(super::OpenFrame {
                table: open.table.clone(),
                cell_uid: open.cell_uid,
                output: open.output,
            }));
        }))
}
