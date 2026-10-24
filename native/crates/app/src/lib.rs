//! Herse: a fast native workspace for Jupyter notebooks.

pub mod actions;
pub mod agent;
pub mod assets;
pub mod broken;
pub mod chrome;
pub mod cli_install;
pub mod dialog;
pub mod extensions;
pub mod ui;
pub mod finder;
pub mod frame_view;
pub mod http;
pub mod images;
pub mod link_sheet;
pub mod recents;
pub mod remote;
pub mod settings;
pub mod share_sheet;
pub mod picker;
pub mod notebook;
pub mod sidebar;
pub mod theme;
pub mod workspace;
mod workspace_scan;

use std::{path::PathBuf, time::Duration};

use gpui_kit::component::{Theme, TitleBar};
use gpui_kit::{
    App, AppContext as _, Bounds, Entity, Menu, MenuItem, Window, WindowBounds, WindowOptions, px,
    size,
};

use actions::*;
use workspace::Workspace;

/// What the system asked Herse to open: a file or folder, or a live
/// session's `herse://join/…` link.
enum OpenRequest {
    Path(PathBuf),
    Join(String),
}

/// Run the app, opening `paths` (notebooks and/or a folder). With no paths
/// the last session's folder and tabs come back.
pub fn run(paths: Vec<PathBuf>) {
    let started = std::time::Instant::now();
    // Files handed over by Finder ("Open With", Dock drops) arrive here,
    // before or after the window exists.
    let (open_tx, open_rx) = herse_kernel::async_channel::unbounded::<OpenRequest>();
    let app = gpui_kit::application()
        .with_assets(assets::Assets)
        .with_http_client(http::ImageClient::new());
    app.on_open_urls(move |urls| {
        for url in urls {
            if url.starts_with("herse://") {
                let _ = open_tx.try_send(OpenRequest::Join(url));
            } else if let Some(path) = file_url_to_path(&url) {
                let _ = open_tx.try_send(OpenRequest::Path(path));
            }
        }
    });
    app.run(move |cx: &mut App| {
        let timing = std::env::var_os("HERSE_TIMING").is_some();
        let mark = |label: &str| {
            if timing {
                eprintln!("{label:>18}: {:6.1} ms", started.elapsed().as_secs_f32() * 1000.);
            }
        };
        mark("platform ready");
        init(cx);
        mark("init");
        cx.on_action(|_: &Quit, cx| quit(cx));
        // The Mac's own app and window commands.
        cx.on_action(|_: &HideApp, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &Minimize, cx| {
            if let Some(w) = cx.active_window() {
                let _ = w.update(cx, |_, window, _| window.minimize_window());
            }
        });
        cx.on_action(|_: &ZoomWindow, cx| {
            if let Some(w) = cx.active_window() {
                let _ = w.update(cx, |_, window, _| window.zoom_window());
            }
        });
        // One window: closing it quits, after the same cleanup as Cmd-Q.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                quit(cx);
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.), px(320.))),
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(gpui_kit::point(
                    px(theme::TRAFFIC_LIGHTS_AT.0),
                    px(theme::TRAFFIC_LIGHTS_AT.1 - 2.),
                )),
            }),
            ..TitleBar::window_options()
        };
        let restore = paths.is_empty();
        let paths = paths.clone();
        if let Some(z) = settings::zoom() {
            theme::set_zoom(z);
        }
        let (window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
            match settings::dark_theme() {
                Some(dark) => Theme::change(
                    if dark { gpui_kit::component::ThemeMode::Dark } else { gpui_kit::component::ThemeMode::Light },
                    Some(window),
                    cx,
                ),
                None => Theme::sync_system_appearance(Some(window), cx),
            }
            let ws = build_workspace(&paths, window, cx);
            if restore {
                ws.update(cx, |ws, cx| ws.restore_session(window, cx));
            }
            ws
        })
        .expect("failed to open window");
        mark("window open");
        // The menu bar costs ~15 ms to build; nobody reaches it before the
        // first frame is on screen.
        let _ = window.update(cx, |_, window, _| window.on_next_frame(|_, cx| set_menus(cx)));
        cx.activate(true);

        cx.spawn(async move |cx| {
            while let Ok(request) = open_rx.recv().await {
                let ws = workspace.clone();
                let _ = window.update(cx, |_, window, cx| {
                    ws.update(cx, |ws, cx| match request {
                        OpenRequest::Path(path) => ws.open_path(&path, window, cx),
                        // A link someone shared: the Join sheet, filled in.
                        OpenRequest::Join(link) => {
                            ws.show_link_sheet(Some(link), window, cx);
                        }
                    });
                });
            }
        })
        .detach();

        if std::env::var_os("HERSE_TIMING").is_some() {
            eprintln!("startup to run loop: {:.1} ms", started.elapsed().as_secs_f32() * 1000.);
        }
    });
}

