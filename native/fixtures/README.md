# Herse Native fixtures

A realistic corpus of Jupyter notebooks for checking **rendering correctness** and **speed** in Herse Native.
Everything is produced by `generate.py`: notebooks are *executed* with `nbclient` (outputs are exactly what a
Jupyter kernel stores), written with `nbformat`, and validated with `nbformat.validate`.  A few are hand-built to
cover formats a kernel never produces (old nbformat versions, Colab / VS Code JSON styles, corrupt files).

## Regenerate

```sh
/opt/anaconda3/bin/python3 fixtures/generate.py          # small set: 1-3 min (kernels are started for 6 notebooks)
/opt/anaconda3/bin/python3 fixtures/generate.py --big    # small set + big/ benchmark set: +5-10 min (big/ is git-ignored)
/opt/anaconda3/bin/python3 fixtures/generate.py --only showcase,plots   # a subset (see --list)
```

Every generated notebook is checked with `nbformat.validate` at the end (a duplicate or missing cell id counts as a failure);
the intentionally broken ones (`corrupt_*`, `nbformat_v3`, `duplicate_cell_ids`) are exempt.  Exit status is non-zero on failure.

Needs `nbformat`, `nbclient`, `ipykernel` (kernel `python3`), `numpy`, `pandas`, `matplotlib`.  Optional libraries
(`seaborn`, `plotly`, `altair`, `bokeh`, `ipywidgets`, `tqdm`, `sympy`, `scipy`, `PIL`, `rich`) are used when installed and skipped otherwise.
The script only writes below its own directory.  Executed notebooks contain real timestamps / temp paths / timings,
so regenerating changes them; the content is otherwise deterministic (fixed seeds, fixed cell ids).

Notes
- `workspace/.gitignore` ignores `ignored_dir/` **for git too**, and many repo-level ignore files drop `.ipynb_checkpoints/`:
  those directories are fixture content but will not be tracked; `generate.py` recreates them.
- Stream outputs are merged per cell (consecutive same-name chunks joined), like JupyterLab does when saving.
  `\r` / `\b` are stored raw (progress bars!).
- A startup hook flushes matplotlib figures created without `plt.show()` (matplotlib 3.10 + matplotlib-inline 0.1.6 stopped doing it),
  so a bare `plt.plot(...)` cell yields the usual `execute_result` + PNG pair.

## Small set (checked in)

| File | Size | What it exercises |
|---|---:|---|
| `dataframes.ipynb` | 383 KB | pandas display zoo: 50 columns, 1000 rows truncated at 60, 400-row HTML table, MultiIndex rows/columns, NaN/NA/NaT, datetime index, Styler gradients/bars, describe(), Series, nasty cell contents. |
| `errors.ipynb` | 443 KB | Every failure shape: SyntaxError/IndentationError, NameError, 15-frame stack, recursion (frame elision), real SIGINT KeyboardInterrupt, 20k-char and 150-line messages, stderr-then-fail, failing %%bash, chained/custom exceptions, SystemExit, ExceptionGroup (stderr only), plus hand-written error outputs (empty/plain/single-string tracebacks). |
| `plots.ipynb` | 6.9 MB | ~30 matplotlib charts (line/scatter/hist/bar/pie/box/violin/3D/subplots/heatmap/polar/retina/dark/CJK warnings/animation) plus seaborn, plotly (4.5 MB bundle in first output), altair (vegalite JSON) and bokeh (bokehjs_exec JSON). |
| `showcase.ipynb` | 409 KB | Everything in one notebook: headings, lists, tables, math, attachments, HTML, all IPython.display types, matplotlib PNG/SVG, DataFrames, ANSI, progress bars, tracebacks, long lines, unicode, empty/raw/unexecuted cells. |

## data/ (data files, for herse-data and the table view)

Made by `data/make.py` (deterministic; `--big` adds `big/data/sales-1.1m.csv`,
100.6 MB, and its Parquet, for benchmarks). `sales.*` is one 2,000-row table
in every format (CSV, TSV, Parquet, Feather, JSON as records / columns /
split / wrapped, JSON Lines with a late key and a bad line, an Excel workbook
of two sheets, SQLite with two tables and a view); `europe.csv` uses `;` and
decimal commas. `edge/` holds what must open or say why without a crash:
empty, header-only, ragged, Latin-1, BOM, UTF-16, quoted newlines, an
unclosed quote, 300 columns, a 200 KB cell, gzip, and broken Parquet, JSON,
xlsx and SQLite files.

## formats/ (hand-built edge cases)

