//! Targeted assertions on fixture tables and hand-written HTML.

mod common;

use common::{fixture, sample};
use herse_table::{Align, CellKind, Flavor, Rgba, Table, is_simple_table_output, parse};

fn rgb(hex: u32) -> Option<Rgba> {
    Some(Rgba::from_rgb_u32(hex))
}

fn texts(row: &[herse_table::Cell]) -> Vec<&str> {
    row.iter().map(|c| c.text.as_str()).collect()
}

/// Structural invariants every parse result must satisfy.
pub fn check_invariants(t: &Table) {
    assert!(t.index_columns <= t.columns);
    if let Some(d) = &t.dtypes {
        assert_eq!(d.len(), t.columns);
    }
    for rows in [&t.header, &t.body] {
        for (r, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), t.columns, "row {r} width");
            for (c, cell) in row.iter().enumerate() {
                match cell.span_origin {
                    Some((or, oc)) => {
                        assert_eq!((cell.colspan, cell.rowspan), (0, 0));
                        assert!(cell.text.is_empty());
                        let o = &rows[or][oc];
                        assert!(o.span_origin.is_none(), "origin of ({r},{c}) is itself covered");
                        assert!(or <= r && oc <= c);
                        assert!(r < or + o.rowspan as usize && c < oc + o.colspan as usize, "({r},{c}) outside span");
                    }
                    None => {
                        assert!(cell.colspan >= 1 && cell.rowspan >= 1);
                        assert!(c + cell.colspan as usize <= t.columns);
                        assert!(r + cell.rowspan as usize <= rows.len());
                    }
                }
            }
        }
    }
}

#[test]
fn wide_frame_with_truncated_columns() {
    let t = parse(&fixture("dataframes.ipynb", 3)).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.index_columns, t.body.len()), (22, 1, 8));
    assert_eq!(t.footer.as_deref(), Some("8 rows × 50 columns"));
    assert!(t.is_ellipsis_column(11));
    assert_eq!(t.header[0][11].kind, CellKind::Ellipsis);
    assert_eq!(t.body[0][11].align, Align::Center);
    assert_eq!(t.header[0][1].text, "feature_00");
    assert_eq!(t.header[0][1].align, Align::Right, "numeric column header aligns with its numbers");
    assert_eq!(t.body[0][1].text, "0.001");
    assert!(t.body[0][1].is_numeric());
    assert!(t.body[0][0].is_index && t.body[0][0].is_header);
    assert_eq!(t.flavor, Flavor::Pandas);
    assert!(t.standalone);
}

#[test]
fn long_frame_with_truncated_rows() {
    let t = parse(&fixture("dataframes.ipynb", 7)).unwrap();
    check_invariants(&t);
    assert_eq!((t.body.len(), t.columns), (61, 7));
    assert_eq!(t.footer.as_deref(), Some("1000 rows × 6 columns"));
    let ell: Vec<usize> = (0..t.body.len()).filter(|&r| t.is_ellipsis_row(r)).collect();
    assert_eq!(ell, [30]);
    assert_eq!(t.body[31][0].text, "970");
    // 60 rows exactly: nothing truncated, no footer.
    let t = parse(&fixture("dataframes.ipynb", 8)).unwrap();
    assert_eq!(t.body.len(), 60);
    assert!(t.footer.is_none());
    assert!((0..60).all(|r| !t.is_ellipsis_row(r)));
}

#[test]
fn multiindex_rows_rowspans() {
    let t = parse(&fixture("dataframes.ipynb", 11)).unwrap();
    check_invariants(&t);
    assert_eq!((t.index_columns, t.columns, t.body.len()), (3, 6, 16));
    assert_eq!(texts(&t.header[1])[..3], ["year", "quarter", "region"]);
    let year = &t.body[0][0];
    assert_eq!((year.text.as_str(), year.rowspan, year.colspan), ("2023", 8, 1));
    assert_eq!(year.align, Align::Left, "index labels are left-aligned even when numeric");
    assert_eq!(t.body[1][0].span_origin, Some((0, 0)));
    assert!(t.body[1][0].is_index && t.body[1][0].is_header);
    assert_eq!((t.body[0][1].text.as_str(), t.body[0][1].rowspan), ("Q1", 2));
    assert_eq!(t.body[3][1].span_origin, Some((2, 1)));
    assert_eq!(t.body[8][0].text, "2024");
    assert_eq!(t.body[15][0].span_origin, Some((8, 0)));
    assert_eq!(t.body[0][3].text, "2997");
}

