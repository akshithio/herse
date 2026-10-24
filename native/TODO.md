# To check and do properly

What's built but unverified, the decisions it leaves open, and what comes
next. Newest work first.

## Tables: data files and DataFrames full size

Built (items 1 and 2 of the data roadmap below): crate `herse-data` (readers,
the chunked frame, sort / filter / search, stats, CSV / Markdown / TSV export,
loading cells) and `frame_view.rs` (the table tab). Data files in the sidebar
with icons and sizes; ⌘O, drag, Finder's Open With and restored tabs open
them. DataFrame outputs get an expand chip; Load All Rows asks a live kernel
for the whole frame (`notebook/frame_fetch.rs`). Tested with herse-snap
(sort, filter, search, stats card, resize, sideways scroll, every reader, the
error banner, dark mode, New Notebook, Load All Rows against a real kernel)
and a crash sweep of every fixture (102 files, with clicks and keys on the
data ones). The generated loading cells were run in pandas and polars for
every fixture format.

**Reader decision.** arrow-rs (`parquet`, `arrow-ipc`, `arrow-cast`) + the
`csv` crate + calamine + the system's SQLite, not DuckDB. Measured on this
machine, a reader that only counts rows: DuckDB (`bundled`, parquet and json)
cost 1,123 s of CPU to build (4 min 11 s wall) and a 45 MB binary, and it
pulls in arrow anyway; the arrow stack cost 161 s of CPU (36 s wall) and
6.7 MB. DuckDB also reads Excel and SQLite only through extensions it
downloads at run time. The cost of not having it: no SQL (roadmap item 4,
where DuckDB would earn its build time, likely as an optional feature).

**Speed** (M2 Max, release; `cargo run --release -p herse-data --example open`):
100.6 MB CSV, 1.1 M rows × 12 columns: first 512 rows in 0.8–1.0 ms, all in
0.56–0.61 s (~170 MB/s, one thread); in the app, first frame 20–26 ms and
redraw 1.5 ms / scroll frame 2.4 ms CPU while it reads. Parquet of the same
rows (24 MB): first 8,192 rows in 12 ms, all in 0.50 s. Sort 1.1 M rows: 44–98
ms (numbers, categories, unique text); search 40 ms. Peak memory 307 MB with
that CSV open (97 MB baseline): the text is kept as read, plus offsets and
parsed numbers, ~2.1× the file.

**Interactive table** (follow-up: "the csv viewer isn't very nice &
interactive"). A profile strip under every header (histograms for numbers
and ISO date / time text, top values for text, true/false for booleans,
the missing share), with a hover card and click-to-filter (again to clear);
filtered views overlay the rows in view in dew. Rows tint under the pointer.
Ranges by drag, shift-click, shift-arrows and ⌘A, summed in a footer (count,
Sum, Mean, Min, Max; click one to copy) with ⌘C as TSV; double-click or Space
shows a cell's whole value. The footer says "191 of 2,000 rows" with Clear.
Measured on the 1.1 M-row CSV: profiles 141 ms and a filtered view's 45 ms
(both in the background; filtered rows show first, their profiles follow);
bar click to rows 39 ms; redraw 2.8 ms (2.3 ms CPU, up from 1.5 ms without
the strip). Tested in herse-snap (hover cards, filter and clear by bar,
footer Clear, drag, shift-arrows, the 200,000-character cell, dark) and
the crash sweep with these gestures added.

**Undo in the table.** `frame::UndoView` / `frame::RedoView` (⌘Z / ⌘⇧Z in
the FrameView context; the search and filter fields keep their own ⌘Z):
sort, filters (typed, from a bar, edited, removed), Clear, search (one step
per focus of the field), column widths (one step per drag), up to 200 steps.
New data (another sheet, a reload, Load All Rows) forgets the steps, since
they name columns by position. "Nothing to undo" / "Nothing to redo" flash
in the meta line, as do "Undid the sort" and the like.

### Verify by hand

- [ ] Finder: Open With › Herse on a CSV and a Parquet file (needs a rebuilt
      bundle for the new Info.plist document type), and a drag onto the window.
