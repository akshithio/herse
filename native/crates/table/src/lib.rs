//! `herse-table`: DataFrame-style HTML tables -> a grid model the GPUI app
//! can draw natively.
//!
//! Notebook DataFrame outputs are `text/html`: pandas `_repr_html_`, pandas
//! `Styler`, polars, R (IRkernel), Julia (DataFrames.jl), or any hand-written
//! `<table>`. [`parse`] turns the first table of such an output into a
//! [`Table`]: header and body rows as a *rectangular* grid (row/col spans
//! expanded), index (row label) columns identified, cells classified
//! (number / missing / bool / truncation marker), alignment decided, and
//! Styler / inline CSS colors resolved per cell.
//!
//! ```
//! let html = r#"<table border="1" class="dataframe">
//!   <thead><tr style="text-align: right;"><th></th><th>a</th><th>b</th></tr></thead>
//!   <tbody><tr><th>0</th><td>1.5</td><td>x</td></tr>
//!          <tr><th>1</th><td>NaN</td><td>y</td></tr></tbody>
//! </table><p>2 rows × 2 columns</p>"#;
//! let t = herse_table::parse(html).unwrap();
//! assert_eq!((t.columns, t.index_columns), (3, 1));
//! assert_eq!(t.body[0][1].text, "1.5");
//! assert_eq!(t.body[0][1].align, herse_table::Align::Right);
//! assert!(t.body[1][1].is_null());
//! assert_eq!(t.footer.as_deref(), Some("2 rows × 2 columns"));
//! assert!(t.standalone);
//! ```
//!
//! Everything is synchronous, never panics on malformed input, and produces
//! owned `Send + Sync` data: call it from `cx.background_spawn` and cache the
//! result per output.
//!
//! # Drawing a [`Table`]
//!
//! * Lay out `columns` columns; size them from [`Table::column_widths_chars`]
//!   (`chars × average advance + padding`) instead of measuring every string.
//! * Header rows first (`Table::header`), then body rows. Each row has exactly
//!   `columns` cells. A cell with `colspan` / `rowspan` > 1 paints over the
//!   slots it spans; covered slots ([`Cell::is_covered`]) carry the origin's
//!   background but no text — skip their text and inner borders. In a
//!   virtualized body, a rowspan origin may be scrolled away: its covered
//!   slots point to it through [`Cell::span_origin`] (draw the label sticky or
//!   repeat it muted).
//! * The first `index_columns` columns are row labels (`is_index`): a good
//!   candidate for a sticky left column with a header-like background.
//! * Use [`Cell::align`] as is (numbers right, text left, markers centered,
//!   index left, CSS / `align` overrides applied). Draw [`Cell::is_null`]
//!   muted and [`Table::is_ellipsis_row`] rows as a short divider.
//! * [`CellStyle`]: `bg` fills the cell, `bar` is a band behind the text from
//!   `start` to `end` of the width, `fg` / `bold` / `italic` style the text.
//!   When only `bg` is set, pick black or white text from
//!   [`Rgba::luminance`].
//! * `caption` goes above the grid; `footer` (the shape line) below it, and
//!   `dtypes` (polars / R / Julia) as a muted line under the last header row.
#![forbid(unsafe_code)]

mod builder;
mod classify;
mod css;
mod text;
mod tokenizer;

/// An sRGB color with alpha, 0–255 per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Opaque color from `0xRRGGBB`.
    pub const fn from_rgb_u32(rgb: u32) -> Self {
        Rgba { r: (rgb >> 16) as u8, g: (rgb >> 8) as u8, b: rgb as u8, a: 255 }
    }

    /// `0xRRGGBBAA`, the layout `gpui::rgba(u32)` takes.
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 24) | ((self.g as u32) << 16) | ((self.b as u32) << 8) | self.a as u32
    }

    /// WCAG relative luminance (0 = black, 1 = white), ignoring alpha. Useful
    /// to pick a readable text color when a cell has a background but no
    /// explicit `fg`.
    pub fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }
}

/// Horizontal text alignment within a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Align {
    #[default]
    Left,
    Right,
    Center,
}

/// What a cell's text is, decided from the text alone (`classify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CellKind {
    /// No text (also every covered span slot and every padding cell).
    #[default]
    Empty,
    Text,
    /// Integer, decimal, scientific, `1,234`, `12%`, `$5`, `inf`, `-inf`.
    Number,
    /// `True` / `False` / `true` / `false` / `TRUE` / `FALSE`.
    Bool,
    /// A missing value: `NaN`, `nan`, `None`, `NaT`, `<NA>`, `NA` (R), `null`
    /// (polars), italic `missing` (Julia), `N/A`, and a lone dash (a common
    /// Styler `na_rep`). Draw muted.
    Null,
    /// A truncation marker: `...` (pandas), `…` (polars), `⋮` / `⋯` (Julia).
    Ellipsis,
}