#[test]
fn multiindex_columns_colspans() {
    let t = parse(&fixture("dataframes.ipynb", 12)).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.index_columns, t.header.len()), (9, 1, 3));
    let h = &t.header[0];
    assert_eq!((h[1].text.as_str(), h[1].colspan), ("2023", 4));
    assert_eq!(h[1].align, Align::Left, "pandas halign=\"left\"");
    assert!((2..5).all(|c| h[c].span_origin == Some((0, 1))));
    assert_eq!((h[5].text.as_str(), h[5].colspan), ("2024", 4));
    assert_eq!(texts(&t.header[1]), ["quarter", "Q1", "Q2", "Q3", "Q4", "Q1", "Q2", "Q3", "Q4"]);
    assert_eq!(t.header[2][0].text, "region");

    let t = parse(&fixture("dataframes.ipynb", 13)).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.index_columns), (7, 2));
    assert_eq!((t.header[0][2].text.as_str(), t.header[0][2].colspan), ("revenue", 2));
    assert_eq!((t.header[0][6].text.as_str(), t.header[0][6].colspan), ("margin", 1));
    assert_eq!(t.body[0][0].rowspan, 2);
    assert_eq!(t.body[1][0].span_origin, Some((0, 0)));
}

#[test]
fn missing_values_and_types() {
    let t = parse(&fixture("dataframes.ipynb", 16)).unwrap();
    let r1 = &t.body[1];
    assert_eq!(texts(r1), ["1", "NaN", "<NA>", "None", "<NA>", "<NA>", "NaT", "NaT", "NaN"]);
    assert!(r1[1..].iter().all(|c| c.is_null()));
    assert_eq!(r1[1].align, Align::Right, "null in a numeric column");
    assert_eq!(r1[3].align, Align::Left, "null in a text column follows the column");
    assert_eq!(t.body[3][1].text, "inf");
    assert!(t.body[3][1].is_numeric() && t.body[4][1].is_numeric());
    assert_eq!(t.body[0][5].kind, CellKind::Bool);
    assert_eq!(t.body[0][6].kind, CellKind::Text, "dates are text");
}

#[test]
fn styler_gradient_colors() {
    let t = parse(&fixture("showcase.ipynb", 22)).unwrap();
    check_invariants(&t);
    assert_eq!(t.flavor, Flavor::PandasStyler);
    assert_eq!(t.caption.as_deref(), Some("Styled DataFrame (background_gradient + highlight_max)"));
    assert_eq!(t.header[0][0].text, "", "&nbsp; corner is empty");
    let score = |r: usize| &t.body[r][3].style;
    assert_eq!((score(0).bg, score(0).fg), (rgb(0x6ece58), rgb(0x000000)));
    assert_eq!((score(1).bg, score(1).fg), (rgb(0x33638d), rgb(0xf1f1f1)));
    // highlight_max adds `background-color: gold` after the gradient: last wins.
    assert_eq!(score(3).bg, rgb(0xffd700));
    // A selector list shared by two cells.
    assert_eq!(t.body[1][5].style.bg, rgb(0x440154));
    assert_eq!(t.body[4][3].style.bg, rgb(0x440154));
    assert_eq!(t.body[0][1].style.bg, None, "unstyled column");
    assert!(t.standalone);

    let t = parse(&fixture("dataframes.ipynb", 24)).unwrap();
    assert_eq!((t.body[0][1].style.bg, t.body[0][1].style.fg), (rgb(0xdb382b), rgb(0xf1f1f1)));
    assert_eq!(t.caption.as_deref(), Some("background_gradient (RdYlGn)"));
}

