#!/usr/bin/env python3
"""Herse Native fixture generator.

Builds a realistic corpus of Jupyter notebooks for checking rendering
correctness and speed.  Notebooks are *executed* with nbclient (so outputs are
exactly what a Jupyter kernel stores) and written with nbformat.  Hand-built
notebooks cover formats nbclient never produces (old nbformat versions, Colab /
VS Code style JSON, corrupt files, ...).

    python3 fixtures/generate.py            # small, checked-in set
    python3 fixtures/generate.py --big      # small set + big/ benchmark set
    python3 fixtures/generate.py --only showcase,plots   # subset (dev loop)
    python3 fixtures/generate.py --list     # list the build steps

Requires: nbformat, nbclient, ipykernel (kernel name "python3"), numpy, pandas,
matplotlib.  Optional (skipped gracefully when missing): seaborn, plotly,
altair, bokeh, ipywidgets, tqdm, sympy, scipy, rich, PIL.

Only files under this script's directory are ever written.
"""
from __future__ import annotations

import argparse
import base64
import copy
import hashlib
import io
import json
import os
import random
import re
import shutil
import sys
import tempfile
import textwrap
import time
import warnings
from pathlib import Path

ROOT = Path(__file__).resolve().parent
BIG = ROOT / "big"

# Make kernel output reproducible where possible.
os.environ.setdefault("PYTHONHASHSEED", "0")
os.environ.setdefault("PYTHONWARNINGS", "")

import nbformat  # noqa: E402
from nbformat.v4 import (  # noqa: E402
    new_code_cell,
    new_markdown_cell,
    new_notebook,
    new_output,
    new_raw_cell,
)
from nbclient import NotebookClient  # noqa: E402

