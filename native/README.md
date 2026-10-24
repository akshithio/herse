# Herse Native

A native, GPU-rendered workspace for Jupyter notebooks, written in Rust on
[GPUI](https://github.com/zed-industries/zed) (through [GPUI Kit](https://gpui-kit.com)).
It succeeds the Electron Herse in the parent directory with the same idea: a
folder of notebooks you can browse, search and run against the kernels already
on your machine, with no `jupyter server`. This version is rebuilt for speed.

```bash
cargo run --release -p herse -- path/to/notebook.ipynb   # or a folder, or nothing
packaging/macos/bundle.sh                                 # builds target/release/Herse.app
```

With no arguments, Herse reopens the last folder and its tabs.

## Reading

- **Opening and scrolling are fast.** Parsing takes 0.5 ms for a 1 MB notebook,
  2.4 ms for 5,000 cells, and 3.5 ms for 50 MB of embedded plots. Only the cells
  on screen are laid out, so a frame's cost doesn't grow with notebook size:
  scrolling costs 2–4 ms of CPU per frame on an M2 Max. Decoded figures are
  kept to a memory budget, so a notebook of 300 figures stays near 0.5 GB
  however far you scroll.
- **Markdown** is rendered natively: GFM tables, task lists, block quotes,
  code blocks with highlighting, and inline HTML, including the habits of
  course notebooks: `<details>` solutions stay folded until clicked, plus
  `<sub>`/`<sup>`, `<kbd>`, coloured `<span>`s, Bootstrap alert boxes and
  GitHub alerts (`> [!NOTE]`) as tinted callouts, `<center>` and `align=`,
  boxed and shaded `<div style>`s, and HTML tables with `colspan` and
  `rowspan`. Images
  work from cell attachments, from files next to the notebook (sized with
  `<img width>`), and from the web.
- **Math.** LaTeX is typeset natively in about 40 µs per formula (STIX Two
  Math). It covers `$…$`, `$$…$$`, `\begin{align}` and the other environments,
  found the way JupyterLab finds them. 1,908 real-world formulas render
  without a failure.
- **DataFrames** from pandas, Styler, polars, R and Julia are drawn as a native
  grid: compact, with right-aligned numbers, Styler colours and `.bar()` data
  bars, a sideways scroll for wide frames, and the "60 rows × 50 columns" footer.
- **Outputs:**
  - stdout and stderr with full ANSI colour, using JupyterLab's exact `\r`/`\b`
    rules, so progress bars collapse;
  - tracebacks;
  - PNG, JPEG, GIF and SVG figures at their logical (retina-aware) size;
  - Markdown, LaTeX and JSON outputs.
- **Widgets** are drawn natively: tqdm and Hugging Face progress bars,
  sliders, text fields, dropdowns, buttons, tabs, accordions, `interact` and
  Output widgets with their captured plots. They come from the state a
  notebook saved (Colab, nbclient and JupyterLab's "Save Widget State" store
  it) or live from a running kernel, and Herse saves their state the same
  way, so a notebook reopens as it looked. With the kernel running they
  work: drag sliders, press buttons, tick boxes, type into text fields and
  pick from dropdowns, radios and lists, and `interact` reruns. Tabs and
  accordions switch even in a saved notebook.
- **Matplotlib animations** (`HTML(anim.to_jshtml())`) play natively, with
  the same controls: play, step, scrub, and once / loop / reflect.
- **Charts.** Plotly figures and Vega-Lite / Altair charts are drawn
  natively (median 0.2 ms per chart), checked against plotly.js and vega:
  scatter, line, bar, histogram, box, heatmap, pie, candlestick, subplots
  and facets, layered and concatenated Vega-Lite, light and dark. 3D charts,
  maps and other types that need JavaScript say so.
- **Charts belong to the page.** With Settings ▸ Charts: Blend with the Page
  (on by default), Plotly and Altair charts drop their white card: the
  background is the page's, gridlines are faint ink, and a chart that sets
  its own colours keeps them. Matplotlib figures get Herse's style, in Herse
  only: transparent, retina, the page's ink and fonts, no top or right
  spines, a left-aligned title, and a palette that starts with dew. It is a
  `matplotlibrc` handed to Python kernels (skipped when you have your own),
  re-applied when the theme switches; the notebook file is untouched.
- **Mermaid diagrams** (```` ```mermaid ```` fences, and `text/vnd.mermaid`
  outputs, as JupyterLab 4.1+ draws them) are laid out natively in about
  1 ms: flowcharts with subgraphs, sequence diagrams, pie charts, state,
  class and ER diagrams, in the theme's ink. Types it can't draw stay code.
- **Colab DataFrames** (wrapped in Colab's buttons and scripts) get the same
  native grid.
- **Other interactive outputs** (Bokeh, third-party widgets) say what they
  are instead of rendering as garbage. HTML that needs a browser (scripts,
  embedded pages like YouTube) and PDF outputs open in one click; library
  loader cells draw nothing, as in JupyterLab.
- **Huge outputs stay cheap.** A long output folds to its head and tail.
  "Show all" opens a virtualized line view in which 200,000 lines scroll like
  20. Cells with thousands of outputs show the first hundred.
- **Notebooks from the web.** File ▸ Open Link… (`Cmd-Shift-L`), or paste a
  link into `Cmd-K`, opens a notebook straight from its page on GitHub, a
  gist, nbviewer, Colab, GitLab or Hugging Face (or any URL serving the
  JSON) in a new tab named after the file. Its relative images load from
  next to it, its links to other notebooks open in Herse too, and the status
  bar says where it came from (click to see the page). Save keeps a copy.
- **Changes since the last commit.** In a git repository, the margin marks
  each cell added (green) or changed (amber) since the notebook's last
  commit, and where cells were removed. Click a changed cell's mark to see
  its lines against the commit's; click a removal to see the removed cells.
  `F7` / `Shift-F7` go to the next or previous change, and the status bar
  counts them. Cells pair up by their ids (nbformat 4.5), so an edited cell
  stays the same cell; the comparison follows every keystroke, in about
  40 µs on a 5,000-cell notebook. View ▸ Changes Since Last Commit turns it
  off.
- **Navigation:**
  - the outline panel (`Cmd-Shift-E`) shows the current section;
  - headings collapse (`←`/`→`), stored the way JupyterLab stores them;
  - `Cmd-Shift-O` goes to a heading;
  - `F8` / `Shift-F8` go to the next or previous error;
  - Markdown links to `#headings` and to other notebooks (on disk or on
    GitHub, nbviewer, Colab…) open in Herse;
  - each notebook reopens at the cell you left it on.
- **Reading comfort:**
  - content zoom (`Cmd +` / `Cmd -` / `Cmd 0`, remembered) scales text,
    code, tables and figures;
  - click a figure to see it large;
  - "Hide All Outputs" leaves just the code, and "Hide All Code" just the
    prose and results;
  - inputs and outputs folded in JupyterLab or the classic notebook (or
    tagged `hide-input` / `hide-output` for Jupyter Book) open folded;
  - drag to select and `Cmd-C` in code, outputs (errors included) and
    Markdown, with no edit mode needed, even while another cell is being
    edited.
- **Output actions.** Hover an output for its Copy button: text (an error, a
  print) as text, a figure as an image (paste it into slides), a DataFrame as
  TSV (paste it into a spreadsheet). Right-click an output to save a figure.
- **Quick Look and HTML.** Select a notebook in Finder and press Space:
  Herse's Quick Look extension shows it as a page (Markdown, math, code,
  figures, DataFrames, Plotly and Vega-Lite charts, colored tracebacks) in
  light or dark, rendered in a few milliseconds with no Python. macOS finds
  the extension once Herse.app has been opened. File ▸ Export as HTML writes
  the same self-contained page next to the notebook, unsaved edits
  included.

