//! End-to-end Mermaid rendering: every sample in tests/data/mermaid renders
//! to valid SVG (or is declined, for `unsupported_*`), diagram types we don't
//! draw are declined, and no input — truncated, mutated or absurd — makes the
//! renderer panic or take long.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use herse_plot::{RenderOptions, Rendered, render_mermaid};

fn samples() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mermaid");
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mmd"))
        .map(|e| (e.path().file_stem().unwrap().to_string_lossy().to_string(), std::fs::read_to_string(e.path()).unwrap()))
        .collect();
    out.sort();
    out
}

fn parse_svg(r: &Rendered) {
    let opt = resvg::usvg::Options::default();
    if let Err(e) = resvg::usvg::Tree::from_str(&r.svg, &opt) {
        panic!("invalid SVG: {e}\n{}", &r.svg[..r.svg.len().min(500)]);
    }
    assert!(r.width > 0. && r.height > 0.);
}

fn render(src: &str) -> Option<Rendered> {
    render_mermaid(src, &RenderOptions::default())
}

#[test]
fn every_sample_renders_valid_svg() {
    let all = samples();
    assert!(all.len() >= 15);
    for dark in [false, true] {
        for width in [640., 300.] {
            let opts = RenderOptions { dark, width, ..Default::default() };
            for (name, src) in &all {
                let r = render_mermaid(src, &opts);
                if name.starts_with("unsupported_") {
                    assert!(r.is_none(), "{name} should be declined");
                    continue;
                }
                let r = r.unwrap_or_else(|| panic!("{name} returned None"));
                parse_svg(&r);
                assert!(r.width <= width + 0.5, "{name} too wide: {}", r.width);
                // Transparent canvas: nothing paints the whole background.
                assert!(!r.svg.contains("<rect x=\"0\" y=\"0\""), "{name}: background rect");
                if dark {
                    assert!(!r.svg.contains("fill=\"#1c1a17\""), "{name}: light-mode ink in dark mode");
                }
            }
        }
    }
}

#[test]
fn natural_size_scaled_to_fit() {
    let src = "flowchart LR\nA[One] --> B[Two] --> C[Three] --> D[Four] --> E[Five] --> F[Six] --> G[Seven]";
    let wide = render_mermaid(src, &RenderOptions { width: 4000., ..Default::default() }).unwrap();
    let narrow = render_mermaid(src, &RenderOptions { width: 300., ..Default::default() }).unwrap();
    assert!(wide.width > 600. && wide.width < 4000., "natural width {}", wide.width);
    assert_eq!(narrow.width, 300.);
    assert!((narrow.height / narrow.width - wide.height / wide.width).abs() < 0.01, "aspect ratio kept");
    // Same drawing, same user space.
    let view = |svg: &str| svg[svg.find("viewBox").unwrap()..].split('"').nth(1).unwrap().to_string();
    assert_eq!(view(&wide.svg), view(&narrow.svg));
    // `height` is ignored.
    let tall = render_mermaid(src, &RenderOptions { width: 4000., height: Some(2000.), ..Default::default() }).unwrap();
    assert_eq!(tall.height, wide.height);
}