#[test]
fn styler_table_styles_and_highlights() {
    let t = parse(&fixture("dataframes.ipynb", 25)).unwrap();
    // set_table_styles: `#T_x th { background-color: #1e293b; color: white; text-align: center }`
    for c in &t.header[0] {
        assert_eq!((c.style.bg, c.style.fg, c.align), (rgb(0x1e293b), rgb(0xffffff), Align::Center));
    }
    assert_eq!(t.body[0][0].style.bg, rgb(0x1e293b), "row headings are <th> too");
    assert_eq!(t.body[0][1].style.bg, rgb(0xfecaca)); // highlight_min
    assert_eq!(t.body[0][4].style.bg, rgb(0xa7f3d0)); // highlight_max
    let nan = &t.body[2][4];
    assert!(nan.is_null());
    assert_eq!(nan.style.bg, rgb(0xfde68a)); // highlight_null
}

#[test]
fn styler_bars_and_fonts() {
    let t = parse(&fixture("dataframes.ipynb", 26)).unwrap();
    assert_eq!(t.index_columns, 0, "hidden index");
    let bar = t.body[0][1].style.bar.unwrap();
    assert_eq!((bar.start, bar.end, bar.color), (0.0, 0.12, Rgba::from_rgb_u32(0x60a5fa)));
    let mid = t.body[1][2].style.bar.unwrap();
    assert!((mid.start - 0.2).abs() < 1e-6 && (mid.end - 0.533).abs() < 1e-6);
    let neg = &t.body[0][2].style;
    assert_eq!((neg.fg, neg.bold), (rgb(0xdc143c), true)); // color: crimson; font-weight: bold
    assert!(t.body[2][2].style.bar.is_none());

    let t = parse(&sample("styler_props")).unwrap();
    let c = &t.body[0][1].style;
    assert_eq!(c.fg, Some(Rgba { r: 255, g: 0, b: 0, a: 128 }));
    assert!(c.bold);
    let c = &t.body[1][2].style;
    assert_eq!((c.bg, c.italic), (rgb(0xadd8e6), true)); // background: lightblue; font-style: italic
}

#[test]
fn styler_multiindex_and_truncation() {
    let t = parse(&sample("styler_multiindex")).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.index_columns), (6, 2));
    assert_eq!((t.header[0][2].text.as_str(), t.header[0][2].colspan), ("x", 2));
    assert_eq!(t.header[0][2].align, Align::Center, "spanning header without halign");
    assert_eq!(t.body[0][0].rowspan, 2);
    assert_eq!(t.body[0][2].style.bg, rgb(0x08519c));

    // The trim row lists every hidden column; it must not widen the grid.
    let t = parse(&sample("styler_truncated")).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.body.len()), (10, 21));
    assert!(t.is_ellipsis_row(20));
    assert!(t.is_ellipsis_column(9));
}

#[test]
fn polars_frames() {
    let t = parse(&sample("polars_basic")).unwrap();
    check_invariants(&t);
    assert_eq!(t.flavor, Flavor::Polars);
    assert_eq!(t.footer.as_deref(), Some("shape: (5, 4)"));
    assert_eq!(t.dtypes.as_deref(), Some(&["i64", "str", "f64", "bool"].map(String::from)[..]));
    assert_eq!((t.header.len(), t.body.len(), t.index_columns), (1, 5, 0));
    assert_eq!(texts(&t.body[0]), ["1", "\"x\"", "1.5", "true"]);
    assert!(t.body[1][2].is_null() && t.body[3][0].is_null());
    assert_eq!(t.body[3][0].align, Align::Right);
    assert_eq!(t.body[0][3].kind, CellKind::Bool);
    assert!(t.standalone);

    let t = parse(&sample("polars_trunc")).unwrap();
    assert_eq!(t.footer.as_deref(), Some("shape: (100, 30)"));
    assert!(t.is_ellipsis_row(5));
    assert_eq!(t.body[5][0].text, "…");
    assert_eq!(t.dtypes.as_ref().unwrap().len(), 30);

    let t = parse(&sample("polars_dates")).unwrap();
    assert_eq!(t.dtypes.as_ref().unwrap()[1], "datetime[μs]");
    assert_eq!(t.body[0][2].text, "\"a<b\"");
    assert_eq!(t.body[1][2].text, "\"&amp;\"");
}

