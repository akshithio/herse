# Herse Native — architecture

A GPUI rewrite of Herse focused on one thing: opening and reading Jupyter
notebooks as fast as the hardware allows, with execution and editing on top.

```
native/
  crates/
    nb/          herse-nb         notebook model, fast parse, diff-clean write, iopub -> output
    kernel/      herse-kernel     kernelspec discovery, launch, Jupyter wire protocol over ZMQ
    workspace/   herse-workspace  folder walk, notebook summaries, fuzzy find, cross-notebook search
    rich/        herse-rich       ANSI -> styled spans, \r / \b stream semantics, LaTeX math -> SVG
    table/       herse-table      DataFrame / HTML tables -> grid model
    widgets/     herse-widgets    ipywidget state (saved or live comms) -> widget tree
    plot/        herse-plot       Plotly / Vega-Lite (Altair) -> static SVG
    data/        herse-data       data files and DataFrame outputs -> one streaming frame
    app/         herse (bin)      the GPUI application (gpui-kit 0.7.3)
  fixtures/                       generated test notebooks (see fixtures/README.md)
  tools/                          dev scripts (window screenshot, benchmarks)
```

Only `app` depends on GPUI. Every other crate is plain Rust, unit-testable in
seconds, and must not pull in an async runtime the app has to adopt: they
expose either synchronous functions (callable from `cx.background_spawn`) or
runtime-agnostic channels (`async-channel`), and run any runtime they need on
their own threads.

Toolchain: `rust-toolchain.toml` pins 1.99.1. Agents working on a non-app
crate should build with their own target dir to avoid lock contention with
the app build, e.g. `CARGO_TARGET_DIR=target/agent-kernel cargo test -p herse-kernel`.

## Speed budget (the point of the project)

| Operation                                   | Target           |
| ------------------------------------------- | ---------------- |
| Cold launch to first frame                  | < 150 ms         |
| Open 1 MB notebook to first painted cells   | < 30 ms          |
| Open 50 MB image-heavy notebook to first paint | < 150 ms      |
| Scroll a 5,000-cell notebook                | 120 fps, no hitch |
| Fuzzy file find over 10k files              | < 5 ms per keystroke |
| Cross-notebook search, 1k notebooks         | first hit < 50 ms |

Techniques: virtualized list (only visible cells are laid out), lazy
per-cell derived state (highlighting, markdown parse, image decode) computed
on first view and cached, base64 image decode on background threads, outputs
kept as borrowed/raw text until needed, no per-keystroke re-parsing.

## Crate contracts (as built)

### herse-nb
`parse(&[u8]) -> Result<Notebook>` (single pass, records the file's formatting
in `Notebook::format`), `to_string(&Notebook)` (byte-identical re-save of an
untouched file), `to_string_with(.., &WriteOptions::jupyter())` (exactly
`nbformat.write`), `output_from_iopub(msg_type, &content)`. In memory every
multiline field is one `String`; unknown keys and outputs are kept
(`Cell::extra`, `Output::Unknown`).