#[test]
fn content_reaches_the_svg() {
    let text = |src: &str| render(src).unwrap_or_else(|| panic!("None for {src}")).svg;
    let svg = text("---\ntitle: My \"pipeline\"\n---\nflowchart TD\n  A[\"Start & go\"] -->|yes| B(End)");
    assert!(svg.contains("My &quot;pipeline&quot;"));
    assert!(svg.contains("Start &amp; go") && svg.contains(">yes<") && svg.contains(">End<"));
    let svg = text("sequenceDiagram\n  participant A as Alice\n  A->>B: Hi there");
    assert_eq!(svg.matches(">Alice<").count(), 2, "participants at top and bottom");
    assert!(svg.contains(">Hi there<"));
    let svg = text("pie title Pets\n  \"Dogs\" : 3\n  \"Cats\" : 1");
    assert!(svg.contains(">75%<") && svg.contains(">25%<") && svg.contains(">Pets<"));
    let svg = text("stateDiagram-v2\n  [*] --> Still\n  Still --> Moving : push\n  Moving --> [*]");
    assert!(svg.contains(">push<") && svg.contains("<circle"));
    let svg = text("classDiagram\n  class Duck {\n    +swim()\n  }\n  Animal <|-- Duck\n  Customer \"1\" --> \"*\" Order");
    assert!(svg.contains(">+swim()<") && svg.contains(">Animal<") && svg.contains(">*<"));
    let svg = text("erDiagram\n  CUSTOMER ||--o{ ORDER : places\n  CUSTOMER {\n    string id PK\n  }");
    assert!(svg.contains(">places<") && svg.contains(">PK<") && svg.contains(">CUSTOMER<"));
    let r = render("stateDiagram-v2\n  A --> B\n  note right of A : hello").unwrap();
    assert_eq!(r.notes, vec!["notes not shown".to_string()]);
}

#[test]
fn declined_inputs() {
    let declined = [
        "",
        "graph",
        "sequenceDiagram",
        "   \n\n",
        "%% just a comment",
        "classDiagram\n  class A {\n  +x",
        "erDiagram\n  CUSTOMER ||--o ORDER : places",
        "gantt\n  title x\n  section s\n  task :a1, 2014-01-01, 30d",
        "journey\n  title My day",
        "gitGraph\n  commit",
        "mindmap\n  root((mindmap))",
        "timeline\n  title History",
        "quadrantChart\n  title Reach",
        "xychart-beta\n  bar [1, 2]",
        "sankey-beta\n  a,b,1",
        "block-beta\n  a b",
        "C4Context\n  title System",
        "requirementDiagram",
        "architecture-beta",
        "not a diagram at all",
        "graph\n  A --> B --> ",
        "graph TD\n  A[[unclosed --> B",
        "flowchart TD\n  subgraph S\n  A --> B",
        "flowchart TD\n  A@{ shape: rect } --> B",
        "sequenceDiagram\n  loop forever\n  A->>B: x",
        "sequenceDiagram\n  A->>B: x\n  end",
        "sequenceDiagram\n  create participant C",
        "pie\n  \"A\" : -1",
        "pie\n  \"A\" : 0",
        "pie\n  A : 1",
        "stateDiagram-v2\n  state A {\n  B --> C\n  --\n  D --> E\n  }",
        "---\ntitle: unterminated frontmatter\nflowchart TD\nA-->B",
        "%%{init: {}\nflowchart TD\nA-->B",
    ];
    for src in declined {
        assert!(render(src).is_none(), "should be declined: {src:?}");
    }
}

