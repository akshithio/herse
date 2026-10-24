use super::*;

const SIMPLE: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "a1b2c3d4",
   "metadata": {},
   "source": [
    "# Title\n",
    "\n",
    "Some *text*"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 3,
   "id": "e5f6a7b8",
   "metadata": {
    "scrolled": true,
    "tags": [
     "x"
    ]
   },
   "outputs": [
    {
     "name": "stdout",
     "output_type": "stream",
     "text": [
      "hello\n",
      "world\n"
     ]
    },
    {
     "data": {
      "text/plain": [
       "42"
      ]
     },
     "execution_count": 3,
     "metadata": {},
     "output_type": "execute_result"
    },
    {
     "ename": "ValueError",
     "evalue": "bad",
     "output_type": "error",
     "traceback": [
      "\u001b[0;31mValueError\u001b[0m: bad"
     ]
    }
   ],
   "source": [
    "print('hello')\n",
    "print('world')\n",
    "42"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3 (ipykernel)",
   "language": "python",
   "name": "python3"
  },
  "language_info": {
   "name": "python",
   "version": "3.11.5"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

#[test]
fn simple_roundtrip() {
    let nb = parse(SIMPLE.as_bytes()).unwrap();
    assert_eq!(nb.cells.len(), 2);
    assert_eq!(nb.cells[0].source, "# Title\n\nSome *text*");
    assert_eq!(nb.cells[1].execution_count, Some(3));
    assert_eq!(nb.kernel_display_name(), Some("Python 3 (ipykernel)"));
    assert_eq!(nb.language(), Some("python"));
    assert!(nb.format.is_jupyter(), "{:?}", nb.format);
    assert_eq!(to_string(&nb), SIMPLE);
    assert_eq!(to_string_with(&nb, &WriteOptions::jupyter()), SIMPLE);
    match &nb.cells[1].outputs[0] {
        Output::Stream { name, text } => {
            assert_eq!(*name, StreamName::Stdout);
            assert_eq!(text, "hello\nworld\n");
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(nb.cells[1].outputs[1].data().unwrap().text("text/plain"), Some("42"));
}

#[test]
fn new_empty_writes() {
    let nb = Notebook::new_empty("python3", "Python 3", "python");
    let s = to_string(&nb);
    let back = parse(s.as_bytes()).unwrap();
    assert_eq!(back.cells.len(), 1);
    assert_eq!(back.cells[0].id.as_ref().unwrap().len(), 8);
    assert_eq!(to_string(&back), s);
}

// ------------------------------------------------------------------ text primitives

#[test]
fn splitlines_matches_python() {
    // Expected values from CPython 3.13 `s.splitlines(True)`.
    let cases: &[(&str, &[&str])] = &[
        ("", &[]),
        ("a", &["a"]),
        ("a\n", &["a\n"]),
        ("\n", &["\n"]),
        ("a\r\nb", &["a\r\n", "b"]),
        ("a\rb", &["a\r", "b"]),
        ("a\r", &["a\r"]),
        ("\r\n", &["\r\n"]),
        ("\r\r\n", &["\r", "\r\n"]),
        ("a\x0bb\x0cc\x1cd\x1de\x1ef", &["a\x0b", "b\x0c", "c\x1c", "d\x1d", "e\x1e", "f"]),
        ("x\u{85}y\u{2028}z\u{2029}w", &["x\u{85}", "y\u{2028}", "z\u{2029}", "w"]),
        ("a\n\n", &["a\n", "\n"]),
        ("no newline at end\nlast", &["no newline at end\n", "last"]),
        ("\n\n\n", &["\n", "\n", "\n"]),
        ("a\x1fb\x1bc", &["a\x1fb\x1bc"]),
        ("\u{85}", &["\u{85}"]),
        ("e\u{301}\n", &["e\u{301}\n"]),
        // UTF-8 lead bytes of the multi-byte breaks, without the break itself.
        ("\u{80}\u{2027}\u{202a}é", &["\u{80}\u{2027}\u{202a}é"]),
    ];
    for (input, want) in cases {
        let got: Vec<&str> = text::split_lines(input).collect();
        assert_eq!(&got, want, "splitlines({input:?})");
    }
}

#[test]
fn escaping_matches_python() {
    let s = "q\" b\\ / \x00\x01\x08\x09\x0a\x0b\x0c\x0d\x1b\x1f\x7f é 😀 \u{2028} \u{85} </script>";
    let mut out = String::new();
    text::write_json_str(&mut out, s, false);
    assert_eq!(
        out,
        "\"q\\\" b\\\\ / \\u0000\\u0001\\b\\t\\n\\u000b\\f\\r\\u001b\\u001f\x7f é 😀 \u{2028} \u{85} </script>\""
    );
    out.clear();
    text::write_json_str(&mut out, s, true);
    assert_eq!(
        out,
        "\"q\\\" b\\\\ / \\u0000\\u0001\\b\\t\\n\\u000b\\f\\r\\u001b\\u001f\\u007f \\u00e9 \\ud83d\\ude00 \\u2028 \\u0085 </script>\""
    );
    // The 8-byte fast path must not skip escapes at any alignment.
    for pad in 0..17 {
        let s = format!("{}\"{}\n", "x".repeat(pad), "y".repeat(pad));
        let mut out = String::new();
        text::write_json_str(&mut out, &s, false);
        assert_eq!(out, format!("\"{}\\\"{}\\n\"", "x".repeat(pad), "y".repeat(pad)));
    }
}

#[test]
fn float_repr_matches_python() {
    // Expected values from CPython 3.13 `repr(float)`.
    let cases: &[(f64, &str)] = &[
        (0.1, "0.1"),
        (1e-05, "1e-05"),
        (1e16, "1e+16"),
        (1e15, "1000000000000000.0"),
        (100.0, "100.0"),
        (-0.0, "-0.0"),
        (0.0, "0.0"),
        (1.7976931348623157e308, "1.7976931348623157e+308"),
        (5e-324, "5e-324"),
        (2.5e-07, "2.5e-07"),
        (123456789.123, "123456789.123"),
        (1.0 / 3.0, "0.3333333333333333"),
        (1e22, "1e+22"),
        (1.5, "1.5"),
        (0.0001, "0.0001"),
        (0.001, "0.001"),
        (1e-4 * 3.0, "0.00030000000000000003"),
        (std::f64::consts::SQRT_2, "1.4142135623730951"),
        (1e100, "1e+100"),
        (-1e-100, "-1e-100"),
        (9007199254740993.0, "9007199254740992.0"),
        (0.30000000000000004, "0.30000000000000004"),
        (123456789012345680.0, "1.2345678901234568e+17"),
        (1234567890123456.7, "1234567890123456.8"),
        (12345678901234567.0, "1.2345678901234568e+16"),
        (-1.5e-10, "-1.5e-10"),
        (2.5e300, "2.5e+300"),
        (1e23, "1e+23"),
        (4.35, "4.35"),
        (f64::NAN, "NaN"),
        (f64::INFINITY, "Infinity"),
        (f64::NEG_INFINITY, "-Infinity"),
    ];
    for &(f, want) in cases {
        assert_eq!(text::py_float_repr(f), want, "repr({f:e})");
    }
}

// ------------------------------------------------------------------ helpers

fn nb_json(cells: &str, minor: u32) -> String {
    format!("{{\n \"cells\": [{cells}],\n \"metadata\": {{}},\n \"nbformat\": 4,\n \"nbformat_minor\": {minor}\n}}\n")
}

fn roundtrip_identical(s: &str) -> Notebook {
    let nb = parse(s.as_bytes()).unwrap();
    assert_eq!(to_string(&nb), s, "preserve-mode round trip");
    nb
}

// ------------------------------------------------------------------ model / parse

#[test]
fn numbers_in_metadata() {
    let src = r#"{
 "cells": [],
 "metadata": {
  "a": 1e-05,
  "b": 1e+16,
  "c": 0.1,
  "d": 100.0,
  "e": -0.0,
  "f": 12345678901234567890123,
  "g": -170141183460469231731687303715884105728,
  "h": 18446744073709551615,
  "i": -9223372036854775808,
  "j": 0,
  "k": [true, false, null]
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;
    let src = src.replace("[true, false, null]", "[\n   true,\n   false,\n   null\n  ]");
    let nb = roundtrip_identical(&src);
    assert_eq!(nb.metadata["a"].as_f64(), Some(1e-5));
    assert_eq!(nb.metadata["h"].as_u64(), Some(u64::MAX));
    assert_eq!(nb.metadata["i"].as_i64(), Some(i64::MIN));
    // Huge integers keep their digits even in canonical mode (Python keeps them exactly).
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    assert!(canon.contains("\"f\": 12345678901234567890123,"));
    assert!(canon.contains("\"g\": -170141183460469231731687303715884105728,"));
    assert!(nb.format.is_jupyter());
}

#[test]
fn js_number_spellings() {
    // JS writes 1e-7 / 1e+21 / 5e-324 differently from Python.
    let src = "{\n \"cells\": [],\n \"metadata\": {\n  \"a\": 1e-7,\n  \"b\": 1.5E+300,\n  \"c\": 0.10\n },\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}\n";
    let nb = roundtrip_identical(src);
    assert_eq!(nb.format.number_spellings(), 3);
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    assert!(canon.contains("\"a\": 1e-07,"), "{canon}");
    assert!(canon.contains("\"b\": 1.5e+300,"), "{canon}");
    assert!(canon.contains("\"c\": 0.1\n"), "{canon}");
}

#[test]
fn missing_optional_fields() {
    // nbformat 4.4: no ids. Output without metadata, null/missing execution counts, cell without metadata.
    let src = r#"{"cells": [
  {"cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [
     {"data": {"text/plain": ["1"]}, "output_type": "execute_result"},
     {"data": {"text/plain": "x"}, "output_type": "display_data"}
  ], "source": "1"},
  {"cell_type": "code", "outputs": [], "source": []},
  {"cell_type": "markdown", "source": "md"}
 ], "metadata": {}, "nbformat": 4, "nbformat_minor": 4}"#;
    let nb = parse(src.as_bytes()).unwrap();
    assert_eq!(nb.cells.len(), 3);
    assert!(nb.cells.iter().all(|c| c.id.is_none()));
    assert!(!nb.uses_cell_ids());
    let c0 = &nb.cells[0];
    assert_eq!(c0.execution_count, None);
    match &c0.outputs[0] {
        Output::ExecuteResult { execution_count, data, metadata } => {
            assert_eq!(*execution_count, None);
            assert_eq!(data.text("text/plain"), Some("1"));
            assert!(metadata.is_empty());
        }
        o => panic!("{o:?}"),
    }
    assert!(c0.outputs[1].is_display_data());
    assert_eq!(nb.cells[1].source, "");
    assert!(nb.cells[2].metadata.is_empty());
    // Canonical output repairs the schema: metadata {} and execution_count null are written.
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    let back = parse(canon.as_bytes()).unwrap();
    assert_eq!(back.cells, nb.cells);
    assert!(!canon.contains("\"id\""));
    // A new cell in a 4.4 notebook gets no id (an id would be a schema violation).
    assert!(nb.new_cell(CellType::Code).id.is_none());
}

#[test]
fn attachments_roundtrip_and_join() {
    let cell = r#"
  {
   "attachments": {
    "a.png": {
     "image/png": "iVBORw0KGgo="
    },
    "note.txt": {
     "text/plain": [
      "line1\n",
      "line2"
     ]
    }
   },
   "cell_type": "markdown",
   "id": "0001",
   "metadata": {},
   "source": [
    "![a](attachment:a.png)"
   ]
  }
 "#;
    let src = nb_json(cell, 5);
    let nb = roundtrip_identical(&src);
    let att = nb.cells[0].attachments.as_ref().unwrap();
    assert_eq!(att["note.txt"]["text/plain"], "line1\nline2");
    assert_eq!(att["a.png"]["image/png"], "iVBORw0KGgo=");
    assert_eq!(to_string_with(&nb, &WriteOptions::jupyter()), src);
}

#[test]
fn unknown_keys_preserved() {
    let src = r#"{
 "cells": [
  {
   "cell_type": "markdown",
   "metadata": {
    "zeta": 1,
    "alpha": {
     "y": 2,
     "x": 1
    }
   },
   "outputs": [],
   "source": [],
   "weird": "kept"
  },
  {
   "cell_type": "code",
   "execution_count": 1,
   "metadata": {},
   "outputs": [
    {
     "output_type": "my_custom",
     "payload": [
      1,
      2
     ]
    },
    {
     "name": "stdout",
     "output_type": "stream",
     "text": "x",
     "extra": 1
    },
    {
     "name": "stdlog",
     "output_type": "stream",
     "text": "y"
    }
   ],
   "source": []
  }
 ],
 "metadata": {
  "z_last": true,
  "a_first": false
 },
 "nbformat": 4,
 "nbformat_minor": 4,
 "top_extra": [
  1
 ]
}
"#;
    let nb = roundtrip_identical(src);
    assert!(!nb.format.sort_keys);
    assert_eq!(nb.extra["top_extra"], serde_json::json!([1]));
    let md = &nb.cells[0];
    assert_eq!(md.cell_type, CellType::Markdown);
    assert_eq!(md.extra["outputs"], serde_json::json!([]));
    assert_eq!(md.extra["weird"], "kept");
    assert_eq!(md.metadata.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
    let outs = &nb.cells[1].outputs;
    assert!(matches!(&outs[0], Output::Unknown(m) if m["payload"] == serde_json::json!([1, 2])));
    assert!(matches!(&outs[1], Output::Unknown(_)), "unexpected key -> verbatim");
    assert!(matches!(&outs[2], Output::Unknown(_)), "unknown stream name -> verbatim");
    assert_eq!(outs[0].output_type(), "my_custom");
}

#[test]
fn transient_is_runtime_only() {
    let content = serde_json::json!({
        "data": {"text/plain": "x"}, "metadata": {}, "transient": {"display_id": "abc"}
    });
    let out = output_from_iopub("display_data", &content).unwrap();
    assert_eq!(out.display_id(), Some("abc"));
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    nb.cells[0].outputs.push(out);
    let s = to_string(&nb);
    assert!(!s.contains("transient"), "{s}");
}

#[test]
fn iopub_outputs() {
    use serde_json::json;
    let o = output_from_iopub("stream", &json!({"name": "stderr", "text": "warn\n"})).unwrap();
    assert_eq!(o, Output::Stream { name: StreamName::Stderr, text: "warn\n".into() });
    assert!(o.is_stream());

    let o = output_from_iopub(
        "display_data",
        &json!({"data": {"image/png": "iVBOR", "text/plain": "<Figure>", "application/json": {"a": 1}},
                "metadata": {"image/png": {"width": 10}}, "transient": {}}),
    )
    .unwrap();
    let Output::DisplayData { data, metadata, transient } = &o else { panic!("{o:?}") };
    assert_eq!(data.mime_types().collect::<Vec<_>>(), ["image/png", "text/plain", "application/json"]);
    assert_eq!(data.get("application/json"), Some(&MimeData::Json(json!({"a": 1}))));
    assert_eq!(metadata["image/png"]["width"], 10);
    assert!(transient.is_none(), "empty transient is dropped");
    assert_eq!(data.richest(&["text/html", "image/png", "text/plain"]).unwrap().0, "image/png");
    assert_eq!(data.richest(&["text/markdown"]), None);

    let o = output_from_iopub(
        "update_display_data",
        &json!({"data": {"text/plain": "2"}, "metadata": {}, "transient": {"display_id": "d1"}}),
    )
    .unwrap();
    assert_eq!(o.display_id(), Some("d1"));

    let o = output_from_iopub(
        "execute_result",
        &json!({"execution_count": 4, "data": {"text/plain": "4", "text/html": "<b>4</b>"}, "metadata": {}}),
    )
    .unwrap();
    assert_eq!(o.execution_count(), Some(4));
    assert_eq!(o.data().unwrap().text("text/html"), Some("<b>4</b>"));

    let o = output_from_iopub(
        "error",
        &json!({"ename": "NameError", "evalue": "name 'x' is not defined", "traceback": ["\u{1b}[0;31mNameError\u{1b}[0m"]}),
    )
    .unwrap();
    assert!(o.is_error());
    let Output::Error { ename, traceback, .. } = &o else { panic!() };
    assert_eq!(ename, "NameError");
    assert_eq!(traceback.len(), 1);

    for t in ["status", "execute_input", "clear_output", "comm_msg"] {
        assert!(output_from_iopub(t, &json!({"execution_state": "idle"})).is_none(), "{t}");
    }

    // Written outputs look exactly like Jupyter's.
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    nb.cells[0].id = Some("fixedid1".into());
    nb.cells[0].source = "x".into();
    nb.cells[0].execution_count = Some(4);
    nb.cells[0].outputs = vec![
        output_from_iopub("stream", &json!({"name": "stdout", "text": "a\nb\n"})).unwrap(),
        output_from_iopub("execute_result", &json!({"execution_count": 4, "data": {"text/plain": "4"}, "metadata": {}}))
            .unwrap(),
    ];
    let s = to_string(&nb);
    assert!(s.contains(
        "   \"outputs\": [\n    {\n     \"name\": \"stdout\",\n     \"output_type\": \"stream\",\n     \"text\": [\n      \"a\\n\",\n      \"b\\n\"\n     ]\n    },\n    {\n     \"data\": {\n      \"text/plain\": [\n       \"4\"\n      ]\n     },\n     \"execution_count\": 4,\n     \"metadata\": {},\n     \"output_type\": \"execute_result\"\n    }\n   ],"
    ), "{s}");
}

#[test]
fn mime_bundle_text_rules() {
    // Non-JSON mimes with line arrays are joined (like nbformat's rejoin); JSON mimes are left alone.
    let cell = r#"{"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [{"data": {
        "image/png": ["iVBO\n", "Rw=="],
        "application/json": ["a", "b"],
        "text/plain": [],
        "text/html": [1, 2]
    }, "metadata": {}, "output_type": "display_data"}], "source": ""}"#;
    let nb = parse(nb_json(cell, 4).as_bytes()).unwrap();
    let d = nb.cells[0].outputs[0].data().unwrap();
    assert_eq!(d.get("image/png"), Some(&MimeData::Text("iVBO\nRw==".into())));
    assert_eq!(d.get("application/json"), Some(&MimeData::Json(serde_json::json!(["a", "b"]))));
    assert_eq!(d.text("text/plain"), Some(""));
    assert_eq!(d.get("text/html"), Some(&MimeData::Json(serde_json::json!([1, 2]))));
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    assert!(canon.contains("\"image/png\": \"iVBO\\nRw==\""), "{canon}");
    assert!(canon.contains("\"text/plain\": []"), "{canon}");
}

