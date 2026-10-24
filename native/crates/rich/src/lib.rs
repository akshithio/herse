//! `herse-rich`: turns raw notebook output and Markdown pieces into
//! structures the GPUI app can paint. No GPUI dependency; everything is
//! synchronous, `Send + Sync`, and fast enough to run on
//! `cx.background_spawn` per cell and cache.
//!
//! # [`ansi`] — escape sequences -> styled text
//!
//! [`ansi::parse`] removes every escape sequence in one pass (~450 MB/s on
//! a densely colored log, ~2.4 GB/s on plain text) and returns the text plus
//! [`ansi::StyledSpan`]s (SGR: bold/dim/italic/underline/strikethrough/inverse,
//! 16/256/RGB colors in `;` and `:` forms) and OSC 8 [`ansi::Link`]s. Colors
//! stay symbolic; [`ansi::AnsiStyle::resolve`] maps them through a
//! [`ansi::Palette`] (`LIGHT` = JupyterLab's colors, `DARK`) with JupyterLab's
//! rules (bold brightens 0–7, inverse, yellow-on-yellow).
//!
//! ```
//! use herse_rich::ansi;
//! let t = ansi::parse("\x1b[0;31mValueError\x1b[0m: bad");
//! assert_eq!(t.text, "ValueError: bad");
//! assert_eq!(t.spans[0].range, 0..10);
//! let rgb = t.spans[0].style.resolve(&ansi::Palette::LIGHT).fg;
//! # let _ = rgb;
//! ```
//!
//! # [`stream`] — `\r`, `\b`, huge outputs
//!
//! Exactly JupyterLab 4's `processText` (cursor-based overwrite, its `\b`
//! quirk, UTF-16 indexing, escapes counted as characters). For live output
//! use [`stream::append`] and display [`stream::visible`] — a buffer whose
//! cursor is mid-line ends with a `\r` + prefix marker that keeps the cursor
//! across messages (it is valid JupyterLab input, so saving it is fine). For
//! saved notebooks use [`stream::normalize`] (borrows when there is nothing
//! to do). Pipe the result through [`ansi::parse`].
//! [`stream::truncate_for_display`] / [`stream::head_tail`] cut 100k-line
//! outputs without copying.
//!
//! ```
//! use herse_rich::stream;
//! let mut buf = String::new();
//! for pct in [10, 50, 100] {
//!     stream::append(&mut buf, &format!("\r{pct:3}%"));
//! }
//! assert_eq!(stream::visible(&buf), "100%");
//! ```
//!
//! # [`math`] — LaTeX -> SVG
//!
//! [`math::render_svg`]`(latex, display)` returns a [`math::MathSvg`]: an SVG
//! document with glyph outlines as paths (no font at paint time), filled with
//! `currentColor` (only the root `<g>` mentions it, so a theme color can be
//! substituted), plus `width_em`, `height_em` and `baseline_em` (bottom of
//! the SVG to the math baseline) for inline placement. Typical formulas take
//! 40–150 µs, warm-up is ~2 ms. Input with `$`/`$$`/`\(`/`\[` delimiters is
//! accepted; unsupported or malformed input is an `Err` (show the source in
//! monospace), never a panic. [`math::remove_math`] finds math in Markdown
//! exactly as JupyterLab does (including bare `\begin{align}` blocks), and
//! [`math::Macros`] carries `\newcommand`s across cells like MathJax.
//!
//! ```
//! use herse_rich::math;
//! let m = math::render_svg(r"\frac{a}{b}", false).unwrap();
//! assert!(m.svg.contains("currentColor") && m.baseline_em > 0.0);
//! assert!(math::render_svg(r"\frac{1}{", false).is_err());
//! ```
//!
//! The engine is a vendored, patched copy of latex-rust 3.0.1 (pure Rust,
//! TeX layout on the STIX Two Math OpenType MATH table); see
//! `src/math/tex/VENDORED.md`.

// The only `unsafe` is two arena dereferences in the vendored TeX engine
// (src/math/tex/layout/engine_walk.rs, with SAFETY notes).
#![deny(unsafe_code)]

pub mod ansi;
pub mod math;
pub mod stream;
