//! Notebook <-> shared document round trips on the repo's fixture corpus.

use std::path::{Path, PathBuf};

use herse_collab::{notebook_from_doc, notebook_to_doc};
use herse_nb::Notebook;
use yrs::updates::decoder::Decode;
use yrs::{ReadTxn, StateVector, Transact, Update};

fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut files = Vec::new();
    for dir in [root.clone(), root.join("formats")] {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ipynb") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// What the round trip should give back: `nb` with the assigned ids.
fn expected(nb: &Notebook, assigned: &[herse_collab::AssignedId]) -> Notebook {
    let mut nb = nb.clone();
    let strip = nb.nbformat == 4 && nb.nbformat_minor <= 4;
    if !strip {
        for a in assigned {
            nb.cells[a.index].id = Some(a.id.clone());
        }
    }
    nb
}

/// The doc as a peer receives it: full state through the lib0 encoding.
fn over_the_wire(doc: &yrs::Doc) -> yrs::Doc {
    let update = doc.transact().encode_state_as_update_v1(&StateVector::default());
    let remote = yrs::Doc::new();
    remote.transact_mut().apply_update(Update::decode_v1(&update).unwrap()).unwrap();
    remote
}

#[test]
fn fixtures_roundtrip_losslessly() {
    let mut checked = 0;
    for path in fixtures() {
        let bytes = std::fs::read(&path).unwrap();
        let Ok(nb) = herse_nb::parse(&bytes) else { continue }; // corrupt_* fixtures
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (doc, assigned) = notebook_to_doc(&nb);
        let want = expected(&nb, &assigned);
        for (how, d) in [("local", doc.clone()), ("wire", over_the_wire(&doc))] {
            let mut back = notebook_from_doc(&d);
            back.format = nb.format.clone();
            assert!(back == want, "{name} ({how}): model differs after the round trip");
            // Byte-exact on save, including key order, number spelling and
            // big integers.
            assert_eq!(herse_nb::to_string(&back), herse_nb::to_string(&want), "{name} ({how}): saved bytes differ");
        }
        if assigned.is_empty() && !name.contains("duplicate") {
            assert_eq!(herse_nb::to_string(&want), herse_nb::to_string(&nb), "{name}");
        }
        checked += 1;
    }
    assert!(checked >= 25, "only {checked} fixtures checked");
}

#[test]
fn ids_are_assigned_where_missing_or_duplicated() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/formats");
    let dup = herse_nb::parse(&std::fs::read(root.join("duplicate_cell_ids.ipynb")).unwrap()).unwrap();
    let (doc, assigned) = notebook_to_doc(&dup);
    assert!(!assigned.is_empty());
    assert!(assigned.iter().all(|a| a.previous.is_some()));
    let back = notebook_from_doc(&doc);
    let ids: Vec<_> = back.cells.iter().map(|c| c.id.clone().unwrap()).collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());

    let old = herse_nb::parse(&std::fs::read(root.join("nbformat_4_4_no_ids.ipynb")).unwrap()).unwrap();
    let (doc, assigned) = notebook_to_doc(&old);
    assert_eq!(assigned.len(), old.cells.len());
    assert!(assigned.iter().all(|a| a.previous.is_none() && a.id.len() == 36));
    // nbformat < 4.5: ids live in the doc but not in the notebook.
    assert!(notebook_from_doc(&doc).cells.iter().all(|c| c.id.is_none()));
}

#[test]
fn doc_follows_jupyter_ydoc_schema() {
    use yrs::{Array, Map, Out};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let nb = herse_nb::parse(&std::fs::read(root.join("showcase.ipynb")).unwrap()).unwrap();
    let (doc, _) = notebook_to_doc(&nb);
    let txn = doc.transact();
    let meta = txn.get_map("meta").unwrap();
    assert!(matches!(meta.get(&txn, "nbformat"), Some(Out::Any(_))));
    assert!(matches!(meta.get(&txn, "metadata"), Some(Out::YMap(_))));
    let cells = txn.get_array("cells").unwrap();
    assert_eq!(cells.len(&txn) as usize, nb.cells.len());
    let mut saw_stream = false;
    for cell in cells.iter(&txn) {
        let Out::YMap(cell) = cell else { panic!("cell is not a Y.Map") };
        assert!(matches!(cell.get(&txn, "id"), Some(Out::Any(yrs::Any::String(_)))));
        assert!(matches!(cell.get(&txn, "source"), Some(Out::YText(_))));
        assert!(matches!(cell.get(&txn, "metadata"), Some(Out::YMap(_))));
        let Some(Out::Any(yrs::Any::String(t))) = cell.get(&txn, "cell_type") else { panic!() };
        if &*t == "code" {
            let Some(Out::YArray(outputs)) = cell.get(&txn, "outputs") else { panic!("outputs") };
            assert!(cell.get(&txn, "execution_count").is_some());
            assert!(matches!(cell.get(&txn, "execution_state"), Some(Out::Any(_))));
            for o in outputs.iter(&txn) {
                let Out::YMap(o) = o else { panic!("output is not a Y.Map") };
                if matches!(o.get(&txn, "output_type"), Some(Out::Any(yrs::Any::String(s))) if &*s == "stream") {
                    assert!(matches!(o.get(&txn, "text"), Some(Out::YText(_))));
                    saw_stream = true;
                }
            }
        } else {
            assert!(cell.get(&txn, "outputs").is_none());
        }
    }
    assert!(saw_stream);
}

/// The git-ignored benchmark corpus (`fixtures/generate.py --big`), when present:
/// `cargo test -p herse-collab --release --test roundtrip -- --ignored --nocapture`.
#[test]
#[ignore]
fn big_fixtures_roundtrip() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/big");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "ipynb") {
            continue;
        }
        let nb = herse_nb::parse(&std::fs::read(&path).unwrap()).unwrap();
        let t = std::time::Instant::now();
        let (doc, assigned) = notebook_to_doc(&nb);
        let to_doc = t.elapsed();
        let t = std::time::Instant::now();
        let update = doc.transact().encode_state_as_update_v1(&StateVector::default());
        let encode = t.elapsed();
        let t = std::time::Instant::now();
        let remote = over_the_wire(&doc);
        let mut back = notebook_from_doc(&remote);
        let from_doc = t.elapsed();
        back.format = nb.format.clone();
        assert!(back == expected(&nb, &assigned), "{path:?}");
        eprintln!(
            "{}: {} cells, sync message {:.1} MB; to doc {to_doc:?}, encode {encode:?}, decode+read {from_doc:?}",
            path.file_name().unwrap().to_string_lossy(),
            nb.cells.len(),
            update.len() as f64 / 1e6
        );
    }
}
