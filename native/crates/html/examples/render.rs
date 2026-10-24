//! `cargo run --release -p herse-html --example render -- in.ipynb out.html`
fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: render in.ipynb out.html");
        std::process::exit(2);
    };
    let bytes = std::fs::read(&input).expect("read");
    let t = std::time::Instant::now();
    let nb = herse_nb::parse(&bytes).expect("parse");
    let title = std::path::Path::new(&input).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let html = herse_html::render(&nb, &herse_html::Options { title, ..Default::default() });
    eprintln!("{} KB -> {} KB in {:.1} ms", bytes.len() / 1024, html.len() / 1024, t.elapsed().as_secs_f64() * 1000.);
    std::fs::write(&output, html).expect("write");
}