## Looking at data

- **Data files open as tables.** CSV, TSV (and `.csv.gz`), Parquet, Arrow /
  Feather, JSON, JSON Lines, Excel (xlsx, xls, ods) and SQLite open from the
  sidebar, `Cmd-O`, a drag onto the window or Finder's Open With, with no
  kernel. The sidebar lists them beside the notebooks, with their size. A file
  is read on its own thread and shows its first rows at once: a 100 MB,
  1.1-million-row CSV paints its first 512 rows in about 1 ms and is read in
  0.6 s (Parquet of the same rows: 0.5 s), while the window keeps drawing at
  1.5 ms a frame. Separators (`,` `;` tab `|`), decimal commas, a UTF-8 BOM,
  Latin-1 text, ragged lines and late JSON keys are handled; what was worked
  around is said above the table. A workbook's sheets and a database's
  tables are a menu.
- **Every DataFrame output opens full size.** Hover a DataFrame (pandas,
  Styler, polars, R, Julia, Colab) and click the expand button beside Copy: it
  opens in a tab of its own. A notebook keeps only the rows pandas printed, and
  the tab says so ("Showing the 10 rows the notebook saved, of 1,000 rows").
  With the notebook's kernel running, **Load All Rows** asks the kernel for the
  whole frame (outside any cell; it writes Parquet, or CSV without pyarrow) and
  shows every row.
