//! Prints a timing table for herse-workspace.
//!
//! cargo run -p herse-workspace --release --example bench -- [scratch_dir]
//!
//! Generates (once) `ws1k/` (1,000 notebooks), `ws10k/` (10,000 notebooks +
//! 3,000 other files + junk dirs) and two 50 MB notebooks under the scratch
//! dir, then times walk / summarize / fuzzy / search. Also uses the real
//! notebooks under /opt/anaconda3 when present. All numbers are warm page
//! cache (a cold cache can't be forced without root).

#[path = "common/gen.rs"]
mod generator;

use herse_workspace::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Table(Vec<[String; 4]>);

impl Table {
    fn row(&mut self, op: &str, detail: impl Into<String>, result: impl Into<String>, target: &str) {
        let r = [op.to_string(), detail.into(), result.into(), target.to_string()];
        println!("  {} | {} | {} | {}", r[0], r[1], r[2], r[3]);
        self.0.push(r);
    }
    fn print(&self) {
        println!("\n| Operation | Workload | Result | Target |\n|---|---|---|---|");
        for r in &self.0 {
            println!("| {} | {} | {} | {} |", r[0], r[1], r[2], r[3]);
        }
    }
}

fn ms(d: Duration) -> String {
    let m = d.as_secs_f64() * 1e3;
    if m < 1.0 { format!("{:.0} µs", m * 1e3) } else if m < 10.0 { format!("{m:.2} ms") } else { format!("{m:.1} ms") }
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn bench<T>(iters: usize, mut f: impl FnMut() -> T) -> (Duration, Duration, Duration, T) {
    let mut out = f(); // warm-up
    let mut v = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        out = f();
        v.push(t.elapsed());
    }
    let med = median(&mut v);
    (v[0], med, *v.last().unwrap(), out)
}

fn ensure(scratch: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let ws1k = scratch.join("ws1k");
    let ws10k = scratch.join("ws10k");
    let img = scratch.join("big/image_heavy_50mb.ipynb");
    let txt = scratch.join("big/text_heavy_50mb.ipynb");
    if !ws1k.join("projects").exists() {
        eprintln!("generating {} ...", ws1k.display());
        generator::workspace(&ws1k, 1000, 0, 1);
    }
    if !ws10k.join("projects").exists() {
        eprintln!("generating {} ...", ws10k.display());
        generator::workspace(&ws10k, 10_000, 3_000, 2);
    }
    if !img.exists() {
        generator::big_notebook(&img, 50, true, 7);
    }
    if !txt.exists() {
        generator::big_notebook(&txt, 50, false, 7);
    }
    (ws1k, ws10k, img, txt)
}

fn corpus() -> Vec<PathBuf> {
    let root = Path::new("/opt/anaconda3");
    if !root.exists() {
        return Vec::new();
    }
    let opts = WalkOptions { include_hidden: true, respect_gitignore: false, ignored_dirs: vec![], max_entries: None, ..Default::default() };
    walk(root, &opts).into_iter().filter(|e| e.is_notebook()).map(|e| e.path).collect()
}