#[test]
fn other_kernels() {
    let path = common::native_root().join("fixtures/formats/kernel_r.ipynb");
    let (_, html) = common::notebook_tables(&path).remove(0);
    let t = parse(&html).unwrap();
    assert_eq!(t.flavor, Flavor::R);
    assert_eq!(t.caption.as_deref(), Some("A data.frame: 3 x 4"));
    assert_eq!(t.dtypes.as_deref().unwrap()[1], "<dbl>");
    assert_eq!((t.index_columns, t.header.len()), (1, 1));
    assert_eq!(t.body[2][0].text, "Datsun 710");
    assert_eq!(t.body[2][4].text, "93", "leading space trimmed");

    let path = common::native_root().join("fixtures/formats/kernel_julia.ipynb");
    let (_, html) = common::notebook_tables(&path).remove(0);
    let t = parse(&html).unwrap();
    assert_eq!(t.flavor, Flavor::Julia);
    assert_eq!(t.footer.as_deref(), Some("3×2 DataFrame"));
    assert_eq!(t.dtypes.as_deref().unwrap()[1..], ["Int64", "String"]);
    assert_eq!(t.index_columns, 1, "row numbers");
    assert!(t.standalone);

    // DataFrames.jl truncation: `⋮` rows and italic `missing`.
    let html = r#"<div><div style = "float: left;"><span>100×2 DataFrame</span></div><div style = "float: right;"><span style = "font-style: italic;">95 rows omitted</span></div><div style = "clear: both;"></div></div><div class = "data-frame" style = "overflow-x: scroll;"><table class = "data-frame" style = "margin-bottom: 6px;"><thead><tr class = "header"><th class = "rowNumber" style = "font-weight: bold; text-align: right;">Row</th><th style = "text-align: left;">a</th><th style = "text-align: left;">b</th></tr><tr class = "subheader headerLastRow"><th class = "rowNumber" style = "font-weight: bold; text-align: right;"></th><th title = "Int64" style = "text-align: left;">Int64</th><th title = "Union{Missing, String}" style = "text-align: left;">String?</th></tr></thead><tbody><tr><td class = "rowNumber" style = "font-weight: bold; text-align: right;">1</td><td style = "text-align: right;">1</td><td style = "font-style: italic; text-align: left;">missing</td></tr><tr><td class = "rowNumber" style = "font-weight: bold; text-align: right;">&vellip;</td><td style = "text-align: right;">&vellip;</td><td style = "text-align: right;">&vellip;</td></tr></tbody></table></div>"#;
    let t = parse(html).unwrap();
    check_invariants(&t);
    assert_eq!(t.footer.as_deref(), Some("100×2 DataFrame · 95 rows omitted"));
    assert_eq!(t.dtypes.as_deref().unwrap()[2], "String?");
    assert!(t.body[0][2].is_null());
    assert!(t.is_ellipsis_row(1));
    assert!(t.standalone);
}

#[test]
fn entities_and_whitespace() {
    let t = parse(&fixture("dataframes.ipynb", 38)).unwrap();
    let col: Vec<&str> = t.body.iter().map(|r| r[1].text.as_str()).collect();
    assert_eq!(
        col,
        ["<b>bold?</b>", "<script>alert('xss')</script>", "&amp; &lt; &gt;", "a < b > c & d", "<img src=x onerror=alert(1)>"]
    );
    assert_eq!(t.body[0][3].text, "日本語");
    assert_eq!(t.body[4][2].text, "leading and trailing spaces");
    assert!(t.standalone, "escaped markup is just text");

    let html = "<table><tr><th>a&nbsp;b</th><th>x&#x2026;&#8230;&hellip;</th><th>&lt;&gt;&amp&copy;</th></tr>\
                <tr><td>\n  line   one<br>line two <BR/> three </td><td>&#215;&times;</td><td><p>p1</p><p>p2</p></td></tr></table>";
    let t = parse(html).unwrap();
    assert_eq!(texts(&t.header[0]), ["a b", "x………", "<>&©"]);
    assert_eq!(texts(&t.body[0]), ["line one\nline two\nthree", "××", "p1\np2"]);
}