/// Save what auto-save would have, stop kernels, then exit.
fn quit(cx: &mut App) {
    for window in cx.windows() {
        let _ = window.update(cx, |root, _, cx| {
            if let Some(ws) = root
                .downcast::<gpui_kit::component::Root>()
                .ok()
                .and_then(|root| root.read(cx).view().clone().downcast::<Workspace>().ok())
            {
                ws.update(cx, |ws, cx| ws.prepare_quit(cx));
            }
        });
    }
    herse_kernel::shutdown_all(Duration::from_secs(2));
    cx.quit();
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    // Percent-decode (Finder encodes spaces and non-ASCII).
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// Library-wide setup: components, key bindings. Shared with the headless snapshot tool.
pub fn init(cx: &mut App) {
    // Started from Finder: read the login shell's PATH (conda, pyenv) in the
    // background so the first run finds the user's Python.
    herse_kernel::shell_env::preload();
    let t = std::time::Instant::now();
    gpui_kit::init(cx);
    let t_kit = t.elapsed();
    theme::install_theme(cx);
    theme::load_fonts(cx);
    if std::env::var_os("HERSE_TIMING").is_some() {
        eprintln!("    gpui_kit::init {:.1} ms, fonts {:.1} ms", t_kit.as_secs_f32() * 1000., (t.elapsed() - t_kit).as_secs_f32() * 1000.);
    }
    actions::bind_keys(cx);
    picker::bind_keys(cx);
    notebook::find::bind_keys(cx);
    sidebar::bind_keys(cx);
    share_sheet::bind_keys(cx);
    link_sheet::bind_keys(cx);
    dialog::bind_keys(cx);
    agent::ask::bind_keys(cx);
    notebook::comments::bind_keys(cx);
    frame_view::bind_keys(cx);
    extensions::init(cx);
}

pub fn build_workspace(paths: &[PathBuf], window: &mut Window, cx: &mut App) -> Entity<Workspace> {
    cx.new(|cx| {
        let mut ws = Workspace::new(window, cx);
        ws.show_extension_problems(cx);
        for path in paths {
            ws.open_path(path, window, cx);
        }
        ws
    })
}

/// What the menu bar's Open Recent was last built with.
struct MenuRecents(Vec<PathBuf>);
impl gpui_kit::Global for MenuRecents {}

/// The menu bar: every command, each beside its live shortcut. Rebuilt when
/// the recent files change.
pub(crate) fn set_menus(cx: &mut App) {
    use notebook::find::{FindNext, FindPrev, ToggleReplace};
    let menu = |name: &str, items: Vec<MenuItem>| Menu { name: name.to_string().into(), items, disabled: false };
    // File ▸ Open Recent: folders, then notebooks, as macOS lists documents.
    let recents = recents::get(cx).recents.clone();
    // Building the bar costs ~15 ms of main thread: only when what Open
    // Recent shows has changed.
    let shown: Vec<PathBuf> = recents.folders.iter().take(5).chain(recents.notebooks.iter().take(10)).cloned().collect();
    if cx.try_global::<MenuRecents>().is_some_and(|m| m.0 == shown) {
        return;
    }
    cx.set_global(MenuRecents(shown));
    let mut recent_items: Vec<MenuItem> = Vec::new();
    let label = |p: &std::path::Path| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match p.parent() {
            Some(dir) => format!("{name} — {}", workspace::tilde(dir)),
            None => name,
        }
    };
    for folder in recents.folders.iter().filter(|p| p.is_dir()).take(5) {
        recent_items.push(MenuItem::action(label(folder), OpenRecent(folder.clone())));
    }
    if !recent_items.is_empty() {
        recent_items.push(MenuItem::separator());
    }
    for nb in recents.notebooks.iter().filter(|p| p.is_file()).take(10) {
        recent_items.push(MenuItem::action(label(nb), OpenRecent(nb.clone())));
    }
    if !recent_items.is_empty() {
        recent_items.push(MenuItem::separator());
    }
    recent_items.push(MenuItem::action("Clear Menu", ClearRecent));
    cx.set_menus(vec![
        menu(
            "Herse",
            vec![
                MenuItem::action("About Herse", ShowAbout),
                MenuItem::separator(),
                MenuItem::action("Settings…", ShowSettings),
                MenuItem::action("Install Command Line Tool…", InstallCommandLineTool),
                MenuItem::separator(),
                MenuItem::action("Hide Herse", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Herse", Quit),
            ],
        ),
        menu(
            "File",
            vec![
                MenuItem::action("New Notebook", NewNotebook),
                MenuItem::action("Open…", Open),
                MenuItem::action("Open Folder…", OpenFolder),
                MenuItem::action("Open Link…", OpenLink),
                MenuItem::submenu(menu("Open Recent", recent_items)),
                MenuItem::separator(),
                MenuItem::action("Go to File…", GoToFile),
                MenuItem::action("Go to Heading…", GoToHeading),
                MenuItem::action("Find in Folder…", FindInWorkspace),
                MenuItem::separator(),
                MenuItem::action("Save", Save),
                MenuItem::action("Save As…", SaveAs),
                MenuItem::action("Share…", Share),
                MenuItem::action("Join Live Session…", JoinLive),
                MenuItem::action("Export as HTML…", ExportHtml),
                MenuItem::action("Reveal in Finder", RevealInFinder),
                MenuItem::action("Reload from Disk", ReloadFromDisk),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Other Tabs", CloseOtherTabs),
                MenuItem::action("Close All Tabs", CloseAllTabs),
                MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
            ],
        ),
        menu(
            "Edit",
            vec![
                MenuItem::action("Undo", EditUndo),
                MenuItem::action("Redo", EditRedo),
                MenuItem::separator(),
                MenuItem::action("Cut", EditCut),
                MenuItem::action("Copy", EditCopy),
                MenuItem::action("Paste", EditPaste),
                MenuItem::action("Select All", EditSelectAll),
                MenuItem::separator(),
                MenuItem::action("Find…", Find),
                MenuItem::action("Find and Replace…", ToggleReplace),
                MenuItem::action("Find Next", FindNext),
                MenuItem::action("Find Previous", FindPrev),
                MenuItem::separator(),
                MenuItem::action("Add Comment", AddComment),
                MenuItem::action("Toggle Comment", ToggleComment),
                MenuItem::action("Copy Line Down", CopyLineDown),
                MenuItem::action("Move Line Up", MoveLineUp),
                MenuItem::action("Move Line Down", MoveLineDown),
                MenuItem::action("Delete Line", DeleteLine),
                MenuItem::separator(),
                MenuItem::action("Emoji & Symbols", ShowEmoji),
            ],
        ),
        menu(
            "View",
            vec![
                MenuItem::action("Command Menu…", CommandMenu),
                MenuItem::action("Command Palette…", CommandPalette),
                MenuItem::action("Ask Claude or Codex…", AskAgent),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Files / Outline", ToggleOutline),
                MenuItem::action("Comments", ToggleComments),
                MenuItem::separator(),
                MenuItem::action("Collapse All Headings", CollapseAllHeadings),
                MenuItem::action("Expand All Headings", ExpandAllHeadings),
                MenuItem::action("Hide All Code", HideAllCode),
                MenuItem::action("Show All Code", ShowAllCode),
                MenuItem::action("Hide All Outputs", HideAllOutputs),
                MenuItem::action("Show All Outputs", ShowAllOutputs),
                MenuItem::action("Line Numbers", ToggleAllLineNumbers),
                MenuItem::action("Changes Since Last Commit", ToggleChanges),
                MenuItem::separator(),
                MenuItem::action("Toggle Dark Mode", ToggleTheme),
                MenuItem::submenu(menu(
                    "Appearance",
                    vec![
                        MenuItem::action("Light", LightMode),
                        MenuItem::action("Dark", DarkMode),
                        MenuItem::action("Match System", SystemAppearance),
                    ],
                )),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullScreen),
            ],
        ),
        menu(
            "Cell",
            vec![
                MenuItem::action("Run Cell", RunCell),
                MenuItem::action("Run and Advance", RunAndAdvance),
                MenuItem::action("Run and Insert Below", RunAndInsert),
                MenuItem::action("Run All", RunAll),
                MenuItem::action("Run All Above", RunAbove),
                MenuItem::action("Run Selected and Below", RunBelow),
                MenuItem::separator(),
                MenuItem::action("Insert Code Cell Above", InsertCodeAbove),
                MenuItem::action("Insert Code Cell Below", InsertCodeBelow),
                MenuItem::action("Insert Markdown Cell Below", InsertMarkdownBelow),
                MenuItem::separator(),
                MenuItem::action("Duplicate Cell", DuplicateCell),
                MenuItem::action("Delete Cell", DeleteCell),
                MenuItem::action("Move Cell Up", MoveCellUp),
                MenuItem::action("Move Cell Down", MoveCellDown),
                MenuItem::action("Merge Cell Below", MergeBelow),
                MenuItem::action("Split Cell at Cursor", SplitCell),
                MenuItem::separator(),
                MenuItem::action("Change to Code", ToCode),
                MenuItem::action("Change to Markdown", ToMarkdown),
                MenuItem::action("Change to Raw", ToRaw),
                MenuItem::separator(),
                MenuItem::action("Toggle Output", ToggleOutput),
                MenuItem::action("Clear Output", ClearOutputs),
                MenuItem::action("Clear All Outputs", ClearAllOutputs),
                MenuItem::action("Next Error", NextError),
                MenuItem::action("Previous Error", PrevError),
                MenuItem::action("Next Change", NextChange),
                MenuItem::action("Previous Change", PrevChange),
            ],
        ),
        menu(
            "Kernel",
            vec![
                MenuItem::action("Interrupt", Interrupt),
                MenuItem::action("Restart", Restart),
                MenuItem::action("Restart and Run All", RestartAndRunAll),
                MenuItem::separator(),
                MenuItem::action("Change Kernel…", ChangeKernel),
                MenuItem::action("Shut Down", ShutdownKernel),
            ],
        ),
        menu(
            "Window",
            vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", ZoomWindow),
                MenuItem::separator(),
                MenuItem::action("Show Next Tab", NextTab),
                MenuItem::action("Show Previous Tab", PrevTab),
            ],
        ),
        menu(
            "Help",
            vec![
                MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
                MenuItem::action("Command Menu…", CommandMenu),
            ],
        ),
    ]);
}