KERNELSPEC = {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"}

# ---------------------------------------------------------------------------
# Registry (drives README.md and validation)
# ---------------------------------------------------------------------------

DESCRIPTIONS: dict[str, str] = {}      # relative path (or dir/ ) -> one-line description
NO_VALIDATE: set[str] = set()          # relative paths intentionally not valid nbformat
GROUPS: dict[str, str] = {}            # relative path -> README section title
_current_group = ["Small set"]


def group(title: str) -> None:
    _current_group[0] = title


def reg(rel: str, desc: str, validate: bool = True) -> None:
    DESCRIPTIONS[rel] = desc
    GROUPS[rel] = _current_group[0]
    if not validate:
        NO_VALIDATE.add(rel)


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


# ---------------------------------------------------------------------------
# Notebook-building DSL
# ---------------------------------------------------------------------------

def M(src: str, **opts):
    """Markdown cell spec."""
    return ("markdown", src.strip("\n"), opts)


def C(src: str, **opts):
    """Code cell spec.  opts: err=True (an error output is expected),
    skip=True (leave unexecuted), tags=[...], meta={...}"""
    return ("code", src.strip("\n"), opts)


def R(src: str, **opts):
    """Raw cell spec."""
    return ("raw", src.strip("\n"), opts)


def cell_id(name: str, i: int) -> str:
    return hashlib.sha1(f"{name}:{i}".encode()).hexdigest()[:8]


SKIPPED: set[str] = set()


def have(lib: str) -> bool:
    import importlib.util
    try:
        return importlib.util.find_spec(lib) is not None
    except Exception:
        return False


def apply_needs(specs, rules=(), sections=()):
    """Attach `needs` (optional libraries) to cell specs.
    rules:    (source_prefix, [libs])  -- cell source starting with the prefix needs the libs
    sections: (header_prefix, lib)     -- every cell after a '## <header>' markdown up to the next '## ' needs lib"""
    out, sec = [], None
    for kind, src, o in specs:
        o = dict(o)
        if kind == "markdown" and src.startswith("## "):
            sec = None
            for head, lib in sections:
                if src.lower().startswith(head):
                    sec = [lib]
        needs = list(o.get("needs", [])) + list(sec or [])
        for prefix, libs in rules:
            if src.startswith(prefix):
                needs += list(libs)
        if needs:
            o["needs"] = needs
        out.append((kind, src, o))
    return out


def build(specs, name: str, *, kernelspec=True):
    """specs -> (notebook, set of cell indexes where an error is expected).
    Cells whose `needs` libraries are not installed are dropped (and reported at the end)."""
    kept = []
    for spec in specs:
        missing = [lib for lib in spec[2].get("needs", []) if not have(lib)]
        if missing:
            SKIPPED.add(f"{name}: {spec[1][:36]!r} needs {'/'.join(missing)}")
        else:
            kept.append(spec)
    specs = kept
    cells, expect_err, skipped = [], set(), set()
    for i, (kind, src, o) in enumerate(specs):
        meta = dict(o.get("meta", {}))
        if o.get("tags"):
            meta["tags"] = list(o["tags"])
        if o.get("skip"):
            meta.setdefault("tags", []).append("skip-execution")
            skipped.add(i)
        if kind == "markdown":
            c = new_markdown_cell(src, metadata=meta)
            if o.get("attachments"):
                c["attachments"] = o["attachments"]
        elif kind == "raw":
            c = new_raw_cell(src, metadata=meta)
        else:
            c = new_code_cell(src, metadata=meta)
        c["id"] = cell_id(name, i)
        if o.get("err"):
            expect_err.add(i)
        cells.append(c)
    nb = new_notebook(cells=cells)
    if kernelspec:
        nb.metadata["kernelspec"] = dict(KERNELSPEC)
    nb.metadata["_skipped"] = sorted(skipped)  # removed again in execute()
    return nb, expect_err


def merge_streams(nb) -> None:
    """Merge consecutive stream outputs of the same name, like the Jupyter
    frontends do before saving (nbclient keeps every IOPub chunk separate)."""
    for cell in nb.cells:
        if cell.cell_type != "code":
            continue
        merged, parts = [], None
        for o in cell.outputs:
            if o.output_type == "stream" and merged and merged[-1].output_type == "stream" \
                    and merged[-1].name == o.name:
                parts.append(o.text)
            else:
                if parts is not None:
                    merged[-1]["text"] = "".join(parts)
                merged.append(o)
                parts = [o.text] if o.output_type == "stream" else None
        if parts is not None:
            merged[-1]["text"] = "".join(parts)
        cell.outputs = merged


# Run inside every kernel before the first cell (no execution count is consumed, nothing is added to the
# user namespace).  matplotlib 3.10 + matplotlib-inline 0.1.6 no longer flush figures that were created
# without plt.show(); every classic notebook relies on that ("plt.plot(...)" as the last line), so restore it.
KERNEL_STARTUP = """
def _herse_flush_open_figures():
    import sys
    plt = sys.modules.get("matplotlib.pyplot")
    if plt is not None and plt.get_fignums():
        from matplotlib_inline.backend_inline import show
        show(True)

get_ipython().events.register("post_execute", _herse_flush_open_figures)
del _herse_flush_open_figures
"""


_TMP_RE = re.compile(r"/(?:private/)?var/folders/[^/\s'\"]+/[^/\s'\"]+/T/")


def _scrub(o):
    if isinstance(o, str):
        return _TMP_RE.sub("/tmp/", o)
    if isinstance(o, list):
        return [_scrub(x) for x in o]
    if isinstance(o, dict):
        return type(o)({k: (v if str(k).startswith("image/") else _scrub(v)) for k, v in o.items()})
    return o


def scrub_paths(nb) -> None:
    """Replace the machine-specific macOS temp dir (kernel cwd, ipykernel warning paths) with /tmp/."""
    for c in nb.cells:
        if c.cell_type == "code":
            c.outputs = [nbformat.from_dict(_scrub(dict(o))) for o in c.outputs]


def execute(nb, name: str, expect_err=frozenset(), *, timeout=180, record_timing=False,
            allow_errors=True, check_errors=True):
    """Run all cells with nbclient in a throw-away working directory."""
    t0 = time.time()
    skipped = set(nb.metadata.pop("_skipped", []))
    with tempfile.TemporaryDirectory(prefix=f"herse-fx-{name}-") as td:
        startup = Path(td) / "_herse_startup.py"
        startup.write_text(KERNEL_STARTUP)
        client = NotebookClient(
            nb, timeout=timeout, kernel_name="python3", allow_errors=allow_errors,
            record_timing=record_timing, startup_timeout=120, iopub_timeout=60,
            resources={"metadata": {"path": td}},
            extra_arguments=[f"--IPKernelApp.exec_files=['{startup}']"],
        )
        client.execute()
    merge_streams(nb)
    scrub_paths(nb)
    nb.metadata["kernelspec"] = dict(KERNELSPEC)
    for i in skipped:  # drop the helper tag again: the cell is simply "never run"
        tags = [t for t in nb.cells[i].metadata.get("tags", []) if t != "skip-execution"]
        if tags:
            nb.cells[i].metadata["tags"] = tags
        else:
            nb.cells[i].metadata.pop("tags", None)
    if check_errors:
        got = {i for i, c in enumerate(nb.cells)
               if c.cell_type == "code" and any(o.output_type == "error" for o in c.outputs)}
        if got != set(expect_err):
            raise RuntimeError(f"{name}: unexpected error cells. expected={sorted(expect_err)} got={sorted(got)}")
    log(f"  executed {name}: {len(nb.cells)} cells in {time.time() - t0:.1f}s")
    return nb


def write_nb(nb, rel: str, desc: str | None = None, validate: bool = True, base: Path | None = None) -> Path:
    path = (base or ROOT) / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    nb.metadata.pop("_skipped", None)
    nbformat.write(nb, str(path))
    if desc is not None:
        reg(rel, desc, validate)
    return path


def dump_json(path: Path, obj, *, indent=1, sort_keys=True, newline="\n", trailing_nl=True, ensure_ascii=False,
              separators=None) -> None:
    """Write raw JSON (for hand-built files that bypass nbformat's writer)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if separators is None:
        separators = (",", ": ") if indent is not None else (",", ":")
    s = json.dumps(obj, indent=indent, sort_keys=sort_keys, ensure_ascii=ensure_ascii, separators=separators)
    if trailing_nl:
        s += "\n"
    if newline != "\n":
        s = s.replace("\n", newline)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(s)


def lines(text: str) -> list[str]:
    """Split like nbformat does: keep line endings, no trailing empty element."""
    return text.splitlines(keepends=True)


# ---------------------------------------------------------------------------
# Tiny image helpers (host side, for attachments and hand-built outputs)
# ---------------------------------------------------------------------------

def _pil():
    try:
        from PIL import Image, ImageDraw
        return Image, ImageDraw
    except Exception:
        return None, None


def png_bytes(w=96, h=64, mode="gradient", fmt="PNG", seed=0) -> bytes:
    Image, ImageDraw = _pil()
    if Image is None:  # 1x1 transparent PNG fallback
        return base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
    im = Image.new("RGBA" if fmt == "PNG" else "RGB", (w, h))
    px = im.load()
    for x in range(w):
        for y in range(h):
            if mode == "gradient":
                v = (int(255 * x / w), int(255 * y / h), (x * y + seed) % 256)
            else:  # checker
                v = (230, 230, 230) if ((x // 8 + y // 8) % 2) else (60 + seed % 100, 90, 160)
            px[x, y] = (*v, int(80 + 175 * (1 - y / h))) if fmt == "PNG" else v
    d = ImageDraw.Draw(im)
    d.rectangle([2, 2, w - 3, h - 3], outline=(255, 255, 255))
    d.text((8, 8), "Herse", fill=(255, 255, 255))
    buf = io.BytesIO()
    im.save(buf, format=fmt)
    return buf.getvalue()


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


BADGE_SVG = ('<svg xmlns="http://www.w3.org/2000/svg" width="140" height="28" viewBox="0 0 140 28">'
             '<rect width="140" height="28" rx="4" fill="#334155"/><rect x="62" width="78" height="28" rx="4" fill="#16a34a"/>'
             '<text x="8" y="19" fill="#fff" font-family="Verdana,sans-serif" font-size="12">herse</text>'
             '<text x="72" y="19" fill="#fff" font-family="Verdana,sans-serif" font-size="12">native</text></svg>')


# ===========================================================================
# 1. showcase.ipynb
# ===========================================================================

_LONG_TOKEN = "UNBROKEN" + "x" * 260 + "END"
_RNG = random.Random(11)


def _big_table_md(rows=60) -> str:
    cats = ["Hardware", "Software", "Service", "Cloud", "Support", "Training"]
    stats = ["shipped", "pending", "backorder", "cancelled", "returned"]
    out = ["## Big Markdown table (%d rows x 8 columns)" % rows, "",
           "| ID | Item | Category | Qty | Unit price | Total | Status | Notes |",
           "|---:|:-----|:---------|----:|-----------:|------:|:------:|:------|"]
    for i in range(1, rows + 1):
        q, p = _RNG.randint(1, 200), round(_RNG.uniform(0.5, 999), 2)
        note = _RNG.choice(["", "rush order", "see `ticket-%d`" % _RNG.randint(100, 999), "**fragile**",
                            "ships from 東京", "needs review", "[link](https://example.com/%d)" % i])
        out.append(f"| {i} | Item-{i:03d} | {_RNG.choice(cats)} | {q} | ${p:,.2f} | ${q * p:,.2f} | "
                   f"{_RNG.choice(stats)} | {note} |")
    return "\n".join(out)


_WIDE_CODE_FENCE = "```python\nvalues = [" + ", ".join(str(i * 7919 % 1009) for i in range(240)) + "]\n```"

_ATTACH = {
    "gradient.png": {"image/png": b64(png_bytes(96, 64, "gradient"))},
    "checker.png": {"image/png": b64(png_bytes(120, 80, "checker", seed=40))},
    "photo.jpg": {"image/jpeg": b64(png_bytes(160, 100, "gradient", fmt="JPEG", seed=7))},
    "badge.svg": {"image/svg+xml": BADGE_SVG},
}

SHOWCASE = [
    M(r'''
# Herse Native: Showcase Notebook

This notebook exercises **every kind of content** a reader meets in real-world Jupyter notebooks:
Markdown, rich outputs, errors and odd edge cases.  It was *executed* with `nbclient`, so every output
is exactly what a Jupyter kernel stored.

# Heading level 1
## Heading level 2
### Heading level 3
#### Heading level 4
##### Heading level 5
###### Heading level 6

Setext heading 1
================

Setext heading 2
----------------

####### Seven hashes is not a heading
'''),
    M(r'''
## Emphasis, inline code and links

Plain, *italic*, _also italic_, **bold**, __also bold__, ***bold italic***, ~~strikethrough~~ and `inline code`.
Inline code containing a backtick: `` `tick` ``.  Escapes: \*not italic\*, \_not italic\_, \# not a heading.
Snake_case_words_in_prose and 2*3*4 must not turn into emphasis; intra**word**bold does.

A hard line break with two trailing spaces:
second line.
A hard line break with a backslash:\
second line.

Links: [inline link](https://jupyter.org "Jupyter home"), [reference link][jup], [relative link](./dataframes.ipynb),
[anchor link](#GFM-tables), <https://example.com/autolink>, a bare URL https://example.com/bare?x=1&y=2 and <someone@example.com>.

[jup]: https://jupyter.org "Project Jupyter"

A deliberately long paragraph so wrapping can be checked: Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod
tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut
aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla
pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.

A very long unbroken token (overflow test): ''' + _LONG_TOKEN),
    M(r'''
<a id="lists"></a>
## Lists

- Unordered item one
- Unordered item two
    - Nested 2.1
    - Nested 2.2
        - Doubly nested 2.2.1
        - Doubly nested 2.2.2
    - Nested 2.3
- Unordered item three

1. First
2. Second
    1. Second-A
    2. Second-B
        - Bullet inside numbered inside numbered
3. Third

7. Starts at seven
8. Eight
1. Numbering ignores the digits

- [x] Completed task
- [ ] Open task
    - [x] Nested completed task
    - [ ] Nested open task
- [ ] Task with **bold** and `code`

* Star bullet
+ Plus bullet
- Dash bullet

- Loose item one

  A second paragraph inside the item.

- Loose item two

  ```python
  print("code block inside a list item")
  ```
'''),
    M(r'''
## Blockquotes and horizontal rules

> A single-level quote with *emphasis* and `code`.
>
> > A nested quote
> > that continues on a second line.
>
> - a list inside a quote
> - second item
>
> ```python
> print("code inside a quote")
> ```

Three kinds of horizontal rule follow:

---

***

___

Text directly after the rules.
'''),
    M(r'''
## Fenced code blocks

```python
def fib(n: int) -> int:
    """Return the n-th Fibonacci number."""
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a

print([fib(i) for i in range(10)])  # a comment
```

```bash
#!/usr/bin/env bash
set -euo pipefail
for f in *.ipynb; do echo "processing $f"; done
```

```json
{"name": "herse", "version": 1.5, "tags": ["fast", "native"], "nested": {"ok": true, "none": null}}
```

```
fence without a language
    keeps   whitespace
```

~~~rust
fn main() { println!("tilde fence"); }
~~~

    indented code block (four spaces)
    still code

```diff
- removed line
+ added line
```

A very wide code line (horizontal scrolling):

''' + _WIDE_CODE_FENCE),
    M(r'''
<a id="GFM-tables"></a>
## GFM tables

| Left | Center | Right |
|:-----|:------:|------:|
| a | b | c |
| **bold** | `code` | [link](https://jupyter.org) |
| *italic* | ~~strike~~ | 1,234.56 |
| escaped \| pipe | emoji 🚀 | 日本語 |
| | empty neighbours | |

A table without leading and trailing pipes:

Name | Value
--- | ---
alpha | 1
beta | 2
'''),
    M(r'''
## Inline HTML

<div style="padding:8px 12px;border:1px solid #888;border-radius:6px">A <b>styled div</b> with <i>inline</i> elements.</div>

Text with <kbd>Ctrl</kbd>+<kbd>C</kbd>, H<sub>2</sub>O, x<sup>2</sup>, <mark>marked</mark>, <del>deleted</del>, <ins>inserted</ins>,
<abbr title="HyperText Markup Language">HTML</abbr> and <span style="color:crimson">a crimson span</span>.

<details><summary>Click to expand a details element</summary>

Hidden **markdown** inside <code>details</code>.

</details>

<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr><tr><td colspan="2">spans both columns</td></tr></table>

<div class="alert alert-block alert-info"><b>Tip:</b> a classic Jupyter alert box (relies on notebook CSS classes).</div>

<center>centered via the legacy &lt;center&gt; tag</center>

<!-- an HTML comment: must not render -->

<script>alert("script tags in markdown must NOT execute")</script>

<iframe src="https://example.com" width="300" height="80" title="iframe in markdown"></iframe>

<style>h1 { color: red !important; }</style>The line above is a style tag: it must NOT restyle the app.
'''),
    M(r'''
## Images in Markdown

Remote image: ![Python logo](https://www.python.org/static/community_logos/python-logo.png)

Remote image through an HTML tag with a width: <img src="https://www.python.org/static/community_logos/python-logo.png" width="120" alt="python logo, 120px">

Broken remote image (alt text should show): ![missing image](https://example.invalid/definitely-missing.png "title of a broken image")

Missing relative image: ![local missing](images/does-not-exist.png)

Attachment (PNG with alpha): ![embedded gradient](attachment:gradient.png)

Attachment (PNG, checker): ![embedded checker](attachment:checker.png)

Attachment (JPEG): ![embedded photo](attachment:photo.jpg)

Attachment (SVG): ![embedded badge](attachment:badge.svg)

An attachment that does not exist: ![nope](attachment:nope.png)

A data URI (1x1 red pixel): ![dot](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==)

A linked image: [![linked](attachment:gradient.png)](https://jupyter.org)
''', attachments=_ATTACH),
    M(r'''
## Math (LaTeX)

Inline: $E = mc^2$, $\alpha + \beta = \gamma$, $\sum_{i=1}^{n} i = \frac{n(n+1)}{2}$, $x_{n+1}$ and $\sqrt[3]{x^2 + y^2}$.
Prices are not math: it costs $5 today and $10 tomorrow.  Escaped dollar: \$100.  Also \(\LaTeX\) delimiters.

Display math with `$$`:

$$\int_{-\infty}^{\infty} e^{-x^2}\,dx = \sqrt{\pi}$$

Multi-line display with a matrix:

$$
\mathbf{A} = \begin{pmatrix} a & b \\ c & d \end{pmatrix},\qquad \det \mathbf{A} = ad - bc
$$

A piecewise function: $$f(x)=\begin{cases} 0 & x<0 \\ 1 & x\ge 0 \end{cases}$$

An `align` environment with no delimiters:

\begin{align}
\nabla \cdot \vec{E} &= \frac{\rho}{\varepsilon_0} \\
\nabla \cdot \vec{B} &= 0 \\
\nabla \times \vec{E} &= -\frac{\partial \vec{B}}{\partial t} \\
\nabla \times \vec{B} &= \mu_0\vec{J} + \mu_0\varepsilon_0\frac{\partial \vec{E}}{\partial t}
\end{align}

And bracket delimiters: \[ x^2 + y^2 = z^2 \]
'''),
    M(r'''
## Non-Latin text in Markdown

日本語のテキスト、中文文本、한국어 텍스트 — Ελληνικά — Русский — العربية (RTL) — עברית (RTL) — हिन्दी — ไทย — 🚀🎉🔥 — 👨‍👩‍👧‍👦 — 🏳️‍🌈
'''),
    M(r'''
## Code outputs

Everything below is produced by real code cells.  The next cell sets things up.
'''),
    # ---- code: plain text outputs ---------------------------------------
    C(r'''
%matplotlib inline
import sys, time, warnings
import numpy as np
import pandas as pd
import matplotlib.pyplot as plt

print("Python", sys.version.split()[0], "| numpy", np.__version__, "| pandas", pd.__version__)
'''),
    C(r'''
print("Hello, Herse!")
print("Second line")
print()
print("tab\tseparated\tvalues")
print("trailing spaces   |")
print("  leading spaces")
for i in range(1, 6):
    print(f"row {i}: {i ** 2:>4} {'*' * i}")
'''),
    C(r'''
2 ** 100
'''),
    C(r'''
{"name": "herse", "tags": ["fast", "native", "gpu"], "nested": {"a": 1, "b": [1, 2, {"c": None}], "d": (1, 2.5, "x")}, "set": {1, 2, 3}}
'''),
    C(r'''
print("printed before the result")
40 + 2
'''),
    C(r'''
list(range(0, 200, 3))
'''),
    C(r'''
np.arange(24).reshape(4, 6) ** 2
'''),
    C(r'''
np.linspace(0, 1, 2000)   # numpy summarises big arrays with "..."
'''),
    C(r'''
import sys
print("normal stdout line 1")
print("error line on stderr", file=sys.stderr)
print("normal stdout line 2")
sys.stderr.write("another stderr line\n")
print("normal stdout line 3")
'''),
    C(r'''
warnings.warn("This function is deprecated and will be removed in 2.0", FutureWarning)
warnings.warn("A plain UserWarning", UserWarning)
print("warnings emitted")
'''),
    # ---- pandas ---------------------------------------------------------
    C(r'''
rng = np.random.default_rng(42)
names = ["Ada", "Grace", "Linus", "Margaret", "Dennis", "Barbara", "Ken", "Radia"]
df = pd.DataFrame({
    "id": range(1, 9),
    "name": names,
    "score": rng.normal(70, 12, 8).round(2),
    "passed": rng.random(8) > 0.3,
    "joined": pd.date_range("2024-01-07", periods=8, freq="W"),
    "ratio": rng.random(8),
})
df
'''),
    C(r'''
(df.drop(columns=["joined"]).style
   .background_gradient(subset=["score", "ratio"], cmap="viridis")
   .highlight_max(subset=["score"], color="gold")
   .format({"score": "{:.1f}", "ratio": "{:.1%}"})
   .set_caption("Styled DataFrame (background_gradient + highlight_max)"))
'''),
    # ---- matplotlib -----------------------------------------------------
    C(r'''
x = np.linspace(0, 4 * np.pi, 400)
fig, ax = plt.subplots(figsize=(7, 3.5))
ax.plot(x, np.sin(x), label=r"$\sin(x)$")
ax.plot(x, np.cos(x), label=r"$\cos(x)$", linestyle="--")
ax.set(title="Matplotlib PNG output", xlabel="x", ylabel="y")
ax.legend(); ax.grid(alpha=.3)
plt.show()
'''),
    C(r'''
plt.figure(figsize=(4, 2.5))
plt.plot([1, 3, 2, 5, 4])    # no semicolon: execute_result (Line2D list) AND the figure
'''),
    C(r'''
%config InlineBackend.figure_formats = ['svg']
fig, ax = plt.subplots(figsize=(6, 3))
ax.bar(list("ABCDE"), [3, 7, 4, 9, 5], color="#4c78a8")
ax.set_title("SVG figure output (image/svg+xml)")
plt.show()
'''),
    C(r'''
%config InlineBackend.figure_formats = ['png', 'svg']
fig, ax = plt.subplots(figsize=(4, 2.5))
ax.scatter(np.arange(10), np.arange(10) ** 1.5, c=np.arange(10), cmap="plasma")
ax.set_title("PNG + SVG in one bundle")
plt.show()
'''),
    C(r'''
%config InlineBackend.figure_formats = ['png']
print("figure format reset to png")
'''),
    # ---- IPython.display -----------------------------------------------
    M(r'''
### `IPython.display` objects
'''),
    C(r'''
from IPython.display import (display, Markdown, Latex, HTML, JSON, Image, Javascript, SVG,
                             Audio, Code, IFrame, Video, FileLink, clear_output)

display(Markdown("""### Markdown from `display(Markdown(...))`

- rendered **by the frontend**
- with a table:

| a | b |
|---|---|
| 1 | 2 |

and math $e^{i\\pi} + 1 = 0$"""))
'''),
    C(r'''
display(Latex(r"$\int_0^1 x^2\,dx = \frac{1}{3}$"))
display(Latex(r"\begin{align} a &= b + c \\ d &= e + f \end{align}"))
'''),
    C(r'''
import sympy as sp
sp.init_printing()
x, n = sp.symbols("x n")
display(sp.Integral(sp.sin(x) ** 2, (x, 0, sp.pi)))
sp.Matrix([[1, x], [x ** 2, sp.sqrt(n)]]) @ sp.Matrix([1, 2])
'''),
    C(r'''
display(HTML("""
<style>.herse-box { padding: 8px 12px; border-left: 4px solid teal; background: #e6f4f1; color: #0b4f4a; }</style>
<div class="herse-box"><b>HTML output</b> with <i>inline</i> styling, a <a href="https://jupyter.org">link</a> and a list:
<ul><li>one</li><li>two</li><li>three</li></ul></div>
<button onclick="alert('JS inside an HTML output')">button with onclick</button>
<script>console.log("script inside an HTML output");</script>
"""))
'''),
    C(r'''
display(JSON({"a": 1, "list": [1, 2, 3], "nested": {"deep": {"deeper": [True, None, 3.14]}}}))
display(JSON([{"id": i, "ok": i % 2 == 0} for i in range(5)], expanded=True))
'''),
    C(r'''
from PIL import Image as PILImage, ImageDraw
import io

def make_png(w=160, h=100, fmt="PNG", seed=0):
    im = PILImage.new("RGB", (w, h))
    px = im.load()
    for x in range(w):
        for y in range(h):
            px[x, y] = (int(255 * x / w), int(255 * y / h), (x * y + seed) % 256)
    d = ImageDraw.Draw(im)
    d.rectangle([4, 4, w - 5, h - 5], outline="white")
    d.text((10, 10), "Herse", fill="white")
    buf = io.BytesIO()
    im.save(buf, format=fmt)
    return buf.getvalue()

display(Image(data=make_png(), format="png"))
display(Image(data=make_png(), format="png", width=80))                       # width in output metadata
display(Image(data=make_png(fmt="JPEG"), format="jpeg"))                      # image/jpeg
display(Image(data=make_png(), format="png", width=200, height=50, unconfined=True))
display(Image(url="https://www.python.org/static/community_logos/python-logo.png"))   # remote: text/html <img>
'''),
    C(r'''
display(Javascript("console.log('hello from a Javascript output'); element.text('Javascript output executed');"))
'''),
    C(r'''
display(SVG('<svg xmlns="http://www.w3.org/2000/svg" width="220" height="90" viewBox="0 0 220 90"><rect width="220" height="90" rx="10" fill="#1e293b"/><circle cx="45" cy="45" r="28" fill="#38bdf8"/><text x="90" y="52" fill="white" font-family="sans-serif" font-size="22">SVG output</text></svg>'))
'''),
    C(r'''
t = np.linspace(0, 0.3, 2400, endpoint=False)
display(Audio(np.sin(2 * np.pi * 440 * t), rate=8000))
display(Code("def hello():\n    return 'highlighted by IPython.display.Code'", language="python"))
display(IFrame("https://example.com", width=300, height=120))
display(Video("https://example.com/video.mp4", embed=False, width=240))
display(FileLink("showcase.ipynb"))
'''),
    C(r'''
display({
    "text/plain": "plain text fallback",
    "text/html": "<b>html</b> representation",
    "text/markdown": "**markdown** representation",
    "text/latex": r"$\LaTeX$ representation",
    "application/json": {"json": ["representation"]},
}, raw=True)
display({"application/x-herse-custom+json": {"unknown": "mime type"},
         "text/plain": "unknown mime type with a text/plain fallback"}, raw=True)
display({"application/x-herse-custom": "unknown mime type, NO text/plain fallback"}, raw=True)
display({"image/png": __import__("base64").b64encode(make_png(60, 40)).decode()}, raw=True,
        metadata={"image/png": {"width": 120, "height": 80}})
'''),
    C(r'''
%%html
<p style="font-family: monospace">Output of the <code>%%html</code> cell magic: <b>bold</b>, <i>italic</i>.</p>
'''),
    C(r'''
%%markdown
Output of the **`%%markdown`** cell magic with a list:

1. one
2. two
'''),
    C(r'''
%%latex
\begin{equation} E = mc^2 \end{equation}
'''),
    C(r'''
%%svg
<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"><ellipse cx="60" cy="30" rx="50" ry="22" fill="#f97316"/></svg>
'''),
    C(r'''
%%javascript
console.log("hello from the %%javascript cell magic");
element.text("%%javascript executed");
'''),
    C(r'''
h = display("progress: 0%", display_id=True)
for p in (25, 50, 100):
    time.sleep(0.05)
    h.update(f"progress: {p}%")      # update_display_data: only the final value is stored
'''),
    C(r'''
for i in range(4):
    clear_output(wait=True)
    print(f"countdown {3 - i}")      # clear_output(wait=True): only the last print survives
    time.sleep(0.05)
'''),
    C(r'''
!echo "hello from the shell"
!printf 'shell line 1\nshell line 2\n'
%timeit -n 200 -r 3 sum(range(1000))
'''),
    C(r'''
%%time
total = sum(i * i for i in range(200_000))
total
'''),
    # ---- progress bars --------------------------------------------------
    M(r'''
### Progress bars and carriage returns
'''),
    C(r'''
from tqdm import tqdm
for _ in tqdm(range(50), desc="tqdm bar"):
    time.sleep(0.01)
'''),
    C(r'''
for pct in range(0, 101, 5):
    bar = "#" * (pct // 5)
    print(f"\rDownloading [{bar:<20}] {pct:3d}%", end="", flush=True)
    time.sleep(0.01)
print("\nDone.")
'''),
    C(r'''
from tqdm.auto import tqdm as tqdm_auto
for _ in tqdm_auto(range(20), desc="tqdm.auto (widget)"):
    time.sleep(0.005)
'''),
    # ---- ANSI -----------------------------------------------------------
    M(r'''
### ANSI escape sequences
'''),
    C(r'''
ESC = "\x1b"
names = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]
print(f"{ESC}[1mBold{ESC}[0m {ESC}[2mDim{ESC}[0m {ESC}[3mItalic{ESC}[0m {ESC}[4mUnderline{ESC}[0m {ESC}[9mStrike{ESC}[0m {ESC}[7mReverse{ESC}[0m")
print("fg:        " + " ".join(f"{ESC}[{30 + i}m{n}{ESC}[0m" for i, n in enumerate(names)))
print("bright fg: " + " ".join(f"{ESC}[{90 + i}m{n}{ESC}[0m" for i, n in enumerate(names)))
print("bg:        " + " ".join(f"{ESC}[{40 + i};97m {n} {ESC}[0m" for i, n in enumerate(names)))
print("bright bg: " + " ".join(f"{ESC}[{100 + i};30m {n} {ESC}[0m" for i, n in enumerate(names)))
print("256 colours: " + "".join(f"{ESC}[38;5;{n}m#{ESC}[0m" for n in range(256)))
for row in range(6):
    print("cube: " + "".join(f"{ESC}[48;5;{16 + row * 36 + c}m {ESC}[0m" for c in range(36)))
print("grey: " + "".join(f"{ESC}[48;5;{n}m {ESC}[0m" for n in range(232, 256)))
print("truecolor: " + "".join(f"{ESC}[48;2;{int(255 * i / 63)};{int(255 * (1 - i / 63))};160m {ESC}[0m" for i in range(64)))
print(f"{ESC}[38;2;255;105;180;48;2;20;20;60m truecolor fg + bg {ESC}[0m")
print(f"{ESC}[1;4;38;5;208mbold underline orange (256){ESC}[0m, {ESC}[1;3;31mbold italic red{ESC}[22;23;39m then plain")
print(f"{ESC}]8;;https://jupyter.org{ESC}\\OSC-8 hyperlink{ESC}]8;;{ESC}\\")
print(f"unterminated colour: {ESC}[32mgreen continues onto the next output line")
print("...still green here" + f"{ESC}[0m (reset)")
print(f"erase/cursor codes: abc{ESC}[2K{ESC}[1Gxyz{ESC}[3D{ESC}[K|")
print(f"unknown sequence {ESC}[?25l hidden cursor {ESC}[?25h and bell \a")
'''),
    C(r'''
from rich.console import Console
from rich.table import Table

table = Table(title="Rich table (HTML display_data when run inside Jupyter)")
table.add_column("Name", style="cyan"); table.add_column("Value", justify="right", style="magenta")
for k, v in [("alpha", 1), ("beta", 22), ("gamma", 333)]:
    table.add_row(k, str(v))
Console().print(table)
Console(force_terminal=True, force_jupyter=False, width=60, color_system="truecolor").print(table, "[bold red]ANSI[/] from rich")
'''),
    # ---- exception ------------------------------------------------------
    M(r'''
### A long traceback (nested calls into pandas internals)
''' ),
    C(r'''
class Pipeline:
    def __init__(self, df): self.df = df
    def run(self): return self.stage_one()
    def stage_one(self): return self.stage_two()
    def stage_two(self): return self.stage_three()
    def stage_three(self): return [self.stage_four(i) for i in range(3)]
    def stage_four(self, i): return self.lookup("missing-row")
    def lookup(self, key): return self.df.loc[key]

def main(): return Pipeline(pd.DataFrame({"a": [1, 2, 3]})).run()
def entrypoint(): return main()

entrypoint()
''', err=True),
    # ---- long lines -----------------------------------------------------
    M(r'''
### Very long lines
'''),
    C(r'''
payload = "lorem-ipsum-" * 2500      # 30,000 characters on ONE line
print(payload)
payload
'''),
    C("data = [" + ", ".join(str(i * 31 % 997) for i in range(1500)) + "]   # a source line of ~7,000 characters\nprint(len(data), sum(data))"),
    # ---- unicode --------------------------------------------------------
    M(r'''
### Unicode, emoji, CJK, RTL
'''),
    C(r'''
samples = {
    "latin": "Café Zoë naïve façade — “quotes” … ñ ü ß Ø Þ",
    "cjk": "日本語のテキスト 中文文本 한국어 텍스트",
    "emoji": "🚀🎉🔥 👨‍👩‍👧‍👦 🏳️‍🌈 👍🏽 🇯🇵🇺🇸 ❤️ ✅",
    "rtl": "مرحبا بالعالم — שלום עולם",
    "bidi": "English עברית English العربية 123 end",
    "combining": "é ä ñ z͡y (decomposed)",
    "math": "∑ ∫ ∞ ≈ ≠ ≤ ≥ √ π ∂ ℝ",
    "box": "┌──┬──┐\n│ a│ b│\n└──┴──┘",
    "zero-width": "a​b‍c﻿d (ZWSP, ZWJ, BOM)",
    "indic/thai": "नमस्ते दुनिया สวัสดีชาวโลก",
    "wide align": "ＡＢＣ fullwidth | ｱｲｳ halfwidth kana",
}
for k, v in samples.items():
    print(f"{k:<12} {v}")
samples["cjk"]
'''),
    # ---- empty / never executed / ran without output --------------------
    M(r'''
### Empty, never-executed and silent cells
'''),
    C(""),
    C(r'''
print("this cell was never executed")
''', skip=True),
    C(r'''
x = 1    # executed, but produces no output
'''),
    C(r'''
from __future__ import annotations
import asyncio, dataclasses as dc
from typing import Generic, TypeVar

T = TypeVar("T")

@dc.dataclass(frozen=True, slots=True)
class Point(Generic[T]):
    """Syntax-highlighting torture test (valid Python, no output)."""
    x: T
    y: T = dc.field(default=0, repr=False)  # trailing comment

    def __add__(self, other: "Point[T]") -> "Point[T]":
        return Point(self.x + other.x, self.y + other.y)

async def agen(n):
    for i in range(n):
        yield i

async def fetch(n: int, *, delay: float = 0.0, **kw) -> list[int]:
    await asyncio.sleep(delay)
    return [i async for i in agen(n)]

def classify(v):
    match v:
        case {"kind": "point", "x": x, "y": y}: return f"point({x}, {y})"
        case [first, *rest] if rest: return f"seq {first!r} +{len(rest)}"
        case int() | float() as num: return f"number {num:>8.3f}"
        case _: return "other"

nums = [1_000_000, 0xFF, 0o17, 0b1010, 1e-3, 3.14j, ..., None, True]
blob = rb"raw bytes \d+" + b"\x00\xff"
label = f"{'nested' + "quotes"!r:^20} {nums[0]:,} {(n := 10)}"      # PEP 701 f-string
lam = lambda *a, **k: (a, k)
long_string = """triple-quoted
    multi-line string with 'single' and "double" quotes"""
total = 1 + \
    2 + 3
_ = [classify(v) for v in ({"kind": "point", "x": 1, "y": 2}, [1, 2, 3], 3.14159, "s")]
'''),
    R(r'''
This is a raw cell (no format metadata).
It is shown verbatim by notebook UIs: *not* rendered as Markdown, <b>not</b> HTML, $not$ math.
'''),
    R(r'''<p>A raw cell with <code>raw_mimetype = text/html</code> (only nbconvert HTML export renders it).</p>''',
      meta={"raw_mimetype": "text/html"}),
    R(r'''
\begin{center}A raw cell with format = text/latex\end{center}
''', meta={"format": "text/latex"}),
    M(_big_table_md(60)),
    M(r'''
### The end

Final markdown cell.
'''),
]


SHOWCASE_RULES = [("from rich.console", ["rich"]), ("from tqdm import tqdm", ["tqdm"]), ("from tqdm.auto", ["tqdm", "ipywidgets"]),
                  ("from PIL import Image as PILImage", ["PIL"]), ('display({\n    "text/plain": "plain text fallback"', ["PIL"]),
                  ("import sympy as sp", ["sympy"])]


def build_showcase():
    nb, err = build(apply_needs(SHOWCASE, SHOWCASE_RULES), "showcase")
    execute(nb, "showcase", err, record_timing=True, timeout=240)
    write_nb(nb, "showcase.ipynb",
             "Everything in one notebook: headings, lists, tables, math, attachments, HTML, all IPython.display types, "
             "matplotlib PNG/SVG, DataFrames, ANSI, progress bars, tracebacks, long lines, unicode, empty/raw/unexecuted cells.")



# ===========================================================================
# 2. dataframes.ipynb
# ===========================================================================

DATAFRAMES = [
    M(r"""
# DataFrame display zoo

Wide, long, MultiIndex, NaN/NA, datetime index, `Styler`, `describe()`, Series and nasty cell contents.
All outputs are `text/html` + `text/plain` pairs written by pandas itself.
"""),
    C(r"""
import numpy as np, pandas as pd
from IPython.display import display
rng = np.random.default_rng(7)
print("pandas", pd.__version__, "| max_rows", pd.get_option("display.max_rows"), "| max_columns", pd.get_option("display.max_columns"))
"""),
    M("## Wide: 50 columns"),
    C(r"""
wide = pd.DataFrame(rng.normal(size=(8, 50)).round(3), columns=[f"feature_{i:02d}" for i in range(50)])
wide    # default max_columns=20: pandas elides the middle columns with "..."
"""),
    C(r"""
with pd.option_context("display.max_columns", None, "display.width", 1000):
    display(wide)    # all 50 columns in the HTML table
"""),
    M("## Long: truncated at 60 rows"),
    C(r"""
long_df = pd.DataFrame({
    "id": np.arange(1000),
    "x": rng.normal(size=1000).round(4),
    "y": rng.integers(0, 100, 1000),
    "group": rng.choice(list("ABCDE"), 1000),
    "flag": rng.random(1000) > .5,
    "ts": pd.date_range("2024-01-01", periods=1000, freq="15min"),
})
long_df     # default: first 5 + last 5 rows
"""),
    C(r"""
with pd.option_context("display.max_rows", 60, "display.min_rows", 60):
    display(long_df)    # 30 head + 30 tail rows with a "..." row between
"""),
    C(r"""
long_df.head(60)        # exactly 60 rows: nothing is truncated
"""),
    C(r"""
with pd.option_context("display.max_rows", None):
    display(long_df.head(400))    # a 400-row HTML table (scroll / perf test)
"""),
    M("## MultiIndex rows and columns"),
    C(r"""
idx = pd.MultiIndex.from_product([["2023", "2024"], ["Q1", "Q2", "Q3", "Q4"], ["north", "south"]], names=["year", "quarter", "region"])
sales = pd.DataFrame({"revenue": rng.integers(1000, 9000, len(idx)), "units": rng.integers(10, 400, len(idx)),
                      "margin": rng.random(len(idx)).round(3)}, index=idx)
sales
"""),
    C(r"""
sales.reset_index().pivot_table(index="region", columns=["year", "quarter"], values="revenue", aggfunc="sum")
"""),
    C(r"""
agg = sales.groupby(level=["year", "region"]).agg({"revenue": ["sum", "mean"], "units": ["min", "max"], "margin": "median"})
agg
"""),
    C(r"""
cols = pd.MultiIndex.from_tuples([("metrics", "mean"), ("metrics", "std"), ("meta", "n"), ("meta", "label")], names=["section", "stat"])
pd.DataFrame([[1.5, 0.2, 10, "a"], [2.5, 0.4, 20, "b"]], columns=cols, index=pd.Index(["first", "second"], name="run"))
"""),
    M("## NaN, None, NA, NaT, inf"),
    C(r"""
mixed = pd.DataFrame({
    "float": [1.5, np.nan, 3.25, np.inf, -np.inf],
    "int_nullable": pd.array([1, None, 3, None, 5], dtype="Int64"),
    "object": ["a", None, "c", np.nan, pd.NA],
    "string": pd.array(["x", None, "z", None, "w"], dtype="string"),
    "bool": pd.array([True, None, False, True, None], dtype="boolean"),
    "datetime": pd.to_datetime(["2024-01-01", None, "2024-03-01", pd.NaT, "2024-05-01"]),
    "timedelta": pd.to_timedelta(["1 days", None, "2h", pd.NaT, "30s"]),
    "category": pd.Categorical(["a", None, "b", "a", None]),
})
mixed
"""),
    C(r"""
mixed.isna()
"""),
    C(r"""
mixed.isna().sum()
"""),
    M("## Datetime index"),
    C(r"""
ts = pd.DataFrame({"temp": (15 + 8 * np.sin(np.arange(96) / 24 * 2 * np.pi) + rng.normal(0, .5, 96)).round(2),
                   "humidity": rng.integers(30, 90, 96)},
                  index=pd.date_range("2024-01-01", periods=96, freq="h", name="timestamp"))
ts
"""),
    C(r"""
ts.resample("D").agg(["mean", "min", "max"]).round(2)    # DatetimeIndex rows + MultiIndex columns
"""),
    C(r"""
pd.DataFrame({"v": range(6)}, index=pd.date_range("2024-03-09 12:00", periods=6, freq="6h", tz="America/New_York", name="local time"))
"""),
    M("## Styler"),
    C(r"""
num = pd.DataFrame(rng.normal(0, 1, (10, 6)).round(3), columns=list("ABCDEF"))
num.iloc[2, 3] = np.nan
num.style.background_gradient(cmap="RdYlGn", axis=None).format("{:.2f}", na_rep="-").set_caption("background_gradient (RdYlGn)")
"""),
    C(r"""
(num.style
    .highlight_max(color="#a7f3d0").highlight_min(color="#fecaca").highlight_null(color="#fde68a")
    .format(precision=1)
    .set_table_styles([{"selector": "th", "props": [("background-color", "#1e293b"), ("color", "white"), ("text-align", "center")]},
                       {"selector": "td", "props": [("border", "1px solid #94a3b8")]}])
    .set_caption("highlight_max / min / null + table styles"))
"""),
    C(r"""
bars = pd.DataFrame({"item": ["alpha", "beta", "gamma", "delta"], "share": [0.12, 0.55, 0.9, 0.33], "delta": [-3, 5, 0, 12]})
(bars.style.bar(subset=["share"], color="#60a5fa", vmin=0, vmax=1)
     .bar(subset=["delta"], align="mid", color=["#fca5a5", "#86efac"])
     .map(lambda v: "color: crimson; font-weight: bold" if isinstance(v, (int, float)) and v < 0 else "", subset=["delta"])
     .hide(axis="index").set_properties(**{"font-family": "monospace"}))
"""),
    C(r"""
big_style = pd.DataFrame(rng.random((40, 12)), columns=[f"c{i}" for i in range(12)])
big_style.style.background_gradient(cmap="viridis").format("{:.3f}")    # one CSS rule per cell: large HTML
"""),
    M("## describe(), Series, info"),
    C(r"""
long_df.describe()
"""),
    C(r"""
long_df.describe(include="all")
"""),
    C(r"""
long_df["x"]     # Series: text/plain only (a Series has no _repr_html_)
"""),
    C(r"""
long_df["group"].value_counts()
"""),
    C(r"""
sales["revenue"]     # Series with a 3-level MultiIndex
"""),
    C(r"""
long_df.dtypes
"""),
    C(r"""
long_df.info()
"""),
    C(r"""
pd.Series([1.5, 2.5, 3.5], index=list("abc"), name="a named series", dtype="float32")
"""),
    M("## Nasty cell contents"),
    C(r"""
nasty = pd.DataFrame({
    "html": ["<b>bold?</b>", "<script>alert('xss')</script>", "&amp; &lt; &gt;", "a < b > c & d", "<img src=x onerror=alert(1)>"],
    "long text": ["x" * 120, "short", "word " * 40, "", " leading and trailing spaces  "],
    "unicode": ["日本語", "🚀 emoji", "مرحبا", "שלום", "Ünïcödé"],
    "multiline": ["line1\nline2", "tab\there", "cr\rhere", "a\n\nb", "plain"],
    "numbers": [1e300, 1e-300, 123456789.123456789, -0.0, 0.1 + 0.2],
    "list/dict": [[1, 2, 3], {"a": 1}, (1, 2), {1, 2}, None],
    "bytes": [b"abc", b"\x00\xff", b"", b"x" * 80, b"caf\xc3\xa9"],
})
nasty
"""),
    C(r"""
odd = pd.DataFrame(rng.integers(0, 9, (3, 4)), columns=["dup", "dup", "a very long column name that goes on and on and on and on", 0])
odd
"""),
    C(r"""
display(pd.DataFrame())
display(pd.DataFrame(columns=["a", "b"]))
display(pd.DataFrame(index=range(3)))
display(pd.DataFrame({"only": [42]}))
display(pd.DataFrame({"iv": pd.interval_range(0, 3), "per": pd.period_range("2024-01", periods=3, freq="M")}))
"""),
    C(r"""
long_df.groupby("group")      # a non-DataFrame object with a text/plain repr
"""),
    C(r"""
print(long_df.head(12).to_string())
"""),
]


def build_dataframes():
    nb, err = build(DATAFRAMES, "dataframes")
    execute(nb, "dataframes", err, timeout=180)
    write_nb(nb, "dataframes.ipynb",
             "pandas display zoo: 50 columns, 1000 rows truncated at 60, 400-row HTML table, MultiIndex rows/columns, "
             "NaN/NA/NaT, datetime index, Styler gradients/bars, describe(), Series, nasty cell contents.")



# ===========================================================================
# 3. plots.ipynb
# ===========================================================================

PLOTS = [
    M(r"""
# Plots

About twenty matplotlib charts of different shapes, plus whatever seaborn / plotly / altair / bokeh write
(vendor MIME types such as `application/vnd.plotly.v1+json`, `application/vnd.vegalite.v5+json`,
`application/vnd.bokehjs_exec.v0+json` come with `text/html` or `text/plain` fallbacks).
"""),
    C(r"""
%matplotlib inline
import numpy as np, pandas as pd, matplotlib as mpl, matplotlib.pyplot as plt
rng = np.random.default_rng(3)
plt.rcParams.update({"figure.figsize": (6.4, 3.8), "axes.grid": True, "grid.alpha": .25})
print("matplotlib", mpl.__version__, "| backend", mpl.get_backend())
"""),
    M("## matplotlib"),
    C(r"""
x = np.linspace(0, 10, 300)
fig, ax = plt.subplots()
ax.plot(x, np.sin(x), label="sin"); ax.plot(x, np.cos(x), label="cos"); ax.plot(x, np.sin(x) * np.cos(x), "--", label="sin*cos")
ax.set(title="01. Line plot", xlabel="x", ylabel="f(x)"); ax.legend()
plt.show()
"""),
    C(r"""
t = np.arange(200); y = np.cumsum(rng.normal(0, 1, 200))
fig, ax = plt.subplots()
ax.plot(t, y, color="C0"); ax.fill_between(t, y - 3, y + 3, alpha=.25)
ax.set_title("02. Random walk with a band")
plt.show()
"""),
    C(r"""
n = 300
a, b = rng.normal(size=n), rng.normal(size=n)
fig, ax = plt.subplots()
sc = ax.scatter(a, a * .6 + b * .4, c=b, s=20 + 60 * rng.random(n), cmap="viridis", alpha=.8, edgecolor="k", linewidth=.3)
fig.colorbar(sc, label="colour value"); ax.set_title("03. Scatter with colour and size")
plt.show()
"""),
    C(r"""
fig, ax = plt.subplots()
ax.hist(rng.normal(0, 1, 2000), bins=40, alpha=.7, label="N(0,1)")
ax.hist(rng.normal(1.5, .6, 1200), bins=40, alpha=.7, label="N(1.5,0.6)")
ax.legend(); ax.set_title("04. Overlaid histograms")
plt.show()
"""),
    C(r"""
cats = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"]; vals = [23, 45, 12, 37, 29]
fig, ax = plt.subplots()
bars = ax.bar(cats, vals, color=plt.cm.tab10.colors[:5])
ax.bar_label(bars); ax.set_title("05. Bar chart with labels")
plt.show()
"""),
    C(r"""
w = .27; xs = np.arange(4)
fig, ax = plt.subplots()
for i, name in enumerate(["2022", "2023", "2024"]):
    ax.bar(xs + (i - 1) * w, rng.integers(10, 50, 4), w, yerr=rng.integers(1, 6, 4), capsize=3, label=name)
ax.set_xticks(xs, ["Q1", "Q2", "Q3", "Q4"]); ax.legend(); ax.set_title("06. Grouped bars with error bars")
plt.show()
"""),
    C(r"""
fig, ax = plt.subplots()
left = np.zeros(5)
for k in range(4):
    v = rng.integers(3, 15, 5); ax.barh(list("VWXYZ"), v, left=left, label=f"series {k}"); left += v
ax.legend(); ax.set_title("07. Stacked horizontal bars")
plt.show()
"""),
    C(r"""
fig, ax = plt.subplots(figsize=(4.5, 4.5))
ax.pie([40, 25, 20, 10, 5], labels=["A", "B", "C", "D", "E"], autopct="%1.0f%%", wedgeprops=dict(width=.45), startangle=90)
ax.set_title("08. Donut chart")
plt.show()
"""),
    C(r"""
data = [rng.normal(m, s, 200) for m, s in [(0, 1), (1, 2), (-1, .5), (2, 1.5)]]
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.6), sharey=True)
a1.boxplot(data, labels=list("ABCD")); a1.set_title("09a. Box plot")
a2.violinplot(data, showmeans=True); a2.set_title("09b. Violin plot")
plt.show()
"""),
    C(r"""
