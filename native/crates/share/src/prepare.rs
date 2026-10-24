//! The copy of the notebook that gets uploaded.

use herse_nb::{Cell, Notebook, WriteOptions};

use crate::meta::{META_KEY, META_NAMESPACE};

/// What to leave out of the published copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrepareOptions {
    /// Clear every code cell's outputs and execution count (and the saved
    /// widget state, which only describes outputs).
    pub strip_outputs: bool,
    /// Drop `metadata.widgets` (saved ipywidgets state).
    pub strip_widget_state: bool,
}

/// The notebook as it will be published: without Herse's own `herse.share`
/// bookkeeping (a downloaded copy must not point at the author's gist), and
/// optionally without outputs / widget state. The original is untouched.
pub fn prepare_for_publish(nb: &Notebook, opts: &PrepareOptions) -> Notebook {
    let cells = nb
        .cells
        .iter()
        .map(|cell| {
            if opts.strip_outputs && cell.is_code() {
                // Don't clone outputs only to drop them (they can be most of the file).
                Cell {
                    id: cell.id.clone(),
                    cell_type: cell.cell_type,
                    source: cell.source.clone(),
                    metadata: cell.metadata.clone(),
                    execution_count: None,
                    outputs: Vec::new(),
                    attachments: cell.attachments.clone(),
                    extra: cell.extra.clone(),
                }
            } else {
                cell.clone()
            }
        })
        .collect();
    let mut metadata = nb.metadata.clone();
    if let Some(serde_json::Value::Object(ns)) = metadata.get_mut(META_NAMESPACE) {
        ns.shift_remove(META_KEY);
        if ns.is_empty() {
            metadata.shift_remove(META_NAMESPACE);
        }
    }
    if opts.strip_outputs || opts.strip_widget_state {
        metadata.shift_remove("widgets");
    }
    Notebook {
        nbformat: nb.nbformat,
        nbformat_minor: nb.nbformat_minor,
        metadata,
        cells,
        extra: nb.extra.clone(),
        format: nb.format.clone(),
    }
}

/// The `.ipynb` text to upload: [`prepare_for_publish`] written exactly as
/// Jupyter (`nbformat.write`) would, which nbviewer renders best.
pub fn publish_content(nb: &Notebook, opts: &PrepareOptions) -> String {
    herse_nb::to_string_with(&prepare_for_publish(nb, opts), &WriteOptions::jupyter())
}

/// Clear all outputs and execution counts in place, and the widget state.
pub fn strip_outputs(nb: &mut Notebook) {
    for cell in nb.cells.iter_mut().filter(|c| c.is_code()) {
        cell.clear_outputs();
    }
    nb.metadata.shift_remove("widgets");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{ShareInfo, read_share_info, write_share_info};
    use herse_nb::{CellType, MimeBundle, MimeData, Output, StreamName};

    fn sample() -> Notebook {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        nb.cells[0].source = "print('hi')".into();
        nb.cells[0].execution_count = Some(3);
        nb.cells[0].outputs.push(Output::Stream {
            name: StreamName::Stdout,
            text: "hi\n".into(),
        });
        let mut data = MimeBundle::new();
        data.insert("text/plain", MimeData::Text("42".into()));
        nb.cells[0].outputs.push(Output::ExecuteResult {
            execution_count: Some(3),
            data,
            metadata: Default::default(),
        });
        nb.cells
            .push(nb.new_cell(CellType::Markdown).with_source("# Title"));
        nb.metadata.insert(
            "widgets".into(),
            serde_json::json!({"application/vnd.jupyter.widget-state+json": {}}),
        );
        nb.metadata
            .insert("herse".into(), serde_json::json!({"theme": "dark"}));
        write_share_info(
            &mut nb,
            &ShareInfo {
                gist_id: "abc".into(),
                public: true,
                html_url: "https://gist.github.com/abc".into(),
                published_at: String::new(),
                filename: None,
                owner: None,
            },
        );
        nb
    }

    #[test]
    fn prepared_copy_drops_share_info_only() {
        let nb = sample();
        let out = prepare_for_publish(&nb, &PrepareOptions::default());
        assert_eq!(read_share_info(&out), None);
        assert_eq!(out.metadata["herse"], serde_json::json!({"theme": "dark"}));
        assert!(out.metadata.contains_key("widgets"));
        assert_eq!(out.cells, nb.cells);
        assert!(read_share_info(&nb).is_some(), "original untouched");
    }

    #[test]
    fn strip_outputs_and_widgets() {
        let nb = sample();
        let out = prepare_for_publish(
            &nb,
            &PrepareOptions {
                strip_outputs: true,
                strip_widget_state: false,
            },
        );
        assert!(out.cells[0].outputs.is_empty());
        assert_eq!(out.cells[0].execution_count, None);
        assert_eq!(out.cells[0].source, "print('hi')");
        assert_eq!(out.cells[1], nb.cells[1]);
        assert!(!out.metadata.contains_key("widgets"));

        let out = prepare_for_publish(
            &nb,
            &PrepareOptions {
                strip_outputs: false,
                strip_widget_state: true,
            },
        );
        assert_eq!(out.cells[0].outputs.len(), 2);
        assert!(!out.metadata.contains_key("widgets"));

        let mut inplace = nb.clone();
        strip_outputs(&mut inplace);
        assert!(inplace.cells[0].outputs.is_empty() && !inplace.metadata.contains_key("widgets"));
    }

    #[test]
    fn content_is_jupyter_formatted() {
        let text = publish_content(
            &sample(),
            &PrepareOptions {
                strip_outputs: true,
                strip_widget_state: false,
            },
        );
        assert!(text.starts_with("{\n \"cells\": [\n"));
        assert!(text.ends_with("}\n"));
        assert!(!text.contains("gist.github.com"));
        let back = herse_nb::parse(text.as_bytes()).unwrap();
        assert_eq!(back.cells.len(), 2);
        assert!(back.cells[0].outputs.is_empty());
    }
}
