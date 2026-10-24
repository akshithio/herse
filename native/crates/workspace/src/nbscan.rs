//! Structural scan of an `.ipynb` file on top of [`crate::json::Cursor`]:
//! yields each cell's type, id and the raw byte spans of its source and
//! outputs, plus the notebook-level kernel metadata. Outputs are skipped,
//! never decoded. Handles nbformat 4 (`cells`) and 3 (`worksheets[*].cells`,
//! `heading` cells, code `input`).

use crate::json::{Cursor, R, ScanError, decode_str};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellKind {
    Code,
    Markdown,
    Raw,
    /// nbformat 3 heading cell (becomes markdown in v4).
    Heading,
    Other,
}

impl CellKind {
    fn from_raw(raw: &[u8]) -> CellKind {
        match raw {
            b"code" => CellKind::Code,
            b"markdown" => CellKind::Markdown,
            b"raw" => CellKind::Raw,
            b"heading" => CellKind::Heading,
            _ => CellKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CellKind::Code => "code",
            CellKind::Markdown | CellKind::Heading => "markdown",
            CellKind::Raw => "raw",
            CellKind::Other => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RawCell<'a> {
    /// Index in the flattened (v4) cell list.
    pub index: usize,
    pub kind: CellKind,
    /// Decoded `cell_type` (only meaningful for [`CellKind::Other`]).
    pub cell_type: Cow<'a, [u8]>,
    /// Raw (escaped) `id` string contents.
    pub id: Option<&'a [u8]>,
    /// Raw JSON value of `source` (or v3 code `input`): string or array of strings.
    pub source: Option<&'a [u8]>,
    /// Raw JSON value of `outputs`.
    pub outputs: Option<&'a [u8]>,
    pub outputs_nonempty: bool,
    /// v3 heading level.
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NbMeta {
    pub nbformat: Option<u32>,
    pub nbformat_minor: Option<u32>,
    pub kernel_display_name: Option<String>,
    pub kernel_name: Option<String>,
    pub kernel_language: Option<String>,
    pub language_info_name: Option<String>,
}

const STOPPED: &str = "stopped by visitor";

/// Scans a notebook. `on_cell` returns `false` to stop early (the scan then
/// returns `Ok` with whatever metadata was seen so far).
pub(crate) fn scan_notebook<'a>(
    bytes: &'a [u8],
    on_cell: &mut dyn FnMut(RawCell<'a>) -> bool,
) -> Result<NbMeta, ScanError> {
    let mut meta = NbMeta::default();
    let mut index = 0usize;
    let mut c = Cursor::new(bytes);
    let res = c.object(|c, key| match &*key {
        b"cells" => scan_cells(c, &mut index, on_cell),
        b"worksheets" => c.array(|c, _| {
            if c.peek()? != b'{' {
                return c.skip_value();
            }
            c.object(|c, k| if &*k == b"cells" { scan_cells(c, &mut index, on_cell) } else { c.skip_value() })
        }),
        b"metadata" => scan_metadata(c, &mut meta),
        b"nbformat" => {
            meta.nbformat = c.opt_u32()?;
            Ok(())
        }
        b"nbformat_minor" => {
            meta.nbformat_minor = c.opt_u32()?;
            Ok(())
        }
        _ => c.skip_value(),
    });
    match res {
        Ok(()) => Ok(meta),
        Err(e) if e.what == STOPPED => Ok(meta),
        Err(e) => Err(e),
    }
}

fn scan_cells<'a>(c: &mut Cursor<'a>, index: &mut usize, on_cell: &mut dyn FnMut(RawCell<'a>) -> bool) -> R<()> {
    if c.peek()? != b'[' {
        return c.skip_value();
    }
    c.array(|c, _| {
        if c.peek()? != b'{' {
            return c.skip_value();
        }
        let mut cell = RawCell {
            index: *index,
            kind: CellKind::Other,
            cell_type: Cow::Borrowed(b""),
            id: None,
            source: None,
            outputs: None,
            outputs_nonempty: false,
            level: None,
        };
        let mut input = None;
        c.object(|c, key| {
            match &*key {
                b"cell_type" => {
                    if c.peek()? == b'"' {
                        let raw = c.string_raw()?;
                        cell.cell_type = if memchr::memchr(b'\\', raw).is_some() {
                            let mut s = String::new();
                            decode_str(raw, &mut s);
                            Cow::Owned(s.into_bytes())
                        } else {
                            Cow::Borrowed(raw)
                        };
                    } else {
                        c.skip_value()?;
                    }
                }
                b"source" => cell.source = Some(c.raw_value()?),
                b"input" => input = Some(c.raw_value()?),
                b"id" => cell.id = c.opt_string_raw()?,
                b"outputs" => {
                    let start = {
                        c.ws();
                        c.i
                    };
                    cell.outputs_nonempty = c.skip_array_nonempty()?.unwrap_or(false);
                    cell.outputs = Some(&c.b[start..c.i]);
                }
                b"level" => cell.level = c.opt_u32()?,
                _ => c.skip_value()?,
            }
            Ok(())
        })?;
        cell.kind = CellKind::from_raw(&cell.cell_type);
        if cell.source.is_none() {
            cell.source = input;
        }
        *index += 1;
        if on_cell(cell) {
            Ok(())
        } else {
            Err(ScanError { at: c.i, what: STOPPED })
        }
    })
}

fn scan_metadata(c: &mut Cursor<'_>, meta: &mut NbMeta) -> R<()> {
    if c.peek()? != b'{' {
        return c.skip_value();
    }
    c.object(|c, key| match &*key {
        b"kernelspec" if c.peek()? == b'{' => c.object(|c, k| {
            match &*k {
                b"display_name" => meta.kernel_display_name = c.opt_string()?,
                b"name" => meta.kernel_name = c.opt_string()?,
                b"language" => meta.kernel_language = c.opt_string()?,
                _ => c.skip_value()?,
            }
            Ok(())
        }),
        b"language_info" if c.peek()? == b'{' => c.object(|c, k| {
            if &*k == b"name" {
                meta.language_info_name = c.opt_string()?;
                Ok(())
            } else {
                c.skip_value()
            }
        }),
        _ => c.skip_value(),
    })
}