| File | Size | What it exercises |
|---|---:|---|
| `formats/cell_metadata.ipynb` | 7.1 KB | Cell metadata zoo: tags (hide-input/hide-output/remove-cell/parameters), jupyter.source_hidden/outputs_hidden, collapsed/scrolled, slideshow, editable, papermill. |
| `formats/colab_style.ipynb` | 12.5 KB | Google Colab export: indent=2, unsorted keys, no trailing newline, `metadata.id`/`outputId`/`colab`, pip `\r\x1b[K` progress, intrinsic+json mime, colab-df html, #@title form cell. |
| `formats/corrupt_binary.ipynb` | 2.0 KB | Binary garbage (not UTF-8, not JSON). |
| `formats/corrupt_empty_file.ipynb` | 0 B | A zero-byte file. |
| `formats/corrupt_json_array.ipynb` | 15 B | Valid JSON whose root is an array. |
| `formats/corrupt_missing_cells.ipynb` | 58 B | Valid JSON but no `cells` key. |
| `formats/corrupt_not_json.ipynb` | 65 B | A git merge-conflict marker file masquerading as a notebook. |
| `formats/corrupt_truncated.ipynb` | 569 B | A valid notebook cut off in the middle of a string literal (interrupted save / bad sync). |
| `formats/corrupt_wrong_types.ipynb` | 217 B | Valid JSON, wrong shapes: bad cell_type, non-string source, non-list outputs, non-object cells. |
| `formats/crlf_file_endings.ipynb` | 965 B | The JSON document itself is CRLF-terminated (every pretty-print newline is `\r\n`). |
| `formats/crlf_in_sources.ipynb` | 1.7 KB | Sources and stream text containing `\r\n` (and lone `\r`) line endings, as string and as list-of-lines. |
| `formats/duplicate_cell_ids.ipynb` | 1.4 KB | nbformat 4.5 with duplicate cell ids (invalid per spec, common in the wild). |
| `formats/empty_0_cells.ipynb` | 453 B | Valid notebook with zero cells (kernelspec + language_info). |
| `formats/empty_minimal.ipynb` | 72 B | Zero cells and empty metadata (no kernelspec). |
| `formats/jupytext_paired.ipynb` | 2.0 KB | Jupytext-paired notebook: `metadata.jupytext` formats/text_representation, lines_to_next_cell cell metadata. |
| `formats/jupytext_paired.py` | 590 B | The text (percent) half of the Jupytext pair; not a notebook. |
| `formats/kernel_julia.ipynb` | 6.9 KB | Julia (IJulia) notebook: kernelspec `julia-1.10`, DataFrames.jl html, svg plot, Julia stacktrace. Not executed. |
| `formats/kernel_r.ipynb` | 6.4 KB | R (IRkernel) notebook: kernelspec `ir`, text/latex + text/markdown + text/html data.frame, plain-R error. Not executed. |
| `formats/mime_data_variants.ipynb` | 20.1 KB | Every shape of an output `data` bundle: png as string/list/wrapped, needs_background, jpeg, gif, svg list, isolated html, json, latex, markdown, pdf, js, unknown-only, empty, vegalite, geojson. |
| `formats/minified_single_line.ipynb` | 3.2 KB | The entire notebook on a single line, no whitespace, no trailing newline. |
| `formats/nbformat_4_0_no_ids.ipynb` | 5.0 KB | nbformat 4.0 (Python 2 era): no cell ids, `collapsed` cell metadata, Python-2 traceback, trailing empty cell. |
| `formats/nbformat_4_4_no_ids.ipynb` | 3.6 KB | nbformat 4.4 (Notebook 6): no cell ids, ExecuteTime / toc / varInspector nbextension metadata. |
| `formats/nbformat_4_5_with_ids.ipynb` | 1.5 KB | nbformat 4.5 with cell ids (1-char, 64-char, dashes/underscores). |
| `formats/nbformat_4_9_future_minor.ipynb` | 1.0 KB | nbformat 4.9 (minor newer than any released): must still open; unknown keys at every level. |
| `formats/nbformat_v3.ipynb` | 4.2 KB | nbformat 3: `worksheets`, heading cells, `input`/`prompt_number`, `pyout`/`pyerr`, png/html as top-level output keys. Needs an upgrade path. |
| `formats/output_text_single_string.ipynb` | 2.2 KB | Stream `text` and `data` values stored as single strings instead of line lists. |
| `formats/source_single_string.ipynb` | 1.3 KB | `source` stored as one string (not a line list) for markdown, code and raw cells. |
| `formats/tab_indent_reverse_keys.ipynb` | 1.2 KB | Tab-indented JSON with reverse-alphabetical keys (`nbformat` first, `source` before `cell_type`). |
| `formats/utf8_bom.ipynb` | 945 B | File begins with a UTF-8 BOM (strict JSON parsers reject it; strip and continue). |
| `formats/vscode_style.ipynb` | 4.0 KB | VS Code export: indent=1, nbformat 4.2 (no ids), `orig_nbformat`, `vscode.interpreter.hash`, per-cell `languageId`. |
| `formats/weird_but_valid.ipynb` | 7.1 KB | Valid oddities: empty markdown/code, huge execution_count, newline-only output, unmerged consecutive streams, whitespace-only markdown. |
| `formats/widgets.ipynb` | 69.5 KB | Executed ipywidgets (slider, button/dropdown/text boxes, Output, Tab, interact, tqdm.notebook): widget-view+json outputs AND widget-state+json in metadata.widgets. |
| `formats/widgets_no_state.ipynb` | 6.9 KB | Same widget-view outputs but `metadata.widgets` removed: apps must fall back to text/plain. |