fig, axes = plt.subplots(3, 3, figsize=(9, 7), constrained_layout=True)
xx = np.linspace(0, 2 * np.pi, 100)
for i, ax in enumerate(axes.flat):
    if i % 3 == 0:   ax.plot(xx, np.sin((i + 1) * xx))
    elif i % 3 == 1: ax.scatter(rng.random(40), rng.random(40), c=rng.random(40))
    else:            ax.hist(rng.normal(size=300), bins=15)
    ax.set_title(f"panel {i + 1}", fontsize=9)
fig.suptitle("10. Subplot grid (3 x 3)")
plt.show()
"""),
    C(r"""
m = rng.random((8, 8)); 
fig, ax = plt.subplots(figsize=(5.5, 4.6))
im = ax.imshow(m, cmap="magma"); fig.colorbar(im)
for (i, j), v in np.ndenumerate(m):
    ax.text(j, i, f"{v:.2f}", ha="center", va="center", color="w" if v < .6 else "k", fontsize=7)
ax.set_title("11. imshow heatmap with annotations"); ax.grid(False)
plt.show()
"""),
    C(r"""
X, Y = np.meshgrid(np.linspace(-3, 3, 200), np.linspace(-3, 3, 200))
Z = np.exp(-(X ** 2 + Y ** 2)) + .5 * np.exp(-((X - 1.5) ** 2 + (Y + 1) ** 2) * 2)
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.8))
cf = a1.contourf(X, Y, Z, levels=15, cmap="coolwarm"); fig.colorbar(cf, ax=a1); a1.set_title("12a. contourf")
cs = a2.contour(X, Y, Z, levels=10, colors="k"); a2.clabel(cs, fontsize=7); a2.set_title("12b. contour")
plt.show()
"""),
    C(r"""
Y, X = np.mgrid[-3:3:20j, -3:3:20j]; U, V = -1 - X ** 2 + Y, 1 + X - Y ** 2
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.8))
a1.quiver(X, Y, U, V, np.hypot(U, V)); a1.set_title("13a. quiver")
a2.streamplot(X, Y, U, V, color=np.hypot(U, V), density=1.4); a2.set_title("13b. streamplot")
plt.show()
"""),
    C(r"""
theta = np.linspace(0, 2 * np.pi, 400)
fig = plt.figure(figsize=(4.8, 4.8)); ax = fig.add_subplot(projection="polar")
ax.plot(theta, 1 + .5 * np.sin(5 * theta)); ax.plot(theta, 1.5 + .3 * np.cos(3 * theta))
ax.set_title("14. Polar plot", pad=14)
plt.show()
"""),
    C(r"""
from matplotlib import cm
X, Y = np.meshgrid(np.linspace(-3, 3, 60), np.linspace(-3, 3, 60)); Z = np.sin(np.hypot(X, Y) * 2) / (1 + np.hypot(X, Y))
fig = plt.figure(figsize=(6, 5)); ax = fig.add_subplot(projection="3d")
ax.plot_surface(X, Y, Z, cmap=cm.viridis, linewidth=0, antialiased=True); ax.set_title("15. 3D surface")
plt.show()
"""),
    C(r"""
fig = plt.figure(figsize=(6, 5)); ax = fig.add_subplot(projection="3d")
p = rng.normal(size=(200, 3)); ax.scatter(p[:, 0], p[:, 1], p[:, 2], c=p[:, 2], cmap="plasma"); ax.set_title("16. 3D scatter")
plt.show()
"""),
    C(r"""
f = np.logspace(0, 4, 200)
fig, ax = plt.subplots()
ax.loglog(f, 1 / (1 + (f / 100) ** 2), label="low-pass"); ax.set_xlabel("frequency [Hz]"); ax.set_ylabel("gain")
ax2 = ax.twinx(); ax2.semilogx(f, -np.degrees(np.arctan(f / 100)), "r--", label="phase"); ax2.set_ylabel("phase [deg]")
ax.set_title("17. Log-log with a twin axis")
plt.show()
"""),
    C(r"""
px, py = rng.normal(size=5000), rng.normal(size=5000)
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.8))
hb = a1.hexbin(px, px * .5 + py, gridsize=25, cmap="Blues"); fig.colorbar(hb, ax=a1); a1.set_title("18a. hexbin")
_ = a2.hist2d(px, px * .5 + py, bins=40, cmap="Greens"); a2.set_title("18b. hist2d")
plt.show()
"""),
    C(r"""
dates = pd.date_range("2023-01-01", periods=365)
s = pd.Series(np.cumsum(rng.normal(0, 1, 365)), index=dates)
fig, ax = plt.subplots(figsize=(8, 3.6))
ax.plot(s.index, s.values, lw=1); ax.plot(s.index, s.rolling(30).mean(), lw=2)
fig.autofmt_xdate(); ax.set_title("19. Time series with dates")
plt.show()
"""),
    C(r"""
k = np.arange(15)
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.4))
a1.step(k, rng.integers(0, 10, 15), where="mid"); a1.set_title("20a. step")
a2.stem(k, rng.random(15)); a2.set_title("20b. stem")
plt.show()
"""),
    C(r"""
with plt.style.context("dark_background"):
    fig, ax = plt.subplots()
    for i in range(5): ax.plot(np.cumsum(rng.normal(size=100)), lw=1.5)
    ax.set_title("21. Dark-background style")
    plt.show()
"""),
    C(r"""
%config InlineBackend.figure_format = 'retina'
fig, ax = plt.subplots(figsize=(5, 3))
ax.plot(np.sin(np.linspace(0, 6, 200)) * np.linspace(1, 0, 200)); ax.set_title("22. Retina PNG (2x pixels, width/height in output metadata)")
plt.show()
"""),
    C(r"""
%config InlineBackend.figure_format = 'png'
fig, ax = plt.subplots(figsize=(1.6, 1.0), dpi=60); ax.plot([1, 2, 3]); ax.set_title("23a. tiny", fontsize=6)
plt.show()
fig, ax = plt.subplots(figsize=(15, 11), dpi=100)
ax.imshow(rng.random((12, 16)), cmap="turbo", interpolation="nearest"); ax.set_title("23b. huge 1500 x 1100 figure")
plt.show()
"""),
    C(r"""
fig, ax = plt.subplots(figsize=(5, 3))
ax.plot([1, 2, 3], [2, 1, 3]); ax.set_title("24. CJK title: 日本語のタイトル / 中文标题 / 한국어")
ax.set_xlabel("emoji label 🚀")
plt.show()
"""),
    C(r"""
for i in range(3):
    fig, ax = plt.subplots(figsize=(3.2, 2.2)); ax.plot(rng.random(10)); ax.set_title(f"25. figure {i + 1} of 3 in one cell")
    plt.show()
"""),
    C(r"""
ts = pd.DataFrame({"a": np.cumsum(rng.normal(size=60)), "b": np.cumsum(rng.normal(size=60)), "c": np.cumsum(rng.normal(size=60))},
                  index=pd.date_range("2024-01-01", periods=60))
ts.plot(subplots=True, figsize=(6, 5), title="26. pandas DataFrame.plot(subplots=True)")    # bare: Axes array repr + figure
"""),
    C(r"""
from matplotlib import animation
from IPython.display import HTML
fig, ax = plt.subplots(figsize=(3.2, 2))
xs = np.linspace(0, 2 * np.pi, 100); (line,) = ax.plot(xs, np.sin(xs)); ax.set_ylim(-1.2, 1.2); ax.set_title("27. FuncAnimation")
def update(i):
    line.set_ydata(np.sin(xs + i / 2)); return (line,)
ani = animation.FuncAnimation(fig, update, frames=12, interval=80, blit=True)
plt.close(fig)
HTML(ani.to_jshtml())      # text/html: a JS player with base64 PNG frames
"""),
    M("## seaborn"),
    C(r"""
import seaborn as sns
sns.set_theme(style="whitegrid")
n = 240
tips = pd.DataFrame({"total_bill": rng.gamma(4, 5, n).round(2), "day": rng.choice(["Thu", "Fri", "Sat", "Sun"], n),
                     "smoker": rng.choice(["Yes", "No"], n), "size": rng.integers(1, 6, n)})
tips["tip"] = (tips.total_bill * rng.uniform(.08, .25, n)).round(2)
sns.scatterplot(data=tips, x="total_bill", y="tip", hue="day", style="smoker")     # bare: Axes repr + figure
"""),
    C(r"""
corr = tips[["total_bill", "tip", "size"]].assign(z=rng.normal(size=n)).corr()
plt.figure(figsize=(5, 4)); sns.heatmap(corr, annot=True, cmap="vlag", vmin=-1, vmax=1); plt.title("seaborn heatmap")
plt.show()
"""),
    C(r"""
sns.pairplot(tips[["total_bill", "tip", "size", "day"]], hue="day", height=1.7)       # PairGrid repr + large figure
"""),
    C(r"""
fig, (a1, a2) = plt.subplots(1, 2, figsize=(9, 3.6))
sns.violinplot(data=tips, x="day", y="tip", ax=a1, order=["Thu", "Fri", "Sat", "Sun"]); sns.lineplot(data=tips, x="size", y="tip", hue="smoker", ax=a2)
plt.show()
"""),
    M("## plotly"),
    C(r"""
import plotly.express as px, plotly.io as pio
print("default renderer:", pio.renderers.default)
fig = px.scatter(tips, x="total_bill", y="tip", color="day", size="size", title="plotly express scatter")
fig.show()
"""),
    C(r"""
import plotly.graph_objects as go
fig = go.Figure(go.Bar(x=["A", "B", "C", "D"], y=[4, 7, 3, 9], marker_color=["#636efa", "#ef553b", "#00cc96", "#ab63fa"]))
fig.update_layout(title="plotly graph_objects bar")
fig.show()
"""),
    C(r"""
