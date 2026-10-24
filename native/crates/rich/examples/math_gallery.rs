//! Renders representative notebook formulas to SVG + PNG and an index.html.
//!
//! `cargo run -p herse-rich --release --example math_gallery -- <out-dir>`
//!
//! Writes `<out>/NN.svg`, `<out>/png/NN.png` (32 px/em), `<out>/sheet-*.png`
//! (contact sheets, numbered) and `<out>/index.html`, and prints per-formula
//! timings plus a list of formulas that failed.
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use herse_rich::math::{self, Macros, MathOptions};

/// (display, source, origin)
const FORMULAS: &[(bool, &str, &str)] = &[
    // From the notebooks in /opt/anaconda3 (all of their math).
    (false, r"t=\frac{\pi}{4}", "anaconda"),
    (false, r"y=100-x^2", "anaconda"),
    (true, r"x_{n+1} = \sin(ay_n) + c \cos(ax_n)", "anaconda"),
    (true, r"y_{n+1} = \sin(bx_n) + d \cos(by_n)", "anaconda"),
    // Typical inline math in ML / stats / physics notebooks.
    (false, r"\alpha + \beta = \gamma", "inline"),
    (false, r"x \in \mathbb{R}^n", "inline"),
    (false, r"\hat{y} = \sigma(w^T x + b)", "inline"),
    (false, r"\mathcal{L}(\theta)", "inline"),
    (false, r"O(n \log n)", "inline"),
    (false, r"p(x \mid \theta)", "inline"),
    (false, r"\sqrt{x^2+y^2}", "inline"),
    (false, r"\sum_{i=1}^n x_i", "inline"),
    (false, r"\lim_{x \to 0} \frac{\sin x}{x} = 1", "inline"),
    (false, r"\bar{x} = \frac{1}{n}\sum_{i=1}^{n} x_i", "inline"),
    (false, r"\vec{v} \cdot \vec{w}", "inline"),
    (false, r"\tilde{x}, \dot{x}, \ddot{x}", "inline"),
    (false, r"a \leq b \geq c \neq d \approx e", "inline"),
    (false, r"\nabla f, \partial_x u, \infty", "inline"),
    (
        false,
        r"\operatorname{argmax}_\theta \, p(\theta)",
        "inline",
    ),
    (false, r"\text{if } x > 0", "inline"),
    (
        false,
        r"\mathrm{d}x, \; \boldsymbol{\mu}, \; \mathbf{x}_i, \; A \times B",
        "inline",
    ),
    (false, r"e^{i\pi} + 1 = 0", "inline"),
    // Display math.
    (
        true,
        r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}",
        "display",
    ),
    (
        true,
        r"\prod_{k=1}^{n} \left(1 + \frac{1}{k}\right) = n + 1",
        "display",
    ),
    (
        true,
        r"f(x) = \frac{1}{\sigma\sqrt{2\pi}} \exp\left( -\frac{(x-\mu)^2}{2\sigma^2} \right)",
        "display",
    ),
    (
        true,
        r"\mathcal{N}(\mu, \Sigma) = \frac{1}{(2\pi)^{k/2}|\Sigma|^{1/2}} e^{-\frac{1}{2}(x-\mu)^T\Sigma^{-1}(x-\mu)}",
        "display",
    ),
    (
        true,
        r"A = \begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}",
        "display",
    ),
    (
        true,
        r"\begin{bmatrix} a_{11} & a_{12} & \cdots & a_{1n} \\ \vdots & \vdots & \ddots & \vdots \\ a_{m1} & a_{m2} & \cdots & a_{mn} \end{bmatrix}",
        "display",
    ),
    (
        true,
        r"\det \begin{vmatrix} a & b \\ c & d \end{vmatrix} = ad - bc",
        "display",
    ),
    (
        true,
        r"I = \begin{matrix} 1 & 0 \\ 0 & 1 \end{matrix}",
        "display",
    ),
    (
        true,
        r"|x| = \begin{cases} x & \text{if } x \geq 0 \\ -x & \text{otherwise} \end{cases}",
        "display",
    ),
    (
        true,
        r"\begin{align} \nabla \cdot \mathbf{E} &= \frac{\rho}{\varepsilon_0} \\ \nabla \times \mathbf{B} &= \mu_0 \mathbf{J} + \mu_0 \varepsilon_0 \frac{\partial \mathbf{E}}{\partial t} \end{align}",
        "display",
    ),
    (
        true,
        r"\begin{aligned} (a+b)^2 &= a^2 + 2ab + b^2 \\ &\geq 4ab \end{aligned}",
        "display",
    ),
    (true, r"\begin{equation} E = mc^2 \end{equation}", "display"),
    (
        true,
        r"\begin{gather} a = b \\ c = d \end{gather}",
        "display",
    ),
    (
        true,
        r"\displaystyle \sum_{n=1}^\infty \frac{1}{n^2} = \frac{\pi^2}{6}",
        "display",
    ),
    (
        true,
        r"\underbrace{1 + 1 + \cdots + 1}_{n \text{ times}} = n",
        "display",
    ),
    (
        true,
        r"\overline{z_1 z_2} = \overline{z_1}\,\overline{z_2}",
        "display",
    ),
    (
        true,
        r"J(\theta) = -\frac{1}{m} \sum_{i=1}^m \left[ y^{(i)} \log h_\theta(x^{(i)}) + (1 - y^{(i)}) \log (1 - h_\theta(x^{(i)})) \right]",
        "display",
    ),
    (
        true,
        r"\theta_j := \theta_j - \alpha \frac{\partial}{\partial \theta_j} J(\theta)",
        "display",
    ),
    (
        true,
        r"\operatorname{softmax}(z)_i = \frac{e^{z_i}}{\sum_{j=1}^K e^{z_j}}",
        "display",
    ),
    (true, r"\hat{\beta} = (X^T X)^{-1} X^T y", "display"),
    (
        true,
        r"P(A \mid B) = \frac{P(B \mid A)\, P(A)}{P(B)}",
        "display",
    ),
    (
        true,
        r"\mathbb{E}[X] = \int_{-\infty}^{\infty} x f(x)\, dx \quad \text{and} \quad \operatorname{Var}(X) = \mathbb{E}[X^2] - \mathbb{E}[X]^2",
        "display",
    ),
    (
        true,
        r"\frac{\partial^2 u}{\partial t^2} = c^2 \nabla^2 u",
        "display",
    ),
    (
        true,
        r"\left\| \mathbf{x} \right\|_2 = \sqrt{\sum_{i} x_i^2}",
        "display",
    ),
    (true, r"\binom{n}{k} = \frac{n!}{k!(n-k)!}", "display"),
    (
        true,
        r"\oint_C \mathbf{F} \cdot d\mathbf{r} = \iint_S (\nabla \times \mathbf{F}) \cdot d\mathbf{S}",
        "display",
    ),
    (
        true,
        r"\sigma(x) = \frac{1}{1 + e^{-x}}, \qquad \tanh(x) = \frac{e^x - e^{-x}}{e^x + e^{-x}}",
        "display",
    ),
    (
        true,
        r"\mathrm{Attention}(Q, K, V) = \mathrm{softmax}\left(\frac{QK^T}{\sqrt{d_k}}\right)V",
        "display",
    ),
    (
        true,
        r"\begin{align*} \min_{w,b} \quad & \frac{1}{2}\|w\|^2 \\ \text{s.t.} \quad & y_i(w^T x_i + b) \geq 1, \; i = 1, \dots, n \end{align*}",
        "display",
    ),
    (true, r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}", "display"),
    (
        true,
        r"\sqrt[3]{x} + \frac{\frac{1}{x}+\frac{1}{y}}{y-z}",
        "display",
    ),
    (
        true,
        r"\forall \epsilon > 0, \exists \delta > 0 : |x - a| < \delta \Rightarrow |f(x) - L| < \epsilon",
        "display",
    ),
    (
        true,
        r"\Gamma(z) = \int_0^\infty t^{z-1} e^{-t}\,dt",
        "display",
    ),
    (
        true,
        r"\left\langle \psi \middle| \hat{H} \middle| \psi \right\rangle",
        "display",
    ),
    (
        true,
        r"R_{\mu\nu} - \frac{1}{2} R g_{\mu\nu} + \Lambda g_{\mu\nu} = \frac{8\pi G}{c^4} T_{\mu\nu}",
        "display",
    ),
    (
        true,
        r"\mathbf{A}\mathbf{x} = \mathbf{b}, \quad \mathbf{A} \in \mathbb{R}^{m \times n}",
        "display",
    ),
    (
        true,
        r"f'(x) = \lim_{h \to 0} \frac{f(x+h) - f(x)}{h}",
        "display",
    ),
    (
        true,
        r"\hat{\theta}_{\text{MLE}} = \operatorname*{arg\,max}_\theta \prod_{i=1}^n p(x_i \mid \theta)",
        "display",
    ),
    (
        true,
        r"\begin{array}{c|cc} & 1 & 2 \\ \hline a & b & c \end{array}",
        "display",
    ),
    (
        true,
        "\\begin{eqnarray}\n  y &=& ax + b \\\\\n  z &=& cx^2\n\\end{eqnarray}",
        "display",
    ),
    (
        true,
        r"\newcommand{\R}{\mathbb{R}} \newcommand{\norm}[1]{\left\lVert#1\right\rVert} f: \R^n \to \R, \quad \norm{\nabla f(x)} \le L",
        "display",
    ),
    (
        true,
        r"\boldsymbol{\Sigma} = \bm{U} \bm{\Lambda} \bm{U}^\top, \qquad \mathrm{tr}(\boldsymbol{\Sigma}) = \sum_i \lambda_i",
        "display",
    ),
    // Fallback: these are expected to fail (show source in monospace).
    (true, r"\ce{H2O}", "unsupported"),
    (false, r"\frac{1}{", "malformed"),
];

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "math-gallery".into());
    let png_dir = format!("{out}/png");
    std::fs::create_dir_all(&png_dir).expect("create output dir");

    let t = Instant::now();
    math::warm_up();
    let warm = t.elapsed();

    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>herse-rich math gallery</title>\
         <style>body{font:16px/1.5 -apple-system,sans-serif;margin:24px;color:#222}\
         td{padding:6px 12px;border-bottom:1px solid #eee;vertical-align:middle}\
         code{font-size:12px;color:#555}.err{color:#b00}img{vertical-align:middle}\
         .math{font-size:22px}</style><table>",
    );
    let mut fails = Vec::new();
    let mut inline_times = Vec::new();
    let mut display_times = Vec::new();
    let mut rendered: Vec<(usize, math::MathSvg)> = Vec::new();
    let macros = Macros::new();
    for (i, &(display, src, origin)) in FORMULAS.iter().enumerate() {
        let opts = MathOptions {
            display,
            macros: Some(&macros),
            ..Default::default()
        };
        let t = Instant::now();
        let r = math::render_svg_with(src, &opts);
        let first = t.elapsed();
        // Warm timing: median of 9 renders (the result is a pure function,
        // the app would cache it; this measures the uncached cost).
        let mut samples: Vec<Duration> = (0..9)
            .map(|_| {
                let t = Instant::now();
                let _ = math::render_svg_with(src, &opts);
                t.elapsed()
            })
            .collect();
        samples.sort();
        let warm_t = samples[4];
        let esc = src.replace('&', "&amp;").replace('<', "&lt;");
        match r {
            Ok(svg) => {
                (if display {
                    &mut display_times
                } else {
                    &mut inline_times
                })
                .push(warm_t);
                std::fs::write(format!("{out}/{i:02}.svg"), &svg.svg).expect("write svg");
                let _ = write!(
                    html,
                    "<tr><td>{i:02}</td><td class=math><img src=\"{i:02}.svg\" style=\"height:{h}em;vertical-align:-{b}em\"></td><td><code>{esc}</code><br><small>{origin} · {:.0} µs · {} B</small></td></tr>",
                    warm_t.as_secs_f64() * 1e6,
                    svg.svg.len(),
                    h = svg.height_em,
                    b = svg.baseline_em,
                );
                println!(
                    "{i:02} ok   first {:>7.0} µs  warm {:>6.0} µs  {:>6} B  {:5.2}x{:4.2} em  {src}",
                    first.as_secs_f64() * 1e6,
                    warm_t.as_secs_f64() * 1e6,
                    svg.svg.len(),
                    svg.width_em,
                    svg.height_em
                );
                rendered.push((i, svg));
            }
            Err(e) => {
                let _ = write!(
                    html,
                    "<tr><td>{i:02}</td><td class=err>{e}</td><td><code>{esc}</code><br><small>{origin}</small></td></tr>"
                );
                println!("{i:02} FAIL {e}  ::  {src}");
                fails.push((i, src, e));
            }
        }
    }
    html.push_str("</table>");
    std::fs::write(format!("{out}/index.html"), html).expect("write html");

    // PNGs + contact sheets.
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let px_per_em = 32.0;
    let mut rows: Vec<(usize, resvg::tiny_skia::Pixmap)> = Vec::new();
    for (i, svg) in &rendered {
        let tree = resvg::usvg::Tree::from_str(&svg.svg, &opt).expect("usvg parses our SVG");
        let w = (svg.width_em * px_per_em).ceil().max(1.0) as u32;
        let h = (svg.height_em * px_per_em).ceil().max(1.0) as u32;
        let mut pm = resvg::tiny_skia::Pixmap::new(w, h).expect("pixmap");
        let sx = w as f32 / tree.size().width();
        let sy = h as f32 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pm.as_mut(),
        );
        pm.save_png(format!("{png_dir}/{i:02}.png")).expect("png");
        rows.push((*i, pm));
    }
    for (n, chunk) in rows.chunks(22).enumerate() {
        sheet(chunk, &format!("{out}/sheet-{n}.png"), &opt);
    }

    let stats = |v: &mut Vec<Duration>| {
        v.sort();
        if v.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e6;
        (p(0.5), p(0.95), v.last().unwrap().as_secs_f64() * 1e6)
    };
    let (i50, i95, imax) = stats(&mut inline_times);
    let (d50, d95, dmax) = stats(&mut display_times);
    println!(
        "\nwarm_up (font + tables + first formula): {:.1} ms",
        warm.as_secs_f64() * 1e3
    );
    println!(
        "inline : p50 {i50:.0} µs  p95 {i95:.0} µs  max {imax:.0} µs  ({} formulas)",
        inline_times.len()
    );
    println!(
        "display: p50 {d50:.0} µs  p95 {d95:.0} µs  max {dmax:.0} µs  ({} formulas)",
        display_times.len()
    );
    println!("failed : {}", fails.len());
    for (i, src, e) in &fails {
        println!("  {i:02} {e} :: {src}");
    }
    println!("wrote {out}/index.html");
}

