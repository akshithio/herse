//! Headless renderer for development: opens paths in a real Workspace, drives
//! it with keystrokes, and writes PNG frames rendered by Metal offscreen. Works
//! with the screen locked, which a window screenshot does not.
//!
//! ```text
//! herse-snap --out shot.png [--size 1280x860] [--dark] [--keys "j j enter"]
//!            [--scroll 900[,300…]] [--frames 6] [--bench] paths...
//! ```
//! `--keys` may contain `|` to take a frame between key groups: the outputs
//! are numbered `shot-0.png`, `shot-1.png`, ... A `click@X,Y` token clicks
//! at window coordinates, `dblclick@X,Y` double-clicks, `rclick@X,Y`
//! right-clicks, and
//! `drag@X1,Y1,X2,Y2` drags.

use std::{path::PathBuf, sync::Arc, time::{Duration, Instant}};

use gpui_kit::component::{Root, Theme, ThemeMode};
use gpui_kit::{AnyWindowHandle, AppContext as _, HeadlessAppContext, Keystroke, px, size};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    // The agent's bridge, when an agent this renderer started asks for it.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == "--mcp-bridge") {
        let socket = args.next().map(PathBuf::from).unwrap_or_default();
        herse::agent::mcp::bridge(&socket)?;
        return Ok(());
    }
    // SAFETY: before any thread starts.
    unsafe { std::env::set_var("HERSE_FAKE_TRAFFIC_LIGHTS", "1") };
    // Never read or write the user's real session state.
    // SAFETY: single-threaded at this point; nothing else reads the environment yet.
    unsafe { std::env::set_var("HERSE_NO_RECENTS", "1") };
    // Nor the user's GitHub account (Share): no gh, no Keychain, no network.
    unsafe { std::env::set_var("HERSE_NO_ACCOUNTS", "1") };
    // Nor the notebooks it opens: edits and runs stay in memory
    // (`HERSE_SNAP_SAVE=1` to test saving).
    if std::env::var_os("HERSE_SNAP_SAVE").is_none() {
        unsafe { std::env::set_var("HERSE_NO_SAVE", "1") };
    }
    let mut out = PathBuf::from("shot.png");
    let mut width = 1280.;
    let mut height = 860.;
    let mut dark = false;
    let mut keys = String::new();
    let mut scroll: Vec<f32> = Vec::new();
    let mut frames = 6;
    let mut bench = false;
    let mut wait_ms = 0u64;
    let mut paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().expect("--out FILE").into(),
            "--size" => {
                let s = args.next().expect("--size WxH");
                let (w, h) = s.split_once('x').expect("WxH");
                width = w.parse()?;
                height = h.parse()?;
            }
            "--dark" => dark = true,
            "--keys" => keys = args.next().expect("--keys \"...\""),
            "--scroll" => {
                scroll = args.next().expect("--scroll PX[,PX…]").split(',').map(str::parse).collect::<Result<_, _>>()?
            }
            "--frames" => frames = args.next().expect("--frames N").parse()?,
            "--bench" => bench = true,
            "--wait" => wait_ms = args.next().expect("--wait MS").parse()?,
            _ => paths.push(PathBuf::from(arg)),
        }
    }

    let platform = gpui_kit::platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(herse::assets::Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        cx.set_http_client(herse::http::ImageClient::new());
        herse::init(cx);
        Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    });

    let t0 = Instant::now();
    let window = cx.open_window(size(px(width), px(height)), |window, cx| {
        // A live session's link or a web link opens the Open Link sheet, as
        // clicking one does.
        let (links, files): (Vec<PathBuf>, Vec<PathBuf>) = paths.iter().cloned().partition(|p| {
            let p = p.to_string_lossy();
            p.starts_with("herse://") || p.starts_with("http://") || p.starts_with("https://")
        });
        let ws = herse::build_workspace(&files, window, cx);
        if let Some(link) = links.first() {
            let link = link.to_string_lossy().into_owned();
            ws.update(cx, |ws, cx| {
                ws.show_link_sheet(Some(link), window, cx);
            });
        }
        // `HERSE_SNAP_ASK="claude:question"` asks an agent about the notebook.
        if let Some((provider, question)) = std::env::var("HERSE_SNAP_ASK").ok().and_then(|v| {
            let (p, q) = v.split_once(':')?;
            Some((herse::agent::Provider::from_id(p)?, q.to_string()))
        }) {
            ws.update(cx, |ws, cx| ws.ask_agent(provider, question, window, cx));
        }
        cx.new(|cx| Root::new(ws, window, cx))
    })?;
    let window: AnyWindowHandle = window.into();
    let t_open = t0.elapsed();
    cx.allow_parking();
    draw(&mut cx, window)?;
    let t_first = t0.elapsed();

    let settle = |cx: &mut HeadlessAppContext| -> anyhow::Result<()> {
        for _ in 0..frames {
            cx.run_until_parked();
            cx.advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(20));
            draw(cx, window)?;
        }
        // Real time for external work (a kernel booting, a cell running).
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            cx.run_until_parked();
            cx.advance_clock(Duration::from_millis(20));
            std::thread::sleep(Duration::from_millis(20));
            draw(cx, window)?;
        }
        Ok(())
    };
    settle(&mut cx)?;

    // One wheel event per value, each settled before the next.
    for scroll in scroll {
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                gpui_kit::PlatformInput::ScrollWheel(gpui_kit::ScrollWheelEvent {
                    position: gpui_kit::point(px(width / 2.), px(height / 2.)),
                    delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-scroll))),
                    ..Default::default()
                }),
                cx,
            );
        })?;
        settle(&mut cx)?;
    }

    let groups: Vec<&str> = if keys.is_empty() { vec![] } else { keys.split('|').collect() };
    if groups.len() <= 1 {
        if let Some(group) = groups.first() {
            press(&mut cx, window, group)?;
            settle(&mut cx)?;
        }
        save(&mut cx, window, &out)?;
    } else {
        for (n, group) in groups.iter().enumerate() {
            press(&mut cx, window, group)?;
            settle(&mut cx)?;
            let stem = out.file_stem().unwrap().to_string_lossy().into_owned();
            let path = out.with_file_name(format!("{stem}-{n}.png"));
            save(&mut cx, window, &path)?;
        }
    }

    if std::env::var_os("HERSE_PRINT_CLIPBOARD").is_some() {
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|c| c.text()));
        eprintln!("clipboard: {text:?}");
    }

    if bench {
        // Time a full redraw of the settled frame, and a scroll sweep. Both
        // wall time and this thread's CPU time: on a loaded machine the CPU
        // time is the number that means something.
        let n: u32 = std::env::var("HERSE_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        let t = Instant::now();
        let c = thread_cpu();
        for _ in 0..n {
            cx.update_window(window, |_, window, _| window.refresh())?;
            draw(&mut cx, window)?;
        }
        let redraw = t.elapsed() / n;
        let redraw_cpu = (thread_cpu() - c) / n;
        let t = Instant::now();
        let c = thread_cpu();
        for _ in 0..n {
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(
                    gpui_kit::PlatformInput::ScrollWheel(gpui_kit::ScrollWheelEvent {
                        position: gpui_kit::point(px(width / 2.), px(height / 2.)),
                        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-120.))),
                        ..Default::default()
                    }),
                    cx,
                );
            })?;
            draw(&mut cx, window)?;
        }
        let scrolled = t.elapsed() / n;
        let scrolled_cpu = (thread_cpu() - c) / n;
        // Typing: HERSE_BENCH_TYPE=1 enters edit mode on the selected cell and
        // times keystrokes (dispatch + the frame they cause).
        if std::env::var_os("HERSE_BENCH_TYPE").is_some() {
            press(&mut cx, window, "enter")?;
            draw(&mut cx, window)?;
            let k: u32 = 100;
            let t = Instant::now();
            let c = thread_cpu();
            for i in 0..k {
                let key = if i % 20 == 19 { "enter" } else { "x" };
                let keystroke = Keystroke::parse(key)?;
                cx.update_window(window, |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })?;
                draw(&mut cx, window)?;
            }
            eprintln!(
                "keystroke {:.2} ms (cpu {:.2})",
                (t.elapsed() / k).as_secs_f32() * 1000.,
                ((thread_cpu() - c) / k).as_secs_f32() * 1000.
            );
        }
        eprintln!(
            "open {:.1} ms · first frame {:.1} ms · redraw {:.2} ms (cpu {:.2}) · scroll frame {:.2} ms (cpu {:.2})",
            t_open.as_secs_f32() * 1000.,
            t_first.as_secs_f32() * 1000.,
            redraw.as_secs_f32() * 1000.,
            redraw_cpu.as_secs_f32() * 1000.,
            scrolled.as_secs_f32() * 1000.,
            scrolled_cpu.as_secs_f32() * 1000.,
        );
    }
    Ok(())
}