#[test]
fn generic_tables() {
    // No <thead>: the leading all-<th> row is the header; <tfoot> goes last.
    let html = "<table border=1><tfoot><tr><td>total</td><td>3</td></tr></tfoot>\
                <tr><th>name</th><th>n</th></tr><tr><td>a</td><td>1</td></tr><tr><td>b</td><td>2</td></tr></table>";
    let t = parse(html).unwrap();
    check_invariants(&t);
    assert_eq!(t.flavor, Flavor::Generic);
    assert_eq!(texts(&t.header[0]), ["name", "n"]);
    assert_eq!(t.body.iter().map(|r| r[0].text.as_str()).collect::<Vec<_>>(), ["a", "b", "total"]);
    assert_eq!(t.index_columns, 0);
    assert_eq!(t.header[0][1].align, Align::Right);

    // Ragged rows are padded; rowspan="0" spans to the end of the group; spans
    // clamp to the grid.
    let html = "<table><tr><td rowspan=0>x</td><td colspan=3>wide</td></tr><tr><td>1</td></tr><tr><td>2</td><td>3</td><td>4</td><td>5</td></tr></table>";
    let t = parse(html).unwrap();
    check_invariants(&t);
    assert_eq!((t.columns, t.body.len()), (5, 3));
    assert_eq!(t.body[0][0].rowspan, 3);
    assert_eq!(t.body[2][0].span_origin, Some((0, 0)));
    assert_eq!(t.body[1][1].text, "1");
    assert_eq!(t.body[0][4].kind, CellKind::Empty);

    // Hidden rows are skipped; bgcolor and zebra CSS resolve; <b> makes bold.
    let html = "<style>tr:nth-child(even) td { background-color: #eeeeee } table td.hi { color: red }</style>\
                <table bgcolor=white><tr><td>1</td></tr><tr style='display:none'><td>hidden</td></tr>\
                <tr><td class=hi>2</td></tr><tr><td><b>3</b></td></tr><tr><td bgcolor='#ff0000'><b>4</b> x</td></tr></table>";
    let t = parse(html).unwrap();
    assert_eq!(t.body.len(), 4);
    assert_eq!(t.body[0][0].style.bg, rgb(0xffffff));
    assert_eq!((t.body[1][0].style.bg, t.body[1][0].style.fg), (rgb(0xffffff), rgb(0xff0000)));
    assert_eq!(t.body[2][0].style.bg, rgb(0xeeeeee), "4th <tr> (3rd visible) is even");
    assert!(t.body[2][0].style.bold);
    assert!(!t.body[3][0].style.bold, "partly bold text is not a bold cell");
    assert_eq!(t.body[3][0].style.bg, rgb(0xff0000));

    // A table nested in a cell is flattened to text; the output is not simple.
    let t = parse("<table><tr><td>outer<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table></td><td>z</td></tr></table>").unwrap();
    assert_eq!(texts(&t.body[0]), ["outer\na b\nc", "z"]);
    assert!(!t.standalone);
}

