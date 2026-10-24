//! Garbage in, no panic out: truncations, random mutations and pathological
//! inputs, with the structural invariants checked on every result.

mod common;

use std::time::{Duration, Instant};

use herse_table::{Table, parse};

fn check(t: &Table) {
    assert!(t.index_columns <= t.columns);
    if let Some(d) = &t.dtypes {
        assert_eq!(d.len(), t.columns);
    }
    for rows in [&t.header, &t.body] {
        for (r, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), t.columns);
            for (c, cell) in row.iter().enumerate() {
                match cell.span_origin {
                    Some((or, oc)) => {
                        let o = &rows[or][oc];
                        assert!(o.span_origin.is_none());
                        assert!(r < or + o.rowspan as usize && c < oc + o.colspan as usize);
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
    let _ = t.column_widths_chars();
    for r in 0..t.body.len() {
        let _ = t.is_ellipsis_row(r);
    }
}

fn parse_checked(html: &str) -> Option<Table> {
    let t = parse(html);
    if let Some(t) = &t {
        check(t);
    }
    t
}

fn corpus() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for nb in ["dataframes.ipynb", "showcase.ipynb", "formats/kernel_r.ipynb", "formats/kernel_julia.ipynb", "formats/colab_style.ipynb"] {
        v.extend(common::notebook_tables(&common::native_root().join("fixtures").join(nb)).into_iter().map(|(_, h)| h));
    }
    for entry in std::fs::read_dir(common::data_dir()).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "html") {
            v.push(std::fs::read_to_string(p).unwrap());
        }
    }
    v
}

