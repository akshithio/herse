//! Vendored TeX math engine: latex-rust 3.0.1 (MIT OR Apache-2.0, Copyright
//! 2026 Jeffrey S Carr, <https://github.com/jscarr64/latex-rust>), parser +
//! box layout + OpenType MATH font code, with herse patches (see
//! `VENDORED.md`, full diff in `herse.patch`). Its SVG/PNG/egui renderers are
//! not included; herse-rich writes SVG itself (`math::svg`). Edit in place,
//! mark changes with a `herse:` comment and keep `VENDORED.md` current.
#![allow(dead_code, unused_imports, clippy::all, rustdoc::all, missing_docs)]
#![allow(unsafe_code)] // arena dereferences in layout/engine_walk.rs

mod atoms;
mod color;
mod dim;
mod error;
mod hash;
mod style_map;
mod symbols;

pub mod font;
pub mod layout;
pub mod parser;

pub use atoms::symbol_atom_kind;
pub use color::{named_color, parse_color_spec, Color, ColorTable};
pub use dim::{Dim, DIM_PREC};
pub use error::{Error, FontError, ParseError};
pub use font::{GlyphMetrics, MathFont, STIX_TWO_MATH_NAME, STIX_TWO_MATH_OTF, STIX_TWO_MATH_SHA256};
pub use layout::{
    layout, layout_with_em_size_pt, layout_with_max_depth, layout_with_numbering,
    layout_with_numbering_and_em_size_pt, BoxContent, MathBox, MathParams, MathStyle, NumberFormat,
    NumberStyle, NumberingConfig, NumberingState, MIDDLE_PLACEHOLDER,
};
pub use parser::{
    format_tokens, parse, parse_with_colors, parse_with_options, preprocess, tokenize, AccentKind,
    AtomKind, ColSpec, DelimSize, Delimiter, EnvRow, EqNumber, FracStyle, IntegralKind, MathNode,
    MatrixStyle, ParseOptions, PhantomKind, SpaceKind, TextItem, TextRun, TextStyle, Token,
    DEFAULT_MAX_NESTING_DEPTH,
};
pub use style_map::styled_char;
pub use symbols::{category_count, glyph_char, lookup, symbols, SymbolEntry, SymbolKind};
