//! `herse-nb`: the Jupyter notebook model, a fast `.ipynb` parser, and a
//! diff-clean writer.
//!
//! ```
//! let bytes = br#"{"cells": [{"cell_type": "code", "execution_count": null, "id": "a1b2c3d4",
//!   "metadata": {}, "outputs": [], "source": ["print(1)\n", "x"]}],
//!   "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;
//! let mut nb = herse_nb::parse(bytes)?;
//! assert_eq!(nb.cells[0].source, "print(1)\nx"); // line arrays joined
//! nb.cells[0].source.push_str(" + 1");
//! let saved = herse_nb::to_string(&nb); // same formatting as the file
//! let jupyter = herse_nb::to_string_with(&nb, &herse_nb::WriteOptions::jupyter()); // nbformat's bytes
//! # assert!(saved.contains("\"x + 1\"") && jupyter.ends_with("}\n"));
//! # Ok::<(), herse_nb::Error>(())
//! ```
//!
//! # Model
//!
//! [`Notebook`] / [`Cell`] / [`Output`] mirror nbformat 4.x. Every multiline
//! field (cell source, stream text, text-like mime payloads, attachment text)
//! is one `String`; nbformat's on-disk line arrays are an encoding detail.
//! Metadata maps are `serde_json::Map`s in file order, so unknown keys
//! survive. Keys a cell should not have ([`Cell::extra`], [`Notebook::extra`])
//! and outputs of unknown type or shape ([`Output::Unknown`]) are kept
//! verbatim, so a save never drops data. A mime payload is
//! [`MimeData::Text`] for everything nbformat treats as text (including
//! base64 images) and [`MimeData::Json`] for `application/json`,
//! `application/*+json` and non-string values.
//!
//! # Reading
//!
//! [`parse`] is a single-pass parser specialized for notebooks: it validates
//! UTF-8 once, builds cells and outputs directly (a `Value` tree only for
//! metadata and JSON payloads), joins line arrays into exactly sized strings,
//! and finds string ends 8 bytes at a time for short strings and with SIMD
//! `memchr` for long ones, so base64 images are copied once at memory speed.
//! It beat serde-derive parsing on serde_json, sonic-rs and simd-json on all
//! three benchmarks (1 MB mixed, 50 MB image-heavy, 5,000 cells). Unusual
//! shapes fall back to a generic parse of that one object, converted by the
//! same rules as [`output_from_iopub`]. nbformat 3 files are upgraded to 4.5
//! on read the way nbformat does ([`FormatHint::converted_from`] is set);
//! older versions are [`Error::UnsupportedVersion`].
//!
//! # Writing
//!
//! nbformat writes `json.dumps(nb, sort_keys=True, indent=1,
//! separators=(",", ": "), ensure_ascii=False) + "\n"` after splitting
//! multiline strings with Python's `str.splitlines(True)`. The writer
//! reproduces that byte for byte: the same split rules (`\r`, `\v`, `\f`,
//! `\x1c`..`\x1e`, U+0085, U+2028/9 are line breaks too), the same mime types
//! split into line arrays (`text/*`, `image/svg+xml`, `application/javascript`),
//! Python's string escaping, Python's float `repr` (`1e-05`, `1e+16`,
//! `100.0`), integers beyond 64 bits kept digit for digit, and nbformat's
//! `strip_transient`. Output `transient` (display ids) is runtime-only and
//! never written, as in Jupyter.
//!
//! Other writers (VS Code and other JS tools, Colab, Python 2 era IPython,
//! hand-edited files) differ in indentation, key order, separators, line
//! cutting, escaping and number spelling. [`parse`] records the source
//! formatting in [`Notebook::format`] ([`FormatHint`]): indent, newline style,
//! separators, key sortedness, BOM, trailing newline, ASCII escaping and
//! line-array style, plus, only for files that need them, key order templates,
//! number spellings and per-field exceptions (a plain string among line
//! arrays, chunks not cut at line breaks, unusual escapes), the latter keyed
//! by content so edited fields fall back to the file's prevailing style.
//! [`to_string`] reuses the hint, so re-saving an untouched notebook
//! reproduces its bytes. [`to_string_with`] + [`WriteOptions::jupyter`]
//! gives exactly what `nbformat.write` would.
//!
//! # Live outputs
//!
//! [`output_from_iopub`] converts iopub `stream` / `display_data` /
//! `update_display_data` / `execute_result` / `error` content (messaging
//! spec 5.x) into an [`Output`].

mod convert;
mod error;
mod format;
mod model;
mod parse;
pub mod text;
mod v3;
mod write;

pub use convert::output_from_iopub;
pub use error::{Error, Result};
pub use format::{FormatHint, Indent, LineSplit, WriteOptions};
pub use model::{
    Cell, CellType, Map, MimeBundle, MimeData, Notebook, Output, StreamName, is_json_mime, is_split_mime,
    new_cell_id,
};
pub use parse::{parse, parse_value};
pub use serde_json::Value;
pub use write::{to_string, to_string_with};

#[cfg(test)]
mod tests;
