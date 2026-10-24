//! The window's own title bar: the top strips of the sidebar and the content
//! column are where the window drags from (Herse draws the title bar; AppKit
//! only draws the traffic lights over it).

use std::{cell::Cell, rc::Rc};

use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::{
    Div, InteractiveElement as _, MouseButton, ParentElement as _, Stateful, Styled as _, WindowControlArea, div, px,
    rgb,
};

/// A strip that moves the window when dragged and zooms it on a double
/// click, like a native title bar. Controls inside it should stop mouse-down
/// propagation, or a click that wobbles starts a drag.
pub fn drag_strip(id: &'static str, pending: &Rc<Cell<bool>>) -> Stateful<Div> {
    let (down, up, out) = (pending.clone(), pending.clone(), pending.clone());
    let moved = pending.clone();
    div()
        .id(id)
        .window_control_area(WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.titlebar_double_click())
        .on_mouse_down(MouseButton::Left, move |_, _, _| down.set(true))
        .on_mouse_up(MouseButton::Left, move |_, _, _| up.set(false))
        .on_mouse_down_out(move |_, _, _| out.set(false))
        .on_mouse_move(move |_, window, _| {
            if moved.replace(false) {
                window.start_window_move();
            }
        })
}

/// Stand-ins for the traffic lights, which AppKit draws and a headless
/// render doesn't: drawn when `HERSE_FAKE_TRAFFIC_LIGHTS` is set (the
/// snapshot tool sets it), so screenshots show the real layout.
pub fn fake_traffic_lights() -> Option<Div> {
    std::env::var_os("HERSE_FAKE_TRAFFIC_LIGHTS")?;
    let (x, y) = crate::theme::TRAFFIC_LIGHTS_AT;
    let light = |c: u32| div().size(px(12.)).rounded_full().bg(rgb(c));
    Some(
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .flex()
            .flex_row()
            .gap(px(8.))
            .child(light(0xff5f57))
            .child(light(0xfebc2e))
            .child(light(0x28c840)),
    )
}
