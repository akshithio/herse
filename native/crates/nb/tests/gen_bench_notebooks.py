#!/usr/bin/env python3
"""Generate the herse-nb benchmark notebooks (written with nbformat).

    python3 -I crates/nb/tests/gen_bench_notebooks.py OUT_DIR
    cargo run -p herse-nb --release --example bench -- OUT_DIR/*.ipynb

mixed_1mb.ipynb    ~1 MB: markdown + code cells with stream text, HTML tables, small PNGs, tracebacks
images_50mb.ipynb  ~50 MB: 100 cells, each a ~500 KB base64 PNG
cells_5000.ipynb   5,000 small cells (markdown / code with short outputs)
"""
import base64, random, sys, os
import nbformat
from nbformat.v4 import new_code_cell, new_markdown_cell, new_notebook, new_output
random.seed(1)
out = sys.argv[1]
def b64(n): return base64.b64encode(random.randbytes(n)).decode()
def words(n): return " ".join(random.choice(["data","frame","plot","value","alpha","beta","model","train","loss","x","y","import","numpy","pandas"]) for _ in range(n))
def code_src(lines): return "\n".join(f"{words(3)} = {random.random():.4f}  # {words(4)}" for _ in range(lines))
def html_table(rows):
    body = "\n".join("<tr>" + "".join(f"<td>{random.random():.6f}</td>" for _ in range(6)) + "</tr>" for _ in range(rows))
    return f"<div>\n<table border=\"1\" class=\"dataframe\">\n<thead><tr><th>a</th><th>b</th></tr></thead>\n<tbody>\n{body}\n</tbody>\n</table>\n</div>"
def kernel(nb):
    nb.metadata = {"kernelspec": {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"},
                   "language_info": {"name": "python", "version": "3.11.5", "file_extension": ".py", "mimetype": "text/x-python",
                                     "codemirror_mode": {"name": "ipython", "version": 3}, "nbconvert_exporter": "python", "pygments_lexer": "ipython3"}}
    return nb

# ~1 MB mixed notebook
nb = kernel(new_notebook())
i = 0
while True:
    i += 1
    nb.cells.append(new_markdown_cell(f"## Section {i}\n\n{words(40)}\n\n- {words(5)}\n- {words(5)}"))
    c = new_code_cell(code_src(random.randint(3, 15)), execution_count=i)
    k = i % 4
    if k == 0: c.outputs = [new_output("stream", name="stdout", text="\n".join(words(8) for _ in range(20)) + "\n")]
    elif k == 1: c.outputs = [new_output("execute_result", execution_count=i, data={"text/plain": "\n".join(words(10) for _ in range(10)), "text/html": html_table(15)})]
    elif k == 2: c.outputs = [new_output("display_data", data={"text/plain": "<Figure size 640x480 with 1 Axes>", "image/png": b64(6000)}, metadata={"image/png": {"width": 640, "height": 480}})]
    else: c.outputs = [new_output("error", ename="ValueError", evalue=words(4), traceback=[f"\x1b[0;31m{words(6)}\x1b[0m" for _ in range(8)])]
    nb.cells.append(c)
    if len(nbformat.writes(nb)) > 1_000_000 if i % 10 == 0 else False: break
nbformat.write(nb, os.path.join(out, "mixed_1mb.ipynb"))

# ~50 MB image-heavy
nb = kernel(new_notebook())
for i in range(100):
    c = new_code_cell(f"plt.plot(x, y{i})\nplt.show()", execution_count=i + 1)
    c.outputs = [new_output("display_data", data={"text/plain": "<Figure size 1200x800 with 1 Axes>", "image/png": b64(375_000)}, metadata={})]
    nb.cells.append(c)
nbformat.write(nb, os.path.join(out, "images_50mb.ipynb"))

# 5000 cells
nb = kernel(new_notebook())
for i in range(5000):
    if i % 3 == 0:
        nb.cells.append(new_markdown_cell(f"### Step {i}\n{words(15)}"))
    else:
        c = new_code_cell(code_src(random.randint(1, 6)), execution_count=i)
        if i % 3 == 1: c.outputs = [new_output("stream", name="stdout", text=f"{words(6)}\n")]
        else: c.outputs = [new_output("execute_result", execution_count=i, data={"text/plain": f"{random.random()}"})]
        nb.cells.append(c)
nbformat.write(nb, os.path.join(out, "cells_5000.ipynb"))