- **The table.** Only the rows on screen are drawn, so a million rows scroll
  like ten. Click a column's name to sort (again for descending, a third time
  for file order): about 50–100 ms for 1.1 million rows. Its funnel filters:
  `text`, `!text`, `=value`, `> 10`, `<= 3`, `10..20`, `null`, `!null`.
  Search (`Cmd-F`) keeps the rows with the text in any column and tints the
  hits (40 ms over 1.1 million rows). Hover a column's name for its count,
  missing values and, for numbers, min, max and mean. Row numbers and a
  DataFrame's index stay put when you scroll sideways; drag a column's edge to
  widen it. Click a cell and move with the arrows, `Page Up`/`Page Down`,
  `Cmd-Up`/`Cmd-Down`; `Cmd-C` copies the cell. Copy copies the rows in view
  as CSV, Markdown or TSV (up to 100,000).
- **Every column's shape, under its name.** A histogram for numbers and for
  text that holds ISO dates or times, the most common values as bars for
  text, the true/false split for booleans, and a track for the missing share.
  Hover a bar for its range or value, its count and share; click it to filter
  to those rows, and click again to clear. With a filter or search on, the
  rows in view show in dew over the whole. Profiles take 0.14 s for 1.1
  million rows, off the main thread; a filtered view's, 45 ms.
- **Ranges add up.** Drag across cells, shift-click, or `Shift-arrows`
  (`Cmd-A` for all): the foot of the table shows the count, Sum, Mean, Min and
  Max, each a click to copy, and `Cmd-C` copies the range as tab-separated
  lines for a spreadsheet. Double-click a cell (or press `Space`) to see its
  whole value, selectable, with Copy. The foot also says how many rows are in
  view ("191 of 2,000 rows") with Clear, which drops every filter and the
  search.
- **`Cmd-Z` takes the view back a step**, and `Cmd-Shift-Z` forward: a sort,
  a filter (typed or from a bar, edited or removed), Clear, a search (one step
  per search, not per letter) and a column's width (one step per drag). Not
  the selection or the scroll. In the search field, `Cmd-Z` is the field's.