/// A pandas `Styler.bar()` data bar: a horizontal band behind the text,
/// spanning `start..end` as fractions (0–1) of the cell's width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DataBar {
    pub start: f32,
    pub end: f32,
    pub color: Rgba,
}

/// Resolved visual style of a cell (Styler `<style>` rules, inline `style`
/// attributes, `bgcolor`, `<b>` / `<i>` inside the cell). `None` / `false` =
/// use the app's default.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct CellStyle {
    pub bg: Option<Rgba>,
    pub fg: Option<Rgba>,
    pub bold: bool,
    pub italic: bool,
    pub bar: Option<DataBar>,
}

impl CellStyle {
    pub fn is_default(&self) -> bool {
        *self == CellStyle::default()
    }
}

/// One slot of the grid.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Cell {
    /// Rendered text: entities decoded, whitespace collapsed as a browser
    /// would, `<br>` / block breaks as `\n`. Empty for covered slots.
    pub text: String,
    /// Columns this cell spans (≥ 1). `0` on a slot covered by another
    /// cell's span (see [`Cell::span_origin`]).
    pub colspan: u16,
    /// Rows this cell spans (≥ 1), clamped to its section. `0` on covered slots.
    pub rowspan: u16,
    /// For a slot covered by another cell's colspan / rowspan: that cell's
    /// `(row, col)` in the same section (`header` or `body`). Covered slots
    /// copy the origin's `style`, `is_header`, `is_index` and `align`, so
    /// they paint the merged area's background; skip their text and borders.
    pub span_origin: Option<(usize, usize)>,
    /// A `<th>` (or a Julia row-number cell).
    pub is_header: bool,
    /// The cell lies in an index (row label) column: `col < Table::index_columns`.
    /// True for header cells in those columns too (the corner / index names).
    pub is_index: bool,
    pub align: Align,
    pub kind: CellKind,
    pub style: CellStyle,
}

impl Cell {
    /// A missing value (`NaN`, `None`, `NaT`, `<NA>`, `null`, ...): draw muted.
    pub fn is_null(&self) -> bool {
        self.kind == CellKind::Null
    }
    pub fn is_numeric(&self) -> bool {
        self.kind == CellKind::Number
    }
    /// A pandas / polars / Julia truncation marker (`...`, `…`, `⋮`).
    pub fn is_ellipsis(&self) -> bool {
        self.kind == CellKind::Ellipsis
    }
    /// Covered by another cell's span: paint its background only.
    pub fn is_covered(&self) -> bool {
        self.span_origin.is_some()
    }
}

/// Which producer the HTML looks like it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Flavor {
    /// Anything else (hand-written HTML, `to_html` of other libraries, ...).
    #[default]
    Generic,
    /// `DataFrame._repr_html_` / `to_html` (`class="dataframe"`).
    Pandas,
    /// `DataFrame.style` (`<table id="T_...">`).
    PandasStyler,
    /// polars `_repr_html_` (`<small>shape: (r, c)</small>`, dtype row).
    Polars,
    /// R / IRkernel data.frame (`<dbl>`-style dtype row).
    R,
    /// Julia DataFrames.jl (`class="data-frame"`).
    Julia,
}

/// A parsed table: a rectangular grid of `columns` columns.
///
/// Every row in `header` and `body` has exactly `columns` cells; spans are
/// expanded (the spanning cell sits at its top-left slot with
/// `colspan` / `rowspan` set, the slots it covers have `span_origin`), and
/// short rows are padded with empty cells.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Table {
    pub caption: Option<String>,
    /// Column header rows (`<thead>`, or leading all-`<th>` rows when there
    /// is no `<thead>`). MultiIndex columns give several rows with colspans.
    pub header: Vec<Vec<Cell>>,
    /// Data rows (`<tbody>` then `<tfoot>`). Leading index cells have
    /// `is_index = true`; MultiIndex rows give rowspans.
    pub body: Vec<Vec<Cell>>,
    /// Leading columns that are row labels (pandas index, R row names,
    /// Julia row numbers). 0 for a Styler with a hidden index or polars.
    pub index_columns: usize,
    /// Grid width.
    pub columns: usize,
    /// The shape line outside the table: pandas `60 rows × 50 columns`,
    /// polars `shape: (5, 3)`, Julia `3×2 DataFrame`; several are joined
    /// with ` · `.
    pub footer: Option<String>,
    /// Column dtypes, one per grid column (empty for index columns), from
    /// polars' dtype row, R's `<dbl>` row or Julia's type row. That row is
    /// removed from `header`.
    pub dtypes: Option<Vec<String>>,
    pub flavor: Flavor,
    /// The whole output is just this table (plus `<style>`, caption, shape
    /// line and invisible wrappers): the app can replace the HTML rendering
    /// with the native grid without losing anything visible.
    pub standalone: bool,
    /// Body rows dropped by the safety cap (grids above ~4M cells); 0 normally.
    pub dropped_rows: usize,
}

