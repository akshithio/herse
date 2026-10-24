//! Throughput of `ansi` and `stream` on ~10 MB synthetic logs.
//!
//! `cargo run -p herse-rich --release --example bench`
use std::hint::black_box;
use std::time::{Duration, Instant};

use herse_rich::{ansi, stream};

/// A colored log in the style of pytest, pip, IPython tracebacks and rich.
fn colored_log(target: usize) -> String {
    let lines = [
        "\x1b[1m============================= test session starts ==============================\x1b[0m\n",
        "tests/test_model.py \x1b[32m.\x1b[0m\x1b[32m.\x1b[0m\x1b[32m.\x1b[0m\x1b[31mF\x1b[0m\x1b[32m.\x1b[0m\x1b[33ms\x1b[0m\x1b[32m                                  [ 42%]\x1b[0m\n",
        "\x1b[31m\x1b[1m_______________________________ test_forward ________________________________\x1b[0m\n",
        "\x1b[1m\x1b[31mE       AssertionError: assert tensor([1., 2.]) == tensor([1., 3.])\x1b[0m\n",
        "\x1b[0;31m---------------------------------------------------------------------------\x1b[0m\n",
        "\x1b[0;31mValueError\x1b[0m                                Traceback (most recent call last)\n",
        "Cell \x1b[0;32mIn[3], line 7\x1b[0m\n",
        "\x1b[1;32m      5\x1b[0m \x1b[38;5;28;01mfor\x1b[39;00m i \x1b[38;5;129;01min\x1b[39;00m \x1b[38;5;28mrange\x1b[39m(\x1b[38;5;241m10\x1b[39m):\n",
        "\x1b[0;32m----> 7\x1b[0m     \x1b[43mmodel\x1b[49m\x1b[38;5;241;43m.\x1b[39;49m\x1b[43mfit\x1b[49m(X, y)\n",
        "Collecting numpy>=1.24 (from pandas)\n",
        "  Downloading numpy-2.1.0-cp312-cp312-macosx_14_0_arm64.whl (5.1 MB)\n",
        "     \x1b[38;2;249;38;114m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m \x1b[32m5.1/5.1 MB\x1b[0m \x1b[31m12.3 MB/s\x1b[0m eta \x1b[36m0:00:00\x1b[0m\n",
        "\x1b[2m2024-05-01 12:00:01\x1b[0m \x1b[34mINFO    \x1b[0m Epoch 3/10 - loss: \x1b[1m0.2345\x1b[0m - acc: 0.9123 \x1b]8;;file:///tmp/run.log\x1b\\run.log\x1b]8;;\x1b\\\n",
        "plain line without any escapes, the most common kind of output in practice......\n",
    ];
    let mut s = String::with_capacity(target + 256);
    let mut i = 0;
    while s.len() < target {
        s.push_str(lines[i % lines.len()]);
        i += 1;
    }
    s
}

fn plain_log(target: usize) -> String {
    let mut s = String::with_capacity(target + 128);
    let mut i = 0u64;
    while s.len() < target {
        s.push_str(&format!(
            "step {i:>8}  loss=0.{:06}  lr=3e-4  grad_norm=1.{:03}\n",
            i * 7919 % 1_000_000,
            i % 1000
        ));
        i += 1;
    }
    s
}

fn tqdm_log(target: usize) -> String {
    let mut s = String::with_capacity(target + 128);
    let mut i = 0u64;
    while s.len() < target {
        let pct = i % 101;
        s.push_str(&format!(
            "\r{pct:3}%|{:<25}| {pct}/100 [00:{:02}<00:10, 9.{}it/s]",
            "█".repeat((pct / 4) as usize),
            i % 60,
            i % 10
        ));
        if pct == 100 {
            s.push('\n');
        }
        i += 1;
    }
    s
}

/// CPU time of the calling thread: stable even when the machine is busy.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer, supported clock id on macOS and Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn bench(name: &str, bytes: usize, mut f: impl FnMut()) {
    f(); // warm up
    let mut best = Duration::MAX;
    let mut best_wall = Duration::MAX;
    for _ in 0..7 {
        let t = Instant::now();
        let c = thread_cpu();
        f();
        best = best.min(thread_cpu() - c);
        best_wall = best_wall.min(t.elapsed());
    }
    let mbps = bytes as f64 / 1e6 / best.as_secs_f64();
    println!(
        "{name:<44} {:>8.2} ms cpu {:>8.2} ms wall {:>8.0} MB/s",
        best.as_secs_f64() * 1e3,
        best_wall.as_secs_f64() * 1e3,
        mbps
    );
}

fn main() {
    let size = 10 << 20;
    let colored = colored_log(size);
    let plain = plain_log(size);
    let tqdm = tqdm_log(size);
    let parsed = ansi::parse(&colored);
    println!(
        "colored log: {:.1} MB, {} ESC, {} spans after parse, {} links",
        colored.len() as f64 / 1e6,
        colored.bytes().filter(|&b| b == 0x1b).count(),
        parsed.spans.len(),
        parsed.links.len()
    );

    bench("ansi::parse   colored 10 MB", colored.len(), || {
        black_box(ansi::parse(black_box(&colored)));
    });
    bench("ansi::strip   colored 10 MB", colored.len(), || {
        black_box(ansi::strip(black_box(&colored)));
    });
    bench("ansi::parse   plain 10 MB", plain.len(), || {
        black_box(ansi::parse(black_box(&plain)));
    });
    bench("ansi::has_escapes plain 10 MB", plain.len(), || {
        black_box(ansi::has_escapes(black_box(&plain)));
    });
    bench(
        "stream::normalize plain 10 MB (borrow)",
        plain.len(),
        || {
            black_box(stream::normalize(black_box(&plain)));
        },
    );
    bench(
        "stream::normalize tqdm 10 MB (\\r heavy)",
        tqdm.len(),
        || {
            black_box(stream::normalize(black_box(&tqdm)));
        },
    );
    bench(
        "stream::normalize colored 10 MB + 1 \\r",
        colored.len(),
        || {
            let mut s = String::with_capacity(colored.len() + 1);
            s.push('\r');
            s.push_str(&colored);
            black_box(stream::normalize(black_box(&s)));
        },
    );
    // Streaming: 4 KB iopub chunks of plain output.
    bench("stream::append plain, 4 KB chunks", plain.len(), || {
        let mut buf = String::new();
        for chunk in plain.as_bytes().chunks(4096) {
            buf.push_str(""); // keep the loop honest
            stream::append(&mut buf, std::str::from_utf8(chunk).unwrap_or(""));
        }
        black_box(buf);
    });
    // Streaming tqdm: one update per message.
    let updates: Vec<&str> = tqdm.split_inclusive('\r').collect();
    bench(
        "stream::append tqdm, 1 update per chunk",
        tqdm.len(),
        || {
            let mut buf = String::new();
            for u in &updates {
                stream::append(&mut buf, u);
            }
            black_box(buf);
        },
    );
    let lines = stream::line_count(&plain);
    bench(
        "stream::truncate_for_display plain 10 MB",
        plain.len(),
        || {
            black_box(stream::truncate_for_display(
                black_box(&plain),
                5_000,
                1 << 20,
            ));
        },
    );
    println!("({lines} lines in the plain log)");
    bench("parse(normalize(x)) colored 10 MB", colored.len(), || {
        black_box(ansi::parse(&stream::normalize(black_box(&colored))));
    });
}
