//! Matplotlib animations (`HTML(anim.to_jshtml())`): the output is a page
//! of JavaScript around the frames as embedded PNGs. Herse takes the frames,
//! the interval and the loop mode out of it and plays them natively with the
//! same controls.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyElement, Bounds, Context, Image, ImageFormat, InteractiveElement as _, IntoElement, MouseButton, ObjectFit,
    ParentElement as _, Pixels, StatefulInteractiveElement as _, StyledImage as _, Styled as _, Task, canvas, div,
    img, prelude::FluentBuilder as _, px,
};

use super::NotebookView;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopMode {
    Once,
    Loop,
    /// Back and forth.
    Reflect,
}

pub struct Animation {
    pub frames: Vec<Arc<Image>>,
    pub interval: Duration,
    pub width: f32,
    pub height: f32,
    state: RefCell<State>,
}

struct State {
    frame: usize,
    /// +1 forward, -1 backward (reflect, or the reverse-play button).
    direction: isize,
    mode: LoopMode,
    playing: Option<Task<()>>,
    track: Bounds<Pixels>,
}

/// The animation in a `to_jshtml()` output, if this is one.
pub fn parse(html: &str) -> Option<Animation> {
    if !html.contains("new Animation(frames") {
        return None;
    }
    let mut frames = Vec::new();
    let mut size = None;
    let mut rest = html;
    while let Some(at) = rest.find("frames[") {
        rest = &rest[at + 7..];
        let Some(open) = rest.find("] = \"") else { break };
        if !rest[..open].bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let body = &rest[open + 5..];
        let Some(close) = body.find('"') else { break };
        let uri = &body[..close];
        rest = &body[close..];
        let Some((mime, data)) = uri.strip_prefix("data:image/").and_then(|u| u.split_once(";base64,")) else {
            // Frames written to files (`embed_frames=False`) aren't in the notebook.
            return None;
        };
        // The JavaScript string continues lines with a backslash.
        let b64: String = data.chars().filter(|c| !c.is_ascii_whitespace() && *c != '\\').collect();
        let bytes = super::outputs::decode_base64(&b64)?;
        let format = match mime {
            "png" => ImageFormat::Png,
            "jpeg" | "jpg" => ImageFormat::Jpeg,
            "gif" => ImageFormat::Gif,
            "svg+xml" => ImageFormat::Svg,
            _ => return None,
        };
        if size.is_none() {
            size = imagesize::blob_size(&bytes).ok().map(|s| (s.width as f32, s.height as f32));
        }
        frames.push(Arc::new(Image::from_bytes(format, bytes)));
    }
    let (width, height) = size?;
    if frames.is_empty() || width <= 0. || height <= 0. {
        return None;
    }
    // new Animation(frames, img_id, slider_id, 80.0, loop_select_id)
    let interval = html
        .find("new Animation(frames")
        .and_then(|i| html[i..].split(',').nth(3))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.)
        .unwrap_or(200.);
    Some(Animation {
        frames,
        interval: Duration::from_secs_f64((interval / 1000.).clamp(0.01, 10.)),
        width,
        height,
        state: RefCell::new(State {
            frame: 0,
            direction: 1,
            mode: loop_mode(html),
            playing: None,
            track: Bounds::default(),
        }),
    })
}