## workspace/ (file tree, natural sort, search)

| File | Size | What it exercises |
|---|---:|---|
| `workspace/` | 85.0 KB (34 .ipynb, 43 files) | ~30 notebooks in a 3-level tree: zero-padded (01-, 10-), `nb1/nb2/nb10`, `Untitled/Untitled1/Untitled10`, `clean-v1/v2/v10`, `trial-1/2/10`, `exp-01/02/10`, unicode names, spaces, dot-files, `.ipynb_checkpoints/` copies, non-notebook .py/.md/.csv/.log, and a `.gitignore` excluding `ignored_dir/`. |

## Big set (`--big`, generated on demand, not checked in)

| File | Size | What it exercises |
|---|---:|---|
| `big/cells_5000.ipynb` | 2.2 MB | 5,000 cells (~25% markdown), every code cell with a small output (stream/result/html/latex/markdown, tiny PNG every 100th, an error every 150th). Hand-built, no kernel. |
| `big/images_300.ipynb` | 55.4 MB | 300 executed matplotlib PNG outputs (scatter / noise heatmap / random walks / hist2d / contour at 900x600), tens of MB. |
| `big/long_output.ipynb` | 46.7 MB | Stress outputs: 200,000-line stream, one 20 MiB stream (with a 1 MB line), 10,000 alternating stdout/stderr outputs, 5M-char line, 200k-element list repr, 10,000 display_data, 20,000 `\r` updates. |
| `big/workspace_2000/` | 8.8 MB (2080 .ipynb, 2141 files) | 2,000 notebooks (plus ~80 checkpoint copies, README/.py/.csv files) in team/project/analysis/run-NN directories (4 levels): perturbed copies of the small fixtures, mixed nbformat 4.2/4.4/4.5, natural-sort name traps, `needle_big_*` search tokens (~1% each in code / markdown / outputs). |

### Search tokens in `workspace/`

| Token | Where it hides |
|---|---|
| `needle_code` | code cells of `01-intro`, `02-data-loading`, `data/explore-sales` (and `src/plotting.py`) |
| `needle_markdown` | markdown cells of `01-intro`, `10-final-report` (and `data/cleaning/notes.md`) |
| `needle_output` | cell *outputs* only: `nb10`, `data/cleaning/clean-v10` |
| `needle_checkpoint` | only in `.ipynb_checkpoints/` copies (should normally be skipped) |
| `needle_ignored` | only in `ignored_dir/` (hidden by `workspace/.gitignore`) |
| `Revenue` / `REVENUE` / `revenue` | case-sensitivity checks (`01-intro`, `03-cleaning`, code cells everywhere) |
| `learning_rate` | regex / identifier search (`01-intro`, `experiments/exp-02/tuning`, many snippets) |

In `big/workspace_2000/` the tokens are `needle_big_code`, `needle_big_markdown`, `needle_big_output` (roughly 1% of notebooks each).

## Environment this set was generated with

| Library | Version |
|---|---|
| seaborn | 0.13.2 |
| plotly | 5.24.1 |
| altair | 5.0.1 |
| bokeh | 3.6.2 |
| ipywidgets | 8.1.5 |
| tqdm | 4.67.1 |
| sympy | 1.13.3 |
| scipy | 1.15.3 |
| PIL | 12.2.0 |
| rich | 15.0.0 |
| pyarrow | 21.0.0 |
| vega_datasets | not installed (related cells skipped) |
| jupytext | not installed (related cells skipped) |
| folium | not installed (related cells skipped) |
| ipympl | not installed (related cells skipped) |

Generation time on the author's machine: showcase 52s, dataframes 23s, plots 177s, errors 35s, formats 11s, workspace 1s, big-cells 20s, big-images 325s, big-output 347s, big-workspace 49s.
