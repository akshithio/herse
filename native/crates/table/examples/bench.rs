//! Parse time of DataFrame HTML: synthetic pandas / Styler / polars tables
//! and the real fixture outputs.
//!
//! `cargo run -p herse-table --release --example bench`
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// What `DataFrame._repr_html_` writes (index + float/int/str/bool columns).
fn pandas_html(rows: usize, cols: usize) -> String {
    let mut s = String::from(
        "<div>\n<style scoped>\n    .dataframe tbody tr th:only-of-type {\n        vertical-align: middle;\n    }\n\n    \
         .dataframe tbody tr th {\n        vertical-align: top;\n    }\n\n    .dataframe thead th {\n        \
         text-align: right;\n    }\n</style>\n<table border=\"1\" class=\"dataframe\">\n  <thead>\n    \
         <tr style=\"text-align: right;\">\n      <th></th>\n",
    );
    for c in 0..cols {
        let _ = writeln!(s, "      <th>col_{c:02}</th>");
    }
    s.push_str("    </tr>\n  </thead>\n  <tbody>\n");
    for r in 0..rows {
        let _ = write!(s, "    <tr>\n      <th>{r}</th>\n");
        for c in 0..cols {
            let v = (r * 7919 + c * 104_729) % 100_000;
            match c % 4 {
                0 => {
                    let _ = writeln!(s, "      <td>{:.4}</td>", v as f64 / 1e4 - 5.0);
                }
                1 => {
                    let _ = writeln!(s, "      <td>{v}</td>");
                }
                2 => {
                    let _ = writeln!(s, "      <td>name_{}</td>", v % 97);
                }
                _ => s.push_str(if v % 3 == 0 { "      <td>NaN</td>\n" } else { "      <td>True</td>\n" }),
            }
        }
        s.push_str("    </tr>\n");
    }
    let _ = write!(s, "  </tbody>\n</table>\n<p>{rows} rows × {cols} columns</p>\n</div>");
    s
}

/// `df.style.background_gradient()`: one `#T_..._rowR_colC` rule per cell.
fn styler_html(rows: usize, cols: usize) -> String {
    let mut s = String::from("<style type=\"text/css\">\n");
    for r in 0..rows {
        for c in 0..cols {
            let v = ((r * 31 + c * 17) % 256) as u8;
            let _ = write!(
                s,
                "#T_bench_row{r}_col{c} {{\n  background-color: #{v:02x}{:02x}{:02x};\n  color: #f1f1f1;\n}}\n",
                255 - v,
                v / 2
            );
        }
    }
    s.push_str("</style>\n<table id=\"T_bench\">\n  <thead>\n    <tr>\n      <th class=\"blank level0\" >&nbsp;</th>\n");
    for c in 0..cols {
        let _ =
            writeln!(s, "      <th id=\"T_bench_level0_col{c}\" class=\"col_heading level0 col{c}\" >c{c}</th>");
    }
    s.push_str("    </tr>\n  </thead>\n  <tbody>\n");
    for r in 0..rows {
        let _ = write!(
            s,
            "    <tr>\n      <th id=\"T_bench_level0_row{r}\" class=\"row_heading level0 row{r}\" >{r}</th>\n"
        );
        for c in 0..cols {
            let _ = writeln!(
                s,
                "      <td id=\"T_bench_row{r}_col{c}\" class=\"data row{r} col{c}\" >{:.3}</td>",
                ((r * 13 + c) % 1000) as f64 / 1000.0
            );
        }
        s.push_str("    </tr>\n");
    }
    s.push_str("  </tbody>\n</table>\n");
    s
}

