// Renders tests/data/vegalite/*.json with vega-lite + vega (node, offline)
// to SVG references for the gallery's side-by-side comparisons.
//
//   VEGA_DIR=/opt/anaconda3/lib/python3.13/site-packages/panel/dist/bundled/vegaplot \
//     node crates/plot/tests/data/reference/vegalite_ref.cjs <out-dir>
//
// VEGA_DIR must hold the UMD bundles `vega@5` and `vega-lite@5` (panel ships them).
const fs = require('fs');
const path = require('path');
const Module = require('module');
const dir = process.env.VEGA_DIR;
const vega = require(path.join(dir, 'vega@5'));
// vega-lite's UMD bundle requires('vega'): hand it the one we loaded.
const origResolve = Module._resolveFilename;
Module._resolveFilename = function (req, ...rest) {
  return req === 'vega' ? path.join(dir, 'vega@5') : origResolve.call(this, req, ...rest);
};
const vl = require(path.join(dir, 'vega-lite@5'));
const dataDir = path.join(__dirname, '..', 'vegalite');
const out = process.argv[2];
fs.mkdirSync(out, {recursive: true});
(async () => {
  let n = 0;
  for (const f of fs.readdirSync(dataDir).filter(f => f.endsWith('.json')).sort()) {
    try {
      const spec = JSON.parse(fs.readFileSync(path.join(dataDir, f), 'utf8'));
      const view = new vega.View(vega.parse(vl.compile(spec).spec), {renderer: 'none'});
      fs.writeFileSync(path.join(out, f.replace(/\.json$/, '.svg')), await view.toSVG());
      n++;
    } catch (e) {
      console.error(f, e.message);
    }
  }
  console.log('wrote', n, 'vega-lite reference SVGs');
})();