#[test]
fn accepted_variants() {
    for src in [
        "graph TD;A-->B;B-->C",
        "flowchart LR\n  A --> B & C --> D",
        "flowchart TB\n  A --- B\n  B -.- C\n  C === D\n  D ~~~ E\n  E --o F\n  F --x G\n  G <--> H\n  H o--o I\n  I x--x J",
        "flowchart TD\n  A -- text --> B\n  B -. dotted .-> C\n  C == thick ==> D\n  D --->|long| E",
        "flowchart TD\n  classDef red fill:#f00\n  A:::red --> B\n  class B red\n  style A fill:#0f0\n  linkStyle 0 stroke:#00f\n  click A callback \"Tooltip\"",
        "flowchart TD\n  subgraph one\n    a1 --> a2\n  end\n  subgraph two [Second]\n    direction LR\n    b1 --> b2\n  end\n  one --> two\n  c --> one",
        "flowchart TD\n  subgraph empty [Nothing here]\n  end\n  A --> empty",
        "flowchart TD\n  A --> A\n  A -->|again| A",
        "%%{init: {'theme': 'forest'}}%%\nflowchart TD\n  A --> B",
        "flowchart TD\n  A[\"`**Markdown** string with a long enough body to wrap around`\"] --> B",
        "sequenceDiagram\n  Alice->>Alice: self\n  autonumber 10 5\n  Alice->>Bob: x\n  autonumber off\n  Bob-->>Alice: y",
        "sequenceDiagram\n  participant A\n  activate A\n  A->>B: open\n  deactivate A\n  activate B",
        "pie\n  \"Only\" : 1",
        "pie showData\n  title Lots\n  \"a\" : 1\n  \"b\" : 2\n  \"c\" : 3\n  \"d\" : 4\n  \"e\" : 5\n  \"f\" : 6\n  \"g\" : 7\n  \"h\" : 8\n  \"i\" : 9\n  \"j\" : 10",
        "stateDiagram\n  [*] --> A\n  A --> [*]",
        "classDiagram\n  Animal <|-- Duck\n  Animal : +int age",
        "classDiagram-v2\n  class A~T~\n  A <|.. B\n  B --> \"*\" C : has",
        "erDiagram\n  CUSTOMER ||--o{ ORDER : places",
        "erDiagram\n  A {\n    int id PK\n  }",
        "stateDiagram-v2\n  state Outer {\n    state Inner {\n      [*] --> X\n    }\n  }\n  Outer --> Y",
    ] {
        let r = render(src).unwrap_or_else(|| panic!("should render: {src:?}"));
        parse_svg(&r);
    }
}

// ---------------------------------------------------------------- robustness

static PANICS: AtomicUsize = AtomicUsize::new(0);
static HOOK: Once = Once::new();
static LOCK: Mutex<()> = Mutex::new(());

fn count_panics() {
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            prev(info);
        }));
    });
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
}

const JUNK: &[&str] = &[
    "-->", "---", "-.->", "==>", "|", "||", "[", "]", "(", ")", "{", "}", "((", "))", "\"", ":", ";", "\n", "end", "subgraph ", "&", ":::", "<br>", "#quot;",
    "[*]", "state ", "{", "}", "note ", "loop ", "alt ", "else ", "activate ", "deactivate ", "+", "-", "->>", "-->>", "-x", "-)", "%%", "%%{", "}%%", "---\n",
    "\u{0}", "\u{feff}", "é", "日本", "🙂", "\t", "   ", "0", "-1", "1e309", "NaN", "pie ", "graph LR\n", "direction RL", "<<fork>>",
];

fn mutate(src: &str, r: &mut Lcg) -> String {
    let mut s: Vec<char> = src.chars().collect();
    for _ in 0..1 + r.next() % 4 {
        if s.is_empty() {
            break;
        }
        let at = r.next() % s.len();
        match r.next() % 4 {
            0 => {
                s.truncate(at);
            }
            1 => {
                let n = (r.next() % 8).min(s.len() - at);
                s.drain(at..at + n);
            }
            _ => {
                let j = JUNK[r.next() % JUNK.len()];
                for (k, c) in j.chars().enumerate() {
                    s.insert(at + k, c);
                }
            }
        }
    }
    s.into_iter().collect()
}

#[test]
fn mutated_sources_never_panic() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    count_panics();
    let before = PANICS.load(Ordering::SeqCst);
    let mut r = Lcg(0x5eed);
    let opts = RenderOptions::default();
    for (_, src) in samples() {
        // Every prefix at a stride.
        let chars: Vec<char> = src.chars().collect();
        for cut in (0..chars.len()).step_by(7) {
            if let Some(out) = render_mermaid(&chars[..cut].iter().collect::<String>(), &opts) {
                parse_svg(&out);
            }
        }
        for _ in 0..40 {
            if let Some(out) = render_mermaid(&mutate(&src, &mut r), &opts) {
                parse_svg(&out);
            }
        }
    }
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "the renderer panicked");
}

