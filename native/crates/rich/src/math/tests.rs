use std::time::{Duration, Instant};

use super::*;

/// Formulas notebooks actually contain; every one must render.
const MUST_RENDER: &[(bool, &str)] = &[
    (false, r"\frac{a}{b}"),
    (false, r"\sqrt{x^2+y^2} + \sqrt[3]{z}"),
    (false, r"x_i^2 + x_{i,j}^{(k)}"),
    (
        true,
        r"\sum_{i=1}^n x_i = \int_0^1 f(x)\,dx \cdot \prod_{k} a_k",
    ),
    (
        true,
        r"\lim_{n \to \infty} \left(1 + \frac{1}{n}\right)^n = e",
    ),
    (
        false,
        r"\alpha\beta\gamma\delta\epsilon\varepsilon\zeta\eta\theta\vartheta\iota\kappa\lambda\mu\nu\xi\pi\rho\sigma\tau\upsilon\phi\varphi\chi\psi\omega\Gamma\Delta\Theta\Lambda\Xi\Pi\Sigma\Phi\Psi\Omega",
    ),
    (
        false,
        r"\mathbb{R}^n, \mathcal{L}, \mathbf{x}, \text{if } x > 0, \mathrm{d}x, \boldsymbol{\theta}, \operatorname{rank}(A)",
    ),
    (
        true,
        r"\left( \frac{a}{b} \right) \left[ x \right] \left\{ y \right\} \left| z \right| \left\| w \right\| \left\langle u \right\rangle",
    ),
    (
        false,
        r"\hat{y} \bar{x} \vec{v} \tilde{n} \dot{x} \ddot{x} \overline{AB} \widehat{xyz}",
    ),
    (
        false,
        r"a \cdot b \times c \leq d \geq e \neq f \approx g \infty \partial \nabla \pm \mp \to \Rightarrow \in \subset \cup \cap",
    ),
    (
        true,
        r"\begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix} \begin{bmatrix} a \\ b \end{bmatrix} \begin{vmatrix} a & b \\ c & d \end{vmatrix} \begin{matrix} x & y \end{matrix} \begin{Vmatrix} 1 \end{Vmatrix} \begin{Bmatrix} 1 \end{Bmatrix}",
    ),
    (
        true,
        r"f(x) = \begin{cases} 0 & x < 0 \\ 1 & \text{otherwise} \end{cases}",
    ),
    (true, r"\begin{align} a &= b + c \\ &= d \end{align}"),
    (true, r"\begin{align*} a &= b \\ c &= d \end{align*}"),
    (true, r"\begin{aligned} a &= b \\ c &= d \end{aligned}"),
    (true, r"\begin{equation} E = mc^2 \end{equation}"),
    (true, r"\begin{equation*} E = mc^2 \end{equation*}"),
    (true, r"\begin{gather} a \\ b \end{gather}"),
    (true, r"\begin{gather*} a \\ b \end{gather*}"),
    (true, r"\begin{multline} a + b \\ + c \end{multline}"),
    (true, r"\begin{split} a &= b \\ &= c \end{split}"),
    (true, r"\begin{alignat}{2} a &= b & c &= d \end{alignat}"),
    (true, r"\begin{eqnarray} a &=& b \\ c &=& d \end{eqnarray}"),
    (
        true,
        r"\begin{array}{c|cc} & 1 & 2 \\ \hline a & b & c \end{array}",
    ),
    (
        true,
        r"\begin{dcases} a & b \\ c & d \end{dcases} \begin{rcases} a & b \end{rcases} \begin{smallmatrix} a & b \end{smallmatrix} \begin{gathered} a \\ b \end{gathered}",
    ),
    (
        true,
        r"\operatorname*{arg\,max}_\theta L(\theta) \quad \operatorname{softmax}(z)_i \quad \mathop{\rm arg\,min}_x f",
    ),
    (
        true,
        r"\displaystyle \frac{1}{2} \quad \textstyle \frac{1}{2} \, \; \: \! \qquad \enspace",
    ),
    (
        true,
        r"\underbrace{a + b}_{n} \overbrace{c + d}^{m} \underset{x}{\arg\min} \overset{!}{=} \stackrel{def}{=}",
    ),
    (
        true,
        r"\left\langle \psi \middle| \hat{H} \middle| \psi \right\rangle",
    ),
    (
        false,
        r"\binom{n}{k} \tbinom{n}{k} \dbinom{n}{k} {n \choose k} {a \over b} {a \atop b}",
    ),
    (
        false,
        r"\bm{x} \mathds{1} \lbrace x \rbrace a \pmod{n} \bmod 2 \coloneqq 90\degree",
    ),
    (
        false,
        r"\hspace{1cm} \kern 2pt \mkern 3mu \hskip 1em plus 1fil a",
    ),
    (
        false,
        r"\color{red}{x} + \textcolor{blue}{y} + \colorbox{yellow}{z} + \boxed{w}",
    ),
    (
        false,
        r"\cancel{x} \phantom{y} \vphantom{z} \smash{q} \not= \neq \notin",
    ),
    (true, r"x \tag{1}"),
    (false, r"\lt \gt \And \unicode{x263A} \require{cancel} \ce"),
    (
        true,
        "\\begin{align}\n  x &= 1 \\\\ % comment\n  y &= 2\n\\end{align}",
    ),
    (false, "$x^2$"),
    (true, "$$\\int_a^b f$$"),
    (false, r"\(a+b\)"),
    (true, r"\[a+b\]"),
];