/// Stack rendered formulas into one numbered PNG on a light background.
fn sheet(rows: &[(usize, resvg::tiny_skia::Pixmap)], path: &str, opt: &resvg::usvg::Options) {
    use resvg::tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Rect, Transform};
    let label_w = 44;
    let pad = 8;
    let width = rows.iter().map(|r| r.1.width()).max().unwrap_or(100) + label_w + 2 * pad;
    let height: u32 = rows.iter().map(|r| r.1.height() + 2 * pad).sum();
    let mut out = Pixmap::new(width, height.max(1)).expect("sheet");
    out.fill(Color::WHITE);
    let mut y = 0;
    for (i, pm) in rows {
        let label = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{label_w}" height="24"><text x="2" y="17" font-family="Helvetica" font-size="14" fill="#999">{i:02}</text></svg>"##
        );
        if let Ok(t) = resvg::usvg::Tree::from_str(&label, opt) {
            resvg::render(
                &t,
                Transform::from_translate(0.0, (y + pad) as f32),
                &mut out.as_mut(),
            );
        }
        let mut paint = Paint::default();
        paint.set_color_rgba8(255, 244, 214, 255);
        if let Some(r) = Rect::from_xywh(
            label_w as f32,
            (y + pad) as f32,
            pm.width() as f32,
            pm.height() as f32,
        ) {
            out.fill_rect(r, &paint, Transform::identity(), None);
        }
        out.draw_pixmap(
            label_w as i32,
            (y + pad) as i32,
            pm.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        y += pm.height() + 2 * pad;
    }
    out.save_png(path).expect("sheet png");
}
