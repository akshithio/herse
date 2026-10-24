#!/usr/bin/env python3
"""Cross-check herse-nb's Jupyter-canonical writer against nbformat itself.

    cargo run -p herse-nb --release --example roundtrip -- [CORPUS...] --canonical OUT_DIR
    python3 -I crates/nb/tests/py_crosscheck.py OUT_DIR

For every OUT_DIR/NNNNN.ipynb (our `to_string_with(.., WriteOptions::jupyter())`
of the notebook whose path is in NNNNN.path) this computes what Jupyter would
save after opening it, `nbformat.write(nbformat.read(path, as_version=4))`,
and compares bytes. Prints a summary plus the first differing line of each
mismatch. Exit status is 1 if any notebook differs.

nbformat ids for v3 -> v4 upgrades are random, so upgraded notebooks are
compared with cell ids masked.
"""
from __future__ import annotations

import io
import logging
import os
import re
import sys

import nbformat

logging.disable(logging.CRITICAL)  # nbformat logs validation errors; we only compare bytes

ID_RE = re.compile(r'^(\s*"id": )"[0-9a-f]{8}"', re.M)


def jupyter_bytes(path: str) -> str:
    nb = nbformat.read(path, as_version=4)
    buf = io.StringIO()
    nbformat.write(nb, buf)
    return buf.getvalue()


def main() -> int:
    out_dir = sys.argv[1]
    names = sorted(n for n in os.listdir(out_dir) if n.endswith(".ipynb"))
    same = 0
    differ = []
    py_errors = []
    for name in names:
        ours = open(os.path.join(out_dir, name), encoding="utf8").read()
        src = open(os.path.join(out_dir, name[:-6] + ".path"), encoding="utf8").read()
        try:
            theirs = jupyter_bytes(src)
        except Exception as e:  # noqa: BLE001
            py_errors.append((src, repr(e)[:120]))
            continue
        upgraded = '"orig_nbformat"' in theirs or not re.search(r'"nbformat": 4', open(src, encoding="utf8", errors="replace").read()[-200:])
        # v3 upgrades: nbformat adds metadata.orig_nbformat* and strips them on write; ids are random.
        if upgraded:
            ours_cmp, theirs_cmp = ID_RE.sub(r'\1"<id>"', ours), ID_RE.sub(r'\1"<id>"', theirs)
        else:
            ours_cmp, theirs_cmp = ours, theirs
        if ours_cmp == theirs_cmp:
            same += 1
            continue
        a, b = ours_cmp.splitlines(), theirs_cmp.splitlines()
        i = next((k for k, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
        differ.append((src, i + 1, a[i] if i < len(a) else "<eof>", b[i] if i < len(b) else "<eof>"))
    print(f"nbformat {nbformat.__version__}: identical {same} / {len(names) - len(py_errors)}"
          f" (python failed to read {len(py_errors)})")
    for src, line, x, y in differ:
        print(f"DIFF {src}:{line}\n  ours:     {x[:160]}\n  nbformat: {y[:160]}")
    for src, err in py_errors:
        print(f"PYERR {src}: {err}")
    return 1 if differ else 0


if __name__ == "__main__":
    sys.exit(main())