- [ ] Load All Rows on your own notebooks: a polars frame, a Styler, a
      MultiIndex frame, a frame of a few million rows (the kernel writes
      Parquet; ~1 s per million rows), and a kernel without pyarrow (CSV).
- [ ] A real-world messy CSV or two (quoted newlines, mixed encodings), and a
      multi-GB file: memory grows ~2× the file; there is no cap yet.

### Decisions left open

- [ ] Load All Rows identifies the frame by `Out[N]` or the cell's last name
      (`df`, `df.head()` → `df`). A cell ending in an expression
      (`df.groupby(…).sum()`) only works while `Out[N]` lives; Herse never
      re-runs code to get it. Good enough?
- [ ] Copy takes at most 100,000 rows. Offer "Save as CSV…" for more?
- [ ] Very large files: keep everything in memory (now), or read Parquet and
      Arrow lazily by row group as you scroll?

### Make it better

- [ ] Brush a histogram (drag across bars) to filter to a range; shift-click
      top values to filter to several.
- [ ] Profiles while a big file is still being read (now they come when it's
      done), and a cheaper strip redraw (cache the shaped labels).
- [ ] The table's footer and the workspace's status line stack on table tabs;
      fold the footer into the status line if that reads better.
- [ ] Filters on several values (`North, South`), dates as dates, regex.
- [ ] Multi-column sort (shift-click); remember sort, filters and widths per
      file.
- [ ] Select whole rows and columns (click the gutter or a name) and copy
      just those.
- [ ] Parallel CSV parsing (split at line starts) for files over ~200 MB.
- [ ] Open the table beside the notebook (split view) instead of a tab.

## The assistant (⌘I, Claude Code / Codex)

Built: the Ask box, texts in the corner, the notebook following the agent,
Herse's MCP tools (overview, read_cells, edit_cell, insert_cell, delete_cell,
run_cells, evaluate), Claude and Codex as headless CLIs. Tested end to end
with Codex (it read the notebook, fixed a cell, ran it); the Claude path was
exercised up to the API call.

### Before shipping

- [ ] **Terms of use.** Herse runs the user's own installed `claude` / `codex`
      CLI, signed in by the user with Anthropic or OpenAI; Herse never sees a
      token. Anthropic's Agent SDK docs ask third-party developers not to
      offer claude.ai login or rate limits in their products without approval.
      Ask Anthropic whether launching the user's own Claude Code this way is
      fine for a product, or get approval. Check OpenAI's terms for Codex with
      ChatGPT plans the same way.
- [ ] **An API-key path** as the fallback that's always allowed:
      `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` (Claude Code and Codex both
      accept them), entered once and stored in the Keychain.
- [x] **Running code needs a decision.** run_cells and evaluate now wait for
      the reader: a card in the bubbles shows the code (by cell, or the
      evaluate snippet) with Allow, Allow for this conversation, and Deny. The
      tool call's reply waits for the answer; Deny returns an error the agent
      reads ("tell them what it would do"). Edits don't ask. The CLIs' MCP
      timeouts are a day (`tool_timeout_sec`, `MCP_TOOL_TIMEOUT`) so a slow
      answer isn't a timeout. Checked with Codex in herse-snap: the cell stays
      unrun for 40 s until Allow, then runs and Codex reads its output; Deny
      and "for this conversation" work; evaluate shows its code.
