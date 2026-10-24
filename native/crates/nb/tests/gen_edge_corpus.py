#!/usr/bin/env python3
"""Generate edge-case notebooks in many writers' styles for herse-nb round-trip tests.

    python3 -I crates/nb/tests/gen_edge_corpus.py OUT_DIR

Writes OUT_DIR/<variant>.ipynb for one notebook full of edge cases (floats
Python and JS spell differently, huge ints, every str.splitlines boundary,
control characters, astral/CJK text, attachments, widget JSON, unknown
keys/outputs), serialized the way nbformat, Colab, VS Code (JS), Python 2,
compact/tab/CRLF/BOM writers do, plus a v4.4 notebook (no cell ids) and a v3
notebook. Checked-in copies live in crates/nb/tests/data/.

Then run the round-trip example on OUT_DIR, and py_crosscheck.py.
"""
from __future__ import annotations

import base64
import copy
import json
import os
import random
import shutil
import subprocess
import sys

import nbformat
from nbformat import v3
from nbformat.v4 import new_code_cell, new_markdown_cell, new_notebook, new_output, new_raw_cell
from nbformat.v4.rwbase import split_lines

random.seed(7)


def b64(n: int) -> str:
    return base64.b64encode(random.randbytes(n)).decode()


FLOATS = [0.1, 1e-05, 1e16, 1e15, 100.0, -0.0, 0.0, 1.7976931348623157e308, 5e-324, 2.5e-07,
          123456789.123, 1 / 3, 1e22, 1.5, 3.14159, 0.0001, 1e-4 * 3, 2.0 ** 0.5, 1e100, -1e-100,
          9007199254740993.0, 0.30000000000000004]
INTS = [0, -1, 2 ** 63 - 1, -(2 ** 63), 2 ** 64 - 1, 2 ** 64, 2 ** 70, -(2 ** 70), 12345678901234567890123]

TRICKY = {
    "emoji": "😀 astral 𝔘",
    "cjk": "数据框 データ 데이터",
    "seps": "a b c\x85d",
    "ctrl": "nul\x00 soh\x01 esc\x1b del\x7f bs\x08 ff\x0c vt\x0b",
    "quotes": 'say "hi" \\ back\\slash / slash </script>',
    "tab": "a\tb",
}