### herse-kernel
`discover() -> Vec<KernelSpec>` (~0.5 ms), `Kernel::launch(&spec, cwd) ->
(Kernel, Receiver<KernelEvent>)`, then `execute`, `interrupt`, `restart`,
`shutdown`, `input_reply`, `complete`, `inspect`. One thread per kernel with a
private executor; events on an unbounded `async_channel` (drain in batches).
Per execute: `execute_input` → outputs → idle → `ExecuteReply`; route by
`parent_msg_id`. Call `shutdown_all` on quit. `shell_env::preload()` at
startup reads the login shell's environment off the main thread when the app
was started without one (from Finder: launchd's bare `PATH`); discovery and every kernel
then use it, so a bare `python` argv resolves as it would in a terminal.

### herse-workspace
`walk`, `summarize` / `summarize_many` (byte scanner, skips outputs),
`FuzzyIndex`, `search` / `CompiledQuery::search_bytes` (parallel, streaming,
cancellable), `watch` (debounced), `Recents` (folders, notebooks, tabs per
folder). The walk honours `.gitignore` inside the opened folder but not
above it: pip and uv write a `.gitignore` of `*` into every site-packages and venv,
which would otherwise hide every notebook under them.

### herse-rich
`ansi::parse` (spans + OSC 8 links), `stream::{normalize, append, visible,
head_tail}` (JupyterLab's `processText`), `math::{render_svg, render_svg_with,
remove_math, replace_math, Macros}` (vendored TeX engine → SVG with
`currentColor`).

### herse-table
`parse(html) -> Option<Table>` (rectangular grid: spans, index columns, kinds,
Styler colours and bars, pandas/polars footers), `Table::column_widths_chars`.

### herse-widgets
`WidgetState::from_notebook_metadata(&metadata)` (ipywidgets 6–8 layouts),
then `view(model_id) -> Option<Widget>`: follows `IPY_MODEL_` references
into a tree of `Kind`s (boxes, tabs, progress, sliders with d3-formatted
readouts, text, selections, buttons, HTML, images from buffers, Output
widgets as raw nbformat outputs). `Widget::key` numbers nodes in pre-order
so the app can keep laid-out HTML and decoded images beside the tree.
Third-party models come back as `Kind::Unsupported`.

Live kernels: `apply_comm(msg_type, content, parent)` (comm_open / msg /
close, echo_update reconciliation), `capture` (Output widgets taking a
request's outputs), `slider_update`, `set_local` / `note_sent`, and
`to_metadata(roots)` to save the state reachable from a notebook's views.

### herse-plot
`render_plotly(&figure, &RenderOptions)`, `render_vegalite(&spec, ..)`,
`vegalite_spec_from_html(html)` → `Rendered { svg, width, height, notes }`
(text as `<text>`, colours per light/dark). `None` for what it can't draw
faithfully (3D, geo, polar, `data.url`, ...); every entry point is
`catch_unwind`-guarded and fuzzed. `tests/data/reference/` renders the same
samples with plotly.js and vega for side-by-side checks.

### herse-data
`read(path, table, sink)` streams `Event::Columns` / `Event::Chunk` /
`Event::Progress` to a callback (return `Flow::Stop` to cancel) for CSV, TSV
(and `.gz`), Parquet, Arrow IPC, JSON, JSON Lines, Excel (calamine) and SQLite
(the system library); `list_tables` for sheets and tables; `Format::of(path)`
and `EXTENSIONS` say what counts as data. A `Frame` is `Arc<Chunk>`s: per
column a text arena + end offsets, a null bitset, and (while every value is
one) the parsed `f64`s, plus per-chunk `Stats`, so drawing never formats and
sorting never parses, and a snapshot is a few pointers. `from_table` turns a
`herse_table::Table` into a frame with what the output says it left out.
`compute_order(frame, &ViewSpec, &cancel)` filters (`Filter::parse`),
searches (memchr over each arena) and sorts (ranks for categories, prefix
keys for text, `total_cmp` for numbers) into a `Vec<u32>` row order;
`to_csv` / `to_tsv` / `to_markdown`; `loading_code` writes the pandas or
polars cell for New Notebook. Everything is synchronous: the app runs `read`
on its own thread and `compute_order` with `background_spawn`.

## App structure

`workspace.rs` (window root: sidebar, tabs, overlays, status bar, disk
watching, session) · `sidebar.rs` (file tree + outline, virtualized) ·
`picker.rs` + `finder.rs` (file finder, command palette, outline, kernel
picker, workspace search) · `notebook/` (`mod.rs` the view: virtualized cell
list, selection, command/edit modes, kernel session; `outputs.rs` render-ready
outputs; `table.rs` canvas-painted grids; `math.rs` Markdown math plugins;
`markdown.rs` image/attachment resolution; `widget_view.rs` saved
ipywidgets; `animation.rs` matplotlib `to_jshtml` player; `find.rs`; `intellisense.rs`
kernel completion/hover; `highlight.rs` background syntax highlighting) ·
`http.rs` (image fetching for `https://` and `file://`) · `ui.rs` (the
controls every surface is built from, and the mark; see DESIGN.md) ·
`chrome.rs` (window drag strips) · `theme.rs` (tokens, fonts: SF Mono and
Charter are probed at launch) · `frame_view.rs` (a table tab: a data file or a
DataFrame output full size; `uniform_list` rows, each one canvas;
`notebook/frame_fetch.rs` asks a live kernel for a frame's every row). A
workspace tab is a `Page`: a notebook or a frame.

`images.rs` keeps decoded output images (figures, charts, animation
frames) within a memory budget: views touch what they draw, and the least
recently drawn are evicted from GPUI's asset cache, which otherwise keeps
every decoded image (1.2 GB after scrolling 300 figures; 0.56 GB with it).

Frame-cost rules that matter (measured): give list rows definite widths (no
max-content text measurement); cache views whose bounds don't move (the
sidebar); paint dense grids on a canvas instead of thousands of elements;
never query a `ListState` from inside its own render callback.