#[test]
fn standalone_detection() {
    let df = "<table class=\"dataframe\"><thead><tr><th></th><th>a</th></tr></thead><tbody><tr><th>0</th><td>1</td></tr></tbody></table>";
    assert!(is_simple_table_output(df));
    assert!(is_simple_table_output(&format!("<div><style scoped>.dataframe td {{}}</style>{df}<p>1 rows × 1 columns</p></div>")));
    assert!(is_simple_table_output(&format!("<!DOCTYPE html>\n<html><head><meta charset='utf-8'><title>t</title></head><body>{df}</body></html>")));
    assert!(is_simple_table_output(&format!("<!-- comment --><div class='wrap'>{df}</div>")));
    assert!(!is_simple_table_output(&format!("<h3>Results</h3>{df}")));
    assert!(!is_simple_table_output(&format!("{df}<p>Note: values are rounded</p>")));
    assert!(!is_simple_table_output(&format!("{df}{df}")));
    assert!(!is_simple_table_output(&format!("{df}<script>render()</script>")));
    assert!(!is_simple_table_output(&format!("{df}<img src='x.png'>")));
    assert!(!is_simple_table_output("<table><tr><td><img src='a.png'></td></tr></table>"));
    assert!(!is_simple_table_output("<table><tr><td><svg><text>1</text></svg></td></tr></table>"));
    assert!(!is_simple_table_output("<p>no table here</p>"));
    assert!(!is_simple_table_output(""));
    assert!(parse("<p>no table</p>").is_none());
    assert!(parse("<script>var s = '<table>';</script>").is_none(), "<table> inside raw text");
    assert!(parse("<!-- <table><tr><td>x</td></tr></table> -->").is_none());
    // Hidden or inert content does not count.
    assert!(is_simple_table_output(&format!("<div style='display:none'>junk</div><template><p>t</p></template>{df}")));
    // Google Colab's interactive wrapper (buttons + script) is inert outside Colab.
    let path = common::native_root().join("fixtures/formats/colab_style.ipynb");
    let (_, colab) = common::notebook_tables(&path).remove(0);
    assert!(colab.contains("<script>") && colab.contains("<button"));
    let t = parse(&colab).unwrap();
    assert!(t.standalone);
    assert_eq!(texts(&t.header[0]), ["", "a", "b"]);
}

#[test]
fn every_fixture_dataframe_is_standalone() {
    for nb in ["dataframes.ipynb", "showcase.ipynb"] {
        for (label, html) in common::notebook_tables(&common::native_root().join("fixtures").join(nb)) {
            let t = parse(&html).unwrap_or_else(|| panic!("{nb} {label}"));
            check_invariants(&t);
            assert!(t.standalone, "{nb} {label}");
        }
    }
    // Plot / widget outputs never claim to be tables.
    let nb: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(common::native_root().join("fixtures/plots.ipynb")).unwrap(),
    )
    .unwrap();
    let mut htmls = 0;
    for cell in nb["cells"].as_array().unwrap() {
        for o in cell["outputs"].as_array().into_iter().flatten() {
            let html: String = match &o["data"]["text/html"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                _ => continue,
            };
            htmls += 1;
            assert!(!is_simple_table_output(&html));
        }
    }
    assert!(htmls >= 10);
}

#[test]
fn column_widths() {
    let t = parse(&fixture("dataframes.ipynb", 38)).unwrap();
    let w = t.column_widths_chars();
    assert_eq!(w[3], 8, "unicode column: 🚀 emoji is 8 wide, 日本語 is 6");
    assert_eq!(w[2], 40, "capped");
    assert_eq!(t.column_widths_chars_capped(100)[2], 49);
    // A spanning header wider than its columns widens them evenly.
    let t = parse("<table><thead><tr><th colspan=2>a long group name</th></tr><tr><th>x</th><th>y</th></tr></thead><tr><td>1</td><td>2</td></tr></table>").unwrap();
    let w = t.column_widths_chars();
    assert_eq!(w, [8, 8]);
    assert!(w[0] + w[1] + 1 >= 17);
}

#[test]
fn colors_api() {
    let c = Rgba { r: 0x12, g: 0x34, b: 0x56, a: 0x78 };
    assert_eq!(c.to_u32(), 0x12345678);
    assert_eq!(Rgba::from_rgb_u32(0xabcdef).to_u32(), 0xabcdefff);
    assert!(Rgba::from_rgb_u32(0xffffff).luminance() > 0.99);
    assert!(Rgba::from_rgb_u32(0x440154).luminance() < 0.1);
}
