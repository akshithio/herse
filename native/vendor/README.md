# Vendored crates

- `gpui-base` 0.7.3 (Apache-2.0, from GPUI Kit), patched in one place, marked
  `herse:` in `src/text/inline_flow.rs`: an unsized Markdown image clearly
  taller than the text line keeps its intrinsic size (upstream shrinks every
  unsized image to 0.75 × line height, which suits badges but not figures).
  Remove the `[patch.crates-io]` entry in the workspace `Cargo.toml` once
  upstream offers this.
- `src/selectable_text.rs`: `SelectableText::with_highlights`, so selectable
  text (code cells, outputs) keeps syntax and ANSI colours, and
  `SelectableText::on_click`, which reports the byte index of a click (a press
  and release in place) so one click on code edits it at that spot.
- `src/input/base/state.rs`: `Copy` with nothing selected in the editor
  propagates instead of emptying the clipboard, so an output selected while
  an editor keeps focus still copies.
- `src/text/contrast.rs` and `mark_highlight` in `src/text/node.rs`: an
  author's text color is kept readable on the theme's page (lightness moved,
  hue kept), and a `<mark>`'s fill gets ink that reads on it. The app uses
  the same `readable_text_color` for ANSI output.
- `src/text/format/html.rs`: `<pre>` becomes a monospace code block with its
  exact text (rich tables, `IPython.display.Code`), and
  `html5minify/mod.rs` keeps whitespace in text anywhere inside a `<pre>`,
  not only directly in it.
- `src/text/inline_flow.rs`: an image that fails to load shows a
  broken-image mark instead of nothing.
- `src/text/state.rs`: `TextViewState::source` is public, to read
  `selected_source_range` against (a double click on Markdown edits at the
  word).
- `gpui-component` `src/kbd.rs` (marked `herse:`): a shortcut hint shows the
  whole binding (`D D`, `⌘K ⌘T`), not only its first keystroke.
- `gpui-component` 0.7.3 (Apache-2.0, from GPUI Kit), patched in
  `src/theme/system_font.rs` and `src/theme/mono_font.rs` (marked `herse:`):
  theme init resolves the default UI and monospace families directly and
  lists every installed family only when one of them is missing. Listing
  them costs ~100 ms on macOS, which was a third of launch.
- `gpui-base` `src/text/` (marked `herse:`): `TextMark::color`, set from
  `<span style="color:…">` and `<font color>` in Markdown and HTML, and
  skipped when it would vanish into the theme's background.
- `gpui-base` HTML and tables (marked `herse:`):
  - `src/text/format/html.rs` reads a block container's box: `<center>`,
    `align`, and inline `text-align`, `background`, `border`, `border-left`
    and `padding`, kept on `BlockNode::Root` as a `BoxStyle`. `compact`
    keeps a styled root.
  - A `<span>`/`<font>`'s `style` gives bold, italic, underline, line-through
    and a background highlight as well as its color, in HTML and in
    Markdown's inline HTML. A block's `color` (kept readable on the theme),
    `font-weight` and `font-style` apply to all its text.
  - `<br>` is a line break inside the running text, as Markdown's hard break
    is, and adds no line at the end of a block. `<code>`/`<tt>`/`<samp>` set
    code and `<var>`/`<cite>`/`<dfn>` italics, in HTML and in Markdown's
    inline HTML. A container with Bootstrap's `alert` class draws as the
    callout of its kind.
  - Table cells keep `colspan` and `rowspan`, and an empty `<td>` holds its
    column. `Table::grid` places the cells. A table with spans lays its
    cells out at widths solved for its row width, which an empty canvas
    under the rows reports on each frame. The first frame groups spanned
    columns instead.
  - The inline flow lays its lines out centred or right-aligned when the
    text style asks, and its fragments paint left-aligned. An image-only
    paragraph in an aligned container is placed the same way.
  - A paragraph without a source span (from HTML) is identified by its own
    state, not one shared id. Before this, two flows in one cell painted
    each other's text.
  - A blockquote that opens with a GitHub alert marker (`[!NOTE]`, `[!TIP]`,
    `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`) renders as a tinted callout.
    So do the untitled `[!jupyter-info|success|warning|danger]` markers that
    the app writes for Bootstrap `alert` divs.
- `gpui-base` speed (marked `herse:`):
  - `src/text/inline.rs` sweeps its table of retained text layouts once the
    table has doubled since the last sweep. It used to sweep at a fixed size,
    and a frame with more live text than that (a long table) swept on every
    insert, which cost about half a second a frame.
  - `src/theme.rs` returns `cx.theme()` by reference instead of cloning the
    whole theme on every call (per table cell, per paragraph).
  - A table over 150 rows keeps its header and scrolls its body in a window
    18 rows tall. Only the rows in view are drawn, at one height, with long
    cells cut off with an ellipsis. A 1,500-row Markdown table went from
    300 ms a frame to 4 ms. The body takes the wheel while it has rows to
    show that way and hands it to the page at either end; GPUI otherwise
    scrolls every scroller under the pointer, so the page moved with it.