#[test]
fn notebook_formulas_render() {
    for &(display, src) in MUST_RENDER {
        let r = render_svg(src, display);
        let s = match r {
            Ok(s) => s,
            Err(e) if src.ends_with(r"\ce") => {
                // `\ce` without argument is an expected failure.
                assert!(matches!(e, MathError::Parse(_) | MathError::Unsupported(_)));
                continue;
            }
            Err(e) => panic!("{src}: {e}"),
        };
        assert!(
            s.svg.starts_with("<svg") && s.svg.ends_with("</svg>"),
            "{src}"
        );
        assert!(!s.svg.contains("NaN") && !s.svg.contains("inf"), "{src}");
        assert!(s.width_em > 0.0 && s.height_em > 0.0, "{src}");
        assert!(
            s.baseline_em >= 0.0 && s.baseline_em <= s.height_em,
            "{src}"
        );
        assert!(s.svg.contains("currentColor"), "{src}");
    }
}

#[test]
fn svgs_parse_with_usvg() {
    let opt = resvg::usvg::Options::default();
    for &(display, src) in MUST_RENDER {
        let Ok(s) = render_svg(src, display) else {
            continue;
        };
        let tree =
            resvg::usvg::Tree::from_str(&s.svg, &opt).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert!(tree.root().has_children(), "{src}: empty SVG");
    }
}

#[test]
fn delimiters_choose_style() {
    let inline = render_svg("$\\sum_{i=1}^n i$", true).unwrap();
    let display = render_svg("$$\\sum_{i=1}^n i$$", false).unwrap();
    assert!(
        display.height_em > inline.height_em,
        "$$ is display even if display=false"
    );
    assert_eq!(strip_delimiters("$x$"), ("x", Some(false)));
    assert_eq!(strip_delimiters(" $$x$$ "), ("x", Some(true)));
    assert_eq!(strip_delimiters(r"\[x\]"), ("x", Some(true)));
    assert_eq!(strip_delimiters(r"\(x\)"), ("x", Some(false)));
    assert_eq!(strip_delimiters("$a$ and $b$"), ("$a$ and $b$", None));
    // Display environments are display even inline.
    let a = render_svg(r"\begin{align} a &= b \end{align}", false).unwrap();
    let b = render_svg(r"\begin{align} a &= b \end{align}", true).unwrap();
    assert_eq!(a, b);
}

