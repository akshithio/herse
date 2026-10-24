"""Renders tests/data/plotly/*.json with real plotly.js (headless Chrome) to
SVG references for the gallery's side-by-side comparisons.

    /opt/anaconda3/bin/python3 crates/plot/tests/data/reference/plotly_ref.py <out-dir>
    cargo run -p herse-plot --release --example gallery -- <gallery-dir> --ref <out-dir>/..

Uses the plotly.min.js bundled with the plotly Python package, so it works
offline. Writes <out-dir>/<name>.svg.
"""
import base64, html, json, os, re, shutil, subprocess, sys, tempfile

import plotly

here = os.path.dirname(os.path.abspath(__file__))
data_dir = os.path.join(here, "..", "plotly")
out_dir = sys.argv[1]
width = int(sys.argv[2]) if len(sys.argv) > 2 else 640
os.makedirs(out_dir, exist_ok=True)
work = tempfile.mkdtemp(prefix="plotly-ref-")
shutil.copy(os.path.join(os.path.dirname(plotly.__file__), "package_data", "plotly.min.js"), work)
figs = {f[:-5]: json.load(open(os.path.join(data_dir, f))) for f in sorted(os.listdir(data_dir)) if f.endswith(".json")}
page = os.path.join(work, "ref.html")
with open(page, "w") as out:
    out.write('<html><head><meta charset="utf-8"><script src="plotly.min.js"></script></head><body>\n<script>')
    out.write("const FIGS = " + json.dumps(figs) + ";\nconst W = %d;\n" % width)
    out.write("""
(async () => {
  for (const [name, fig] of Object.entries(FIGS)) {
    const pre = document.createElement('pre'); pre.id = 'fig-' + name; document.body.appendChild(pre);
    try {
      const div = document.createElement('div'); document.body.appendChild(div);
      const layout = Object.assign({}, fig.layout || {});
      layout.width = layout.width || W; layout.height = layout.height || 400;
      await Plotly.newPlot(div, fig.data || [], layout, {staticPlot: true});
      const url = await Plotly.toImage(div, {format: 'svg', width: layout.width, height: layout.height});
      pre.textContent = btoa(unescape(encodeURIComponent(decodeURIComponent(url.slice(url.indexOf(',') + 1)))));
      Plotly.purge(div); div.remove();
    } catch (e) { pre.textContent = 'ERR ' + e; }
  }
  const done = document.createElement('pre'); done.id = 'done'; document.body.appendChild(done);
})();
</script></body></html>""")
chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
dom = subprocess.run([chrome, "--headless=new", "--disable-gpu", "--no-first-run", "--user-data-dir=" + os.path.join(work, "profile"),
                      "--virtual-time-budget=120000", "--dump-dom", "file://" + page], capture_output=True, text=True, timeout=600).stdout
n = 0
for m in re.finditer(r'<pre id="fig-([^"]+)">([^<]*)</pre>', dom):
    body = html.unescape(m.group(2))
    if body.startswith("ERR"):
        print(m.group(1), body)
        continue
    open(os.path.join(out_dir, m.group(1) + ".svg"), "w").write(base64.b64decode(body).decode("utf-8"))
    n += 1
shutil.rmtree(work, ignore_errors=True)
print("wrote", n, "plotly reference SVGs")