def build() -> nbformat.NotebookNode:
    nb = new_notebook()
    nb.metadata = {
        "kernelspec": {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"},
        "language_info": {"name": "python", "version": "3.11.5", "codemirror_mode": {"name": "ipython", "version": 3}},
        "floats": FLOATS,
        "ints": INTS,
        "tricky": TRICKY,
        "nested": {"z": [1, {"b": None, "a": True}], "a": [], "m": {}},
        "zzz_unknown": {"keep": "me"},
    }
    cells = []
    cells.append(new_markdown_cell("# Title\n\nline with trailing spaces   \n"))
    cells.append(new_markdown_cell(""))
    cells.append(new_markdown_cell("\n"))
    cells.append(new_code_cell("crlf = 1\r\nlone_cr = 2\rvt\x0bff\x0cfs\x1cgs\x1drs\x1enel\x85ls ps end"))
    cells.append(new_raw_cell("raw\ncontent", metadata={"format": "text/x-rst", "raw_mimetype": "text/x-rst"}))
    md = new_markdown_cell("![img](attachment:pic.png)\n![t](attachment:note.txt)")
    md.attachments = {"pic.png": {"image/png": b64(300)}, "note.txt": {"text/plain": "line1\nline2\n"}}
    cells.append(md)
    code = new_code_cell("import sys\nprint('hi')\nprint('err', file=sys.stderr)\n1/3", execution_count=7)
    code.metadata = {"scrolled": True, "tags": ["a", "b"], "collapsed": False, "jupyter": {"source_hidden": False}}
    code.outputs = [
        new_output("stream", name="stdout", text="hi\nprogress\r50%\r100%\n\x1b[31mred\x1b[0m\n"),
        new_output("stream", name="stderr", text="err\n"),
        new_output("execute_result", execution_count=7, data={
            "text/plain": "0.3333333333333333",
            "text/html": "<table>\n<tr><td>1</td></tr>\n</table>",
            "text/latex": "$\\frac{1}{3}$",
            "image/png": b64(2000),
            "image/jpeg": b64(500) + "\n",
            "image/svg+xml": "<svg>\n<rect/>\n</svg>\n",
            "application/javascript": "console.log(1);\nconsole.log(2);",
            "application/json": {"b": [1, 2.5, None], "a": "x"},
            "application/vnd.jupyter.widget-view+json": {"model_id": "abc", "version_major": 2, "version_minor": 0},
            "application/vnd.plotly.v1+json": {"data": [{"x": FLOATS[:6], "y": [1, 2, 3]}]},
        }, metadata={"image/png": {"width": 640, "height": 480.5}}),
        new_output("display_data", data={"text/plain": "<Figure size 640x480 with 1 Axes>", "image/png": b64(4000)},
                   metadata={}),
        new_output("error", ename="ZeroDivisionError", evalue="division by zero",
                   traceback=["\x1b[0;31m---------\x1b[0m", "\x1b[0;31mZeroDivisionError\x1b[0m: division by zero"]),
    ]
    cells.append(code)
    cells.append(new_code_cell(""))
    weird = new_code_cell("x = 1")
    weird.outputs = [nbformat.NotebookNode({"output_type": "custom_widget_thing", "payload": {"k": [1, 2]}, "text": ["a\n", "b"]})]
    cells.append(weird)
    bad_md = new_markdown_cell("markdown with an invalid outputs key")
    bad_md["outputs"] = []
    cells.append(bad_md)
    nb.cells = cells
    return nb


def disk_form(nb) -> dict:
    """The JSON object nbformat would dump (split lines, transient stripped)."""
    d = copy.deepcopy(nb)
    d = split_lines(d)
    return json.loads(json.dumps(d))


COLAB_CELL_ORDER = ["cell_type", "source", "metadata", "id", "execution_count", "outputs", "attachments"]


def colab_order(d: dict) -> dict:
    def cell(c):
        out = {k: c[k] for k in COLAB_CELL_ORDER if k in c}
        out.update({k: v for k, v in c.items() if k not in out})
        out["outputs"] = [{"output_type": o["output_type"], **{k: v for k, v in o.items() if k != "output_type"}}
                          for o in c.get("outputs", [])] if "outputs" in c else None
        if out["outputs"] is None:
            del out["outputs"]
        return out
    return {"nbformat": d["nbformat"], "nbformat_minor": d["nbformat_minor"], "metadata": d["metadata"],
            "cells": [cell(c) for c in d["cells"]]}


def main() -> None:
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    nb = build()
    with open(os.path.join(out, "nbformat.ipynb"), "w", encoding="utf8") as f:
        nbformat.write(nb, f)
    d = disk_form(nb)

    def dump(name, text, newline="\n", bom=False):
        data = (text + newline) if newline else text
        with open(os.path.join(out, name), "wb") as f:
            if bom:
                f.write(b"\xef\xbb\xbf")
            f.write(data.encode("utf8"))

    canon = json.dumps(d, sort_keys=True, indent=1, separators=(",", ": "), ensure_ascii=False)
    dump("ensure_ascii.ipynb", json.dumps(d, sort_keys=True, indent=1, separators=(",", ": "), ensure_ascii=True))
    dump("colab_indent2_unsorted.ipynb", json.dumps(colab_order(d), indent=2, ensure_ascii=False))
    dump("compact.ipynb", json.dumps(d, sort_keys=True, separators=(",", ":"), ensure_ascii=False), newline="")
    dump("compact_py_default.ipynb", json.dumps(d, sort_keys=True, ensure_ascii=False))
    dump("py2_comma_space.ipynb", json.dumps(d, sort_keys=True, indent=1, separators=(", ", ": "), ensure_ascii=False))
    dump("crlf.ipynb", canon.replace("\n", "\r\n"), newline="\r\n")
    dump("bom.ipynb", canon, bom=True)
    dump("no_trailing_newline.ipynb", canon, newline="")
    dump("tabs.ipynb", json.dumps(d, sort_keys=True, indent="\t", ensure_ascii=False))
    dump("indent0.ipynb", json.dumps(d, sort_keys=True, indent=0, ensure_ascii=False))
    unsplit = json.loads(json.dumps(nb))  # in-memory form: multiline fields as plain strings
    dump("unsplit_strings.ipynb", json.dumps(unsplit, sort_keys=True, indent=1, ensure_ascii=False))

    # VS Code / JS writers: JSON.stringify of key-sorted content.
    node = shutil.which("node")
    if node:
        js = ("const fs=require('fs');const d=JSON.parse(fs.readFileSync(0,'utf8'));"
              "const s=o=>Array.isArray(o)?o.map(s):(o&&typeof o==='object')?Object.fromEntries(Object.keys(o).sort().map(k=>[k,s(o[k])])):o;"
              "process.stdout.write(JSON.stringify(s(d),null,parseInt(process.argv[1]))+'\\n')")
        src = json.dumps(d, ensure_ascii=False)
        for indent in (1, 2):
            res = subprocess.run([node, "-e", js, str(indent)], input=src.encode(), capture_output=True, check=True)
            with open(os.path.join(out, f"js_sorted_indent{indent}.ipynb"), "wb") as f:
                f.write(res.stdout)

    # nbformat 4.4: no cell ids.
    nb44 = build()
    nb44.nbformat_minor = 4
    for c in nb44.cells:
        c.pop("id", None)
    with open(os.path.join(out, "v4_4_no_ids.ipynb"), "w", encoding="utf8") as f:
        nbformat.write(nb44, f)

    # nbformat 3.
    nb3 = v3.new_notebook(name="old")
    ws = v3.new_worksheet()
    ws.cells = [
        v3.new_heading_cell("Old heading\nsecond line", level=2),
        v3.new_text_cell("markdown", "Some *md*\ntext"),
        v3.new_text_cell("html", "<b>html</b>"),
        v3.new_code_cell("print(1)\n1+1", prompt_number=3, outputs=[
            v3.new_output("stream", output_text="1\n", stream="stdout"),
            v3.new_output("pyout", output_text="2", prompt_number=3, output_png=b64(100), output_json='{"a": 1}'),
            v3.new_output("pyerr", ename="E", evalue="v", traceback=["tb"]),
        ], collapsed=True),
    ]
    nb3.worksheets = [ws]
    with open(os.path.join(out, "v3.ipynb"), "w", encoding="utf8") as f:
        f.write(v3.writes_json(nb3))


if __name__ == "__main__":
    main()