#[test]
fn errors_not_panics() {
    let bad = [
        r"\frac{1}{",
        r"\frac",
        r"x^",
        r"\left( x",
        r"\begin{align} x",
        r"\begin{tikzpicture}\end{tikzpicture}",
        r"\ce{H2O}",
        r"\foo",
        "}",
        r"x \right)",
        r"\def\a{\a}\a",
        r"\newcommand{\b}{\b\b}\b",
    ];
    for src in bad {
        match render_svg(src, true) {
            Ok(_) => panic!("{src}: expected an error"),
            Err(MathError::Internal(m)) => panic!("{src}: internal error {m}"),
            Err(_) => {}
        }
    }
    assert_eq!(
        render_svg(&"x".repeat(MAX_INPUT_BYTES + 1), false),
        Err(MathError::TooLarge)
    );
}

#[test]
fn top_level_line_breaks_stack() {
    // MathJax breaks display math at a top-level `\\` (Kalman notebooks).
    let one = render_svg(r"a = b", true).unwrap();
    let two = render_svg(r"a = b \\ c = d", true).unwrap();
    assert!(two.height_em > one.height_em * 1.8);
    let aligned = render_svg(r"a &= b \\ c &= d", true).unwrap();
    assert!(aligned.height_em > one.height_em * 1.8);
    // Inside environments and braces nothing changes.
    assert!(render_svg(r"\begin{pmatrix} a \\ b \end{pmatrix}", true).is_ok());
}

#[test]
fn tex_group_classes() {
    // `{=}` is an Ord atom in TeX: no relation spacing.
    let spaced = render_svg("x=y", false).unwrap();
    let tight = render_svg("x{=}y", false).unwrap();
    assert!(tight.box_width_em < spaced.box_width_em - 0.3);
}

#[test]
fn empty_and_definition_only() {
    for src in ["", " ", "$$", r"\newcommand{\R}{\mathbb{R}}"] {
        let s = render_svg(src, false).unwrap();
        assert!(s.svg.starts_with("<svg"));
    }
}

#[test]
fn macros_carry_across_formulas() {
    let mut m = Macros::new();
    m.scan(r"$\newcommand{\R}{\mathbb{R}} \DeclareMathOperator{\Tr}{Tr} \def\half{\frac12}$");
    assert!(m.contains("R") && m.contains("Tr") && m.contains("half"));
    let opts = MathOptions {
        macros: Some(&m),
        ..Default::default()
    };
    let with = render_svg_with(r"x \in \R^n, \Tr(A), \half", &opts).unwrap();
    let expanded = render_svg(r"x \in \mathbb{R}^n, \operatorname{Tr}(A), \frac12", false).unwrap();
    assert_eq!(with.svg, expanded.svg);
    assert!(
        render_svg(r"\R", false).is_err(),
        "unknown without the table"
    );
}