- **New Notebook from This File** writes `name.ipynb` beside the file with
  the cell that loads it, in pandas or polars (`pd.read_csv("eu.csv", sep=";",
  decimal=",")`, the sheet, the table, the JSON's shape), and opens it.

## Finding

| Key | What |
|---|---|
| `Cmd-K` | The command menu: every command with its keys, open and recent notebooks, the folder's notebooks and this notebook's headings, in one list. `>` narrows to commands, `#` to headings, `/` to notebooks. Paste a link to open it. |
| `?` | Every keyboard shortcut, filterable (in command mode). |
| `Cmd-P` | Fuzzy go-to-notebook. Under 1 ms per keystroke across 12k paths. |
| `Cmd-Shift-P` | Command palette with each command's live shortcut. |
| `Cmd-F` | Find in this notebook, across sources, rendered Markdown and outputs, with highlights. `Cmd-G` goes to the next match. |
| `Cmd-Alt-F` | Find and replace in cell sources. `Return` replaces the current match and moves on; `Cmd-Return` replaces them all as one undoable step. |
| `Cmd-Shift-F` | Search every notebook in the folder. First hit in about 2 ms, streaming; 1,000 notebooks in about 60 ms. |

The sidebar tree shows each notebook's cell count (with the kernel and title
on hover). A single click opens a notebook as a preview tab, which the next
click replaces, so browsing a folder doesn't pile up tabs; double-click (or
edit) to keep it. The tree takes Finder's keys: arrows move and fold, `Space`
previews, `Cmd-Down` opens, `Return` renames in place, `Cmd-Delete` moves to
the Trash, `Cmd-D` duplicates and `Cmd-Alt-C` copies the path; right-click
offers the same, and a tab's menu closes the others. Folders show a folder
that opens with them; notebooks show a page holding Herse's brackets, and
their full names, the extension a shade lighter. A notebook that won't open
says why above the page, as selectable text with Copy, until you close it. `Cmd-1`…`Cmd-9` pick a
tab, `Cmd-Shift-T` reopens the last one closed. The tree follows the disk, and a
notebook edited by another program reloads in place when it has no unsaved
edits.

## Sharing and comments

