//! Generates a synthetic notebook workspace for benchmarks.
//!
//! cargo run -p herse-workspace --release --example gen_workspace -- <out_dir> [notebooks=1000] [other_files=0] [seed=1]
//! cargo run -p herse-workspace --release --example gen_workspace -- --big <out.ipynb> [mb=50] [image|text]

#[path = "common/gen.rs"]
mod generator;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let t = std::time::Instant::now();
    if args.first().map(String::as_str) == Some("--big") {
        let path = std::path::PathBuf::from(args.get(1).expect("output path"));
        let mb = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
        let image = args.get(3).map(String::as_str) != Some("text");
        generator::big_notebook(&path, mb, image, 7);
        println!("wrote {} ({} bytes) in {:?}", path.display(), std::fs::metadata(&path).unwrap().len(), t.elapsed());
        return;
    }
    let out = std::path::PathBuf::from(args.first().expect("usage: gen_workspace <out_dir> [notebooks] [other_files] [seed]"));
    let n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let other = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let seed = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
    let g = generator::workspace(&out, n, other, seed);
    let bytes: u64 = g.notebooks.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    println!(
        "wrote {} notebooks ({:.1} MB), {} other files, {} planted needles under {} in {:?}",
        g.notebooks.len(),
        bytes as f64 / 1e6,
        other,
        g.needles.len(),
        out.display(),
        t.elapsed()
    );
}
