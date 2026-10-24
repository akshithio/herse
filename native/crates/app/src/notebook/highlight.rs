//! Syntax highlighting for code cells shown in their static (non-editing) form.
//!
//! Highlights are computed off the main thread, in parallel chunks, as soon as
//! a notebook opens. Until a cell's highlights arrive it renders as plain text
//! in the same font, so nothing moves when colour appears.

use std::{cell::RefCell, ops::Range, sync::Arc};

use gpui_kit::HighlightStyle;
use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};
use gpui_kit::component::input::{Rope, RopeExt as _};
use tree_sitter::{InputEdit, Point};

pub type Highlights = Arc<[(Range<usize>, HighlightStyle)]>;

thread_local! {
    static HIGHLIGHTER: RefCell<Option<SyntaxHighlighter>> = const { RefCell::new(None) };
}

/// Map a notebook's language name to a grammar we ship.
pub fn grammar_for(language: &str) -> &'static str {
    match language.to_ascii_lowercase().as_str() {
        "python" | "python3" | "ipython" | "ipython3" => "python",
        "markdown" => "markdown",
        "rust" => "rust",
        "javascript" | "js" | "typescript" | "ts" => "javascript",
        "bash" | "sh" | "shell" => "bash",
        "sql" => "sql",
        "json" => "json",
        _ => "python",
    }
}

pub fn highlight(language: &str, source: &str, theme: &HighlightTheme) -> Highlights {
    if source.is_empty() {
        return Arc::from(Vec::new());
    }
    HIGHLIGHTER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let hl = match slot.as_mut() {
            Some(hl) if hl.language().as_ref() == language => hl,
            _ => slot.insert(SyntaxHighlighter::new(language)),
        };
        // Replace the previous cell's text wholesale: one edit spanning all of it.
        let old_len = hl.text().len();
        let old_end = hl.text().offset_to_point(old_len);
        let rope = Rope::from_str(source);
        let new_end = rope.offset_to_point(source.len());
        let edit = InputEdit {
            start_byte: 0,
            old_end_byte: old_len,
            new_end_byte: source.len(),
            start_position: Point::new(0, 0),
            old_end_position: Point::new(old_end.row, old_end.column),
            new_end_position: Point::new(new_end.row, new_end.column),
        };
        hl.update(Some(edit), &rope, None);
        let styles = hl.styles(&(0..source.len()), theme);
        // Drop default-styled runs: they cost layout time and change nothing.
        let styles: Vec<_> = styles
            .into_iter()
            .filter(|(_, style)| *style != HighlightStyle::default())
            .collect();
        Arc::from(styles)
    })
}
