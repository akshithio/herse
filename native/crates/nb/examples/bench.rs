//! Parse / serialize timings for notebooks.
//!
//! ```text
//! python3 -I crates/nb/tests/gen_bench_notebooks.py /tmp/nbbench
//! cargo run -p herse-nb --release --example bench -- /tmp/nbbench/*.ipynb
//! ```
//!
//! Prints min and median wall time over N runs (min is the useful number on
//! a busy machine). Cross-library comparisons (serde_json / sonic-rs /
//! simd-json derive parsers) were run out of tree to keep those crates out of
//! this crate's dev-dependencies; see the crate docs for the results.

use std::time::{Duration, Instant};

use herse_nb::{WriteOptions, parse, to_string, to_string_with};

fn time<T>(label: &str, iters: usize, mut f: impl FnMut() -> T) {
    for _ in 0..2 {
        std::hint::black_box(f());
    }
    let mut times: Vec<Duration> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            let r = f();
            let d = t.elapsed();
            drop(std::hint::black_box(r));
            d
        })
        .collect();
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!("  {label:<28} min {:>8.2} ms   median {:>8.2} ms", ms(times[0]), ms(times[iters / 2]));
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: bench NOTEBOOK.ipynb...");
        std::process::exit(2);
    }
    for path in paths {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let nb = parse(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
        let iters = if bytes.len() > 10_000_000 { 20 } else { 60 };
        let outputs: usize = nb.cells.iter().map(|c| c.outputs.len()).sum();
        let same = to_string(&nb).as_bytes() == bytes.as_slice();
        println!(
            "{path}: {:.1} MB, {} cells, {outputs} outputs, round-trip {}",
            bytes.len() as f64 / 1e6,
            nb.cells.len(),
            if same { "identical" } else { "DIFFERS" }
        );
        time("parse", iters, || parse(&bytes).unwrap());
        time("to_string", iters, || to_string(&nb));
        time("to_string_with(jupyter)", iters, || to_string_with(&nb, &WriteOptions::jupyter()));
        time("clone", iters, || nb.clone());
    }
}