#[test]
fn edited_cell_diff_is_minimal() {
    let mut nb = parse(SIMPLE.as_bytes()).unwrap();
    nb.cells[0].source = "# New title\n\nSome *text*".into();
    let out = to_string(&nb);
    let changed: Vec<_> = out.lines().zip(SIMPLE.lines()).filter(|(a, b)| a != b).collect();
    assert_eq!(changed, [("    \"# New title\\n\",", "    \"# Title\\n\",")]);
}

#[test]
fn cells_and_ids() {
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    assert_eq!(nb.kernel_name(), Some("python3"));
    let c = nb.new_cell(CellType::Markdown);
    let id = c.id.clone().unwrap();
    assert_eq!(id.len(), 8);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    nb.cells.push(c);
    assert_eq!(nb.cell_index(&id), Some(1));
    let ids: std::collections::HashSet<_> = (0..1000).map(|_| new_cell_id()).collect();
    assert_eq!(ids.len(), 1000);

    let mut old = parse(nb_json(r#"{"cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": []}"#, 4).as_bytes()).unwrap();
    old.upgrade_to_4_5();
    assert_eq!(old.nbformat_minor, 5);
    assert!(old.cells[0].id.is_some());
}

#[test]
fn errors() {
    match parse(b"{\n \"cells\": [\n  {,\n") {
        Err(Error::Json { line: 3, column: 4, .. }) => {}
        r => panic!("{r:?}"),
    }
    match parse(br#"{"nbformat": 2, "nbformat_minor": 0, "worksheets": []}"#) {
        Err(Error::UnsupportedVersion { nbformat: 2, .. }) => {}
        r => panic!("{r:?}"),
    }
    assert!(matches!(parse(b"[1, 2]"), Err(Error::Invalid(_))));
    assert!(matches!(parse(b"{\"cells\": [], \"nbformat\": 4} x"), Err(Error::Json { .. })));
    assert!(matches!(parse(b"\xff\xfe"), Err(Error::Json { .. })));
    assert!(matches!(parse(br#"{"cells": [{"cell_type": "bogus", "source": ""}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#), Err(Error::Invalid(_))));
    let deep = format!("{{\"cells\": [], \"metadata\": {{\"a\": {}{}}}, \"nbformat\": 4}}", "[".repeat(5000), "]".repeat(5000));
    assert!(parse(deep.as_bytes()).is_err());
    let e = parse(b"{").unwrap_err();
    assert!(e.to_string().contains("line 1"), "{e}");
}

#[test]
fn v3_upgrade() {
    let src = r#"{
 "metadata": {"name": "old", "signature": "x"},
 "nbformat": 3,
 "nbformat_minor": 0,
 "worksheets": [{"cells": [
   {"cell_type": "heading", "level": 2, "metadata": {}, "source": ["Old heading\n", "second line"]},
   {"cell_type": "code", "collapsed": true, "input": ["print(1)\n", "1+1"], "language": "python", "metadata": {},
    "outputs": [
      {"output_type": "stream", "stream": "stdout", "text": ["1\n"]},
      {"output_type": "pyout", "prompt_number": 3, "text": ["2"], "png": "iVBOR", "json": "{\"a\": 1}", "metadata": {}},
      {"output_type": "pyerr", "ename": "E", "evalue": "v", "traceback": ["tb"]}
    ], "prompt_number": 3}
 ], "metadata": {}}]
}"#;
    let nb = parse(src.as_bytes()).unwrap();
    assert_eq!((nb.nbformat, nb.nbformat_minor), (4, 5));
    assert_eq!(nb.format.converted_from, Some((3, 0)));
    assert!(nb.metadata.is_empty());
    assert_eq!(nb.cells[0].cell_type, CellType::Markdown);
    assert_eq!(nb.cells[0].source, "## Old heading second line");
    let code = &nb.cells[1];
    assert_eq!(code.source, "print(1)\n1+1");
    assert_eq!(code.execution_count, Some(3));
    assert_eq!(code.metadata["collapsed"], true);
    assert_eq!(code.outputs[0], Output::Stream { name: StreamName::Stdout, text: "1\n".into() });
    let d = code.outputs[1].data().unwrap();
    assert_eq!(d.text("text/plain"), Some("2"));
    assert_eq!(d.text("image/png"), Some("iVBOR"));
    assert_eq!(d.get("application/json"), Some(&MimeData::Json(serde_json::json!({"a": 1}))));
    assert!(code.outputs[2].is_error());
    assert!(nb.cells.iter().all(|c| c.id.as_ref().is_some_and(|id| id.len() == 8)));
}

#[test]
fn formatting_variants_roundtrip() {
    let base = parse(SIMPLE.as_bytes()).unwrap();
    let variants: Vec<(FormatHint, &str)> = vec![
        (FormatHint { indent: Indent::Spaces(2), ..FormatHint::jupyter() }, "indent2"),
        (FormatHint { indent: Indent::Tabs(1), ..FormatHint::jupyter() }, "tabs"),
        (FormatHint { indent: Indent::Spaces(0), ..FormatHint::jupyter() }, "indent0"),
        (FormatHint { indent: Indent::Compact, colon_space: false, trailing_newline: false, ..FormatHint::jupyter() }, "compact"),
        (FormatHint { indent: Indent::Compact, comma_space: true, ..FormatHint::jupyter() }, "compact py"),
        (FormatHint { crlf: true, ..FormatHint::jupyter() }, "crlf"),
        (FormatHint { bom: true, ..FormatHint::jupyter() }, "bom"),
        (FormatHint { ensure_ascii: true, ..FormatHint::jupyter() }, "ascii"),
        (FormatHint { comma_space: true, ..FormatHint::jupyter() }, "py2"),
        (FormatHint { split_lines: false, ..FormatHint::jupyter() }, "unsplit"),
    ];
    for (hint, name) in variants {
        let opts = WriteOptions { format: Some(hint.clone()), ..WriteOptions::preserve() };
        let s = to_string_with(&base, &opts);
        let nb = parse(s.as_bytes()).unwrap_or_else(|e| panic!("{name}: {e}\n{s}"));
        assert_eq!(nb.cells, base.cells, "{name}");
        let mut detected = nb.format.clone();
        detected.key_order = None;
        detected.numbers = None;
        if name != "ascii" {
            // ensure_ascii is only detectable when there is non-ASCII text.
            assert_eq!(detected, hint, "{name}");
        }
        assert_eq!(to_string(&nb), s, "{name} round trip");
    }
}

#[test]
fn changing_cell_type_never_duplicates_keys() {
    let src = nb_json(r#"{"cell_type": "markdown", "metadata": {}, "outputs": [], "execution_count": null, "source": "x"}"#, 4);
    let mut nb = parse(src.as_bytes()).unwrap();
    assert!(nb.cells[0].extra.contains_key("outputs"));
    nb.cells[0].cell_type = CellType::Code;
    nb.cells[0].execution_count = Some(1);
    let out = to_string(&nb);
    assert_eq!(out.matches("\"outputs\"").count(), 1, "{out}");
    assert_eq!(out.matches("\"execution_count\"").count(), 1, "{out}");
    assert!(out.contains("\"execution_count\": 1"), "{out}");
    let back = parse(out.as_bytes()).unwrap();
    assert_eq!(back.cells[0].execution_count, Some(1));
}

#[test]
fn odd_escapes_are_preserved() {
    // Escapes other writers produce that Python's json never would.
    let src = "{\n \"cells\": [],\n \"metadata\": {\n  \"a\": \"http:\\/\\/x\",\n  \"b\": \"\\u001B[0m\",\n  \"c\": \"\\u0041\\u00e9 raw é\",\n  \"d\": \"\\ud83D\\ude00\"\n },\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}\n";
    let nb = roundtrip_identical(src);
    assert_eq!(nb.metadata["a"], "http://x");
    assert_eq!(nb.metadata["b"], "\u{1b}[0m");
    assert_eq!(nb.metadata["c"], "Aé raw é");
    assert_eq!(nb.metadata["d"], "😀");
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    assert!(canon.contains("\"a\": \"http://x\""), "{canon}");
    assert!(canon.contains("\"b\": \"\\u001b[0m\""), "{canon}");
    assert!(canon.contains("\"c\": \"Aé raw é\""), "{canon}");
}

#[test]
fn duplicate_ids_repair() {
    let cell = r#"{"cell_type": "markdown", "id": "dup", "metadata": {}, "source": []}"#;
    let src = nb_json(&format!("{cell},{cell},{cell}"), 5);
    let mut nb = parse(src.as_bytes()).unwrap();
    assert_eq!(nb.repair_duplicate_cell_ids(), 2);
    assert_eq!(nb.cells[0].id.as_deref(), Some("dup"));
    let ids: std::collections::HashSet<_> = nb.cells.iter().map(|c| c.id.clone().unwrap()).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(nb.repair_duplicate_cell_ids(), 0);
}

#[test]
fn deep_nesting_on_small_stack() {
    // MAX_DEPTH nesting must not overflow a 1 MiB thread stack.
    let depth = 511;
    let src = format!(
        "{{\"cells\": [], \"metadata\": {{\"a\": {}{}}}, \"nbformat\": 4, \"nbformat_minor\": 5}}",
        "[".repeat(depth),
        "]".repeat(depth)
    );
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let nb = parse(src.as_bytes()).unwrap();
            let _ = to_string(&nb);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn types_are_send_sync_clone_debug() {
    fn check<T: Clone + std::fmt::Debug + Send + Sync + 'static>() {}
    check::<Notebook>();
    check::<Cell>();
    check::<CellType>();
    check::<Output>();
    check::<MimeBundle>();
    check::<MimeData>();
    check::<StreamName>();
    check::<FormatHint>();
    check::<WriteOptions>();
    check::<Error>();
}

#[test]
fn escaping_long_strings_simd() {
    // Exercise the 32-byte SIMD skip with escapes at every offset, both modes.
    for ascii in [false, true] {
        for at in 0..100 {
            for special in ["\"", "\\", "\n", "\u{1}", "\u{7f}", "é"] {
                let s = format!("{}{special}{}", "a".repeat(at), "b".repeat(100));
                let mut fast = String::new();
                text::write_json_str(&mut fast, &s, ascii);
                // Reference: escape char by char.
                let mut slow = String::from("\"");
                for ch in s.chars() {
                    let mut one = String::new();
                    text::write_json_str(&mut one, ch.encode_utf8(&mut [0; 4]), ascii);
                    slow.push_str(&one[1..one.len() - 1]);
                }
                slow.push('"');
                assert_eq!(fast, slow, "ascii={ascii} at={at} special={special:?}");
            }
        }
    }
}

#[test]
fn ill_typed_cell_fields_kept_verbatim() {
    let src = r#"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": 1.0,
   "id": 7,
   "metadata": null,
   "outputs": [],
   "source": "x"
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;
    let nb = roundtrip_identical(src);
    assert_eq!(nb.cells[0].source, "x");
    assert_eq!(nb.cells[0].execution_count, None);
    assert!(nb.cells[0].id.is_none());
    assert_eq!(nb.cells[0].extra["execution_count"], serde_json::json!(1.0));
    // Once the app gives the field a real value, the typed value is written.
    let mut nb = nb;
    nb.cells[0].execution_count = Some(2);
    let out = to_string(&nb);
    assert!(out.contains("\"execution_count\": 2,") && out.matches("execution_count").count() == 1, "{out}");
}

#[test]
fn unknown_outputs_canonical_like_nbformat() {
    // A stream with an extra key and a display_data with an extra key are kept
    // verbatim in memory; canonical writing normalizes them like nbformat does.
    let cell = r#"{"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [
        {"name": "stdout", "output_type": "stream", "text": "a\nb", "x": 1},
        {"data": {"text/plain": "p\nq", "image/png": ["iV", "BO"]}, "metadata": {}, "output_type": "display_data", "x": 1}
    ], "source": "s"}"#;
    let nb = parse(nb_json(cell, 5).as_bytes()).unwrap();
    assert!(nb.cells[0].outputs.iter().all(|o| matches!(o, Output::Unknown(_))));
    let canon = to_string_with(&nb, &WriteOptions::jupyter());
    // Expected bytes from nbformat 5.10 (`nbformat.writes(nbformat.reads(...))`).
    assert!(canon.contains("     \"text\": [\n      \"a\\n\",\n      \"b\"\n     ],"), "{canon}");
    assert!(canon.contains("      \"image/png\": \"iVBO\",\n      \"text/plain\": [\n       \"p\\n\",\n       \"q\"\n      ]"), "{canon}");
}