#[test]
fn odd_options_never_panic() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    count_panics();
    let before = PANICS.load(Ordering::SeqCst);
    let (_, src) = samples().into_iter().find(|s| s.0 == "flow_ci_pipeline").unwrap();
    for w in [0., f32::NAN, f32::INFINITY, -5., 1e9, 10.] {
        let o = RenderOptions { width: w, height: Some(f32::NAN), dark: true, font_family: String::new(), blend: false };
        if let Some(r) = render_mermaid(&src, &o) {
            parse_svg(&r);
            assert!(r.width.is_finite() && r.width <= 8000.);
        }
    }
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "the renderer panicked");
}

fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
}

/// A 50-node, 75-edge flowchart with three subgraphs and a few cycles.
fn fifty_nodes() -> String {
    let mut r = Lcg(50);
    let mut s = String::from("flowchart TD\n");
    for g in 0..3 {
        s.push_str(&format!("  subgraph g{g} [Group {g}]\n"));
        for i in g * 10..g * 10 + 8 {
            s.push_str(&format!("    n{i}[Step number {i}]\n"));
        }
        s.push_str("  end\n");
    }
    for i in 0..50 {
        let shape = match i % 5 {
            0 => format!("n{i}[Task {i}]"),
            1 => format!("n{i}{{Check {i}?}}"),
            2 => format!("n{i}(Rounded {i})"),
            3 => format!("n{i}[(Store {i})]"),
            _ => format!("n{i}([Event {i}])"),
        };
        s.push_str(&format!("  {shape}\n"));
    }
    for i in 1..50 {
        let from = r.next() % i;
        s.push_str(&format!("  n{from} --> n{i}\n"));
    }
    for k in 0..26 {
        let (a, b) = (r.next() % 50, r.next() % 50);
        let label = if k % 3 == 0 { "|maybe|" } else { "" };
        s.push_str(&format!("  n{a} -->{label} n{b}\n"));
    }
    s
}

#[test]
fn fifty_node_flowchart_is_fast() {
    let src = fifty_nodes();
    let opts = RenderOptions::default();
    let first = render_mermaid(&src, &opts).expect("renders");
    parse_svg(&first);
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        let t0 = thread_cpu_ms();
        let r = render_mermaid(&src, &opts).unwrap();
        best = best.min(thread_cpu_ms() - t0);
        assert_eq!(r.svg, first.svg, "deterministic");
    }
    eprintln!("50-node flowchart: {best:.2} ms");
    // Release builds take ~1-2 ms; leave room for debug builds and busy CI.
    let budget = if cfg!(debug_assertions) { 100. } else { 5. };
    assert!(best < budget, "50-node flowchart took {best:.2} ms");
}

#[test]
fn big_inputs_stay_bounded() {
    // A long chain and a dense graph: declined or drawn, but quickly.
    let mut chain = String::from("graph LR\n");
    for i in 0..400 {
        chain.push_str(&format!("  a{i} --> a{}\n", i + 1));
    }
    let mut dense = String::from("graph TD\n");
    for i in 0..60 {
        for j in (i + 1..60).step_by(3) {
            dense.push_str(&format!("  d{i} --> d{j}\n"));
        }
    }
    let mut seq = String::from("sequenceDiagram\n");
    for i in 0..400 {
        seq.push_str(&format!("  P{} ->> P{}: message {i}\n", i % 12, (i * 7 + 1) % 12));
    }
    for (name, src) in [("chain", chain), ("dense", dense), ("sequence", seq)] {
        let t0 = thread_cpu_ms();
        if let Some(r) = render(&src) {
            parse_svg(&r);
        }
        let dt = thread_cpu_ms() - t0;
        eprintln!("{name}: {dt:.1} ms");
        let budget = if cfg!(debug_assertions) { 3000. } else { 300. };
        assert!(dt < budget, "took {dt:.0} ms");
    }
}
