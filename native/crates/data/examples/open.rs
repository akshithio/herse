//! Read data files the way the app does and report how long it took:
//!
//! ```bash
//! cargo run --release -p herse-data --example open -- fixtures/big/data/sales-1m.csv
//! ```
//!
//! Prints the time to the first rows (what the reader waits for), the whole
//! read, rows, columns and the text held; then a sort and a search over the
//! result, as the viewer runs them. Times are wall and this thread's CPU.

use std::{path::Path, sync::atomic::AtomicBool, time::Instant};

use herse_data::{Event, Flow, Frame, Order, ViewSpec};

fn cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out-pointer for the calling thread's CPU clock.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1000. + ts.tv_nsec as f64 / 1e6
}

fn main() {
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let (t0, c0) = (Instant::now(), cpu_ms());
        let mut frame = Frame::default();
        let mut first: Option<(f64, usize)> = None;
        let result = herse_data::read(path, None, &mut |event| {
            match event {
                Event::Columns(c) => frame.set_columns(c),
                Event::Chunk(chunk) => {
                    frame.push_chunk(chunk);
                    first.get_or_insert((t0.elapsed().as_secs_f64() * 1000., frame.rows()));
                }
                Event::Progress(_) => {}
            }
            Flow::Continue
        });
        let (wall, cpu) = (t0.elapsed().as_secs_f64() * 1000., cpu_ms() - c0);
        match result {
            Err(e) => println!("{arg}: error: {e}"),
            Ok(loaded) => {
                println!(
                    "{arg}: {:.1} MB, first {} rows in {:.1} ms, all {} rows × {} columns in {:.0} ms ({:.0} ms CPU, {:.0} MB/s), {} chunks, {:.1} MB of text{}",
                    size as f64 / 1e6,
                    first.map_or(0, |f| f.1),
                    first.map_or(0., |f| f.0),
                    loaded.rows,
                    frame.columns().len(),
                    wall,
                    cpu,
                    size as f64 / 1e6 / (wall / 1000.),
                    frame.chunk_count(),
                    frame.text_bytes() as f64 / 1e6,
                    if loaded.warnings.is_empty() { String::new() } else { format!(" — {}", loaded.warnings.join(" ")) },
                );
                if frame.rows() > 10_000 {
                    let cancel = AtomicBool::new(false);
                    for col in [frame.columns().len().saturating_sub(1), 2, 1, 5] {
                        let t = Instant::now();
                        let spec = ViewSpec { sort: Some((col, Order::Descending)), ..Default::default() };
                        let order = herse_data::compute_order(&frame, &spec, &cancel).unwrap();
                        println!(
                            "  sort by {} ({:?}): {:.0} ms for {} rows",
                            frame.columns()[col].name,
                            frame.kind(col),
                            t.elapsed().as_secs_f64() * 1000.,
                            order.len()
                        );
                    }
                    let t = Instant::now();
                    let spec = ViewSpec { search: "gizmo".into(), ..Default::default() };
                    let order = herse_data::compute_order(&frame, &spec, &cancel).unwrap();
                    println!("  search \"gizmo\": {:.0} ms, {} rows", t.elapsed().as_secs_f64() * 1000., order.len());
                    let t = Instant::now();
                    let profiles: Vec<_> = (0..frame.columns().len()).map(|c| herse_data::profile(&frame, c, None, &cancel)).collect();
                    println!("  profiles of {} columns: {:.0} ms", profiles.len(), t.elapsed().as_secs_f64() * 1000.);
                    // The profiles a filter shows: the rows in view, in the same bins.
                    let t = Instant::now();
                    let like: Vec<_> = profiles
                        .iter()
                        .enumerate()
                        .map(|(c, p)| p.as_ref().and_then(|p| herse_data::profile_like(&frame, c, &order, p, &cancel)))
                        .collect();
                    println!("  profiles of the {} rows in view: {:.0} ms ({} columns)", order.len(), t.elapsed().as_secs_f64() * 1000., like.len());
                    let t = Instant::now();
                    let stats: Vec<_> = (0..frame.columns().len()).map(|c| frame.stats(c)).collect();
                    println!("  stats for {} columns: {:.2} ms", stats.len(), t.elapsed().as_secs_f64() * 1000.);
                }
            }
        }
    }
}