/// Largest char boundary <= i.
fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[test]
fn every_truncation() {
    for html in corpus() {
        // Every byte for small outputs, ~600 cut points for big ones.
        let step = (html.len() / 600).max(1);
        let mut i = 0;
        while i <= html.len() {
            let cut = floor(&html, i);
            parse_checked(&html[..cut]);
            parse_checked(&html[cut..]);
            i += step;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const DICT: &[&str] = &[
    "<table>", "</table>", "<tr>", "</tr>", "<td>", "</td>", "<th>", "</th>", "<thead>", "</thead>", "<tbody>",
    "<tfoot>", "<caption>", "</caption>", "<td colspan=\"999999\">", "<td rowspan=0>", "<th rowspan=\"65535\" colspan=1000>",
    "<td colspan=-3>", "&", "&#", "&#x", "&#xD800;", "&#99999999999;", "&amp", "&nbsp;", "&notin", "<!--", "-->", "<!",
    "<?", "<style>", "</style>", "#T_x td { color: red; background: linear-gradient(90deg, red 10%, ", "{", "}",
    "*{color:#fff}", "tr:nth-child(", "\"", "'", "<", ">", "/", "=", " ", "\n", "<br>", "<b>", "</b>", "<pre>",
    "</pre>", "<svg>", "</svg>", "<script>", "</script>", "<div style='display:none'>", "</div>", "<template>",
    "<table><tr><td><table>", "日本", "🚀", "\u{0}", "<p>3 rows × 2 columns</p>", "<small>shape: (1, 1)</small>",
    "<td class=rowNumber>", "<tr class=subheader>", "<textarea>", "<plaintext>", "<![CDATA[", "]]>",
];

#[test]
fn random_mutations() {
    let corpus = corpus();
    // `HERSE_TABLE_FUZZ=200000` for a long campaign (seed via HERSE_TABLE_SEED).
    let iters: usize = std::env::var("HERSE_TABLE_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(6000);
    let seed: u64 = std::env::var("HERSE_TABLE_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut rng = Rng(seed | 1);
    let budget = if iters > 6000 { 3600 } else { 20 };
    let deadline = Instant::now() + Duration::from_secs(budget);
    let mut n = 0;
    while n < iters && Instant::now() < deadline {
        let base = &corpus[rng.below(corpus.len())];
        let mut bytes = base.as_bytes().to_vec();
        for _ in 0..1 + rng.below(8) {
            let len = bytes.len();
            let at = rng.below(len + 1);
            match rng.below(5) {
                0 => {
                    let end = (at + rng.below(64)).min(len);
                    bytes.drain(at..end);
                }
                1 => {
                    let tok = DICT[rng.below(DICT.len())].as_bytes();
                    bytes.splice(at..at, tok.iter().copied());
                }
                2 if len > 0 => {
                    let i = rng.below(len);
                    bytes[i] = rng.next() as u8;
                }
                3 => {
                    let end = (at + rng.below(200)).min(len);
                    let chunk = bytes[at..end].to_vec();
                    bytes.splice(at..at, chunk);
                }
                _ => {
                    let end = (at + rng.below(32)).min(len);
                    bytes[at..end].reverse();
                }
            }
        }
        let html = String::from_utf8_lossy(&bytes);
        parse_checked(&html);
        n += 1;
    }
    assert!(n >= 1000, "only {n} mutations ran");
}

fn timed(name: &str, html: &str) -> Option<Table> {
    let t = Instant::now();
    let r = parse_checked(html);
    let el = t.elapsed();
    assert!(el < Duration::from_secs(5), "{name}: {el:?}");
    r
}

#[test]
fn pathological_inputs() {
    // Spans: huge colspans are capped (grid width <= 4096), rowspans clamp.
    let t = timed("colspans", &format!("<table>{}</table>", "<td colspan=1000>x".repeat(100))).unwrap();
    assert_eq!(t.columns, 4096);
    let t = timed("rowspans", &format!("<table>{}</table>", "<tr><td rowspan=65534>x".repeat(2000))).unwrap();
    assert_eq!(t.body.len(), 2000);
    assert!(t.columns >= 2000);
    // One very wide row then many rows: the grid-cell cap drops rows instead of
    // allocating rows × 4096 cells.
    let html = format!("<table><tr><td colspan=4096>w</td></tr>{}</table>", "<tr><td>1</td></tr>".repeat(100_000));
    let t = timed("wide+long", &html).unwrap();
    assert!(t.dropped_rows > 0);
    assert!(t.body.len() * t.columns <= 4_000_000);
    // Deep nesting: tables in cells, wrappers, inline tags.
    timed("nested tables", &"<table><tr><td>".repeat(20_000)).unwrap();
    timed("deep divs", &format!("{}<table><tr><td>x</td></tr></table>", "<div>".repeat(50_000))).unwrap();
    timed("deep bold", &format!("<table><tr><td>{}x</td></tr></table>", "<b>".repeat(100_000))).unwrap();
    // Selector matching must not backtrack exponentially over deep wrappers.
    let sel = format!("{} td", vec!["div"; 200].join(" "));
    let html = format!(
        "<style>{sel} {{ color: red }} div > div div > div td {{ color: blue }}</style>{}<table>{}</table>",
        "<div>".repeat(300),
        "<tr><td>1</td><td>2</td></tr>".repeat(200)
    );
    let t = timed("selector bomb", &html).unwrap();
    assert!(t.body[0][0].style.fg.is_some());
    // CSS garbage.
    timed("css braces", &format!("<style>{}</style><table><td>x</table>", "{".repeat(200_000))).unwrap();
    timed("css selectors", &format!("<style>{}</style><table><td>x</table>", "a,".repeat(100_000) + "{color:red}")).unwrap();
    timed("css rules", &format!("<style>{}</style><table><td id=r5>x</table>", (0..50_000).map(|i| format!("#r{i}{{color:#{:06x}}}", i)).collect::<String>())).unwrap();
    // Tokenizer edge cases.
    assert!(timed("unterminated comment", "<!--<table><tr><td>x").is_none());
    assert!(timed("unterminated attr", "<table><tr><td class=\"x>1</td></tr></table>").is_some());
    timed("many attrs", &format!("<table><tr><td {}>x</td></tr></table>", "a=1 ".repeat(100_000))).unwrap();
    timed("long entity", &format!("<table><td>&{};</td></table>", "a".repeat(1_000_000))).unwrap();
    timed("entities", &format!("<table><td>{}</td></table>", "&#99999999999999999999;&#x;&;&#;&amp".repeat(10_000))).unwrap();
    assert!(timed("no table, 10 MB of cells", &"<td>x".repeat(2_000_000)).is_none());
    timed("lt soup", &format!("<table><td>{}</td></table>", "<<<>>></ <! <? ".repeat(50_000))).unwrap();
    let t = timed("bom + crlf", "\u{feff}<table>\r\n<tr>\r\n<td>a\r\nb</td></tr></table>").unwrap();
    assert_eq!(t.body[0][0].text, "a b");
    // Empty structures.
    for html in ["<table>", "<table></table>", "<table><tr></tr></table>", "<table><caption>c</caption></table>", "<TABLE><TR><TD>"] {
        let t = timed(html, html).unwrap();
        assert!(t.body.len() <= 1);
    }
}