/// The checked radio of the "Repetition mode" form.
fn loop_mode(html: &str) -> LoopMode {
    let mut rest = html;
    while let Some(at) = rest.find("<input type=\"radio\"") {
        rest = &rest[at..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if tag.contains("checked") {
            if tag.contains("value=\"once\"") {
                return LoopMode::Once;
            }
            if tag.contains("value=\"reflect\"") {
                return LoopMode::Reflect;
            }
            return LoopMode::Loop;
        }
        rest = &rest[end..];
    }
    LoopMode::Loop
}

impl Animation {
    pub fn current(&self) -> usize {
        self.state.borrow().frame
    }

    fn is_playing(&self) -> bool {
        self.state.borrow().playing.is_some()
    }

    /// Advance one frame in the play direction; false when playback ends.
    fn tick(&self) -> bool {
        let n = self.frames.len();
        let mut s = self.state.borrow_mut();
        let next = s.frame as isize + s.direction;
        if (0..n as isize).contains(&next) {
            s.frame = next as usize;
            return true;
        }
        match s.mode {
            LoopMode::Once => false,
            LoopMode::Loop => {
                s.frame = if s.direction > 0 { 0 } else { n - 1 };
                true
            }
            LoopMode::Reflect => {
                s.direction = -s.direction;
                s.frame = (s.frame as isize + s.direction).clamp(0, n as isize - 1) as usize;
                true
            }
        }
    }

    fn seek(&self, frame: usize) {
        self.state.borrow_mut().frame = frame.min(self.frames.len() - 1);
    }

    fn stop(&self) {
        self.state.borrow_mut().playing = None;
    }
}

impl NotebookView {
    fn play(&mut self, anim: &Rc<Animation>, direction: isize, cx: &mut Context<Self>) {
        {
            let mut s = anim.state.borrow_mut();
            s.direction = direction;
            // Playing forward from the last frame starts over (and backward from the first).
            let n = anim.frames.len();
            if s.mode == LoopMode::Once && ((direction > 0 && s.frame + 1 == n) || (direction < 0 && s.frame == 0)) {
                s.frame = if direction > 0 { 0 } else { n - 1 };
            }
        }
        let weak: Weak<Animation> = Rc::downgrade(anim);
        let interval = anim.interval;
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let Some(anim) = weak.upgrade() else { return };
                let more = anim.tick();
                if !more {
                    anim.stop();
                }
                if this.update(cx, |_, cx| cx.notify()).is_err() || !more {
                    return;
                }
            }
        });
        anim.state.borrow_mut().playing = Some(task);
        cx.notify();
    }

    pub(super) fn render_animation(&mut self, uid: u64, n: usize, anim: &Rc<Animation>, pad: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let z = theme::zoom();
        let count = anim.frames.len();
        let frame = anim.current().min(count - 1);
        let playing = anim.is_playing();
        let mode = anim.state.borrow().mode;
        let id = uid * 1000 + n as u64;
        let fg = theme.foreground;
        let muted = theme.muted_foreground;
        let track_bg = theme.foreground.opacity(0.12);
        let accent = theme.foreground.opacity(0.55);
        let button = |key: &'static str, glyph: &'static str, on: bool| {
            div()
                .id((key, id))
                .w(px(30.))
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if on { fg } else { muted })
                .when(on, |d| d.bg(fg.opacity(0.1)))
                .hover(|s| s.text_color(fg).bg(fg.opacity(0.07)))
                .cursor_pointer()
                .child(glyph)
        };
        let a = anim.clone();
        let first = button("anim-first", "⏮", false).on_click(cx.listener(move |_, _, _, cx| {
            a.stop();
            a.seek(0);
            cx.notify();
        }));
        let a = anim.clone();
        let prev = button("anim-prev", "◀︎◀︎", false).on_click(cx.listener(move |_, _, _, cx| {
            a.stop();
            a.seek(a.current().saturating_sub(1));
            cx.notify();
        }));
        let a = anim.clone();
        let toggle = button("anim-play", if playing { "❚❚" } else { "▶︎" }, playing).on_click(cx.listener(move |this, _, _, cx| {
            if a.is_playing() {
                a.stop();
                cx.notify();
            } else {
                this.play(&a, 1, cx);
            }
        }));
        let a = anim.clone();
        let next = button("anim-next", "▶︎▶︎", false).on_click(cx.listener(move |_, _, _, cx| {
            a.stop();
            a.seek(a.current() + 1);
            cx.notify();
        }));
        let a = anim.clone();
        let last = button("anim-last", "⏭", false).on_click(cx.listener(move |_, _, _, cx| {
            a.stop();
            a.seek(usize::MAX);
            cx.notify();
        }));
        let modes = [(LoopMode::Once, "Once"), (LoopMode::Loop, "Loop"), (LoopMode::Reflect, "Reflect")];
        let mut mode_group = div().flex().flex_row().gap(px(2.)).ml(px(8.));
        for (m, label) in modes {
            let a = anim.clone();
            mode_group = mode_group.child(
                div()
                    .id(("anim-mode", id * 4 + m as u64))
                    .px(px(6.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .text_color(if m == mode { fg } else { muted })
                    .when(m == mode, |d| d.bg(fg.opacity(0.1)))
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        a.state.borrow_mut().mode = m;
                        cx.notify();
                    })),
            );
        }
        // Click or drag on the track to seek.
        let a = anim.clone();
        let fraction = if count > 1 { frame as f32 / (count - 1) as f32 } else { 1. };
        let track_w = px((anim.width * z).clamp(160., 640.));
        let seek_to = move |a: &Animation, x: Pixels| {
            let t = a.state.borrow().track;
            if t.size.width > px(0.) {
                let f = ((x - t.origin.x) / t.size.width).clamp(0., 1.);
                a.seek((f * (a.frames.len() - 1) as f32).round() as usize);
            }
        };
        let b = anim.clone();
        let track = div()
            .id(("anim-track", id))
            .relative()
            // Room for the knob at either end.
            .mx(px(6.))
            .w(track_w)
            .h(px(16.))
            .cursor_pointer()
            .child(
                canvas(
                    {
                        let a = anim.clone();
                        move |bounds, _, _| a.state.borrow_mut().track = bounds
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(div().absolute().top(px(6.)).left_0().w_full().h(px(4.)).rounded_full().bg(track_bg))
            .child(div().absolute().top(px(6.)).left_0().w(track_w * fraction).h(px(4.)).rounded_full().bg(accent))
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(track_w * fraction - px(6.))
                    .size(px(12.))
                    .rounded_full()
                    .bg(fg.opacity(0.75)),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(move |_, e: &gpui_kit::MouseDownEvent, _, cx| {
                a.stop();
                seek_to(&a, e.position.x);
                cx.notify();
            }))
            .on_mouse_move(cx.listener(move |_, e: &gpui_kit::MouseMoveEvent, _, cx| {
                if e.pressed_button == Some(MouseButton::Left) {
                    seek_to(&b, e.position.x);
                    cx.notify();
                }
            }));
        let image = anim.frames[frame].clone();
        crate::images::touch(&image, cx);
        div()
            .px(pad)
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                img(image)
                    .w(px(anim.width * z))
                    .max_w_full()
                    .aspect_ratio(anim.width / anim.height)
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(super::broken_image(muted)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.))
                    .child(track)
                    .child(
                        div()
                            .ml(px(8.))
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(format!("{} / {count}", frame + 1)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.))
                    .child(first)
                    .child(prev)
                    .child(toggle)
                    .child(next)
                    .child(last)
                    .child(mode_group),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jshtml_fixture() {
        let nb: serde_json::Value =
            serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/plots.ipynb")).unwrap())
                .unwrap();
        let html: String = nb["cells"][29]["outputs"][0]["data"]["text/html"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let anim = parse(&html).expect("an animation");
        assert_eq!(anim.frames.len(), 12);
        assert_eq!(anim.interval, Duration::from_millis(80));
        assert_eq!((anim.width, anim.height), (320., 200.));
        assert_eq!(anim.state.borrow().mode, LoopMode::Loop);
        assert!(parse("<b>not an animation</b>").is_none());
    }

    #[test]
    fn loop_modes_step() {
        let make = |mode| Animation {
            frames: (0..3).map(|_| Arc::new(Image::from_bytes(ImageFormat::Png, Vec::new()))).collect(),
            interval: Duration::from_millis(10),
            width: 1.,
            height: 1.,
            state: RefCell::new(State { frame: 0, direction: 1, mode, playing: None, track: Bounds::default() }),
        };
        let walk = |a: &Animation| (0..5).map(|_| (a.tick(), a.current())).collect::<Vec<_>>();
        assert_eq!(walk(&make(LoopMode::Loop)), [(true, 1), (true, 2), (true, 0), (true, 1), (true, 2)]);
        assert_eq!(walk(&make(LoopMode::Once)), [(true, 1), (true, 2), (false, 2), (false, 2), (false, 2)]);
        assert_eq!(walk(&make(LoopMode::Reflect)), [(true, 1), (true, 2), (true, 1), (true, 0), (true, 1)]);
    }
}