- [x] **Privacy notice.** The Ask box says once what goes where ("Codex sees
      what you ask and this notebook's code and outputs, data included: they
      go to OpenAI…"), until a question is asked with it showing
      (`assistant_notice_seen` in settings.json). A first question from ⌘K,
      which skips the Ask box, waits in the bubbles behind the same notice
      with "Ask Codex" / Cancel.
- [x] **Stop should stop the cells too.** Each run_cells and evaluate records
      its execute requests (msg ids, so never a run of the reader's); Stop,
      closing the conversation mid-run, or replacing it interrupts the kernel
      when one of them is queued or running, and the agent's waiting calls
      end. Cells held for a package install are interrupted as they start.
      Checked: a 120 s cell stopped at 30 s with KeyboardInterrupt.

### Verify by hand

- [ ] **Approval with Claude.** Claude can call tools in parallel: two runs
      should show two cards, and "Allow for this conversation" should let both
      go. Check `MCP_TOOL_TIMEOUT` keeps a call waiting past a few minutes.
- [ ] **Stop while the reader's own cell runs ahead of the agent's.** The
      interrupt hits whatever the kernel is running, so the reader's cell is
      interrupted too (Jupyter can't drop one queued request). Decide whether
      Stop should wait for the reader's cell instead.

- [ ] **Claude end to end in the real app.** Claude Code's saved sign-in
      is broken on this Mac: `claude -p` gets "401 OAuth access token is
      invalid" from an empty environment too, in Herse and outside it, while
      `claude auth status` (which doesn't check the token) says you're
      signed in. Run `claude auth login` in Terminal, then launch Herse from
      the Dock, open a notebook, press ⌘I, ask "Cell 3 fails. Fix it and run
      it."
- [ ] **Codex once answered without Herse's tools** ("no Herse tools are
      available"), in herse-snap, 1 run of about 9 that day; the next run
      had them. Probably Codex starting its turn before the bridge
      answered. Check whether it happens in the real app; if it does, have
      Codex wait for the server (`startup_timeout_sec`, or fail the run when
      `herse` isn't connected).
- [ ] **Codex's default model.** Your `~/.codex/config.toml` has
      `model = "gpt-6.1-sol"`, which ChatGPT accounts can't use; Codex refuses
      every request. Herse says so, with a hint. Change it there, or set
      `"assistant_models": {"codex": "gpt-6-luna"}` in Herse's settings.json.
- [ ] Launched from Finder: both CLIs are found (`~/.local/bin/claude`, Volta's
      `codex`) through the login shell's PATH.
- [ ] Dark mode: bubbles, the Ask box, the agent's ring and label.
- [ ] A 500-cell notebook (tool replies clip at 24k characters; check the agent
      copes), a notebook with no kernel, a Markdown-only one.

### Make it better

- [ ] Model picker in Settings (now only `assistant_models` in settings.json).
- [ ] Let the agent see charts: return figures as MCP image content, not
      `[image]`.
- [ ] Stream partial text (`--include-partial-messages`) so replies type out.
- [ ] Review what it changed: "Codex changed 3 cells" with per-cell diff and
      Undo, using the changes view.
- [ ] Keep conversations per notebook and resume them (`--resume`, `codex exec
      resume`) after closing.
- [ ] Questions across the folder while a notebook is open (other notebooks,
      data files). ⌘I already works on a table and, with nothing open, on the
      folder.
- [ ] On a table, let the assistant show its answer in the viewer: sort,
      filter or select the rows it found, as the notebook follows its edits.
- [ ] The bridge is a Unix socket: Windows and Linux builds need their own.

## Extensions

Built: the host (folder discovery, JSON-RPC processes started on demand),
commands in ⌘K, renderers for unknown output types (cached), events
(`notebook.opened`, `notebook.saved`, `cell.executed`), the notebook API (the
assistant's operations plus `notebook.get` and `ui.message`), EXTENSIONS.md,
and an example in Python. Tested with the example: its command, its table of
contents edit, its renderer.

Built: the trust step. Nothing in an extension runs until it's allowed once:
a banner ("Notebook Tools wants to run: python3 main.py", in its folder) with
Allow, Don't Allow and Show in Finder, remembered in settings.json by folder
and a SHA-256 of the manifest plus the files `run` names, so a changed
extension asks again (checked again before each start, not only at launch).
Until allowed, its ⌘K commands ask when chosen, its outputs show a note with
Allow, and it hears no events. Settings ▸ Extensions… lists each with
Allowed / Not allowed and allows or revokes. Tested with herse-snap.

- [ ] The fingerprint covers the manifest and the run file only: an
      extension whose `main.py` imports a sibling module can change that
      module without asking. Hash the whole folder (with a size cap) before
      recommending other people's extensions?
- [ ] Check the trust banner in the real app from Finder (herse-snap only so
      far), including two windows open at once (each shows the question).
- [ ] Freeze API 1 (names, numbering from 1, result shapes) once a second
      real extension exists; version changes after that.
- [ ] More to hook into: panels or sidebars of an extension's own, settings,
      keyboard shortcuts for commands, kernel messages, cell toolbar buttons,
      export formats.
- [ ] A Bokeh renderer as an extension (headless browser → PNG) would close
      the last common "opens in the browser" output.
- [ ] Later: a catalog to find and install extensions in the app.

## Missing packages and one-click fixes

Built: import scan on open, the amber banner with Install (uv, else pip) and
"Use <kernel>" when another kernel has everything, install-on-run setting,
fixes under `ModuleNotFoundError` and `NameError`. Tested against a scratch
venv: banner, install, each fix, auto-install.

- [ ] Try it on your real environments: conda base, the `robustness` env, a
      pyenv or Homebrew Python. Homebrew's Python is "externally managed"
      (PEP 668): installs fail with a clear error, but Herse should offer the
      way out (next item).
- [ ] **Opinionated: a Herse Python.** On first run with no usable kernel (or
      an externally managed one), create one uv environment with the data
      stack preinstalled (numpy, pandas, polars, pyarrow, matplotlib, seaborn,
      plotly, altair, scikit-learn, duckdb, ipykernel) and register it as a
      kernel. Zero setup, fast (uv), shared across notebooks.
- [ ] Grow the import → package table (`sklearn` → `scikit-learn`, …) from
      the 414-notebook corpus, or ask PyPI when unsure.
- [ ] Run what a cell depends on: before running cell N, find the earlier
      cells that define the names it uses and haven't run in this kernel, and
      offer (or, opinionated, just do) "run those first".
- [ ] After installing into a running kernel, a package that was half
      imported needs a restart: detect it and offer Restart.
- [ ] Install-on-run installs whatever a notebook imports; a downloaded
      notebook could import a typosquat. Show the list once per notebook even
      with the setting on.

## Charts

Built: Plotly and Altair blend with the page; Matplotlib gets Herse's style
(transparent, retina, ink, dew palette) through a `matplotlibrc` for Python
kernels.

- [ ] Your Altair example (faceted histogram with `.facet("origin:N")`) and
      seaborn, plotnine, `plotly_dark`, a figure with its own background.
- [ ] Bokeh isn't drawn natively yet (it opens in the browser); your
      `NameError: np` example now gets a fix button, but the chart itself
      still needs Bokeh rendering.
- [ ] Quiet known library warnings (Altair's FutureWarning on stderr).

## Check by hand (couldn't from here)

- [ ] Quick Look: press Space on a notebook in Finder. The extension is
      registered but you set it to ignored (`pluginkit -e ignore -i
      dev.herse.app.quicklook`); `pluginkit -e use …` turns it back on.
- [ ] A live session between two Macs.
- [ ] Publishing to GitHub from Share, with your account.

## Data analytics: what Herse needs to be more than a viewer

In rough order of what it gives per day of work:

1. ~~**A real DataFrame viewer.**~~ Built: see "Tables" at the top.
2. ~~**Open data files directly.**~~ Built, with arrow-rs and friends rather
   than DuckDB (why: see "Tables").
3. **Variable explorer.** The kernel's DataFrames, arrays and models with
   shape, dtype and memory; click one to view it.
4. **SQL cells** (DuckDB) over files and DataFrames, results as DataFrames.
5. **Profiling in one click.** Per-column histograms, nulls, distinct counts
   for any DataFrame.
6. **Chart from a DataFrame.** Pick x, y, color: Herse writes the Altair (or
   Plotly) cell.
7. **The assistant, for data.** Tools to describe a DataFrame and to see
   charts; "explain this chart", "clean this column", "why is this NaN".
8. **Reports.** Export to PDF and slides; parameters and scheduled reruns
   (papermill-style) for notebooks that are reports.