#[test]
fn numbering_and_tags() {
    let plain = render_svg(r"\begin{equation} x \end{equation}", true).unwrap();
    let numbered = render_svg_with(
        r"\begin{equation} x \end{equation}",
        &MathOptions {
            display: true,
            number_equations: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        numbered.width_em > plain.width_em + 1.0,
        "(1) only when asked"
    );
    let tagged = render_svg(r"x \tag{3}", true).unwrap();
    assert!(tagged.width_em > plain.width_em + 1.0, "\\tag always shows");
}

#[test]
fn single_color_by_default() {
    let s = render_svg(r"\color{red} x \colorbox{yellow}{y}", false).unwrap();
    assert!(!s.svg.contains("#ff0000") && !s.svg.contains("#ffff00"));
    let c = render_svg_with(
        r"\color{red} x",
        &MathOptions {
            colors: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(c.svg.contains("#ff0000"));
    let f = render_svg_with(
        "x",
        &MathOptions {
            fill: Fill::Rgb(1, 2, 3),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(f.svg.contains("#010203") && !f.svg.contains("currentColor"));
}

#[test]
fn deterministic_and_thread_safe() {
    let srcs: Vec<&str> = MUST_RENDER.iter().map(|(_, s)| *s).collect();
    let expect: Vec<_> = srcs.iter().map(|s| render_svg(s, true)).collect();
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let srcs = srcs.clone();
            std::thread::spawn(move || {
                let mut out = Vec::new();
                for i in 0..srcs.len() {
                    let k = (i + t * 7) % srcs.len();
                    out.push((k, render_svg(srcs[k], true)));
                }
                out
            })
        })
        .collect();
    for h in handles {
        for (k, r) in h.join().unwrap() {
            assert_eq!(r, expect[k], "{}", srcs[k]);
        }
    }
}

/// Random token soup: no panics (would surface as `Internal`), bounded time.
#[test]
fn fuzz_token_soup() {
    let toks = [
        r"\frac",
        "{",
        "}",
        "^",
        "_",
        "x",
        "1",
        r"\sqrt",
        "[",
        "]",
        r"\left(",
        r"\right)",
        r"\left",
        r"\right",
        r"\middle|",
        r"\begin{matrix}",
        r"\end{matrix}",
        r"\begin{align}",
        r"\end{align}",
        r"\begin{cases}",
        r"\end{cases}",
        r"\begin{array}{c|l}",
        r"\end{array}",
        r"\begin{eqnarray}",
        r"\end{eqnarray}",
        "&",
        r"\\",
        r"\text{",
        r"\mathbb",
        r"\sum",
        r"\int",
        r"\lim",
        r"\hat",
        r"\vec",
        r"\overbrace",
        r"\underbrace",
        r"\color{red}",
        r"\hspace{",
        "pt",
        "-",
        "+",
        "=",
        ":",
        "$",
        r"\",
        "%",
        "#",
        "#1",
        r"\hline",
        r"\genfrac",
        r"\tag{",
        r"\label{",
        r"\operatorname",
        r"\operatorname*",
        r"\mathop",
        r"\mathrel",
        r"\mathbf",
        r"\boldsymbol",
        "'",
        r"\prime",
        r"\not",
        r"\big",
        "é",
        "\u{1F600}",
        " ",
        "\n",
        r"\displaystyle",
        r"\over",
        r"\atop",
        r"\choose",
        r"\xrightarrow",
        r"\substack{",
        r"\cfrac",
        r"\binom",
        r"\{",
        r"\}",
        r"\|",
        "|",
        r"\,",
        r"\!",
        r"\quad",
        r"\phantom",
        r"\boxed",
        r"\newcommand{\a}{",
        r"\def\b#1{",
        r"\a",
        r"\b",
        r"\let\c=",
        r"\kern",
        r"3mu",
        r"\unicode{",
        r"\bbox[",
        r"\smash",
        r"\limits",
        r"\nolimits",
        "(",
        ")",
        r"\stackrel",
        r"\overset",
        r"\sideset",
        r"\DeclareMathOperator{\d}{",
        r"\end",
        r"\begin",
        "~",
    ];
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut worst = Duration::ZERO;
    let mut ok = 0;
    for _ in 0..20_000 {
        let len = (rnd() % 30) as usize;
        let mut s = String::new();
        for _ in 0..len {
            s.push_str(toks[(rnd() % toks.len() as u64) as usize]);
            if rnd() % 3 == 0 {
                s.push(' ');
            }
        }
        let t = Instant::now();
        let r = render_svg(&s, rnd() % 2 == 0);
        worst = worst.max(t.elapsed());
        match r {
            Err(MathError::Internal(m)) => panic!("internal error on {s:?}: {m}"),
            Ok(svg) => {
                ok += 1;
                assert!(!svg.svg.contains("NaN"), "{s:?}");
            }
            Err(_) => {}
        }
    }
    assert!(
        ok > 500,
        "fuzz corpus should produce some valid formulas ({ok})"
    );
    assert!(
        worst < Duration::from_millis(500),
        "slowest fuzz input took {worst:?}"
    );
}

/// Random bytes / unicode: never panics.
#[test]
fn fuzz_garbage() {
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..5_000 {
        let len = (rnd() % 64) as usize;
        let s: String = (0..len)
            .map(|_| match rnd() % 4 {
                0 => char::from_u32((rnd() % 0x80) as u32).unwrap(),
                1 => {
                    ['\\', '{', '}', '^', '_', '&', '$', '%', '#', '[', ']'][(rnd() % 11) as usize]
                }
                2 => char::from_u32((rnd() % 0x3000) as u32).unwrap_or('?'),
                _ => char::from_u32((rnd() % 0x10FFFF) as u32).unwrap_or('x'),
            })
            .collect();
        if let Err(MathError::Internal(m)) = render_svg(&s, rnd() % 2 == 0) {
            panic!("internal error on {s:?}: {m}");
        }
    }
}

#[test]
fn pathological_inputs_are_bounded() {
    let cases = [
        "{".repeat(5000),
        "x^".repeat(2000) + "x",
        r"\frac{".repeat(300) + &"}{x}".repeat(300),
        r"\sqrt{".repeat(1000) + &"}".repeat(1000),
        r"\left(".repeat(500) + &r"\right)".repeat(500),
        "a".repeat(MAX_INPUT_BYTES),
        "a+".repeat(MAX_INPUT_BYTES / 2),
        format!(r"\begin{{align}}{}\end{{align}}", r"a&=b\\".repeat(1500)),
        r"\newcommand{\x}[1]{#1#1}".to_string() + &r"\x{".repeat(14) + "a" + &"}".repeat(14),
    ];
    for src in &cases {
        let t = Instant::now();
        let r = render_svg(src, true);
        let e = t.elapsed();
        assert!(
            !matches!(r, Err(MathError::Internal(_))),
            "{}: {r:?}",
            &src[..40.min(src.len())]
        );
        assert!(
            e < Duration::from_secs(3),
            "{} took {e:?}",
            &src[..40.min(src.len())]
        );
    }
}

#[test]
fn typical_formula_speed() {
    warm_up();
    let inline = r"\hat{y} = \sigma(w^T x + b)";
    let display =
        r"f(x) = \frac{1}{\sigma\sqrt{2\pi}} \exp\left( -\frac{(x-\mu)^2}{2\sigma^2} \right)";
    let time = |s: &str, d: bool| {
        let t = Instant::now();
        for _ in 0..50 {
            render_svg(s, d).unwrap();
        }
        t.elapsed() / 50
    };
    let (ti, td) = (time(inline, false), time(display, true));
    eprintln!("inline {ti:?}, display {td:?}");
    // Generous bounds (debug builds, busy CI machines); release is ~20x faster.
    assert!(ti < Duration::from_millis(20) && td < Duration::from_millis(50));
}

#[test]
fn pixel_sized_variant() {
    let m = render_svg(r"\frac{a}{b}", false).unwrap();
    let px = m.svg_px(32.0);
    assert!(px.contains(&format!("width=\"{:.2}\"", m.width_em * 32.0)));
    assert!(!px[..px.find('>').unwrap()].contains("em\""));
    let tree = resvg::usvg::Tree::from_str(&px, &resvg::usvg::Options::default()).unwrap();
    assert!((tree.size().width() - m.width_em * 32.0).abs() < 0.1);
    // Only the root group carries the color, so a theme can swap it.
    assert_eq!(m.svg.matches("currentColor").count(), 2);
}

#[test]
fn types_are_send_sync_and_fingerprints_stable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MathSvg>();
    assert_send_sync::<Macros>();
    assert_send_sync::<MathError>();
    assert_send_sync::<crate::ansi::StyledText>();
    let mut a = Macros::new();
    a.scan(r"\newcommand{\R}{\mathbb{R}}\newcommand{\N}{\mathbb{N}}");
    let mut b = Macros::new();
    b.scan(r"\newcommand{\N}{\mathbb{N}}");
    b.scan(r"\newcommand{\R}{\mathbb{R}}");
    assert_eq!(a.fingerprint(), b.fingerprint());
    b.scan(r"\renewcommand{\R}{\mathbf{R}}");
    assert_ne!(a.fingerprint(), b.fingerprint());
}