/// CPU time consumed by the calling thread.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes into the timespec we own.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn draw(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> anyhow::Result<()> {
    cx.update_window(window, |_, window, cx| {
        let token = window.draw(cx);
        token.clear(cx);
    })
}

fn press(cx: &mut HeadlessAppContext, window: AnyWindowHandle, keys: &str) -> anyhow::Result<()> {
    for key in keys.split_whitespace() {
        // `drag@X1,Y1,X2,Y2` presses at the first point and releases at the second.
        if let Some(pos) = key.strip_prefix("drag@") {
            let v: Vec<f32> = pos.split(',').map(|n| n.parse()).collect::<Result<_, _>>()?;
            anyhow::ensure!(v.len() == 4, "drag@X1,Y1,X2,Y2");
            let a = gpui_kit::point(px(v[0]), px(v[1]));
            let b = gpui_kit::point(px(v[2]), px(v[3]));
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent { position: a, ..Default::default() }), cx);
                window.dispatch_event(
                    gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                        button: gpui_kit::MouseButton::Left,
                        position: a,
                        click_count: 1,
                        ..Default::default()
                    }),
                    cx,
                );
                for i in 1..=8 {
                    let t = i as f32 / 8.;
                    let p = gpui_kit::point(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
                    window.dispatch_event(
                        gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                            position: p,
                            pressed_button: Some(gpui_kit::MouseButton::Left),
                            ..Default::default()
                        }),
                        cx,
                    );
                }
                window.dispatch_event(
                    gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                        button: gpui_kit::MouseButton::Left,
                        position: b,
                        click_count: 1,
                        ..Default::default()
                    }),
                    cx,
                );
            })?;
            cx.run_until_parked();
            continue;
        }
        // `move@X,Y` moves the mouse there (hover states), no buttons.
        if let Some(pos) = key.strip_prefix("move@") {
            let (x, y) = pos.split_once(',').ok_or_else(|| anyhow::anyhow!("move@X,Y"))?;
            let position = gpui_kit::point(px(x.parse()?), px(y.parse()?));
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(
                    gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent { position, ..Default::default() }),
                    cx,
                );
            })?;
            cx.run_until_parked();
            draw(cx, window)?;
            continue;
        }
        // `click@X,Y` clicks at window coordinates (logical pixels);
        // `dblclick@X,Y` double-clicks, a frame between the two clicks.
        let (clicks, pos) = match (key.strip_prefix("click@"), key.strip_prefix("dblclick@")) {
            (Some(pos), _) => (1, Some(pos)),
            (_, Some(pos)) => (2, Some(pos)),
            _ => (0, None),
        };
        let (button, pos) = match key.strip_prefix("rclick@") {
            Some(pos) => (gpui_kit::MouseButton::Right, Some(pos)),
            None => (gpui_kit::MouseButton::Left, pos),
        };
        let clicks = if button == gpui_kit::MouseButton::Right { 1 } else { clicks };
        if let Some(pos) = pos {
            let (x, y) = pos.split_once(',').ok_or_else(|| anyhow::anyhow!("click@X,Y"))?;
            let position = gpui_kit::point(px(x.parse()?), px(y.parse()?));
            for click_count in 1..=clicks {
                cx.update_window(window, |_, window, cx| {
                    window.dispatch_event(
                        gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                            position,
                            ..Default::default()
                        }),
                        cx,
                    );
                    window.dispatch_event(
                        gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                            button,
                            position,
                            click_count,
                            ..Default::default()
                        }),
                        cx,
                    );
                    window.dispatch_event(
                        gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                            button,
                            position,
                            click_count,
                            ..Default::default()
                        }),
                        cx,
                    );
                })?;
                cx.run_until_parked();
                draw(cx, window)?;
            }
            continue;
        }
        // `type:Some_text` types it (`_` for a space).
        if let Some(text) = key.strip_prefix("type:") {
            for c in text.chars() {
                let name = match c {
                    '_' => "space".to_string(),
                    c => c.to_string(),
                };
                let keystroke = Keystroke::parse(&name)?;
                cx.update_window(window, |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })?;
            }
            cx.run_until_parked();
            continue;
        }
        let keystroke = Keystroke::parse(key)?;
        cx.update_window(window, |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx);
        })?;
        cx.run_until_parked();
    }
    Ok(())
}

fn save(cx: &mut HeadlessAppContext, window: AnyWindowHandle, path: &std::path::Path) -> anyhow::Result<()> {
    draw(cx, window)?;
    let image = cx.capture_screenshot(window)?;
    image.save(path)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}