X, Y = np.meshgrid(np.linspace(-3, 3, 40), np.linspace(-3, 3, 40))
go.Figure(go.Surface(z=np.sin(np.hypot(X, Y) * 2), x=X, y=Y), layout=dict(title="plotly 3D surface (bare figure -> _repr_mimebundle_)", height=450))
"""),
    M("## altair"),
    C(r"""
import altair as alt
cars = pd.DataFrame({"hp": rng.integers(60, 250, 120), "mpg": rng.normal(25, 6, 120).round(1), "origin": rng.choice(["USA", "Europe", "Japan"], 120)})
alt.Chart(cars).mark_point().encode(x="hp", y="mpg", color="origin", tooltip=["hp", "mpg", "origin"]).properties(title="altair scatter", width=360, height=220)
"""),
    C(r"""
base = alt.Chart(cars).encode(x=alt.X("origin:N"), y=alt.Y("mean(mpg):Q"))
(base.mark_bar(opacity=.6) + base.mark_point(color="red", size=100)).properties(title="altair layered chart", width=300)
"""),
    C(r"""
alt.Chart(cars).mark_bar().encode(x=alt.X("mpg:Q", bin=alt.Bin(maxbins=20)), y="count()", color="origin").facet("origin:N", columns=3).properties(title="altair faceted histogram")
"""),
    M("## bokeh"),
    C(r"""
from bokeh.plotting import figure, show
from bokeh.io import output_notebook
output_notebook()
p = figure(width=500, height=260, title="bokeh line + scatter")
xs = np.arange(30); p.line(xs, np.cumsum(rng.normal(size=30)), line_width=2); p.scatter(xs, np.cumsum(rng.normal(size=30)), size=8, color="firebrick")
show(p)
"""),
]


def build_plots():
    sections = [("## seaborn", "seaborn"), ("## plotly", "plotly"), ("## altair", "altair"), ("## bokeh", "bokeh")]
    nb, err = build(apply_needs(PLOTS, (), sections), "plots")
    execute(nb, "plots", err, timeout=300)
    write_nb(nb, "plots.ipynb",
             "~30 matplotlib charts (line/scatter/hist/bar/pie/box/violin/3D/subplots/heatmap/polar/retina/dark/CJK warnings/animation) "
             "plus seaborn, plotly (4.5 MB bundle in first output), altair (vegalite JSON) and bokeh (bokehjs_exec JSON).")



# ===========================================================================
# 4. errors.ipynb
# ===========================================================================

_DEEP_CHAIN = ("\n".join(f"def level_{i:02d}(x):\n    return level_{i + 1:02d}(x + 1)" for i in range(1, 15))
               + "\ndef level_15(x):\n    return 100 / (x - 14)\n\nlevel_01(0)")

ERRORS = [
    M(r"""
# Errors

Every kind of failure a notebook shows.  Cells after a failing cell still run (`allow_errors`), so
later cells are normal.  The traceback strings contain ANSI colour codes exactly as ipykernel sends them.
"""),
    C(r"""
import sys, os, time, warnings
print("setup ok")
"""),
    M("## SyntaxError family"),
    C("def broken(:\n    pass", err=True),
    C('print("unclosed paren"\nprint("next line")', err=True),
    C("if True:\nprint('no indent')", err=True),
    C("1abc = 3", err=True),
    M("## NameError / AttributeError / TypeError / KeyError / IndexError / FileNotFoundError"),
    C("print(undefined_variable_name)", err=True),
    C("import math\nmath.sinn(1.0)", err=True),
    C('"1" + 1', err=True),
    C('{"a": 1}["b"]', err=True),
    C('[1, 2, 3][10]', err=True),
    C('open("/definitely/not/here.txt")', err=True),
    C('import not_a_real_module_xyz', err=True),
    M("## A ZeroDivisionError fourteen frames deep"),
    C(_DEEP_CHAIN, err=True),
    M("## Recursion (IPython elides repeated frames)"),
    C(r"""
def descend(n):
    if n == 0:
        return 1 / 0
    return descend(n - 1) + 1

descend(25)
""", err=True),
    C(r"""
def forever(n):
    return forever(n + 1)

forever(0)
""", err=True),
    M("## Frames inside lambda / genexpr / comprehension"),
    C(r"""