- **Edit together, live.** Share ▸ Start Live Session serves the notebook to
  people on your network or Tailscale tailnet: send them the link (or a
  view-only one), and they join by clicking it, or by pasting it into
  File ▸ Open Link… (`Cmd-Shift-L`) or `Cmd-K`. Everyone types in the same cells at once;
  each person's selected cell shows their name in their color, and their
  initials sit in the tab strip. There is no server: the host's Herse
  serves the session, and every message is encrypted end to end with a key
  derived from the link (XChaCha20-Poly1305), so the link is the key. The
  shared document is a CRDT (yrs, in JupyterLab's real-time schema), so
  edits merge however they cross. A view-only link carries a different
  secret, and the host refuses edits and comments from anyone who joined
  with it, even from a modified client. Guests who drop off reconnect and
  catch up; the host saves the merged notebook as usual.
- **Comments.** `Cmd-Alt-M` starts a thread on the selected cell, or on the
  selected text while editing. Threads sit in a panel beside the page, with
  replies and Resolve; a cell with open threads shows their count in its
  margin. They are saved in the notebook (`metadata.herse.comments`), so
  they travel with the file.
- **Publish a link.** Share (the tab strip's share button, File ▸ Share…, or
  `Cmd-K`) publishes the notebook as a GitHub gist and gives you its
  nbviewer link, unlisted or public, with or without outputs. Before it
  goes, a scan points out secrets (API keys, tokens, passwords), your home
  folder's paths, huge outputs and saved widget state. Publishing again
  updates the same link; comments people leave on it show in the sheet, and
  you can answer them there. The account is the GitHub CLI's, or a token
  pasted once and kept in the Keychain.

## Running and editing

- **Kernels** are discovered from the Jupyter data directories in about 0.5 ms
  and launched lazily on the first run. Herse speaks the Jupyter protocol over
  its own pure-Rust ZMQ client. Started from Finder or the Dock, Herse reads
  your login shell's environment in the background (as VS Code and Zed do),
  so kernels find the same `python` (conda, pyenv, venvs) as in a terminal.
- **Problems stay put.** A kernel that won't start or dies, or a failed save,
  shows a banner above the notebook with the full error as selectable text,
  a Copy button and the fix (Choose Kernel…), until you dismiss it.
- **A notebook that won't open says why**, in a sentence, with the file's
  own text below (numbered, the bad line marked; its bytes, when it isn't
  text), and the fix when there is one: a file cut off mid-save gives back
  the cells before the cut; one with git conflict markers opens as either
  side; an empty file becomes a new notebook. Recovered cells open unsaved,
  for the file, and ⌘S writes them over it.
- **Missing packages, before they bite.** On open, Herse reads the
  notebook's imports and asks the kernel's Python which it can't find
  (`importlib.util.find_spec`: nothing is imported, ~40 ms, in the
  background). What's missing shows above the notebook in amber, with the
  exact command, **Install** (`uv pip install --python …` when you have uv,
  else `python -m pip install`, its output streaming in the banner), and,
  when another installed kernel already has everything, **Use that kernel**.
  Packages the notebook installs itself (`%pip install`) don't count.
  Settings ▸ Install Missing Packages When Running installs them on the
  first run instead of asking. Nothing installs on open: a package's build
  runs code, and opening a file must not.
- **One-click fixes.** Under an error Herse knows how to fix, a line says
  what's wrong and offers the fix: `ModuleNotFoundError` → Install and Run;
  `NameError` on a name an earlier cell defines (an import cell you
  skipped) → Run Cell 2 and This One; `NameError` on a usual alias (`np`,
  `pd`, `plt`, `sns`, `alt`, `px`…) → add `import numpy as np` and run.
- **Execution:**
  - streaming output and `input()` prompts;
  - interrupt (`I I`, about 50 ms) and restart (`0 0`);
  - run all, run above and run below;
  - per-cell timing and a kernel picker.
- **Completion** comes as you type, and `Tab` takes the suggestion (`Return`
  still starts a new line). The running kernel answers first: the names
  really defined, an object's real attributes. The notebook's own text fills
  in, and answers alone when no kernel runs: every name the notebook defines,
  the attributes it has used on each (`df.groupby`), the modules it imports,
  Python's keywords and builtins. Settings ▸ Code Completion switches to
  On Tab. Hovering shows docs from the kernel. The editor indents four spaces after a
  `:`, pairs brackets and quotes; `Cmd-/` comments or uncomments the selected
  lines, `Alt-Up` / `Alt-Down` move them, `Alt-Shift-Up` / `Alt-Shift-Down`
  copy them, `Cmd-L` selects whole lines and `Cmd-Shift-K` deletes them. The
  Mac's text keys all work: `Cmd-Delete` and `Alt-Delete` delete to the line
  or word start, `Cmd`/`Alt`-arrows move by line and word (with `Shift`, they
  select), and the Emacs keys every Mac field knows (`Ctrl-A`/`E`/`K`/`D`/
  `H`/`F`/`B`/`N`/`P`). In command mode, `Cmd-Delete` deletes the selected
  cells, `Cmd-D` duplicates them, `Z` / `Shift-Z` undo and redo, and `Cmd-A`
  selects every cell.
- **One undo history.** `Cmd-Z` goes all the way back through everything you
  did, in order: the typing in the cell you're in, then (leaving the cell)
  what you typed in other cells, added, deleted, moved or retyped cells,
  find-and-replace, cleared outputs, comments posted, answered, resolved or
  deleted, and the assistant's edits. Each visit to a cell's editor is one
  step once you've left it; `Cmd-Shift-Z` comes back.
- **`Cmd-Z` everywhere else too.** Each place undoes its own changes, from
  the keyboard or Edit ▸ Undo: a text field (search, ⌘K, a name being typed)
  its typing; a table its sort, filters, search and column widths; the file
  tree its renames, duplicates and moves to the Trash (put back from where
  the Trash put them). With nothing in front, the last file operation.
- **Click to edit.** One click on code puts the cursor where you clicked; a
  drag selects text instead. Markdown edits on a double click (at the word
  you clicked) or `Enter`, so its links stay clickable.
- **Mouse too.** Right-click a cell for its commands (run, cut/copy/paste,
  insert, move, change type, hide or clear output), each showing its key; a +
  in the margin adds a cell below, and "+ Code" / "+ Markdown" follow the last
  cell, each showing its keys (⇧⌘B and ⇧⌘M add one below the selected cell). Run All, Interrupt and Restart sit at the right of the tab strip.
- **Jupyter's modal keys.** In command mode: `J`/`K`, `A`/`B`, `M`/`Y`/`R`,
  `D D`, `X`/`C`/`V`, `Z` / `Shift-Z` (undo and redo), `Shift-M`, `O`, and
  `L` / `Shift-L` for line numbers in one cell or all of them. `Enter` and
  `Esc` switch to and from the editor. Entering edit mode moves no pixel of
  the text. The Mac's own keys work too: `Cmd-C` / `Cmd-X` / `Cmd-V` /
  `Cmd-Z` / `Cmd-Shift-Z` copy, cut, paste, undo and redo cells, and `Cmd-A`
  selects every cell (`Cmd-C` copies selected text instead when there is
  some, and text copied elsewhere pastes as a new code cell).
- **Saving:**
  - Auto-saves 1 s after you stop typing, but never over a file changed
    elsewhere.
  - Saves are atomic.
  - Saves are diff-clean: all 340 Jupyter-written notebooks in the test corpus
    re-save byte for byte.

Light and dark follow the system; `Cmd-Alt-T` switches between them, and
Settings (`Cmd-,`) sets the appearance, text size and line numbers. Every
command is in the menu bar beside its shortcut, including a full Edit menu
whose verbs act on text in a field and on cells in command mode. The
design language (a notebook is a page: warm paper and ink, Markdown set in
Charter, machinery in the margin, and one accent, dew, for where you are and
what is live) is described in [DESIGN.md](DESIGN.md).

## Extensions

Programs in any language can add commands to ⌘K, draw output types Herse
doesn't know (as SVG, PNG, Markdown or text), hear when a notebook opens,
saves or runs a cell, and read and change the notebook through the same
operations the assistant has. An extension is a folder with a
`herse-extension.json` in `~/Library/Application Support/Herse/extensions/`,
speaking JSON-RPC over stdin and stdout. There's no store yet. See
[EXTENSIONS.md](EXTENSIONS.md), and
[examples/extensions/notebook-tools](examples/extensions/notebook-tools) for a
complete one in plain Python.

## Asking an assistant

Herse asks the coding agent you already pay for, Claude Code or Codex, with
its own sign-in and plan. It is not a chat window:

- **Ask (⌘I), wherever you are.** A box in the middle of the app, like
  Spotlight. Type a question or a request about what's in front and press
  Return; Tab switches between Claude and Codex. The sparkle button at the
  top right opens it too, and typing a sentence into ⌘K offers "Ask Claude".
  - On a notebook, it reads, edits and runs cells.
  - On a table (a CSV, Parquet, Excel… file, or a DataFrame opened full
    size), it reads the table as Herse read it, without changing it:
    describe it, read rows, find rows with the viewer's filters, group and
    add up ("Which region sold the most units?").
  - With nothing open, it answers about the folder in the sidebar by reading
    its files.
- **Texts in the corner.** The agent's replies arrive as message bubbles at
  the bottom right, its steps ("Reading cells 2–4", "Running cell 4") as
  quiet lines between them. Reply there to follow up (the conversation
  continues); Stop ends a run and interrupts the cells it started, − tucks
  it into a pill.
- **The notebook follows it.** Every cell the agent reads, edits or runs is
  selected and scrolled to, with a dew ring and a label ("Codex is running
  this") while it works there. Its edits are ordinary edits: one undo step
  each, saved like yours.
- **Running code asks first.** Running cells, or evaluating code in the
  kernel, can do anything your account can, so the agent's runs wait for
  you: a card in the bubbles shows the code with Allow, Allow for this
  conversation, and Deny. A denial goes back to the agent as an error it
  reads. Edits don't ask, since you can undo them.
- **What it sees.** What you ask, and the notebook's code and outputs (data
  included), go to Anthropic or OpenAI through your own sign-in. The Ask box
  says so once, the first time; a first question from ⌘K waits behind the
  same notice.
- **How.** Herse runs the CLI headless (`claude -p … --output-format
  stream-json`, `codex exec --json`) with one MCP server: Herse itself
  (`herse --mcp-bridge <socket>`), which relays to the app over a private
  Unix socket. The tools: overview, read_cells, edit_cell, insert_cell,
  delete_cell, run_cells, and evaluate (run Python in the kernel without
  adding a cell, to look at data). Claude also gets read-only Read, Glob and
  Grep; Codex keeps its read-only sandbox. The agent never writes the .ipynb
  itself.
- **Settings.** Assistant (⌘I) picks the default; `assistant_models` in
  `settings.json` (`{"claude": "sonnet", "codex": "gpt-6-luna"}`) picks a
  model; `assistant_notice_seen` records that the notice was shown.
  Sign-in, rate limit and unavailable-model failures say how to fix them.

See [TODO.md](TODO.md) for what still needs checking before this ships.

## The `herse` command

Herse ▸ Install Command Line Tool… (or ⌘K) links `herse` into the first of
`~/.local/bin`, `~/bin`, `/opt/homebrew/bin` or `/usr/local/bin` that's on
your PATH and needs no password; the command itself ships inside the app.

```text
herse [PATH...]                       open files and folders in Herse ("herse ." for this one)
herse cat FILE [--no-outputs]         a notebook as it reads, or a data file's columns and first rows
herse run NOTEBOOK [-o OUT] [--kernel NAME] [--timeout SECONDS] [--keep-going]
                                      Restart and Run All in the notebook's kernel, then save
herse export NOTEBOOK --to html|py|md [-o FILE]
```

`run` clears every output first, runs the code cells top to bottom, stops at
the first error (printing its traceback, keeping the outputs so far) unless
`--keep-going`, and writes the notebook back in its own formatting; it exits
1 when a cell failed. `export --to html` is the page Quick Look shows; `py`
is a script with `# %%` cells; `md` keeps outputs as text and images inline.

## Layout

```
crates/
  nb/         herse-nb         notebook model, single-pass parser, byte-exact writer
  kernel/     herse-kernel     kernelspec discovery, launch, Jupyter protocol (pure-Rust ZMTP)
  workspace/  herse-workspace  folder walk, notebook summaries, fuzzy index, search, watcher, recents
  rich/       herse-rich       ANSI, JupyterLab stream semantics, LaTeX math -> SVG
  table/      herse-table      DataFrame / HTML table -> grid model
  widgets/    herse-widgets    ipywidget state (saved or live comms) -> widget tree
  plot/       herse-plot       Plotly / Vega-Lite (Altair) -> static SVG
  html/       herse-html       a notebook -> one self-contained HTML page (Quick Look, export)
  data/       herse-data       data files and DataFrame outputs -> one streaming frame; sort, filter, search, export
  quicklook/  herse-quicklook  the Quick Look extension's renderer (C ABI over herse-html)
  cli/        herse-cli        the `herse` command: open in the app; cat, run, export
  app/        herse            the GPUI application (frame_view.rs: tables full size; agent/: the assistant, its CLIs and MCP bridge;
                               notebook/preflight.rs: missing packages and one-click fixes;
                               extensions.rs: the extension host; broken.rs: notebooks that won't open)
vendor/                        small patches to GPUI Kit (see vendor/README.md)
packaging/macos/               Info.plist, icon, bundle.sh, QuickLook/ (the preview extension)
fixtures/                      generated test notebooks (fixtures/README.md)
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the crate contracts. Every crate
except `app` is plain Rust with its own tests:

```bash
cargo test --release -p herse-nb -p herse-kernel -p herse-workspace -p herse-rich -p herse-table -p herse-widgets -p herse-plot -p herse-html -p herse-quicklook -p herse-data
```

## Development

The dev profile optimizes dependencies (`opt-level 3`) so GPUI is usable, and
keeps the workspace crates quick to rebuild.

`herse-snap` renders the real app offscreen through Metal and writes PNGs, so
you can check UI work without a visible window, even with the screen locked:

```bash
cargo run --release -p herse --features snapshot --bin herse-snap -- \
  --out /tmp/shot.png --keys "j j|cmd-f p r i n t" --wait 2000 --bench fixtures/showcase.ipynb
```

- `--keys` takes GPUI keystrokes, and `|` separates frames (`shot-0.png`, `shot-1.png`, ...).
- `--wait MS` lets real work finish, such as a kernel booting.
- `--bench` prints open, redraw and scroll-frame wall time and main-thread CPU
  time. The CPU number is the one to trust on a busy machine.
- `--dark` renders in dark mode, and `--size WxH` sets the window size.

## Credits

- GPUI (Zed Industries) and GPUI Kit (Longbridge), Apache-2.0.
- The math engine is a vendored, patched latex-rust (MIT/Apache-2.0).
- The STIX Two Math font is SIL OFL.
- Lucide icons.