/// Parse the first `<table>` of an HTML output. `None` if there is none.
pub fn parse(html: &str) -> Option<Table> {
    builder::build(html)
}

/// A CSS colour value (`#rgb`, `rgb()`, `hsl()`, named colours); `transparent`
/// comes back with zero alpha.
pub fn parse_css_color(v: &str) -> Option<Rgba> {
    css::parse_color(v).map(|c| c.unwrap_or_default())
}

/// True when the output is "just a table" (plus optional `<style>`, caption,
/// shape line): the app can render [`parse`]'s grid instead of the HTML.
/// Same cost as [`parse`]; prefer `parse(html).filter(|t| t.standalone)`
/// when you need the table anyway.
pub fn is_simple_table_output(html: &str) -> bool {
    parse(html).is_some_and(|t| t.standalone)
}

impl Table {
    /// Number of data (non-index) columns.
    pub fn data_columns(&self) -> usize {
        self.columns.saturating_sub(self.index_columns)
    }

    /// A body row that is only truncation markers (pandas `...` row, polars
    /// `…` row, Julia `⋮` row): draw it as a compact divider.
    pub fn is_ellipsis_row(&self, row: usize) -> bool {
        let Some(r) = self.body.get(row) else { return false };
        let mut any = false;
        for c in r {
            match c.kind {
                CellKind::Ellipsis => any = true,
                CellKind::Empty => {}
                _ => return false,
            }
        }
        any
    }

    /// A column of truncation markers (pandas / Styler `...` column).
    pub fn is_ellipsis_column(&self, col: usize) -> bool {
        let mut any = false;
        for r in self.header.iter().chain(&self.body) {
            match r.get(col).map(|c| c.kind) {
                Some(CellKind::Ellipsis) => any = true,
                Some(CellKind::Empty) | None => {}
                _ => return false,
            }
        }
        any
    }

    /// Per-column content width in characters (East Asian wide = 2, widest
    /// line of multi-line cells), over header and body, capped at 40.
    /// Spanning header cells widen the columns they cover evenly when they
    /// do not fit. See [`Table::column_widths_chars_capped`].
    pub fn column_widths_chars(&self) -> Vec<u16> {
        self.column_widths_chars_capped(40)
    }

    /// [`Table::column_widths_chars`] with a custom cap. Multiply by the
    /// font's average advance (digits are tabular in most UI fonts) and add
    /// cell padding to get pixel widths without measuring every string.
    pub fn column_widths_chars_capped(&self, cap: u16) -> Vec<u16> {
        let cap = cap as usize;
        let mut w = vec![0usize; self.columns];
        let rows = self.header.iter().chain(&self.body);
        for r in rows.clone() {
            for (cell, wc) in r.iter().zip(w.iter_mut()) {
                if cell.colspan == 1 && !cell.text.is_empty() {
                    *wc = (*wc).max(classify::display_width(&cell.text));
                }
            }
        }
        if let Some(d) = &self.dtypes {
            for (t, wc) in d.iter().zip(w.iter_mut()) {
                *wc = (*wc).max(classify::display_width(t));
            }
        }
        for v in w.iter_mut() {
            *v = (*v).min(cap);
        }
        // Spanning cells: one char of gap between columns counts toward the span.
        for r in rows {
            for (c, cell) in r.iter().enumerate().take(w.len()) {
                let span = cell.colspan as usize;
                if span > 1 && !cell.text.is_empty() {
                    let need = classify::display_width(&cell.text).min(cap * span);
                    let end = (c + span).min(self.columns);
                    let have: usize = w[c..end].iter().sum::<usize>() + (end - c - 1);
                    if need > have {
                        let extra = need - have;
                        let n = end - c;
                        for (k, v) in w[c..end].iter_mut().enumerate() {
                            *v += extra / n + usize::from(k < extra % n);
                        }
                    }
                }
            }
        }
        w.into_iter().map(|v| v.min(u16::MAX as usize) as u16).collect()
    }
}