data = [1, 2, 0, 4]
list(map(lambda v: 10 // v, data))
""", err=True),
    C(r"""
sum(10 // v for v in data)
""", err=True),
    M("## KeyboardInterrupt"),
    C(r"""
import os, signal, time
os.kill(os.getpid(), signal.SIGINT)    # a real SIGINT delivered to the kernel mid-cell
time.sleep(3)
print("unreachable")
""", err=True),
    C('raise KeyboardInterrupt("simulated Ctrl-C")', err=True),
    M("## Huge messages"),
    C('raise ValueError("long message: " + "lorem ipsum dolor sit amet " * 800)', err=True),
    C(r"""
raise ValueError("Validation failed:\n" + "\n".join(f"  - field_{i}: expected int, got 'str' (row {i * 7})" for i in range(150)))
""", err=True),
    M("## stdout + stderr, then failure"),
    C(r"""
import sys
print("starting step 1")
print("WARNING: disk almost full", file=sys.stderr)
print("step 2 ...", file=sys.stderr)
raise RuntimeError("step 2 failed after writing to stderr")
""", err=True),
    C(r"""
%%bash
echo "starting build"
echo "warning: something looks odd" >&2
ls /definitely/not/here
exit 3
""", err=True),
    M("## Chained, grouped and custom exceptions"),
    C(r"""
def parse(s):
    try:
        return int(s)
    except ValueError as e:
        raise RuntimeError(f"could not parse {s!r}") from e

parse("12x")
""", err=True),
    C(r"""
try:
    1 / 0
except ZeroDivisionError:
    undefined_name      # NameError raised while handling the ZeroDivisionError
""", err=True),
    C(r"""
raise ExceptionGroup("multiple failures", [ValueError("bad value"), TypeError("bad type"), ExceptionGroup("nested", [KeyError("k")])])
"""),   # IPython 8.30 cannot format exception groups: the kernel stores a stderr stream, not an error output
    C(r"""
class DomainError(Exception):
    def __init__(self, code, msg):
        super().__init__(f"[{code}] {msg}")
        self.code = code

raise DomainError(42, "Ünïcödé message 日本語 🚀 \twith a tab")
""", err=True),
    C("raise Exception()", err=True),
    C("raise ValueError", err=True),
    C("import sys\nsys.exit(3)", err=True),
    C('assert 1 + 1 == 3, "arithmetic is broken:\\n  expected 3\\n  got 2"', err=True),
    C('b"\\xff\\xfe".decode("utf-8")', err=True),
    C(r"""
with warnings.catch_warnings():
    warnings.simplefilter("error")
    warnings.warn("promoted to an error", RuntimeWarning)
""", err=True),
    M("## Succeeding cells between failures"),
    C(r"""
print("this cell runs fine after all those failures")
21 * 2
"""),
    M("## An object whose repr() raises"),
    C(r"""
class Bad:
    def __repr__(self):
        raise RuntimeError("repr failed")

Bad()
""", err=True),
]


def _hand_error_cells(name: str, start: int, base_count: int):
    """Error outputs that real ipykernel never writes but other kernels / tools do."""
    ansi_tb = ["\x1b[0;31m---------------------------------------------------------------------------\x1b[0m",
               "\x1b[0;31mZeroDivisionError\x1b[0m                         Traceback (most recent call last)",
               "Cell \x1b[0;32mIn[1], line 1\x1b[0m\n\x1b[0;32m----> 1\x1b[0m \x1b[38;5;241;43m1\x1b[39;49m\x1b[38;5;241;43m/\x1b[39;49m\x1b[38;5;241;43m0\x1b[39;49m\n",
               "\x1b[0;31mZeroDivisionError\x1b[0m: division by zero"]
    plain_tb = ["---------------------------------------------------------------------------",
                "ZeroDivisionError                         Traceback (most recent call last)",
                "<ipython-input-1-9e1622b385b6> in <module>\n----> 1 1/0\n",
                "ZeroDivisionError: division by zero"]
    specs = [
        ("# error output with an EMPTY traceback list", dict(ename="CustomError", evalue="no traceback was recorded", traceback=[])),
        ("# traceback WITHOUT ANSI colour codes (ipython --colors=NoColor / old nbconvert)", dict(ename="ZeroDivisionError", evalue="division by zero", traceback=plain_tb)),
        ("# the whole traceback packed into ONE string", dict(ename="ZeroDivisionError", evalue="division by zero", traceback=["\n".join(plain_tb)])),
        ("# empty ename and evalue", dict(ename="", evalue="", traceback=[])),
        ("# R kernel (IRkernel) error", dict(ename="simpleError", evalue="object 'x' not found", traceback=["Error in eval(expr, envir, enclos): object 'x' not found\n", "1. stop(...)"])),
        ("# Python 2 era traceback", dict(ename="IOError", evalue="[Errno 2] No such file or directory: 'x.txt'",
                                          traceback=["\x1b[1;31m---------------------------------------------------------------------------\x1b[0m",
                                                     "\x1b[1;31mIOError\x1b[0m                                   Traceback (most recent call last)",
                                                     "\x1b[1;32m<ipython-input-3-b8a2d8a0fd9b>\x1b[0m in \x1b[0;36m<module>\x1b[1;34m()\x1b[0m\n\x1b[1;32m----> 1\x1b[1;33m \x1b[0mopen\x1b[0m\x1b[1;33m(\x1b[0m\x1b[1;34m'x.txt'\x1b[0m\x1b[1;33m)\x1b[0m\x1b[1;33m\x1b[0m\x1b[0m\n",
                                                     "\x1b[1;31mIOError\x1b[0m: [Errno 2] No such file or directory: 'x.txt'"])),
        ("# huge multi-megabyte-ish traceback frame list (2000 frames)",
         dict(ename="RecursionError", evalue="maximum recursion depth exceeded",
              traceback=[f"File \x1b[0;32m/app/module_{i % 7}.py:{100 + i}\x1b[0m, in \x1b[0;36mfunc_{i}\x1b[0;34m(x)\x1b[0m\n\x1b[1;32m    {100 + i}\x1b[0m     return func_{i + 1}(x + 1)\n" for i in range(2000)])),
    ]
    out = [new_markdown_cell("## Hand-written error outputs (appended after execution)\n\nThese could not be produced by ipykernel; "
                             "they model other kernels and tools.")]
    out[0]["id"] = cell_id(name, start)
    for k, (src, err) in enumerate(specs):
        c = new_code_cell(src, execution_count=base_count + k + 1,
                          outputs=[new_output("error", **err)])
        c["id"] = cell_id(name, start + 1 + k)
        out.append(c)
    return out


def build_errors():
    nb, err = build(ERRORS, "errors")
    execute(nb, "errors", err, timeout=180)
    last = max(c.execution_count or 0 for c in nb.cells if c.cell_type == "code")
    nb.cells.extend(_hand_error_cells("errors", len(nb.cells), last))
    write_nb(nb, "errors.ipynb",
             "Every failure shape: SyntaxError/IndentationError, NameError, 15-frame stack, recursion (frame elision), real SIGINT KeyboardInterrupt, "
             "20k-char and 150-line messages, stderr-then-fail, failing %%bash, chained/custom exceptions, SystemExit, ExceptionGroup (stderr only), "
             "plus hand-written error outputs (empty/plain/single-string tracebacks).")



# ===========================================================================
# 5. formats/  (hand-built edge cases)
# ===========================================================================

def ml(text: str):
    """multiline string as the line-list form nbformat writes."""
    return lines(text)


PY3_LANG = {"codemirror_mode": {"name": "ipython", "version": 3}, "file_extension": ".py", "mimetype": "text/x-python",
            "name": "python", "nbconvert_exporter": "python", "pygments_lexer": "ipython3", "version": "3.11.4"}
PY3_KS = {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"}


def d_md(src, id=None, meta=None, as_str=False):
    c = {"cell_type": "markdown", "metadata": meta or {}, "source": src if as_str else ml(src)}
    if id:
        c["id"] = id
    return c


def d_raw(src, id=None, meta=None, as_str=False):
    c = {"cell_type": "raw", "metadata": meta or {}, "source": src if as_str else ml(src)}
    if id:
        c["id"] = id
    return c


def d_code(src, outputs=(), count=None, id=None, meta=None, as_str=False):
    c = {"cell_type": "code", "execution_count": count, "metadata": meta or {}, "outputs": list(outputs),
         "source": src if as_str else ml(src)}
    if id:
        c["id"] = id
    return c


def d_stream(text, name="stdout", as_str=False):
    return {"name": name, "output_type": "stream", "text": text if as_str else ml(text)}


def d_result(data, count, meta=None):
    return {"data": data, "execution_count": count, "metadata": meta or {}, "output_type": "execute_result"}


def d_display(data, meta=None):
    return {"data": data, "metadata": meta or {}, "output_type": "display_data"}


def d_error(ename, evalue, tb):
    return {"ename": ename, "evalue": evalue, "output_type": "error", "traceback": tb}


def d_nb(cells, minor=5, meta=None, major=4):
    return {"cells": cells, "metadata": meta if meta is not None else {"kernelspec": dict(PY3_KS), "language_info": dict(PY3_LANG)},
            "nbformat": major, "nbformat_minor": minor}


def hand(rel: str, obj, desc: str, validate=True, **dump_kw):
    dump_json(ROOT / rel, obj, **dump_kw)
    reg(rel, desc, validate)


SMALL_PNG = b64(png_bytes(64, 40, "gradient"))
TINY_PDF = ("%PDF-1.1\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n"
            "3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n"
            "4 0 obj<</Length 44>>stream\nBT /F1 18 Tf 20 50 Td (Herse PDF) Tj ET\nendstream endobj\n"
            "5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n")


def _fmt_nbformat_versions():
    # --- 4.0: a Python 2 era notebook, no cell ids, `collapsed` metadata ------------------------------
    py2_meta = {
        "kernelspec": {"display_name": "Python 2", "language": "python", "name": "python2"},
        "language_info": {"codemirror_mode": {"name": "ipython", "version": 2}, "file_extension": ".py",
                          "mimetype": "text/x-python", "name": "python", "nbconvert_exporter": "python",
                          "pygments_lexer": "ipython2", "version": "2.7.10"}}
    cells = [
        d_md("# Legacy notebook (nbformat 4.0)\n\nWritten by Jupyter 4.0 with a **Python 2** kernel.  No cell ids anywhere."),
        d_code('print "hello world"\nprint 1/2', [d_stream("hello world\n0\n")], 1, meta={"collapsed": False}),
        d_code("import numpy as np\nnp.arange(5)", [d_result({"text/plain": ml("array([0, 1, 2, 3, 4])")}, 2)], 2, meta={"collapsed": False}),
        d_code("%matplotlib inline\nimport matplotlib.pyplot as plt\nplt.plot([1, 2, 3])",
               [d_result({"text/plain": ml("[<matplotlib.lines.Line2D at 0x7f1f8e0a9d50>]")}, 3),
                d_display({"image/png": SMALL_PNG, "text/plain": ml("<matplotlib.figure.Figure at 0x7f1f8e0e9190>")})],
               3, meta={"collapsed": False, "scrolled": True}),
        d_code("1 / 0", [d_error("ZeroDivisionError", "integer division or modulo by zero",
                                 ["\x1b[1;31m---------------------------------------------------------------------------\x1b[0m",
                                  "\x1b[1;31mZeroDivisionError\x1b[0m                         Traceback (most recent call last)",
                                  "\x1b[1;32m<ipython-input-4-b710d87c980c>\x1b[0m in \x1b[0;36m<module>\x1b[1;34m()\x1b[0m\n\x1b[1;32m----> 1\x1b[1;33m \x1b[1;36m1\x1b[0m \x1b[1;33m/\x1b[0m \x1b[1;36m0\x1b[0m\x1b[1;33m\x1b[0m\x1b[0m\n\x1b[0m",
                                  "\x1b[1;31mZeroDivisionError\x1b[0m: integer division or modulo by zero"])], 4,
               meta={"collapsed": False}),
        d_code("", [], None, meta={"collapsed": True}),
    ]
    hand("formats/nbformat_4_0_no_ids.ipynb", d_nb(cells, 0, py2_meta),
         "nbformat 4.0 (Python 2 era): no cell ids, `collapsed` cell metadata, Python-2 traceback, trailing empty cell.")

    # --- 4.4: classic Notebook 6 with nbextensions metadata --------------------------------------------
    meta44 = {"kernelspec": dict(PY3_KS), "language_info": dict(PY3_LANG),
              "hide_input": False,
              "toc": {"base_numbering": 1, "nav_menu": {}, "number_sections": True, "sideBar": True, "skip_h1_title": False,
                      "title_cell": "Table of Contents", "title_sidebar": "Contents", "toc_cell": False,
                      "toc_position": {}, "toc_section_display": True, "toc_window_display": False},
              "varInspector": {"cols": {"lenName": 16, "lenType": 16, "lenVar": 40}, "kernels_config": {
                  "python": {"delete_cmd_postfix": "", "delete_cmd_prefix": "del ", "library": "var_list.py", "varRefreshCmd": "print(var_dic_list())"}},
                  "types_to_exclude": ["module", "function", "builtin_function_or_method", "instance", "_Feature"], "window_display": False}}
    cells = [
        d_md("# nbformat 4.4 (Notebook 6.x)\n\nNo cell ids (they arrived in 4.5).  Cell metadata carries `ExecuteTime` and `collapsed` flags from nbextensions."),
        d_code("import math\nprint(math.pi)", [d_stream("3.141592653589793\n")], 1,
               meta={"ExecuteTime": {"end_time": "2020-03-04T10:11:12.131415Z", "start_time": "2020-03-04T10:11:12.120000Z"}}),
        d_code("[x ** 2 for x in range(5)]", [d_result({"text/plain": ml("[0, 1, 4, 9, 16]")}, 2)], 2,
               meta={"ExecuteTime": {"end_time": "2020-03-04T10:11:15.000000Z", "start_time": "2020-03-04T10:11:14.990000Z"}}),
        d_code("from IPython.display import HTML\nHTML('<b>bold</b> and <i>italic</i>')",
               [d_result({"text/html": ml("<b>bold</b> and <i>italic</i>"), "text/plain": ml("<IPython.core.display.HTML object>")}, 3)], 3,
               meta={"scrolled": True}),
        d_code("raise ValueError('boom')", [d_error("ValueError", "boom", [
            "\x1b[0;31m---------------------------------------------------------------------------\x1b[0m",
            "\x1b[0;31mValueError\x1b[0m                                Traceback (most recent call last)",
            "\x1b[0;32m<ipython-input-4-d3c1d6c7d3a1>\x1b[0m in \x1b[0;36m<module>\x1b[0;34m\x1b[0m\n\x1b[0;32m----> 1\x1b[0;31m \x1b[0;32mraise\x1b[0m \x1b[0mValueError\x1b[0m\x1b[0;34m(\x1b[0m\x1b[0;34m'boom'\x1b[0m\x1b[0;34m)\x1b[0m\x1b[0;34m\x1b[0m\x1b[0;34m\x1b[0m\x1b[0m\n\x1b[0m",
            "\x1b[0;31mValueError\x1b[0m: boom"])], 4),
    ]
    hand("formats/nbformat_4_4_no_ids.ipynb", d_nb(cells, 4, meta44),
         "nbformat 4.4 (Notebook 6): no cell ids, ExecuteTime / toc / varInspector nbextension metadata.")

    # --- 4.5: with ids ---------------------------------------------------------------------------------
    cells = [
        d_md("# nbformat 4.5\n\nEvery cell has an `id` (1-64 chars of `[a-zA-Z0-9-_]`).", id="intro-cell"),
        d_code("print('hello')", [d_stream("hello\n")], 1, id="a1b2c3d4"),
        d_code("21 * 2", [d_result({"text/plain": ml("42")}, 2)], 2, id="cell_with-dash_and_underscore"),
        d_raw("raw cell with an id", id="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),   # 64 chars
        d_code("", [], None, id="x"),                                                                          # 1 char
    ]
    hand("formats/nbformat_4_5_with_ids.ipynb", d_nb(cells, 5),
         "nbformat 4.5 with cell ids (1-char, 64-char, dashes/underscores).")

    # --- a future minor version ------------------------------------------------------------------------
    cells = [d_md("# nbformat_minor from the future\n\nnbformat 4.9: readers should open it anyway.", id="f1"),
             d_code("print('from the future')", [d_stream("from the future\n")], 1, id="f2",
                    meta={"a_future_field": {"nested": [1, 2, 3]}})]
    nb = d_nb(cells, 9)
    nb["a_future_top_level_key"] = "ignored"
    hand("formats/nbformat_4_9_future_minor.ipynb", nb, "nbformat 4.9 (minor newer than any released): must still open; unknown keys at every level.")


def _fmt_string_variants():
    # source as a single string -------------------------------------------------------------------------
    cells = [
        d_md("# `source` as a single string\n\nThe schema allows either a list of lines or one string.", id="s1", as_str=True),
        d_code("import os\nprint('a single string source')\nfor i in range(3):\n    print(i)", [d_stream("a single string source\n0\n1\n2\n")], 1, id="s2", as_str=True),
        d_code("x = 1", [], 2, id="s3", as_str=True),
        d_raw("raw as a string\nwith two lines", id="s4", as_str=True),
        d_code("", [], None, id="s5", as_str=True),
    ]
    hand("formats/source_single_string.ipynb", d_nb(cells, 5), "`source` stored as one string (not a line list) for markdown, code and raw cells.")

    # output text as a single string -----------------------------------------------------------------------
    cells = [
        d_md("# Output `text` as single strings\n\nstream `text`, `data` values and traceback entries stored as plain strings.", id="o1"),
        d_code("print('one')\nprint('two')", [d_stream("one\ntwo\n", as_str=True)], 1, id="o2"),
        d_code("print('err')", [d_stream("err\n", name="stderr", as_str=True)], 2, id="o3"),
        d_code("1+1", [d_result({"text/plain": "2"}, 3)], 3, id="o4"),
        d_code("HTML('<b>x</b>')", [d_result({"text/html": "<b>x</b>\n<i>y</i>", "text/plain": "<IPython.core.display.HTML object>"}, 4)], 4, id="o5"),
        d_code("1/0", [d_error("ZeroDivisionError", "division by zero", ["Traceback (most recent call last):\n  File \"<stdin>\", line 1, in <module>\nZeroDivisionError: division by zero"])], 5, id="o6"),
    ]
    hand("formats/output_text_single_string.ipynb", d_nb(cells, 5), "Stream `text` and `data` values stored as single strings instead of line lists.")

    # mime variants ---------------------------------------------------------------------------------------
    wrapped = "\n".join(SMALL_PNG[i:i + 76] for i in range(0, len(SMALL_PNG), 76))
    cells = [
        d_md("# MIME bundle variants\n\nOne cell per shape of `data`.", id="m0"),
        d_code("png as one string with trailing newline", [d_display({"image/png": SMALL_PNG + "\n", "text/plain": ml("<IPython.core.display.Image object>")})], 1, id="m1"),
        d_code("png as a LIST of 76-column base64 lines", [d_display({"image/png": ml(wrapped + "\n"), "text/plain": ml("<IPython.core.display.Image object>")})], 2, id="m2"),
        d_code("png with embedded newlines in the string", [d_display({"image/png": wrapped, "text/plain": ["x"]})], 3, id="m3"),
        d_code("png with needs_background / width / height metadata",
               [d_display({"image/png": SMALL_PNG, "text/plain": ml("<Figure size 64x40>")}, {"needs_background": "light", "image/png": {"height": 40, "width": 64}})], 4, id="m4"),
        d_code("jpeg + gif", [d_display({"image/jpeg": b64(png_bytes(80, 50, "gradient", fmt="JPEG")), "text/plain": ["jpeg"]}),
                              d_display({"image/gif": "R0lGODlhAQABAIAAAAUEBAAAACwAAAAAAQABAAACAkQBADs=", "text/plain": ["gif"]})], 5, id="m5"),
        d_code("svg as a list", [d_display({"image/svg+xml": ml(BADGE_SVG.replace("><", ">\n<")), "text/plain": ["<IPython.core.display.SVG object>"]})], 6, id="m6"),
        d_code("html isolated", [d_display({"text/html": ml("<h3 style='color:purple'>isolated html</h3><script>document.title='x'</script>"), "text/plain": ["html"]}, {"isolated": True})], 7, id="m7"),
        d_code("json / latex / markdown", [d_display({"application/json": {"a": [1, 2, {"b": None}], "c": "d"}, "text/plain": ["<IPython.core.display.JSON object>"]},
                                                      {"application/json": {"expanded": True, "root": "root"}}),
                                          d_display({"text/latex": ml("$\\alpha^2 + \\beta^2$"), "text/plain": ["alpha"]}),
                                          d_display({"text/markdown": ml("**bold** *italic* `code`\n\n- a\n- b"), "text/plain": ["md"]})], 8, id="m8"),
        d_code("pdf", [d_display({"application/pdf": b64(TINY_PDF.encode()), "text/plain": ["<IPython.core.display.IFrame object>"]})], 9, id="m9"),
        d_code("javascript", [d_display({"application/javascript": ml("element.text('js ran');"), "text/plain": ["<IPython.core.display.Javascript object>"]})], 10, id="m10"),
        d_code("unknown mime only (no text/plain)", [d_display({"application/x-unknown": "opaque payload for an unknown mime type"})], 11, id="m11"),
        d_code("empty data bundle", [d_display({})], 12, id="m12"),
        d_code("text/plain with ANSI colours", [d_result({"text/plain": ml("\x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[0m plain")}, 13)], 13, id="m13"),
        d_code("execute_result with metadata and no text/plain", [d_result({"text/html": ["<em>only html</em>"]}, 14, {"anything": 1})], 14, id="m14"),
        d_code("vega-lite + vega", [d_display({"application/vnd.vegalite.v4+json": {"$schema": "https://vega.github.io/schema/vega-lite/v4.json", "data": {"values": [{"a": "A", "b": 3}, {"a": "B", "b": 7}]}, "mark": "bar", "encoding": {"x": {"field": "a", "type": "nominal"}, "y": {"field": "b", "type": "quantitative"}}},
                                              "text/plain": ["<VegaLite 4 object>"]})], 15, id="m15"),
        d_code("geojson + plotly + holoviews style", [d_display({"application/geo+json": {"type": "Point", "coordinates": [102.0, 0.5]}, "text/plain": ["<IPython.display.GeoJSON object>"]}),
                                                       d_display({"application/vnd.holoviews_load.v0+json": "", "application/javascript": ["/* holoviews load */"]})], 16, id="m16"),
    ]
    hand("formats/mime_data_variants.ipynb", d_nb(cells, 5),
         "Every shape of an output `data` bundle: png as string/list/wrapped, needs_background, jpeg, gif, svg list, isolated html, json, latex, markdown, pdf, js, unknown-only, empty, vegalite, geojson.")


def _fmt_tool_styles():
    # --- Colab (old style: nbformat first, id/colab_type in cell metadata, 2-space indent, unsorted keys) --
    uid = "9f2c1c1e-6d6a-4a3b-8f7a-0d2f7c6f7a11"
    colab = {
        "nbformat": 4,
        "nbformat_minor": 0,
        "metadata": {
            "colab": {"name": "Untitled0.ipynb", "provenance": [{"file_id": "1AbCdEfGhIjKlMnOpQrStUvWxYz012345", "timestamp": 1690000000000}],
                      "collapsed_sections": ["abc123"], "toc_visible": True, "machine_shape": "hm", "gpuType": "T4"},
            "kernelspec": {"name": "python3", "display_name": "Python 3"},
            "language_info": {"name": "python"},
            "accelerator": "GPU",
        },
        "cells": [
            {"cell_type": "markdown", "metadata": {"id": "view-in-github", "colab_type": "text"},
             "source": ['<a href="https://colab.research.google.com/github/example/repo/blob/main/Untitled0.ipynb" target="_parent">'
                        '<img src="https://colab.research.google.com/assets/colab-badge.svg" alt="Open In Colab"/></a>']},
            {"cell_type": "markdown", "metadata": {"id": "abc123", "colab_type": "text"},
             "source": ["# Colab-style notebook\n", "\n", "Cell ids live in `metadata.id`; outputs carry `outputId`; the file is 2-space indented with unsorted keys and no trailing newline."]},
            {"cell_type": "code", "metadata": {"id": "pip001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/"}, "outputId": uid},
             "source": ["!pip install -q tqdm requests"], "execution_count": 1,
             "outputs": [{"output_type": "stream", "name": "stdout", "text": [
                 "\r\u001b[K     |████████████████████████████████| 77 kB 3.1 MB/s eta 0:00:01\r\u001b[K     |████████████████████████████████| 77 kB 3.1 MB/s \n",
                 "\u001b[?25h"]}]},
            {"cell_type": "code", "metadata": {"id": "mnt001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/"}, "outputId": "1d1a8a63-3c25-4f5b-8bbd-6f8d1f1d0f0a"},
             "source": ["from google.colab import drive\n", "drive.mount('/content/drive')"], "execution_count": 2,
             "outputs": [{"output_type": "stream", "name": "stdout", "text": ["Mounted at /content/drive\n"]}]},
            {"cell_type": "code", "metadata": {"id": "form001", "cellView": "form", "colab": {"base_uri": "https://localhost:8080/"}, "outputId": "2b3b0c2a-5e0b-4d1c-a8d5-2f0f0b8e1111"},
             "source": ["#@title Parameters { run: \"auto\" }\n", "learning_rate = 0.001 #@param {type:\"number\"}\n", "optimizer = \"adam\" #@param [\"adam\", \"sgd\"]\n", "print(learning_rate, optimizer)"],
             "execution_count": 3, "outputs": [{"output_type": "stream", "name": "stdout", "text": ["0.001 adam\n"]}]},
            {"cell_type": "code", "metadata": {"id": "plot001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/", "height": 265}, "outputId": "5d3a2b0e-0000-4000-8000-000000000001"},
             "source": ["import matplotlib.pyplot as plt\n", "plt.plot([1, 4, 9])\n", "plt.show()"], "execution_count": 4,
             "outputs": [{"output_type": "display_data", "data": {"text/plain": ["<Figure size 432x288 with 1 Axes>"], "image/png": SMALL_PNG},
                         "metadata": {"tags": [], "needs_background": "light"}}]},
            {"cell_type": "code", "metadata": {"id": "df001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/", "height": 142}, "outputId": "5d3a2b0e-0000-4000-8000-000000000002"},
             "source": ["import pandas as pd\n", "pd.DataFrame({'a': [1, 2], 'b': ['x', 'y']})"], "execution_count": 5,
             "outputs": [{"output_type": "execute_result", "execution_count": 5, "metadata": {"tags": []},
                          "data": {"text/plain": ["   a  b\n", "0  1  x\n", "1  2  y"],
                                   "text/html": ['\n  <div id="df-5f1e" class="colab-df-container">\n    <div>\n<style scoped>\n    .dataframe tbody tr th:only-of-type {\n        vertical-align: middle;\n    }\n</style>\n'
                                                 '<table border="1" class="dataframe">\n  <thead>\n    <tr style="text-align: right;">\n      <th></th>\n      <th>a</th>\n      <th>b</th>\n    </tr>\n  </thead>\n  <tbody>\n'
                                                 '    <tr>\n      <th>0</th>\n      <td>1</td>\n      <td>x</td>\n    </tr>\n    <tr>\n      <th>1</th>\n      <td>2</td>\n      <td>y</td>\n    </tr>\n  </tbody>\n</table>\n</div>\n'
                                                 '    <div class="colab-df-buttons">\n      <button class="colab-df-convert" onclick="convertToInteractive(\'df-5f1e\')"\n              title="Convert this dataframe to an interactive table."\n              style="display:none;">\n        Convert\n      </button>\n    </div>\n'
                                                 '    <script>\n      const buttonEl =\n        document.querySelector(\'#df-5f1e button.colab-df-convert\');\n      buttonEl.style.display =\n        google.colab.kernel.accessAllowed ? \'block\' : \'none\';\n    </script>\n  </div>\n  ']}}]},
            {"cell_type": "code", "metadata": {"id": "big001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/", "height": 70}, "outputId": "5d3a2b0e-0000-4000-8000-000000000003"},
             "source": ["'x' * 100000"], "execution_count": 6,
             "outputs": [{"output_type": "execute_result", "execution_count": 6, "metadata": {"tags": []},
                          "data": {"application/vnd.google.colaboratory.intrinsic+json": {"type": "string"}, "text/plain": ["'" + "x" * 2000 + "...'"]}}]},
            {"cell_type": "code", "metadata": {"id": "err001", "colab_type": "code", "colab": {"base_uri": "https://localhost:8080/", "height": 197}, "outputId": "5d3a2b0e-0000-4000-8000-000000000004"},
             "source": ["import not_installed"], "execution_count": 7,
             "outputs": [{"output_type": "error", "ename": "ModuleNotFoundError", "evalue": "No module named 'not_installed'",
                          "traceback": ["\u001b[0;31m---------------------------------------------------------------------------\u001b[0m",
                                        "\u001b[0;31mModuleNotFoundError\u001b[0m                       Traceback (most recent call last)",
                                        "\u001b[0;32m<ipython-input-7-2f0f8f4f9d4e>\u001b[0m in \u001b[0;36m<module>\u001b[0;34m()\u001b[0m\n\u001b[0;32m----> 1\u001b[0;31m \u001b[0;32mimport\u001b[0m \u001b[0mnot_installed\u001b[0m\u001b[0;34m\u001b[0m\u001b[0;34m\u001b[0m\u001b[0m\n\u001b[0m",
                                        "\u001b[0;31mModuleNotFoundError\u001b[0m: No module named 'not_installed'",
                                        "",
                                        "---------------------------------------------------------------------------",
                                        "NOTE: If your import is failing due to a missing package, you can",
                                        "manually install dependencies using either !pip or !apt.",
                                        "",
                                        "To view examples of installing some common dependencies, click the",
                                        "\"Open Examples\" button below.",
                                        "---------------------------------------------------------------------------\n"]}]},
            {"cell_type": "code", "metadata": {"id": "empty001", "colab_type": "code"}, "source": [], "execution_count": None, "outputs": []},
        ],
    }
    hand("formats/colab_style.ipynb", colab, "Google Colab export: indent=2, unsorted keys, no trailing newline, `metadata.id`/`outputId`/`colab`, "
         "pip `\\r\\x1b[K` progress, intrinsic+json mime, colab-df html, #@title form cell.", indent=2, sort_keys=False, trailing_nl=False)

    # --- VS Code style: indent=1, nbformat_minor 2, vscode metadata ---------------------------------------
    vs = {
        "cells": [
            {"cell_type": "markdown", "metadata": {}, "source": ["# VS Code notebook\n", "\n", "`nbformat_minor` is 2 (no ids), `orig_nbformat` and `vscode` metadata present."]},
            {"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [{"name": "stdout", "output_type": "stream", "text": ["3.11.5\n"]}],
             "source": ["import platform\n", "print(platform.python_version())"]},
            {"cell_type": "code", "execution_count": 2, "metadata": {"vscode": {"languageId": "shellscript"}},
             "outputs": [{"name": "stdout", "output_type": "stream", "text": ["total 0\n"]}], "source": ["%%bash\n", "ls -l | head -1"]},
            {"cell_type": "code", "execution_count": 3, "metadata": {}, "outputs": [
                {"data": {"text/plain": ["<Figure size 640x480 with 1 Axes>"], "image/png": SMALL_PNG}, "metadata": {}, "output_type": "display_data"}],
             "source": ["import matplotlib.pyplot as plt\n", "plt.plot([3, 1, 2])"]},
            {"cell_type": "code", "execution_count": None, "metadata": {"vscode": {"languageId": "python"}}, "outputs": [], "source": ["# not run yet"]},
        ],
        "metadata": {
            "kernelspec": {"display_name": "Python 3.11.5 ('venv': venv)", "language": "python", "name": "python3"},
            "language_info": {"codemirror_mode": {"name": "ipython", "version": 3}, "file_extension": ".py", "mimetype": "text/x-python",
                              "name": "python", "nbconvert_exporter": "python", "pygments_lexer": "ipython3", "version": "3.11.5"},
            "orig_nbformat": 4,
            "vscode": {"interpreter": {"hash": "5f1d6e9d8a4d1b4c2b0a3e7f9c6d5b4a3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f"}},
        },
        "nbformat": 4,
        "nbformat_minor": 2,
    }
    hand("formats/vscode_style.ipynb", vs, "VS Code export: indent=1, nbformat 4.2 (no ids), `orig_nbformat`, `vscode.interpreter.hash`, per-cell `languageId`.", indent=1)

    # --- reversed keys + tab indent, and a minified single-line file ------------------------------------------
    base = d_nb([d_md("# Tab-indented, reverse-sorted keys\n\nA third whitespace/ordering style.", id="t1"),
                 d_code("print('tabs')", [d_stream("tabs\n")], 1, id="t2"),
                 d_code("1+1", [d_result({"text/plain": ["2"]}, 2)], 2, id="t3")], 5)

    def rev(o):
        if isinstance(o, dict):
            return {k: rev(o[k]) for k in sorted(o, reverse=True)}
        if isinstance(o, list):
            return [rev(x) for x in o]
        return o
    hand("formats/tab_indent_reverse_keys.ipynb", rev(base), "Tab-indented JSON with reverse-alphabetical keys (`nbformat` first, `source` before `cell_type`).", indent="\t", sort_keys=False)

    mini = d_nb([d_md("# Minified notebook\n\nThe whole file is one line without any whitespace (as written by `jq -c` or some servers).", id="n1"),
                 d_code("print('one line')", [d_stream("one line\n")], 1, id="n2"),
                 d_code("import json\njson.dumps({'k': [1, 2, 3]})", [d_result({"text/plain": ["'{\"k\": [1, 2, 3]}'"]}, 2)], 2, id="n3"),
                 d_code("plot", [d_display({"image/png": SMALL_PNG, "text/plain": ["<Figure>"]})], 3, id="n4")], 5)
    hand("formats/minified_single_line.ipynb", mini, "The entire notebook on a single line, no whitespace, no trailing newline.", indent=None, trailing_nl=False)

    # --- Jupytext pair -------------------------------------------------------------------------------------
    jt_meta = {"jupytext": {"formats": "ipynb,py:percent", "text_representation": {"extension": ".py", "format_name": "percent", "format_version": "1.3", "jupytext_version": "1.16.4"}},
               "kernelspec": {"display_name": "Python 3", "language": "python", "name": "python3"},
               "language_info": {"codemirror_mode": {"name": "ipython", "version": 3}, "file_extension": ".py", "mimetype": "text/x-python", "name": "python",
                                 "nbconvert_exporter": "python", "pygments_lexer": "ipython3", "version": "3.12.2"}}
    cells = [
        d_md("# Jupytext paired notebook\n\nPaired with `jupytext_paired.py` (percent format).  Outputs live only in the `.ipynb`.", id="j1", meta={"jupytext": {"cell_metadata_filter": "-all"}}),
        d_code("import numpy as np\nimport pandas as pd", [], 1, id="j2", meta={"lines_to_next_cell": 0}),
        d_code("def square(x):\n    return x * x\n\n\nsquare(7)", [d_result({"text/plain": ["49"]}, 2)], 2, id="j3", meta={"lines_to_end_of_cell_marker": 2, "lines_to_next_cell": 1}),
        d_code("# %% tags=['parameters']\nalpha = 0.5", [], 3, id="j4", meta={"tags": ["parameters"], "title": "Parameters"}),
        d_raw("---\ntitle: front matter in a raw cell\n---", id="j5", meta={"active": "", "raw_mimetype": ""}),
    ]
    hand("formats/jupytext_paired.ipynb", d_nb(cells, 5, jt_meta), "Jupytext-paired notebook: `metadata.jupytext` formats/text_representation, lines_to_next_cell cell metadata.")
    py = textwrap.dedent('''\
        # ---
        # jupyter:
        #   jupytext:
        #     formats: ipynb,py:percent
        #     text_representation:
        #       extension: .py
        #       format_name: percent
        #       format_version: '1.3'
        #       jupytext_version: 1.16.4
        #   kernelspec:
        #     display_name: Python 3
        #     language: python
        #     name: python3
        # ---

        # %% [markdown]
        # # Jupytext paired notebook
        #
        # Paired with `jupytext_paired.ipynb` (percent format).  Outputs live only in the `.ipynb`.

        # %%
        import numpy as np
        import pandas as pd
        # %%
        def square(x):
            return x * x


        square(7)

        # %% tags=["parameters"] title="Parameters"
        alpha = 0.5
        ''')
    (ROOT / "formats/jupytext_paired.py").write_text(py)
    reg("formats/jupytext_paired.py", "The text (percent) half of the Jupytext pair; not a notebook.", validate=False)


def _fmt_kernels():
    # --- R --------------------------------------------------------------------------------------------------
    r_meta = {"kernelspec": {"display_name": "R", "language": "R", "name": "ir"},
              "language_info": {"codemirror_mode": "r", "file_extension": ".r", "mimetype": "text/x-r-source", "name": "R", "pygments_lexer": "r", "version": "4.3.2"}}
    r_html = ('<table class="dataframe">\n<caption>A data.frame: 3 x 4</caption>\n<thead>\n\t<tr><th></th><th scope=col>mpg</th><th scope=col>cyl</th><th scope=col>disp</th><th scope=col>hp</th></tr>\n'
              '\t<tr><th></th><th scope=col>&lt;dbl&gt;</th><th scope=col>&lt;dbl&gt;</th><th scope=col>&lt;dbl&gt;</th><th scope=col>&lt;dbl&gt;</th></tr>\n</thead>\n<tbody>\n'
              '\t<tr><th scope=row>Mazda RX4</th><td>21.0</td><td>6</td><td>160</td><td>110</td></tr>\n\t<tr><th scope=row>Mazda RX4 Wag</th><td>21.0</td><td>6</td><td>160</td><td>110</td></tr>\n'
              '\t<tr><th scope=row>Datsun 710</th><td>22.8</td><td>4</td><td>108</td><td> 93</td></tr>\n</tbody>\n</table>\n')
    cells = [
        d_md("# R notebook (IRkernel)\n\nOnly metadata and stored outputs; never executed here.", id="r0"),
        d_code('library(ggplot2)\nx <- c(1, 2, 3)\nmean(x)', [
            d_stream("Warning message:\n\"package 'ggplot2' was built under R version 4.3.3\"\n", "stderr"),
            d_result({"text/html": ["2"], "text/latex": ["2"], "text/markdown": ["2"], "text/plain": ["[1] 2"]}, 1)], 1, id="r1"),
        d_code("head(mtcars, 3)", [d_result({"text/html": ml(r_html), "text/latex": ml("A data.frame: 3 x 4\n\\begin{tabular}{r|llll}\n  & mpg & cyl & disp & hp\\\\\n\\hline\n\tMazda RX4 & 21.0 & 6 & 160 & 110\\\\\n\\end{tabular}\n"),
                                           "text/markdown": ml("\nA data.frame: 3 x 4\n\n| | mpg | cyl | disp | hp |\n|---|---|---|---|---|\n| Mazda RX4 | 21.0 | 6 | 160 | 110 |\n"),
                                           "text/plain": ml("                mpg cyl disp  hp\nMazda RX4      21.0   6  160 110\nMazda RX4 Wag  21.0   6  160 110\nDatsun 710     22.8   4  108  93")}, 2)], 2, id="r2"),
        d_code("plot(1:10)", [d_display({"image/png": SMALL_PNG, "text/plain": ["plot without title"]}, {"image/png": {"width": 420, "height": 420}})], 3, id="r3"),
        d_code("print('hello')\ncat('no newline')", [d_stream("[1] \"hello\"\nno newline")], 4, id="r4"),
        d_code("stop('custom R error')", [d_error("simpleError", "custom R error", ["Error in eval(expr, envir, enclos): custom R error\n", "1. stop(\"custom R error\")"])], 5, id="r5"),
        d_code("df <- data.frame(a = 1:3, b = letters[1:3])\nstr(df)", [d_stream("'data.frame':\t3 obs. of  2 variables:\n $ a: int  1 2 3\n $ b: chr  \"a\" \"b\" \"c\"\n")], 6, id="r6"),
    ]
    hand("formats/kernel_r.ipynb", d_nb(cells, 5, r_meta), "R (IRkernel) notebook: kernelspec `ir`, text/latex + text/markdown + text/html data.frame, plain-R error. Not executed.")

    # --- Julia ----------------------------------------------------------------------------------------------
    j_meta = {"kernelspec": {"display_name": "Julia 1.10.0", "language": "julia", "name": "julia-1.10"},
              "language_info": {"file_extension": ".jl", "mimetype": "application/julia", "name": "julia", "version": "1.10.0"}}
    jl_html = ('<div><div style = "float: left;"><span>3&times;2 DataFrame</span></div><div style = "clear: both;"></div></div><div class = "data-frame" style = "overflow-x: scroll;">'
               '<table class = "data-frame" style = "margin-bottom: 6px;"><thead><tr class = "header"><th class = "rowNumber" style = "font-weight: bold; text-align: right;">Row</th>'
               '<th style = "text-align: left;">x</th><th style = "text-align: left;">y</th></tr><tr class = "subheader headerLastRow"><th class = "rowNumber" style = "font-weight: bold; text-align: right;"></th>'
               '<th title = "Int64" style = "text-align: left;">Int64</th><th title = "String" style = "text-align: left;">String</th></tr></thead><tbody>'
               '<tr><td class = "rowNumber" style = "font-weight: bold; text-align: right;">1</td><td style = "text-align: right;">1</td><td style = "text-align: left;">a</td></tr>'
               '<tr><td class = "rowNumber" style = "font-weight: bold; text-align: right;">2</td><td style = "text-align: right;">2</td><td style = "text-align: left;">b</td></tr>'
               '<tr><td class = "rowNumber" style = "font-weight: bold; text-align: right;">3</td><td style = "text-align: right;">3</td><td style = "text-align: left;">c</td></tr></tbody></table></div>')
    cells = [
        d_md("# Julia notebook (IJulia)\n\nOnly metadata and stored outputs; never executed here.", id="jl0"),
        d_code('using DataFrames, Plots\nprintln("Hello from Julia")', [d_stream("Hello from Julia\n")], 1, id="jl1"),
        d_code("@time sum(rand(10^6))", [d_stream("  0.003281 seconds (2 allocations: 7.629 MiB)\n"), d_result({"text/plain": ["500123.7421"]}, 2)], 2, id="jl2"),
        d_code('DataFrame(x = 1:3, y = ["a", "b", "c"])', [d_result({"text/html": [jl_html], "text/latex": ["\\begin{tabular}{r|cc}\n\t& x & y\\\\\n\t\\hline\n\t1 & 1 & a \\\\\n\\end{tabular}\n"],
                                                                    "text/plain": ml("3×2 DataFrame\n Row │ x      y\n     │ Int64  String\n─────┼───────────────\n   1 │     1  a\n   2 │     2  b\n   3 │     3  c")}, 3)], 3, id="jl3"),
        d_code("plot(1:5, (1:5) .^ 2)", [d_result({"image/svg+xml": ml('<?xml version="1.0" encoding="utf-8"?>\n' + BADGE_SVG), "text/html": ["<img src=\"data:image/png;base64," + SMALL_PNG + "\"/>"]}, 4)], 4, id="jl4"),
        d_code("sqrt(-1)", [d_error("DomainError", "DomainError with -1.0:\nsqrt was called with a negative real argument but will only return a complex result if called with a complex argument. Try sqrt(Complex(x)).",
                                    ["DomainError with -1.0:\nsqrt was called with a negative real argument but will only return a complex result if called with a complex argument. Try sqrt(Complex(x)).",
                                     "Stacktrace:", " [1] throw_complex_domainerror(f::Symbol, x::Float64)", "   @ Base.Math ./math.jl:33", " [2] sqrt", "   @ ./math.jl:677 [inlined]"])], 5, id="jl5"),
    ]
    hand("formats/kernel_julia.ipynb", d_nb(cells, 5, j_meta), "Julia (IJulia) notebook: kernelspec `julia-1.10`, DataFrames.jl html, svg plot, Julia stacktrace. Not executed.")


def _fmt_widgets():
    cells = [
        M(r'''
# ipywidgets

Executed with nbclient: the outputs reference `application/vnd.jupyter.widget-view+json` models and the full
widget state is stored in `metadata.widgets["application/vnd.jupyter.widget-state+json"]`.
'''),
        C(r'''
import ipywidgets as w
from IPython.display import display
slider = w.IntSlider(value=7, min=0, max=10, description="Slider")
display(slider)
'''),
        C(r'''
btn = w.Button(description="Click me", button_style="success", icon="check")
dd = w.Dropdown(options=["alpha", "beta", "gamma"], value="beta", description="Choice")
txt = w.Text(value="hello", description="Text")
display(w.VBox([w.HBox([btn, dd]), txt]))
'''),
        C(r'''
out = w.Output()
with out:
    print("captured by an Output widget")
display(out)
'''),
        C(r'''
display(w.HTML("<b>HTML widget</b> with <i>markup</i>"), w.Checkbox(value=True, description="Checked"), w.FloatProgress(value=40, min=0, max=100, description="Loading"))
'''),
        C(r'''
tab = w.Tab(children=[w.Label("first"), w.Label("second")])
tab.set_title(0, "One"); tab.set_title(1, "Two")
display(tab, w.Accordion(children=[w.IntText(value=3)], titles=("Closed section",)))
'''),
        C(r'''
@w.interact(x=(0, 10), name="world")
def greet(x=5, name="world"):
    print(f"hello {name} x2={x * 2}")
'''),
        C(r'''
from tqdm.notebook import tqdm
for _ in tqdm(range(10), desc="notebook tqdm"):
    pass
'''),
        C(r'''
# a view whose model is NOT in the saved state would be simulated by widgets_no_state.ipynb
slider.value
'''),
    ]
    if not (have("ipywidgets") and have("tqdm")):
        log("  ipywidgets/tqdm missing: skipping formats/widgets*.ipynb")
        SKIPPED.add("formats/widgets*.ipynb needs ipywidgets/tqdm")
        return
    nb, err = build(cells, "widgets")
    execute(nb, "widgets", err, timeout=120)
    write_nb(nb, "formats/widgets.ipynb",
             "Executed ipywidgets (slider, button/dropdown/text boxes, Output, Tab, interact, tqdm.notebook): widget-view+json outputs AND widget-state+json in metadata.widgets.")
    nb2 = copy.deepcopy(nb)
    nb2.metadata.pop("widgets", None)
    nb2.cells[0].source = "# ipywidgets WITHOUT saved state\n\nSame outputs as `widgets.ipynb`, but `metadata.widgets` was stripped: only the `text/plain` fallbacks can be shown."
    write_nb(nb2, "formats/widgets_no_state.ipynb",
             "Same widget-view outputs but `metadata.widgets` removed: apps must fall back to text/plain.")


def _fmt_misc():
    # --- empty notebooks -----------------------------------------------------------------------------------
    nb = new_notebook()
    nb.metadata["kernelspec"] = dict(KERNELSPEC)
    nb.metadata["language_info"] = dict(PY3_LANG)
    write_nb(nb, "formats/empty_0_cells.ipynb", "Valid notebook with zero cells (kernelspec + language_info).")
    hand("formats/empty_minimal.ipynb", {"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}, "Zero cells and empty metadata (no kernelspec).")

    # --- CRLF --------------------------------------------------------------------------------------------------
    cells = [
        d_md("# CRLF inside strings\r\n\r\nThis markdown cell uses `\\r\\n` line endings.\r\n\r\n- item 1\r\n- item 2", id="c1",
             as_str=True),
        d_code("print('line 1')\r\nprint('line 2')\r\nfor i in range(2):\r\n    print(i)\r\n", [d_stream("line 1\r\nline 2\r\n0\r\n1\r\n")], 1, id="c2", as_str=True),
        d_code("x = 1\r\ny = 2\r\nx + y", [d_result({"text/plain": ["3"]}, 2)], 2, id="c3", meta={}),
        d_code("print('lone CR line endings')\rprint('old Mac')", [d_stream("lone\rCR\rendings\r")], 3, id="c4", as_str=True),
        d_raw("raw\r\ncell", id="c5", as_str=True),
    ]
    cells[2]["source"] = ["x = 1\r\n", "y = 2\r\n", "x + y"]   # list form, CRLF kept inside each line
    hand("formats/crlf_in_sources.ipynb", d_nb(cells, 5), "Sources and stream text containing `\\r\\n` (and lone `\\r`) line endings, as string and as list-of-lines.")
    hand("formats/crlf_file_endings.ipynb", d_nb([d_md("# CRLF file\n\nThe JSON file itself uses CRLF newlines (Windows git autocrlf).", id="f1"),
                                                   d_code("print('crlf file')", [d_stream("crlf file\n")], 1, id="f2")], 5),
         "The JSON document itself is CRLF-terminated (every pretty-print newline is `\\r\\n`).", newline="\r\n")

    # --- cell metadata zoo ---------------------------------------------------------------------------------------
    cells = [
        d_md("# Cell metadata zoo\n\nTags, collapsed flags, slideshow, editability and Jupyter-Book style tags.", id="cm0"),
        d_code("# tag: hide-input\nprint('input hidden by tag')", [d_stream("input hidden by tag\n")], 1, id="cm1", meta={"tags": ["hide-input"]}),
        d_code("# tag: hide-output\nprint('output hidden by tag')", [d_stream("output hidden by tag\n")], 2, id="cm2", meta={"tags": ["hide-output"]}),
        d_code("# tag: remove-cell\nprint('removed')", [d_stream("removed\n")], 3, id="cm3", meta={"tags": ["remove-cell"]}),
        d_code("# JupyterLab: source_hidden\nprint('collapsed input')", [d_stream("collapsed input\n")], 4, id="cm4", meta={"jupyter": {"source_hidden": True}}),
        d_code("# JupyterLab: outputs_hidden\nprint('collapsed output')", [d_stream("collapsed output\n")], 5, id="cm5", meta={"jupyter": {"outputs_hidden": True}}),
        d_code("# classic: collapsed + scrolled\nfor i in range(60): print(i)", [d_stream("".join(f"{i}\n" for i in range(60)))], 6, id="cm6", meta={"collapsed": True, "scrolled": True}),
        d_code("# scrolled: 'auto'\nprint('auto')", [d_stream("auto\n")], 7, id="cm7", meta={"scrolled": "auto"}),
        d_code("# not editable, not deletable\nx = 1", [], 8, id="cm8", meta={"editable": False, "deletable": False}),
        d_code("# papermill parameters\nalpha = 0.1\nn = 10", [], 9, id="cm9", meta={"tags": ["parameters"]}),
        d_code("# papermill injected\nalpha = 0.5", [], 10, id="cm10", meta={"tags": ["injected-parameters"], "papermill": {"duration": 0.001, "end_time": "2024-01-01T00:00:01.000Z", "exception": None, "start_time": "2024-01-01T00:00:00.000Z", "status": "completed"}}),
        d_code("# raises-exception tag\n1/0", [d_error("ZeroDivisionError", "division by zero", ["ZeroDivisionError: division by zero"])], 11, id="cm11", meta={"tags": ["raises-exception"]}),
        d_md("## slide\n\nslideshow metadata", id="cm12", meta={"slideshow": {"slide_type": "slide"}}),
        d_md("fragment", id="cm13", meta={"slideshow": {"slide_type": "fragment"}, "tags": ["a", "b", "c"], "custom": {"nested": {"deep": True}}}),
        d_md("### Markdown cell with `jupyter.source_hidden`", id="cm14", meta={"jupyter": {"source_hidden": True}}),
        d_code("# many tags\n1", [d_result({"text/plain": ["1"]}, 12)], 12, id="cm15", meta={"tags": [f"tag-{i}" for i in range(25)]}),
        d_code("# unicode metadata\n1", [], 13, id="cm16", meta={"title": "日本語 🚀", "tags": ["日本語", "emoji-🚀"]}),
        d_code("# ExecuteTime + Colab-ish id keys\n1", [], 14, id="cm17", meta={"ExecuteTime": {"end_time": "2023-05-05T05:05:05.000000Z", "start_time": "2023-05-05T05:05:04.000000Z"}, "colab": {"base_uri": "https://localhost:8080/"}, "outputId": "abc"}),
    ]
    hand("formats/cell_metadata.ipynb", d_nb(cells, 5), "Cell metadata zoo: tags (hide-input/hide-output/remove-cell/parameters), jupyter.source_hidden/outputs_hidden, collapsed/scrolled, slideshow, editable, papermill.")

    # --- BOM -----------------------------------------------------------------------------------------------------------
    p = ROOT / "formats/utf8_bom.ipynb"
    nbd = d_nb([d_md("# UTF-8 BOM\n\nThis file starts with the bytes EF BB BF (Notepad / some Windows tools).  Valid once the BOM is stripped.", id="b1"),
                d_code("print('bom')", [d_stream("bom\n")], 1, id="b2")], 5)
    p.write_bytes(b"\xef\xbb\xbf" + (json.dumps(nbd, indent=1, sort_keys=True, ensure_ascii=False) + "\n").encode("utf-8"))
    reg("formats/utf8_bom.ipynb", "File begins with a UTF-8 BOM (strict JSON parsers reject it; strip and continue).")

    # --- duplicate ids ----------------------------------------------------------------------------------------------------
    cells = [d_md("# Duplicate cell ids\n\nTwo cells share `id = dup`, as happens when a cell is copy-pasted by hand-editing JSON.", id="dup"),
             d_code("print('first')", [d_stream("first\n")], 1, id="dup"),
             d_code("print('second')", [d_stream("second\n")], 2, id="dup"),
             d_code("print('unique')", [d_stream("unique\n")], 3, id="uniq")]
    hand("formats/duplicate_cell_ids.ipynb", d_nb(cells, 5), "nbformat 4.5 with duplicate cell ids (invalid per spec, common in the wild).", validate=False)

    # --- missing optional keys / weird-but-valid -------------------------------------------------------------------------------
    cells = [
        {"cell_type": "markdown", "metadata": {}, "source": [], "id": "w1"},
        {"cell_type": "code", "execution_count": None, "metadata": {}, "outputs": [], "source": [], "id": "w2"},
        {"cell_type": "code", "execution_count": 99999999, "metadata": {}, "outputs": [d_stream("")], "source": ["huge execution count"], "id": "w3"},
        {"cell_type": "code", "execution_count": 0, "metadata": {}, "outputs": [d_stream("\n\n\n")], "source": ["# only newlines in the output"], "id": "w4"},
        {"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [d_stream("x" * 10, "stdout"), d_stream("y" * 10, "stderr"), d_stream("z" * 10, "stdout")], "source": ["# 3 consecutive stream outputs, not merged"], "id": "w5"},
        {"cell_type": "markdown", "metadata": {}, "source": "   \n\n   ", "id": "w6"},
        {"cell_type": "code", "execution_count": 2, "metadata": {}, "outputs": [d_result({"text/plain": ["unicode: " + "é" * 5]}, 2), d_display({"text/plain": ["a" * 5000]})], "source": ["# 5000 char plain output"], "id": "w7"},
    ]
    hand("formats/weird_but_valid.ipynb", d_nb(cells, 5), "Valid oddities: empty markdown/code, huge execution_count, newline-only output, unmerged consecutive streams, whitespace-only markdown.")

    # --- v3 -----------------------------------------------------------------------------------------------------------------------
    v3 = {
        "metadata": {"name": "legacy v3 notebook"},
        "nbformat": 3,
        "nbformat_minor": 0,
        "worksheets": [{
            "cells": [
                {"cell_type": "heading", "level": 1, "metadata": {}, "source": ["nbformat 3 notebook"]},
                {"cell_type": "markdown", "metadata": {}, "source": ["IPython 2.x era: `worksheets`, `heading` cells, `input`/`prompt_number`, `pyout`/`pyerr`."]},
                {"cell_type": "code", "collapsed": False, "input": ["print 'hello v3'"], "language": "python", "metadata": {}, "outputs": [{"output_type": "stream", "stream": "stdout", "text": ["hello v3\n"]}], "prompt_number": 1},
                {"cell_type": "code", "collapsed": False, "input": ["2 + 2"], "language": "python", "metadata": {}, "outputs": [{"output_type": "pyout", "prompt_number": 2, "text": ["4"], "metadata": {}}], "prompt_number": 2},
                {"cell_type": "code", "collapsed": False, "input": ["plot(range(3))"], "language": "python", "metadata": {}, "outputs": [{"output_type": "display_data", "png": SMALL_PNG, "text": ["<matplotlib.figure.Figure at 0x1>"], "metadata": {}}, {"output_type": "display_data", "html": ["<b>html in v3</b>"], "metadata": {}}], "prompt_number": 3},
                {"cell_type": "code", "collapsed": False, "input": ["1/0"], "language": "python", "metadata": {}, "outputs": [{"output_type": "pyerr", "ename": "ZeroDivisionError", "evalue": "integer division or modulo by zero", "traceback": ["ZeroDivisionError: integer division or modulo by zero"]}], "prompt_number": 4},
                {"cell_type": "raw", "metadata": {}, "source": ["raw v3 cell"]},
            ],
            "metadata": {},
        }],
    }
    hand("formats/nbformat_v3.ipynb", v3, "nbformat 3: `worksheets`, heading cells, `input`/`prompt_number`, `pyout`/`pyerr`, png/html as top-level output keys. Needs an upgrade path.", validate=False)

    # --- corrupt files ------------------------------------------------------------------------------------------------------------------
    good = json.dumps(d_nb([d_md("# Will be truncated", id="t1"), d_code("print('hi')", [d_stream("hi\n")], 1, id="t2"),
                            d_code("print('more')", [d_stream("more\n" * 20)], 2, id="t3")], 5), indent=1, sort_keys=True, ensure_ascii=False)
    cut = good.index('"more')
    (ROOT / "formats/corrupt_truncated.ipynb").write_text(good[:cut + 3], encoding="utf-8")
    reg("formats/corrupt_truncated.ipynb", "A valid notebook cut off in the middle of a string literal (interrupted save / bad sync).", validate=False)
    (ROOT / "formats/corrupt_empty_file.ipynb").write_bytes(b"")
    reg("formats/corrupt_empty_file.ipynb", "A zero-byte file.", validate=False)
    (ROOT / "formats/corrupt_not_json.ipynb").write_text("<<<<<<< HEAD\n{\"cells\": []}\n=======\n{\"cells\": [1]}\n>>>>>>> branch\n", encoding="utf-8")
    reg("formats/corrupt_not_json.ipynb", "A git merge-conflict marker file masquerading as a notebook.", validate=False)
    (ROOT / "formats/corrupt_binary.ipynb").write_bytes(bytes(range(256)) * 8)
    reg("formats/corrupt_binary.ipynb", "Binary garbage (not UTF-8, not JSON).", validate=False)
    dump_json(ROOT / "formats/corrupt_missing_cells.ipynb", {"metadata": {}, "nbformat": 4, "nbformat_minor": 5})
    reg("formats/corrupt_missing_cells.ipynb", "Valid JSON but no `cells` key.", validate=False)
    dump_json(ROOT / "formats/corrupt_wrong_types.ipynb", {"cells": [{"cell_type": "code", "source": 5, "outputs": "nope", "metadata": []}, {"cell_type": "bogus"}, 7, None],
                                                             "metadata": "x", "nbformat": "four", "nbformat_minor": None})
    reg("formats/corrupt_wrong_types.ipynb", "Valid JSON, wrong shapes: bad cell_type, non-string source, non-list outputs, non-object cells.", validate=False)
    dump_json(ROOT / "formats/corrupt_json_array.ipynb", [1, 2, 3])
    reg("formats/corrupt_json_array.ipynb", "Valid JSON whose root is an array.", validate=False)


def build_formats():
    if (ROOT / "formats").exists():
        shutil.rmtree(ROOT / "formats")      # no stale files from older versions of this script
    (ROOT / "formats").mkdir()
    _fmt_nbformat_versions()
    _fmt_string_variants()
    _fmt_tool_styles()
    _fmt_kernels()
    _fmt_misc()
    _fmt_widgets()
    log("  formats/ done")



# ===========================================================================
# 6. workspace/  (file tree, natural sort, search)
# ===========================================================================

TOPICS = {
    "intro": ["Introduction", "Getting started with the dataset", "Basic statistics"],
    "data": ["Loading data", "Reading CSV files", "Joining tables"],
    "clean": ["Cleaning", "Handling missing values", "Outlier removal"],
    "model": ["Modelling", "Training a baseline", "Hyper-parameter search"],
    "viz": ["Visualisation", "Plotting results", "Dashboards"],
    "report": ["Final report", "Summary of findings", "Appendix"],
}

CODE_SNIPPETS = [
    ("import numpy as np\nimport pandas as pd", []),
    ("df = pd.read_csv('data/sales.csv')\ndf.shape", [("result", "(1200, 8)")]),
    ("learning_rate = 0.01\nepochs = 20\nprint(f'lr={learning_rate} epochs={epochs}')", [("stdout", "lr=0.01 epochs=20\n")]),
    ("for epoch in range(3):\n    print(f'epoch {epoch}: loss={1 / (epoch + 1):.4f}')", [("stdout", "epoch 0: loss=1.0000\nepoch 1: loss=0.5000\nepoch 2: loss=0.3333\n")]),
    ("total = sum(range(100))\ntotal", [("result", "4950")]),
    ("x = np.linspace(0, 1, 5)\nx", [("result", "array([0.  , 0.25, 0.5 , 0.75, 1.  ])")]),
    ("df.describe()", [("html", "<div><table border=\"1\" class=\"dataframe\"><thead><tr><th></th><th>count</th><th>mean</th></tr></thead><tbody><tr><th>qty</th><td>1200</td><td>12.4</td></tr></tbody></table></div>")]),
    ("def normalise(s):\n    return (s - s.mean()) / s.std()", []),
    ("print('TODO: handle the edge cases')", [("stdout", "TODO: handle the edge cases\n")]),
    ("# FIXME: this is slow\nresult = [i ** 2 for i in range(1000)]\nlen(result)", [("result", "1000")]),
    ("import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])\nplt.show()", [("png", "")]),
    ("assert df.qty.min() >= 0, 'negative quantity'", []),
    ("1 / 0", [("error", "ZeroDivisionError")]),
    ("print('Revenue by region')\nprint(df.groupby('region').revenue.sum())", [("stdout", "Revenue by region\nregion\nnorth    1200.5\nsouth     980.0\nName: revenue, dtype: float64\n")]),
]

MD_SNIPPETS = [
    "Some notes about the **method** and the data.",
    "## Results\n\nThe model converges after about 20 epochs.",
    "- item one\n- item two\n- item three",
    "> Remember to re-run everything before sharing.",
    "| col | value |\n|-----|-------|\n| a | 1 |\n| b | 2 |",
    "### Next steps\n\n1. clean the data\n2. fit a model\n3. write the report",
]


def _out_cells(outputs, count):
    res = []
    for kind, val in outputs:
        if kind == "stdout":
            res.append(new_output("stream", name="stdout", text=val))
        elif kind == "result":
            res.append(new_output("execute_result", data={"text/plain": val}, execution_count=count))
        elif kind == "html":
            res.append(new_output("execute_result", data={"text/html": val, "text/plain": "<DataFrame>"}, execution_count=count))
        elif kind == "png":
            res.append(new_output("display_data", data={"image/png": SMALL_PNG_T, "text/plain": "<Figure size 640x480 with 1 Axes>"}))
        elif kind == "error":
            res.append(new_output("error", ename=val, evalue="division by zero", traceback=[f"\x1b[0;31m{val}\x1b[0m: division by zero"]))
    return res


SMALL_PNG_T = ""   # filled lazily (needs PIL); set in build_workspace


def mini_nb(name: str, rng: random.Random, *, topic=None, n_cells=None, extra_md=(), extra_code=(), extra_out=(), minor=5, executed=True):
    """A small, plausible notebook.  extra_* inject search tokens into markdown / code / outputs only."""
    topic = topic or rng.choice(list(TOPICS))
    title = rng.choice(TOPICS[topic])
    n_cells = n_cells or rng.randint(3, 8)
    specs, count = [], 0
    specs.append(("markdown", f"# {title}\n\n{rng.choice(MD_SNIPPETS)}"))
    for _ in range(n_cells - 1):
        if rng.random() < .3:
            specs.append(("markdown", rng.choice(MD_SNIPPETS)))
        else:
            specs.append(("code", rng.choice(CODE_SNIPPETS)))
    for t in extra_md:
        specs.append(("markdown", t))
    for t in extra_code:
        specs.append(("code", (t, [])))
    for t in extra_out:
        specs.append(("code", (f"print('token check')", [("stdout", t + "\n")])))
    cells = []
    for i, (kind, body) in enumerate(specs):
        if kind == "markdown":
            c = new_markdown_cell(body)
        else:
            src, outs = body
            if executed:
                count += 1
            c = new_code_cell(src, execution_count=count if executed else None, outputs=_out_cells(outs, count) if executed else [])
        c["id"] = cell_id(name, i)
        cells.append(c)
    nb = new_notebook(cells=cells)
    nb.metadata["kernelspec"] = dict(KERNELSPEC)
    nb.metadata["language_info"] = {"name": "python", "version": "3.11.4", "file_extension": ".py", "mimetype": "text/x-python"}
    if minor < 5:
        nb.nbformat_minor = minor
        for c in nb.cells:
            c.pop("id", None)
    return nb


def build_workspace():
    global SMALL_PNG_T
    SMALL_PNG_T = b64(png_bytes(48, 32, "gradient"))
    base = ROOT / "workspace"
    if base.exists():
        shutil.rmtree(base)
    base.mkdir()
    rng = random.Random(2024)

    # token -> where it hides (documented in the README)
    spec = {
        # path: kwargs for mini_nb
        "01-intro.ipynb": dict(topic="intro", extra_md=["Quarterly Revenue overview and the needle_markdown token."], extra_code=["learning_rate = 0.01  # needle_code"]),
        "02-data-loading.ipynb": dict(topic="data", extra_code=["data_path = 'data/sales.csv'  # needle_code"]),
        "03-cleaning.ipynb": dict(topic="clean", extra_md=["REVENUE in capitals, revenue in lower case."]),
        "10-final-report.ipynb": dict(topic="report", extra_md=["The final needle_markdown report."]),
        "nb1.ipynb": dict(), "nb2.ipynb": dict(), "nb10.ipynb": dict(extra_out=["needle_output only appears in a cell output here"]),
        "Untitled.ipynb": dict(n_cells=1), "Untitled1.ipynb": dict(n_cells=2), "Untitled10.ipynb": dict(n_cells=2),
        "data/explore-sales.ipynb": dict(topic="data", extra_code=["revenue_by_region = df.groupby('region').revenue.sum()  # needle_code"]),
        "data/cleaning/clean-v1.ipynb": dict(topic="clean"), "data/cleaning/clean-v2.ipynb": dict(topic="clean"),
        "data/cleaning/clean-v10.ipynb": dict(topic="clean", extra_out=["needle_output second hit"]),
        "data/cleaning/deep/outliers.ipynb": dict(topic="clean", extra_md=["Deeply nested (level 3) notebook."]),
        "src/prototype.ipynb": dict(topic="model", minor=4),
        "experiments/exp-01/baseline.ipynb": dict(topic="model"),
        "experiments/exp-02/baseline.ipynb": dict(topic="model"),
        "experiments/exp-02/tuning.ipynb": dict(topic="model", extra_code=["grid = {'learning_rate': [0.1, 0.01, 0.001]}"]),
        "experiments/exp-02/run-a/trial-1.ipynb": dict(topic="model"), "experiments/exp-02/run-a/trial-2.ipynb": dict(topic="model"),
        "experiments/exp-02/run-a/trial-10.ipynb": dict(topic="model"),
        "experiments/exp-10/baseline.ipynb": dict(topic="model"),
        "archive/old-analysis.ipynb": dict(topic="report", minor=4), "archive/Copy of old-analysis.ipynb": dict(topic="report", minor=4),
        "archive/résumé.ipynb": dict(extra_md=["Accented file name: résumé."]), "archive/日本語ノート.ipynb": dict(extra_md=["日本語のノートブック。"]),
        "archive/with space.ipynb": dict(), "archive/.hidden.ipynb": dict(extra_md=["A dot-file notebook."]),
        # ignored by workspace/.gitignore
        "ignored_dir/secret-experiment.ipynb": dict(extra_code=["needle_ignored = True"]),
        "ignored_dir/scratch.ipynb": dict(),
    }
    for rel, kw in spec.items():
        nb = mini_nb(rel, rng, **kw)
        p = base / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        nbformat.write(nb, str(p))

    # checkpoint copies (Jupyter writes these next to the originals)
    for rel, src in {".ipynb_checkpoints/01-intro-checkpoint.ipynb": "01-intro.ipynb",
                     ".ipynb_checkpoints/Untitled-checkpoint.ipynb": "Untitled.ipynb",
                     "data/cleaning/.ipynb_checkpoints/clean-v2-checkpoint.ipynb": "data/cleaning/clean-v2.ipynb"}.items():
        nb = nbformat.read(str(base / src), as_version=nbformat.NO_CONVERT)
        nb.cells.append(new_code_cell("checkpoint_marker = 'needle_checkpoint'", execution_count=None))
        nb.cells[-1]["id"] = cell_id("ckpt", len(rel))
        p = base / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        nbformat.write(nb, str(p))

    # non-notebook files
    (base / ".gitignore").write_text("# one ignored sub-folder (and a pattern) for gitignore-aware walkers\nignored_dir/\n*.log\n")
    (base / "README.md").write_text("# Workspace fixture\n\nA small tree for the file explorer, natural sort and search.  See ../README.md.\n")
    (base / "data/sales.csv").write_text("date,region,qty,revenue\n" + "".join(
        f"2024-{1 + i % 12:02d}-{1 + i % 28:02d},{['north', 'south', 'east', 'west'][i % 4]},{1 + i * 7 % 40},{round(10 + i * 3.17 % 500, 2)}\n" for i in range(120)))
    (base / "data/cleaning/notes.md").write_text("# Cleaning notes\n\n- drop duplicates\n- fill NaN with the median\n- needle_markdown (in a .md file, not a notebook)\n")
    (base / "src").mkdir(exist_ok=True)
    (base / "src/helpers.py").write_text("\"\"\"Helpers.\"\"\"\n\n\ndef normalise(values):\n    m = sum(values) / len(values)\n    return [v - m for v in values]\n")
    (base / "src/plotting.py").write_text("import matplotlib.pyplot as plt\n\n\ndef quick(x, y):\n    plt.plot(x, y)  # needle_code in a .py file\n")
    (base / "experiments/exp-02/run-a/metrics.csv").write_text("epoch,loss\n" + "".join(f"{i},{1 / (i + 1):.4f}\n" for i in range(20)))
    (base / "ignored_dir/data.csv").write_text("a,b\n1,2\n")
    (base / "archive/old.log").write_text("this file matches *.log in .gitignore\n")
    reg("workspace/", "~30 notebooks in a 3-level tree: zero-padded (01-, 10-), `nb1/nb2/nb10`, `Untitled/Untitled1/Untitled10`, `clean-v1/v2/v10`, "
        "`trial-1/2/10`, `exp-01/02/10`, unicode names, spaces, dot-files, `.ipynb_checkpoints/` copies, non-notebook .py/.md/.csv/.log, "
        "and a `.gitignore` excluding `ignored_dir/`.")
    n = sum(1 for _ in base.rglob("*.ipynb"))
    log(f"  workspace/ done ({n} notebooks incl. checkpoints)")



# ===========================================================================
# 7. big/  (generated on demand with --big; not checked in)
# ===========================================================================

def build_cells_5000():
    """5,000 hand-built cells (markdown + code), every code cell with a small output.  No kernel needed."""
    rng = random.Random(5000)
    png = b64(png_bytes(40, 24, "gradient"))
    cells, count = [], 0
    md_bits = ["Some **bold** and *italic* text with `inline code`.", "A list:\n\n- alpha\n- beta\n- gamma",
               "> A quote about data, repeated for volume.", "| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |",
               "Inline math $x_i^2 + y_i^2 = r_i^2$ and a link [docs](https://example.com/docs).",
               "1. first\n2. second\n3. third", "Plain paragraph. " * 12]
    words = ["alpha", "beta", "gamma", "delta", "theta", "sigma", "omega", "kappa"]

    def code_cell(i):
        nonlocal count
        count += 1
        k = rng.randrange(14)
        a, b = rng.randint(2, 999), rng.randint(2, 999)
        v = words[i % len(words)]
        outs = []
        if k == 0:
            src = f"print('step {i}: {v} =', {a} * {b})"
            outs = [new_output("stream", name="stdout", text=f"step {i}: {v} = {a * b}\n")]
        elif k == 1:
            src = f"{v}_{i} = {a} * {b}\n{v}_{i}"
            outs = [new_output("execute_result", data={"text/plain": str(a * b)}, execution_count=count)]
        elif k == 2:
            src = f"[x * {a} for x in range(6)]"
            outs = [new_output("execute_result", data={"text/plain": str([x * a for x in range(6)])}, execution_count=count)]
        elif k == 3:
            src = f"{{'id': {i}, 'name': '{v}', 'tags': ['a', 'b'], 'score': {a / 7:.4f}}}"
            outs = [new_output("execute_result", data={"text/plain": f"{{'id': {i}, 'name': '{v}', 'tags': ['a', 'b'], 'score': {a / 7:.4f}}}"}, execution_count=count)]
        elif k == 4:
            src = f"import pandas as pd\npd.DataFrame({{'x': [{a}, {b}], 'y': ['{v}', 'z']}})"
            html = (f'<div>\n<style scoped>\n    .dataframe tbody tr th:only-of-type {{ vertical-align: middle; }}\n</style>\n<table border="1" class="dataframe">\n'
                    f'  <thead>\n    <tr style="text-align: right;">\n      <th></th>\n      <th>x</th>\n      <th>y</th>\n    </tr>\n  </thead>\n  <tbody>\n'
                    f'    <tr>\n      <th>0</th>\n      <td>{a}</td>\n      <td>{v}</td>\n    </tr>\n    <tr>\n      <th>1</th>\n      <td>{b}</td>\n      <td>z</td>\n    </tr>\n  </tbody>\n</table>\n</div>')
            outs = [new_output("execute_result", data={"text/html": html, "text/plain": f"     x      y\n0  {a:>3}  {v}\n1  {b:>3}      z"}, execution_count=count)]
        elif k == 5:
            src = "import warnings\nwarnings.warn('deprecated call')"
            outs = [new_output("stream", name="stderr", text=f"/tmp/ipykernel_1/{i}.py:2: UserWarning: deprecated call\n  warnings.warn('deprecated call')\n")]
        elif k == 6:
            src = f"for j in range(5):\n    print(f'{{j}}: {v} {{j * {a}}}')"
            outs = [new_output("stream", name="stdout", text="".join(f"{j}: {v} {j * a}\n" for j in range(5)))]
        elif k == 7:
            src = f"from IPython.display import Markdown\nMarkdown('**{v}** {i}')"
            outs = [new_output("execute_result", data={"text/markdown": f"**{v}** {i}", "text/plain": "<IPython.core.display.Markdown object>"}, execution_count=count)]
        elif k == 8:
            src = f"'{v} ' * 4"
            outs = [new_output("execute_result", data={"text/plain": repr(f'{v} ' * 4)}, execution_count=count)]
        elif k == 9:
            src = f"# a longer cell\n" + "\n".join(f"v{j} = {j} * {a}  # comment {j}" for j in range(30)) + "\nv29"
            outs = [new_output("execute_result", data={"text/plain": str(29 * a)}, execution_count=count)]
        elif k == 10:
            src = f"print('{v}')\n{a} / {b}"
            outs = [new_output("stream", name="stdout", text=f"{v}\n"), new_output("execute_result", data={"text/plain": repr(a / b)}, execution_count=count)]
        elif k == 11:
            src = f"x = {a}\nx ** 2"
            outs = [new_output("execute_result", data={"text/plain": str(a * a)}, execution_count=count)]
        elif k == 12:
            src = f"from IPython.display import Latex\nLatex(r'$\\sum_{{n=1}}^{{{a}}} n$')"
            outs = [new_output("execute_result", data={"text/latex": f"$\\sum_{{n=1}}^{{{a}}} n$", "text/plain": "<IPython.core.display.Latex object>"}, execution_count=count)]
        else:
            src = f"print('{v}', {a})"
            outs = [new_output("stream", name="stdout", text=f"{v} {a}\n")]
        if i % 100 == 7:   # a tiny image every 100 cells
            src = "import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])"
            outs = [new_output("display_data", data={"image/png": png, "text/plain": "<Figure size 640x480 with 1 Axes>"})]
        if i % 150 == 11:  # and an occasional failure
            src = f"1 / (len('{v}') - {len(v)})"
            outs = [new_output("error", ename="ZeroDivisionError", evalue="division by zero",
                               traceback=["\x1b[0;31m---------------------------------------------------------------------------\x1b[0m",
                                          "\x1b[0;31mZeroDivisionError\x1b[0m                         Traceback (most recent call last)",
                                          f"Cell \x1b[0;32mIn[{count}], line 1\x1b[0m\n\x1b[0;32m----> 1\x1b[0m \x1b[38;5;241m1\x1b[39m \x1b[38;5;241m/\x1b[39m (\x1b[38;5;28mlen\x1b[39m(\x1b[38;5;124m'{v}'\x1b[39m) \x1b[38;5;241m-\x1b[39m {len(v)})\n",
                                          "\x1b[0;31mZeroDivisionError\x1b[0m: division by zero"])]
        return new_code_cell(src, execution_count=count, outputs=outs)

    for i in range(5000):
        if i % 25 == 0:
            c = new_markdown_cell(f"# Section {i // 25 + 1}\n\n{rng.choice(md_bits)}")
        elif rng.random() < .2:
            c = new_markdown_cell(f"### Note {i}\n\n{rng.choice(md_bits)}")
        else:
            c = code_cell(i)
        c["id"] = cell_id("cells_5000", i)
        cells.append(c)
    nb = new_notebook(cells=cells)
    nb.metadata["kernelspec"] = dict(KERNELSPEC)
    nb.metadata["language_info"] = {"name": "python", "version": "3.11.4", "file_extension": ".py", "mimetype": "text/x-python",
                                    "codemirror_mode": {"name": "ipython", "version": 3}, "pygments_lexer": "ipython3", "nbconvert_exporter": "python"}
    n_code = sum(c.cell_type == "code" for c in cells)
    write_nb(nb, "cells_5000.ipynb", base=BIG)
    log(f"  cells_5000: {len(cells)} cells ({n_code} code)")


IMG_SETUP = r'''
%matplotlib inline
import numpy as np
import matplotlib.pyplot as plt
try:
    from scipy.ndimage import gaussian_filter
except ImportError:                      # tiny separable fallback
    def gaussian_filter(a, sigma):
        r = int(4 * sigma + 1); k = np.exp(-0.5 * (np.arange(-r, r + 1) / sigma) ** 2); k /= k.sum()
        a = np.apply_along_axis(lambda v: np.convolve(v, k, mode="same"), 0, a)
        return np.apply_along_axis(lambda v: np.convolve(v, k, mode="same"), 1, a)

plt.rcParams.update({"figure.dpi": 100, "figure.figsize": (9, 6)})

def fig_scatter(i, n=3000):
    rng = np.random.default_rng(i)
    fig, ax = plt.subplots()
    x = rng.normal(size=n); y = x * rng.uniform(-1, 1) + rng.normal(size=n)
    ax.scatter(x, y, c=rng.random(n), s=rng.integers(4, 40, n), cmap="viridis", alpha=.6, edgecolors="none")
    ax.set_title(f"scatter #{i}"); ax.grid(alpha=.2)

def fig_noise(i, size=170):
    rng = np.random.default_rng(i)
    fig, ax = plt.subplots()
    im = ax.imshow(gaussian_filter(rng.normal(size=(size, size)), 1.2), cmap="magma", interpolation="nearest")
    fig.colorbar(im); ax.set_title(f"smoothed noise #{i}")

def fig_lines(i, k=10, n=2000):
    rng = np.random.default_rng(i)
    fig, ax = plt.subplots()
    for j in range(k):
        ax.plot(np.cumsum(rng.normal(size=n)), lw=.8, label=f"s{j}")
    ax.legend(ncol=5, fontsize=7); ax.set_title(f"random walks #{i}"); ax.grid(alpha=.2)

def fig_hist2d(i, n=24000):
    rng = np.random.default_rng(i)
    fig, ax = plt.subplots()
    x = rng.normal(size=n); y = .6 * x + rng.normal(size=n)
    h = ax.hist2d(x, y, bins=110, cmap="inferno"); fig.colorbar(h[3]); ax.set_title(f"hist2d #{i}")

def fig_contour(i, size=100):
    rng = np.random.default_rng(i)
    fig, ax = plt.subplots()
    z = gaussian_filter(rng.normal(size=(size, size)), 3) * 30
    cf = ax.contourf(z, levels=16, cmap="coolwarm"); ax.contour(z, levels=16, colors="k", linewidths=.3)
    fig.colorbar(cf); ax.set_title(f"contour #{i}")

KINDS = [fig_scatter, fig_noise, fig_lines, fig_hist2d, fig_contour]
print(len(KINDS), "figure kinds ready")
'''


def build_images_300():
    n_figs = 300
    specs = [M("# 300 matplotlib figures\n\nEach code cell renders one PNG (900x600).  Total file size is tens of MB."), C(IMG_SETUP)]
    for i in range(n_figs):
        if i % 30 == 0:
            specs.append(M(f"## Batch {i // 30 + 1}\n\nFigures {i + 1} to {min(i + 30, n_figs)}."))
        kind = ["fig_scatter", "fig_noise", "fig_lines", "fig_hist2d", "fig_contour"][i % 5]
        specs.append(C(f"{kind}({i + 1000})\nplt.show()\nprint('figure {i + 1} of {n_figs}: {kind}')"))
    nb, err = build(specs, "images_300")
    execute(nb, "images_300", err, timeout=300)
    write_nb(nb, "images_300.ipynb", base=BIG)
    pngs = sum(1 for c in nb.cells if c.cell_type == "code" for o in c.outputs if o.output_type == "display_data" and "image/png" in o.data)
    log(f"  images_300: {pngs} PNG outputs")
    if pngs != n_figs:
        raise RuntimeError(f"images_300: expected {n_figs} PNGs, got {pngs}")


LONG_A = r'''
for i in range(200_000):
    print(f"line {i:06d} the quick brown fox jumps over the lazy dog {i * 7919 % 10007}")
'''
LONG_B = r'''
import random, sys
rnd = random.Random(7)
alphabet = "abcdefghijklmnopqrstuvwxyz0123456789 .,;:-_/"
target, written, i, buf = 20 * 1024 * 1024, 0, 0, []
while written < target:
    n = rnd.choice((0, 10, 40, 80, 120, 200, 400))
    if i == 123_456:
        n = 1_000_000                      # one 1 MB line in the middle
    s = "".join(rnd.choices(alphabet, k=n)) + "\n"
    buf.append(s); written += len(s); i += 1
    if len(buf) >= 2000:
        sys.stdout.write("".join(buf)); buf.clear()
sys.stdout.write("".join(buf))
print(f"END after {i} lines, {written} characters", file=sys.stderr)
'''
LONG_C = r'''
import sys
for i in range(5000):
    print(f"stdout {i}"); sys.stdout.flush()
    print(f"stderr {i}", file=sys.stderr); sys.stderr.flush()
'''
LONG_F = r'''
from IPython.display import display
for i in range(10_000):
    display(f"display_data output number {i}")
'''
LONG_G = r'''
import sys
for i in range(20_001):
    sys.stdout.write(f"\rprogress {i / 200:6.2f}% [{'#' * (i // 1000):<20}]")
sys.stdout.write("\ndone\n")
'''


def build_long_output():
    specs = [
        M("# Long outputs\n\nEvery cell below produces an enormous output.  Expect: virtualised / truncated rendering, no UI freeze, bounded memory."),
        M("## 200,000 lines of stdout (one stream output, ~15 MB)"),
        C(LONG_A),
        M("## One ~20 MiB stream output (mixed line lengths, a 1 MB line in the middle)"),
        C(LONG_B),
        M("## 10,000 alternating stdout / stderr outputs (10,000 separate stream outputs)"),
        C(LONG_C),
        M("## A single 5,000,000 character line"),
        C('print("0123456789" * 500_000)'),
        M("## Huge text/plain execute_result (200,000-element list repr, ~35,000 lines)"),
        C("list(range(200_000))"),
        M("## A 3,000,000 character string repr"),
        C('"x" * 3_000_000'),
        M("## 10,000 display_data outputs in one cell"),
        C(LONG_F),
        M("## 20,000 carriage-return progress updates in one stream"),
        C(LONG_G),
        M("## An ordinary cell at the end"),
        C('print("the end")\n42'),
    ]
    nb, err = build(specs, "long_output")
    execute(nb, "long_output", err, timeout=600)
    # sanity checks: the outputs really are as big as advertised
    def stream_len(cell, name="stdout"):
        return sum(len(o.text) for o in cell.outputs if o.output_type == "stream" and o.name == name)
    codes = [c for c in nb.cells if c.cell_type == "code"]
    a = codes[0]
    n_lines = sum(o.text.count("\n") for o in a.outputs if o.output_type == "stream")
    if n_lines != 200_000:
        raise RuntimeError(f"long_output: expected 200000 lines, got {n_lines}")
    if stream_len(codes[1]) < 20 * 1024 * 1024:
        raise RuntimeError(f"long_output: 20 MiB cell only produced {stream_len(codes[1])} bytes")
    if len(codes[2].outputs) < 10_000:
        raise RuntimeError(f"long_output: alternating cell produced {len(codes[2].outputs)} outputs")
    if len(codes[6].outputs) < 10_000:
        raise RuntimeError(f"long_output: display cell produced {len(codes[6].outputs)} outputs")
    write_nb(nb, "long_output.ipynb", base=BIG)
    log(f"  long_output: cell A {n_lines} lines; cell B {stream_len(codes[1]) / 2**20:.1f} MiB; "
        f"{len(codes[2].outputs)} alternating outputs; {len(codes[6].outputs)} display outputs")


def _seed_cells():
    pool = []
    for fn in ("showcase", "dataframes", "errors", "plots"):
        p = ROOT / f"{fn}.ipynb"
        if not p.exists():
            continue
        nb = nbformat.read(str(p), as_version=4)
        for c in nb.cells:
            if len(json.dumps(c)) < 22000:
                pool.append(c)
    return pool


def build_workspace_2000():
    """2,000 notebooks in nested directories, derived from (and perturbing) the small fixtures."""
    global SMALL_PNG_T
    if not SMALL_PNG_T:
        SMALL_PNG_T = b64(png_bytes(48, 32, "gradient"))
    base = BIG / "workspace_2000"
    if base.exists():
        shutil.rmtree(base)
    base.mkdir(parents=True)
    rng = random.Random(2000)
    pool = _seed_cells()
    log(f"  workspace_2000: seed pool = {len(pool)} cells from the small fixtures")
    teams = ["analytics", "ml-platform", "growth", "finance", "research", "ops", "marketing", "data-eng", "support", "labs"]
    slugs = ["intro", "eda", "cleaning", "features", "baseline", "tuning", "report", "dashboard", "etl", "forecast", "ab-test", "cohorts",
             "churn", "pricing", "anomaly", "clustering", "nlp", "ranking", "audit", "scratch-work"]
    total, made, token_hits = 2000, 0, {"code": 0, "md": 0, "out": 0}
    per_project = 25
    projects = [(t, p) for t in teams for p in range(1, 9)]   # 80 projects x 25 = 2000
    for pi, (team, pnum) in enumerate(projects):
        pdir = base / team / f"project-{pnum:02d}"
        used: set[str] = set()
        for k in range(per_project):
            r = rng.random()
            sub = pdir if r < .55 else (pdir / "analysis" if r < .8 else (pdir / "analysis" / f"run-{rng.randint(1, 12):02d}" if r < .93 else pdir / "scratch"))
            slug = rng.choice(slugs)
            style = rng.randrange(7)
            n = rng.randint(1, 12)
            name = [f"nb{n}", f"Untitled{n if n > 1 else ''}", f"{k % 15 + 1:02d}-{slug}", f"{slug}_v{n}", f"Copy of {slug}", f"{slug}-final", f"{slug} ({n})"][style]
            fn = f"{name}.ipynb"
            while str(sub / fn) in used:
                fn = f"{name}-{rng.randint(100, 999)}.ipynb"
            used.add(str(sub / fn))
            nbname = f"{team}/{pnum}/{fn}/{k}"
            kw: dict = {}
            if rng.random() < .01:
                kw["extra_code"] = [f"needle_big_code = {made}"]; token_hits["code"] += 1
            elif rng.random() < .01:
                kw["extra_md"] = [f"needle_big_markdown {made}"]; token_hits["md"] += 1
            elif rng.random() < .01:
                kw["extra_out"] = [f"needle_big_output {made}"]; token_hits["out"] += 1
            if rng.random() < .02:
                kw["n_cells"] = rng.randint(30, 80)
            minor = rng.choices([5, 4, 2], weights=[80, 15, 5])[0]
            nb = mini_nb(nbname, rng, minor=minor, executed=rng.random() < .9, **kw)
            if pool and rng.random() < .15:     # graft a few cells from the real fixtures
                for c in rng.sample(pool, k=rng.randint(1, 6)):
                    c = copy.deepcopy(c)
                    c.pop("id", None)
                    c.metadata.pop("execution", None)
                    if c.cell_type == "markdown":
                        c.source = f"(copied) {c.source}"
                    if minor >= 5:
                        c["id"] = cell_id(nbname, len(nb.cells) + 1000)
                    nb.cells.append(c)
            p = sub / fn
            p.parent.mkdir(parents=True, exist_ok=True)
            nbformat.write(nb, str(p))
            made += 1
        # some non-notebook files and a checkpoint per project
        if pnum % 4 == 1:
            (pdir / "README.md").write_text(f"# {team} / project-{pnum:02d}\n\nGenerated fixture.\n")
        if pnum % 5 == 2:
            (pdir / "helpers.py").write_text("def helper(x):\n    return x\n")
        if pnum % 3 == 0:
            (pdir / "data.csv").write_text("a,b\n1,2\n3,4\n")
        first = next(iter(sorted(pdir.glob("*.ipynb"))), None)
        if first is not None:
            ck = pdir / ".ipynb_checkpoints" / f"{first.stem}-checkpoint.ipynb"
            ck.parent.mkdir(exist_ok=True)
            shutil.copyfile(first, ck)
    (base / ".gitignore").write_text("scratch/\n*.csv\n")
    nbs = [p for p in base.rglob("*.ipynb") if ".ipynb_checkpoints" not in p.parts]
    if len(nbs) != total:
        raise RuntimeError(f"workspace_2000: expected {total} notebooks, found {len(nbs)}")
    size = sum(p.stat().st_size for p in base.rglob("*") if p.is_file())
    log(f"  workspace_2000: {len(nbs)} notebooks, {size / 2**20:.1f} MiB; search tokens {token_hits}")


BIG_STATIC = {
    "big/cells_5000.ipynb": "5,000 cells (~25% markdown), every code cell with a small output (stream/result/html/latex/markdown, tiny PNG every 100th, an error every 150th). Hand-built, no kernel.",
    "big/images_300.ipynb": "300 executed matplotlib PNG outputs (scatter / noise heatmap / random walks / hist2d / contour at 900x600), tens of MB.",
    "big/long_output.ipynb": "Stress outputs: 200,000-line stream, one 20 MiB stream (with a 1 MB line), 10,000 alternating stdout/stderr outputs, 5M-char line, 200k-element list repr, 10,000 display_data, 20,000 `\\r` updates.",
    "big/workspace_2000/": "2,000 notebooks (plus ~80 checkpoint copies, README/.py/.csv files) in team/project/analysis/run-NN directories (4 levels): perturbed copies of the small fixtures, "
                           "mixed nbformat 4.2/4.4/4.5, natural-sort name traps, `needle_big_*` search tokens (~1% each in code / markdown / outputs).",
}



# ===========================================================================
# 8. validation, README, main
# ===========================================================================

OPTIONAL_LIBS = ["seaborn", "plotly", "altair", "bokeh", "ipywidgets", "tqdm", "sympy", "scipy", "PIL", "rich", "pyarrow",
                 "vega_datasets", "jupytext", "folium", "ipympl"]


def detect_libs() -> dict[str, str | None]:
    import importlib
    import importlib.metadata as md
    import importlib.util
    res: dict[str, str | None] = {}
    for m in OPTIONAL_LIBS:
        if importlib.util.find_spec(m) is None:
            res[m] = None
            continue
        dist = {"PIL": "pillow"}.get(m, m)
        try:
            res[m] = md.version(dist)
        except Exception:
            res[m] = "?"
    return res


def validate_targets() -> list[Path]:
    out: list[Path] = []
    for rel in DESCRIPTIONS:
        if rel.endswith("/"):
            out += sorted((ROOT / rel).rglob("*.ipynb"))
        elif rel.endswith(".ipynb") and rel not in NO_VALIDATE:
            out.append(ROOT / rel)
    return out


def validate_all() -> int:
    t0 = time.time()
    targets = validate_targets()
    bad: list[tuple[Path, str]] = []
    for p in targets:
        try:
            nb = nbformat.from_dict(json.loads(p.read_text(encoding="utf-8-sig")))   # utf-8-sig: tolerate the BOM fixture
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always")
                nbformat.validate(nb)
            for w in caught:                  # nbformat only warns (and repairs) for duplicate / missing ids: count that as a failure
                if "cell id" in str(w.message).lower() or "missingid" in w.category.__name__.lower():
                    raise ValueError(str(w.message))
        except Exception as e:  # noqa: BLE001
            bad.append((p, f"{type(e).__name__}: {str(e).splitlines()[0][:200] if str(e) else ''}"))
    log(f"validated {len(targets)} notebooks with nbformat.validate in {time.time() - t0:.1f}s: {len(bad)} failures")
    for p, msg in bad:
        log(f"  INVALID {p.relative_to(ROOT)}: {msg}")
    return len(bad)


def human(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else (f"{n:.1f} {unit}" if n < 100 else f"{n:.0f} {unit}")
        n /= 1024
    return str(n)


def _size_of(rel: str) -> str:
    p = ROOT / rel
    if rel.endswith("/"):
        if not p.exists():
            return "not generated"
        files = [f for f in p.rglob("*") if f.is_file()]
        nbs = [f for f in files if f.suffix == ".ipynb"]
        return f"{human(sum(f.stat().st_size for f in files))} ({len(nbs)} .ipynb, {len(files)} files)"
    return human(p.stat().st_size) if p.exists() else "not generated"


README_HEAD = """\
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
  `\\r` / `\\b` are stored raw (progress bars!).
- A startup hook flushes matplotlib figures created without `plt.show()` (matplotlib 3.10 + matplotlib-inline 0.1.6 stopped doing it),
  so a bare `plt.plot(...)` cell yields the usual `execute_result` + PNG pair.
"""


def write_readme(libs: dict[str, str | None], timings: dict[str, float]) -> None:
    sections: dict[str, list[tuple[str, str]]] = {"Small set (checked in)": [], "formats/ (hand-built edge cases)": [],
                                                  "workspace/ (file tree, natural sort, search)": [], "Big set (`--big`, generated on demand, not checked in)": []}
    rels = dict(DESCRIPTIONS)
    for k, v in BIG_STATIC.items():
        rels.setdefault(k, v)
    for rel, desc in rels.items():
        if rel.startswith("big/"):
            sec = "Big set (`--big`, generated on demand, not checked in)"
        elif rel.startswith("formats/"):
            sec = "formats/ (hand-built edge cases)"
        elif rel.startswith("workspace/"):
            sec = "workspace/ (file tree, natural sort, search)"
        else:
            sec = "Small set (checked in)"
        sections[sec].append((rel, desc))
    out = [README_HEAD]
    for title, items in sections.items():
        out.append(f"\n## {title}\n\n| File | Size | What it exercises |\n|---|---:|---|\n")
        for rel, desc in sorted(items, key=lambda t: t[0]):
            safe = desc.replace("|", "\\|")
            out.append(f"| `{rel}` | {_size_of(rel)} | {safe} |\n")
    out.append("""
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

""")
    out.append("| Library | Version |\n|---|---|\n")
    for k, v in libs.items():
        out.append(f"| {k} | {v if v else 'not installed (related cells skipped)'} |\n")
    if timings:
        out.append("\nGeneration time on the author's machine: " + ", ".join(f"{k} {v:.0f}s" for k, v in timings.items()) + ".\n")
    (ROOT / "README.md").write_text("".join(out), encoding="utf-8")
    log("wrote README.md")


GITIGNORE = """\
# Big benchmark set: regenerate with `python3 generate.py --big`
big/
__pycache__/
*.pyc
.DS_Store
# NOTE: .ipynb_checkpoints/ directories under workspace/ are intentional fixtures, so they are NOT ignored here.
"""


STEPS = [
    ("showcase", build_showcase),
    ("dataframes", build_dataframes),
    ("plots", build_plots),
    ("errors", build_errors),
    ("formats", build_formats),
    ("workspace", build_workspace),
]
BIG_STEPS = [
    ("big-cells", build_cells_5000, "big/cells_5000.ipynb"),
    ("big-images", build_images_300, "big/images_300.ipynb"),
    ("big-output", build_long_output, "big/long_output.ipynb"),
    ("big-workspace", build_workspace_2000, "big/workspace_2000/"),
]


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--big", action="store_true", help="also generate the big/ benchmark set")
    ap.add_argument("--only", help="comma separated step names (see --list); skips README and the full-run bookkeeping")
    ap.add_argument("--list", action="store_true", help="list the build steps and exit")
    ap.add_argument("--no-validate", action="store_true", help="skip the nbformat.validate pass")
    args = ap.parse_args(argv)
    names = [n for n, *_ in STEPS] + [n for n, *_ in BIG_STEPS]
    if args.list:
        print("\n".join(names))
        return 0
    only = set(args.only.split(",")) if args.only else None
    if only and only - set(names):
        ap.error(f"unknown step(s): {sorted(only - set(names))}; choose from {names}")

    t_start = time.time()
    ROOT.mkdir(exist_ok=True)
    (ROOT / ".gitignore").write_text(GITIGNORE)
    libs = detect_libs()
    log("optional libraries: " + ", ".join(f"{k}={v or 'MISSING'}" for k, v in libs.items()))
    timings: dict[str, float] = {}

    def run(name, fn, bigrel=None):
        t0 = time.time()
        log(f"== {name}")
        fn()
        if bigrel:
            reg(bigrel, BIG_STATIC[bigrel])
        timings[name] = time.time() - t0

    for name, fn in STEPS:
        if only is None or name in only:
            run(name, fn)
    if args.big or (only and any(n.startswith("big-") for n in only)):
        BIG.mkdir(exist_ok=True)
        for name, fn, rel in BIG_STEPS:
            if only is None or name in only:
                run(name, fn, rel)

    failures = 0
    if not args.no_validate:
        failures = validate_all()
    if only is None:
        write_readme(libs, {k: v for k, v in timings.items()})
    skipped = sorted(SKIPPED)
    if skipped:
        log("skipped cells (missing libraries): " + "; ".join(skipped))
    log(f"TOTAL {time.time() - t_start:.1f}s  ({', '.join(f'{k} {v:.0f}s' for k, v in timings.items())})")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
