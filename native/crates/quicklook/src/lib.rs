//! The renderer behind Herse's Quick Look extension: Finder's space bar on a
//! notebook calls the Swift preview provider (`packaging/macos/QuickLook`),
//! which calls [`herse_ql_render`] with the file's path and shows the page
//! `herse-html` makes of it.

use std::ffi::{CStr, c_char};
use std::path::Path;

/// Bigger notebooks get a short page instead of a slow preview.
const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// The preview page for the notebook at `path`. Never fails: a notebook
/// that can't be read gets a page saying why.
pub fn preview(path: &Path) -> String {
    let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_BYTES {
        return message(&title, &format!("This notebook is {} MB, too big to preview. Open it in Herse.", size / (1024 * 1024)));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return message(&title, &format!("Herse couldn't read this file: {err}")),
    };
    match herse_nb::parse(&bytes) {
        Ok(nb) => herse_html::render(&nb, &herse_html::Options { title, max_output_lines: 200, inputs: true }),
        Err(err) => message(&title, &format!("This isn't a notebook Herse can read: {err}")),
    }
}

fn message(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"color-scheme\" content=\"light dark\"><title>{}</title></head>\
         <body style=\"font:15px -apple-system,sans-serif;margin:48px;color:#78736c\">{}</body></html>",
        herse_html::escape(title),
        herse_html::escape(text)
    )
}

/// C entry point: the page for the notebook at the NUL-terminated `path`,
/// as UTF-8 bytes whose length is stored in `out_len`. Free it with
/// [`herse_ql_free`].
///
/// # Safety
/// `path` must be a valid NUL-terminated string and `out_len` a valid
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn herse_ql_render(path: *const c_char, out_len: *mut usize) -> *mut u8 {
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned();
    let page = std::panic::catch_unwind(|| preview(Path::new(&path)))
        .unwrap_or_else(|_| message("", "Herse couldn't preview this notebook."));
    let bytes = page.into_bytes().into_boxed_slice();
    unsafe { *out_len = bytes.len() };
    Box::into_raw(bytes) as *mut u8
}

/// Free a page returned by [`herse_ql_render`].
///
/// # Safety
/// `ptr` and `len` must come from one call to [`herse_ql_render`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn herse_ql_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_never_fail() {
        let dir = std::env::temp_dir().join(format!("herse-ql-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.ipynb");
        std::fs::write(&bad, b"{not json").unwrap();
        assert!(preview(&bad).contains("a notebook Herse can read"));
        assert!(preview(&dir.join("missing.ipynb")).contains("read this file"));
        let good = dir.join("good.ipynb");
        std::fs::write(&good, br#"{"cells": [{"cell_type": "markdown", "metadata": {}, "source": "Hello"}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#).unwrap();
        let page = preview(&good);
        assert!(page.contains("<title>good.ipynb</title>") && page.contains("<p>Hello</p>"));
        let c = std::ffi::CString::new(good.to_str().unwrap()).unwrap();
        let mut len = 0;
        let ptr = unsafe { herse_ql_render(c.as_ptr(), &mut len) };
        assert_eq!(unsafe { std::slice::from_raw_parts(ptr, len) }, page.as_bytes());
        unsafe { herse_ql_free(ptr, len) };
        std::fs::remove_dir_all(&dir).ok();
    }
}