fn main() {
    let scratch = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/bench-workspace"));
    let (ws1k, ws10k, img, txt) = ensure(&scratch);
    let mut t = Table(Vec::new());
    println!("herse-workspace bench ({} threads)", std::thread::available_parallelism().map_or(0, |n| n.get()));

    // ---- walk ----
    let opts = WalkOptions::default();
    let (min, med, max, entries) = bench(20, || walk(&ws10k, &opts));
    let nbs = entries.iter().filter(|e| e.is_notebook()).count();
    t.row("walk", format!("ws10k: {} entries ({nbs} notebooks, {} dirs)", entries.len(), entries.len() - nbs), format!("min {} / median {} / max {}", ms(min), ms(med), ms(max)), "< 30 ms");
    let topts = WalkOptions { other_files: OtherFiles::All, ..Default::default() };
    let (min, med, _, all) = bench(20, || walk(&ws10k, &topts));
    t.row("walk (all files)", format!("ws10k: {} entries", all.len()), format!("min {} / median {}", ms(min), ms(med)), "");
    let popts = WalkOptions { prune_empty_dirs: true, ..Default::default() };
    let (min, med, _, _) = bench(20, || walk(&ws10k, &popts));
    t.row("walk (prune empty dirs)", "ws10k", format!("min {} / median {}", ms(min), ms(med)), "");
    let (min, med, _, e1k) = bench(20, || walk(&ws1k, &opts));
    t.row("walk", format!("ws1k: {} entries", e1k.len()), format!("min {} / median {}", ms(min), ms(med)), "");
    let corpus = corpus();
    if !corpus.is_empty() {
        let aopts = WalkOptions { other_files: OtherFiles::All, include_hidden: true, respect_gitignore: false, ignored_dirs: vec![], max_entries: None, ..Default::default() };
        let (_, med, _, a) = bench(3, || walk(Path::new("/opt/anaconda3"), &aopts));
        t.row("walk (stress)", format!("/opt/anaconda3, everything: {} entries", a.len()), format!("median {}", ms(med)), "");
        let (_, med, _, a) = bench(3, || walk(Path::new("/opt/anaconda3"), &opts));
        t.row("walk (stress)", format!("/opt/anaconda3, defaults: {} entries", a.len()), format!("median {}", ms(med)), "");
    }

    // ---- summarize ----
    for (label, path) in [("image-heavy", &img), ("text-heavy", &txt)] {
        let size = std::fs::metadata(path).unwrap().len();
        let bytes = std::fs::read(path).unwrap();
        let mb = format!("{label} {:.0} MB", size as f64 / 1048576.0);
        let (min, med, _, s) = bench(20, || summarize(path).unwrap());
        t.row("summarize (read + scan)", format!("{mb}, {} cells", s.cells), format!("min {} / median {}", ms(min), ms(med)), "a few ms");
        let (min, med, _, _) = bench(20, || summarize_bytes(&bytes).unwrap());
        t.row("  byte scanner only", &mb, format!("min {} ({:.1} GB/s) / median {}", ms(min), size as f64 / min.as_secs_f64() / 1e9, ms(med)), "");
        let (min, _, _, s2) = bench(5, || serde_impl::summarize_bytes_serde(&bytes).unwrap());
        assert_eq!(s, s2);
        t.row("  serde IgnoredAny + RawValue", &mb, format!("min {} ({:.1} GB/s)", ms(min), size as f64 / min.as_secs_f64() / 1e9), "");
        let (min, _, _, s3) = bench(3, || serde_impl::summarize_value(&serde_json::from_slice(&bytes).unwrap()));
        assert_eq!(s, s3);
        t.row("  serde_json::Value full parse", &mb, format!("min {}", ms(min)), "");
        let (min, _, _, _) = bench(20, || std::fs::read(path).unwrap().len());
        t.row("  fs::read alone (fresh Vec)", &mb, format!("min {}", ms(min)), "");
        let (min, _, _, _) = bench(20, || {
            let f = std::fs::File::open(path).unwrap();
            let m = unsafe { memmap2::Mmap::map(&f).unwrap() };
            summarize_bytes(&m).unwrap()
        });
        t.row("  mmap + scan (not used: SIGBUS risk)", &mb, format!("min {}", ms(min)), "");
    }
    for (label, paths) in [("corpus", corpus.clone()), ("ws1k", generator_paths(&ws1k)), ("ws10k", generator_paths(&ws10k))] {
        if paths.is_empty() {
            continue;
        }
        let bytes: u64 = paths.iter().map(|p| std::fs::metadata(p).map_or(0, |m| m.len())).sum();
        let errors = AtomicUsize::new(0);
        let (min, med, _, _) = bench(5, || {
            summarize_many(&paths, |_, r| {
                if r.is_err() {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            })
        });
        t.row(
            "summarize_many (parallel)",
            format!("{label}: {} notebooks, {:.0} MB", paths.len(), bytes as f64 / 1e6),
            format!("min {} / median {} total ({} / notebook)", ms(min), ms(med), ms(min / paths.len() as u32)),
            "",
        );
        let (min, _, _, _) = bench(3, || paths.iter().map(|p| summarize(p).is_ok() as usize).sum::<usize>());
        t.row("summarize (sequential, 1 thread)", label, format!("{} / notebook", ms(min / paths.len() as u32)), "");
    }

    // ---- fuzzy ----
    let files: Vec<String> = walk(&ws10k, &WalkOptions { other_files: OtherFiles::Text, ..Default::default() })
        .into_iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.rel)
        .collect();
    let (_, med, _, mut idx) = bench(5, || FuzzyIndex::new(files.clone()));
    t.row("FuzzyIndex::new", format!("{} paths", files.len()), ms(med), "");
    let recents: Vec<String> = files.iter().step_by(97).take(50).cloned().collect();
    idx.set_recents(&recents);
    let queries = ["train", "p12/exp eval", "data cleaning 42", "nb9999", "Customer churn", "zqxj"];
    let mut typed = Vec::new();
    let mut cold = Vec::new();
    let mut results = Vec::new();
    for q in queries {
        for end in 1..=q.len() {
            if !q.is_char_boundary(end) {
                continue;
            }
            let s = Instant::now();
            let hits = idx.query(&q[..end], 50);
            typed.push(s.elapsed());
            if end == q.len() {
                results.push(format!("{q:?}→{}", hits.first().map_or("-", |h| idx.path(h.index))));
            }
        }
        for end in 1..=q.len() {
            idx.query("", 1); // reset the incremental cache
            let s = Instant::now();
            idx.query(&q[..end], 50);
            cold.push(s.elapsed());
        }
    }
    let n = typed.len();
    let tmax = *typed.iter().max().unwrap();
    let cmax = *cold.iter().max().unwrap();
    t.row("FuzzyIndex::query (typing)", format!("{} paths, {n} keystrokes", files.len()), format!("median {} / max {}", ms(median(&mut typed)), ms(tmax)), "< 5 ms");
    t.row("FuzzyIndex::query (no incremental)", format!("{} paths", files.len()), format!("median {} / max {}", ms(median(&mut cold)), ms(cmax)), "< 5 ms");
    let (_, med, _, _) = bench(20, || idx.query("", 50));
    t.row("FuzzyIndex::query (empty → recents)", format!("{} paths", files.len()), ms(med), "");
    println!("  top hits: {}", results.join(", "));

    // ---- search ----
    let cancel = AtomicBool::new(false);
    let searches: Vec<(&str, SearchQuery)> = vec![
        ("rare literal", SearchQuery::new("herse_needle_3")),
        ("common literal", SearchQuery { max_hits: None, ..SearchQuery::new("train_test_split") }),
        ("case-sensitive", SearchQuery { case_sensitive: true, max_hits: None, ..SearchQuery::new("DataFrame") }),
        ("whole word", SearchQuery { whole_word: true, max_hits: None, ..SearchQuery::new("fit") }),
        ("regex", SearchQuery { regex: true, max_hits: None, ..SearchQuery::new(r"def \w+_model\(") }),
        ("include outputs", SearchQuery { include_outputs: true, max_hits: None, ..SearchQuery::new("Epoch 3:") }),
        ("default max_hits", SearchQuery::new("import")),
    ];
    for (label, q) in &searches {
        let mut firsts = Vec::new();
        let (min, med, _, st) = bench(7, || {
            let st = search(&ws1k, q, &cancel, |_| {}).unwrap();
            firsts.push(st.first_hit.unwrap_or_default());
            st
        });
        t.row(
            &format!("search ({label})"),
            format!("ws1k {:?}: {} hits in {} files", q.text, st.hits, st.files_matched),
            format!("first hit {} / total min {} median {}", ms(median(&mut firsts)), ms(min), ms(med)),
            "first < 50 ms, total < 300 ms",
        );
    }
    let st = search(&ws1k, &searches[1].1, &cancel, |_| {}).unwrap();
    println!("  ws1k scanned {} files, {:.0} MB", st.files_scanned, st.bytes_scanned as f64 / 1e6);
    let mut firsts = Vec::new();
    let q = SearchQuery::new("herse_needle_42");
    let (_, med, _, st) = bench(3, || {
        let st = search(&ws10k, &q, &cancel, |_| {}).unwrap();
        firsts.push(st.first_hit.unwrap_or_default());
        st
    });
    t.row("search (rare literal)", format!("ws10k: {} notebooks, {:.1} GB", st.files_scanned, st.bytes_scanned as f64 / 1e9), format!("first hit {} / total {}", ms(median(&mut firsts)), ms(med)), "");
    if !corpus.is_empty() {
        let q = SearchQuery { max_hits: None, ..SearchQuery::new("import") };
        let (_, med, _, st) = bench(7, || search_paths(&corpus, &q, &cancel, |_| {}).unwrap());
        t.row("search_paths", format!("corpus {} notebooks {:?}: {} hits", corpus.len(), q.text, st.hits), ms(med), "");
    }
    // Cancel latency: cancel after the first hit.
    let q = SearchQuery { max_hits: None, ..SearchQuery::new("import") };
    let mut lat = Vec::new();
    for _ in 0..5 {
        let cancel = AtomicBool::new(false);
        let at = std::sync::Mutex::new(None);
        let s = Instant::now();
        search(&ws10k, &q, &cancel, |_| {
            if !cancel.swap(true, Ordering::Relaxed) {
                *at.lock().unwrap() = Some(Instant::now());
            }
        })
        .unwrap();
        let end = Instant::now();
        lat.push(end - at.lock().unwrap().unwrap_or(s));
    }
    t.row("search cancel latency", "ws10k, cancel at first hit", format!("median {}", ms(median(&mut lat))), "");

    t.print();
}

fn generator_paths(root: &Path) -> Vec<PathBuf> {
    walk(root, &WalkOptions::default()).into_iter().filter(|e| e.is_notebook()).map(|e| e.path).collect()
}
