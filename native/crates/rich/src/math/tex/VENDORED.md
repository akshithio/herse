# Vendored: latex-rust 3.0.1

Source: <https://crates.io/crates/latex-rust/3.0.1>
(<https://github.com/jscarr64/latex-rust>), MIT OR Apache-2.0, Copyright 2026
Jeffrey S Carr. See `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE` (STIX Two Math is
SIL OFL 1.1, `crates/rich/assets/stix-two-math/OFL.txt`; `data/dvipsnames.tsv`
is LPPL-1.3c, keep its header).

## What was taken

`src/{atoms,color,dim,error,hash,style_map,symbols}.rs`, `src/font`,
`src/layout`, `src/parser`, `data/*.tsv`, the STIX Two Math font. Not taken:
the SVG/PNG/egui renderers (`math/svg.rs` replaces them), tests, benches.

Mechanical changes: `crate::` became `crate::math::tex::`, doc-comment code
blocks became ```` ```ignore ```` (they are `latex_rust::` examples), include
paths point at `data/` and `assets/`, `skrifa` 0.39 -> 0.44 (no code change;
it is the version the GPUI app already links).

## herse patches

Every change is marked with a `herse:` comment; `herse.patch` is the full
diff against upstream 3.0.1 (`src/`, renderers excluded). In short:

Layout (`layout/engine.rs`, `layout/engine_walk.rs`, `layout/numbering.rs`, `font/`):

* Vertical delimiter **glyph assembly** (OpenType MATH extensible recipes):
  `\left|`, `\|`, `vmatrix`/`Vmatrix` bars and tall parens/brackets/braces no
  longer stop at the largest pre-built variant.
* `\middle` (e-TeX), sized with the enclosing `\left`/`\right`.
* `\lim` and `\max \min \sup \inf \det \gcd \Pr \limsup \liminf`, big
  operators, `\operatorname*`, `\mathop`: limits above/below in display
  style only (`\displaylimits`), scripts in text style.
* Limit placement: baseline-drop/rise minimum *or* gap, not both added
  (limits and `\underbrace` labels sat ~0.3 em too far).
* `\vec` is the small combining arrow, positioned like a diacritic (it was a
  0.95 em arrow that widened its nucleus); accents over multi-letter nuclei
  are centred, not skewed by the first glyph.
* `align`: amsmath's `{}` before the right column of each pair (`&=` gets
  relation spacing); `align`/`gather` rows use `\baselineskip + \jot`.
* `array`: `\arraycolsep` padding, `\arraystretch` struts, rules spanning
  rows, `\hline` between rows, centred on the axis.
* Rule-less fractions for `\binom`, `\atop`, `\genfrac{}{}{0pt}` (binomials
  had a fraction bar).
* `NumberingConfig::auto` to switch off automatic equation numbers (MathJax
  `tags: 'none'`), and a `\qquad` before numbers/tags.

Parser (`parser/`):

* `\mathop \mathrel \mathbin \mathord \mathopen \mathclose \mathpunct
  \mathinner` (new `MathNode::Class`); braced single atoms are Ord (`{=}`,
  `3{,}14`).
* `\operatorname*` / `\operatornamewithlimits`; `\,` `\:` `\;` `\!` inside
  operator names.
* Infix `\atop`, `\brace`, `\brack`; `\genfrac` honours 0pt rules, its style
  argument and bare `[`/`]` delimiters.
* TeX dimension units `in cm mm pc dd cc sp px ex`.
* `:` is a relation, `!` and `?` close (TeX mathcodes).
* A script after `)` attaches to `)` and an argument `(` is just `(` (TeX);
  upstream grouped `(…)^2`, which turned `\operatorname{f}(z)_i` into an Ord.
* Explicit spaces (`\quad &`) and `{}` survive in environment cells.
* A script with no nucleus gets an empty one (`^\circ`); accents accept an
  empty base (`\tilde{}`, `\tilde\;`).
* `\hom`, `\injlim`, `\projlim`.

To update upstream: unpack the new latex-rust, apply `herse.patch` (expect
conflicts where upstream fixed the same thing), redo the mechanical changes.