/// polars `_repr_html_`: compact, dtype row, quoted strings.
fn polars_html(rows: usize, cols: usize) -> String {
    let mut s = format!(
        "<div><style>\n.dataframe > thead > tr,\n.dataframe > tbody > tr {{\n  text-align: right;\n  white-space: pre-wrap;\n}}\n</style>\n<small>shape: ({rows}, {cols})</small><table border=\"1\" class=\"dataframe\"><thead><tr>"
    );
    for c in 0..cols {
        let _ = write!(s, "<th>c{c}</th>");
    }
    s.push_str("</tr><tr>");
    for _ in 0..cols {
        s.push_str("<td>f64</td>");
    }
    s.push_str("</tr></thead><tbody>");
    for r in 0..rows {
        s.push_str("<tr>");
        for c in 0..cols {
            let _ = write!(s, "<td>{:.3}</td>", ((r * 13 + c) % 1000) as f64 / 7.0);
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table></div>");
    s
}

fn fixture_tables(path: &str) -> Vec<(String, String)> {
    let Ok(src) = std::fs::read_to_string(path) else { return vec![] };
    let Ok(nb) = serde_json::from_str::<serde_json::Value>(&src) else { return vec![] };
    let mut out = Vec::new();
    for (i, cell) in nb["cells"].as_array().into_iter().flatten().enumerate() {
        for o in cell["outputs"].as_array().into_iter().flatten() {
            let html: String = match &o["data"]["text/html"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                _ => continue,
            };
            if html.contains("<table") {
                out.push((format!("cell {i}"), html));
            }
        }
    }
    out
}

/// CPU time of the calling thread: stable even when the machine is busy.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer, supported clock id on macOS and Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn bench(name: &str, html: &str) {
    let f = || black_box(herse_table::parse(black_box(html)));
    let t = f().expect("table");
    let iters = (2_000_000 / html.len().max(1)).clamp(3, 200);
    let mut best = Duration::MAX;
    let mut best_wall = Duration::MAX;
    for _ in 0..iters {
        let w = Instant::now();
        let c = thread_cpu();
        f();
        best = best.min(thread_cpu() - c);
        best_wall = best_wall.min(w.elapsed());
    }
    println!(
        "{name:<40} {:>5}x{:<3} {:>8.1} KB {:>8.3} ms cpu {:>8.3} ms wall {:>6.0} MB/s",
        t.body.len(),
        t.columns,
        html.len() as f64 / 1e3,
        best.as_secs_f64() * 1e3,
        best_wall.as_secs_f64() * 1e3,
        html.len() as f64 / 1e6 / best.as_secs_f64()
    );
}

fn main() {
    bench("pandas 60x50", &pandas_html(60, 50));
    bench("pandas 1000x20", &pandas_html(1000, 20));
    bench("pandas 10000x20", &pandas_html(10_000, 20));
    bench("styler gradient 60x50 (3000 rules)", &styler_html(60, 50));
    bench("styler gradient 1000x20 (20k rules)", &styler_html(1000, 20));
    bench("polars 1000x20", &polars_html(1000, 20));
    let no_table = format!("<div>{}</div>", "x".repeat(4 << 20));
    let w = Instant::now();
    let c = thread_cpu();
    for _ in 0..10 {
        black_box(herse_table::parse(black_box(&no_table)));
    }
    println!("{:<40} {:>8.3} ms cpu (4 MB, no <table>)", "parse() on non-table HTML", (thread_cpu() - c).as_secs_f64() * 1e2);
    let _ = w;
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/");
    let mut all = fixture_tables(&format!("{root}dataframes.ipynb"));
    all.extend(fixture_tables(&format!("{root}showcase.ipynb")));
    let mut total = Duration::ZERO;
    let mut bytes = 0;
    for (label, html) in &all {
        bytes += html.len();
        let mut best = Duration::MAX;
        for _ in 0..20 {
            let c = thread_cpu();
            black_box(herse_table::parse(black_box(html)));
            best = best.min(thread_cpu() - c);
        }
        total += best;
        if html.len() > 20_000 {
            bench(&format!("fixture dataframes {label}"), html);
        }
    }
    println!(
        "all {} fixture tables: {:.3} ms cpu total, {:.1} KB",
        all.len(),
        total.as_secs_f64() * 1e3,
        bytes as f64 / 1e3
    );
}
