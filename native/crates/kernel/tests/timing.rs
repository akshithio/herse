//! Boot and round-trip measurements against the real `python3` kernel.
//! Run alone for meaningful numbers:
//!
//! ```sh
//! cargo test -p herse-kernel --test timing -- --nocapture
//! HERSE_ASSERT_TIMING=1 cargo test ...   # also enforce the < 5 ms round-trip target
//! ```

mod common;

use std::time::{Duration, Instant};

use common::*;
use herse_kernel::{Kernel, KernelEvent, KernelStatus};

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn summarize(label: &str, mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let median = percentile(&samples, 0.5);
    eprintln!(
        "{label:<28} n={:<4} min {:>8.2?}  median {:>8.2?}  p90 {:>8.2?}  p99 {:>8.2?}",
        samples.len(),
        samples[0],
        median,
        percentile(&samples, 0.9),
        percentile(&samples, 0.99),
    );
    median
}

fn round_trip(s: &Session, code: &str) -> Duration {
    let t = Instant::now();
    let id = s.kernel.execute(code);
    loop {
        match next(&s.events, TIMEOUT).expect("event") {
            KernelEvent::ExecuteReply {
                parent_msg_id,
                status,
                ..
            } if parent_msg_id == id => {
                assert_eq!(status, "ok");
                return t.elapsed();
            }
            KernelEvent::Died { reason } => panic!("{reason}"),
            _ => {}
        }
    }
}

#[test]
fn timings() {
    let spec = python3();
    let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    eprintln!();

    // Discovery.
    let mut disc = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let specs = herse_kernel::discover();
        disc.push(t.elapsed());
        assert!(!specs.is_empty());
    }
    summarize("discover()", disc);

    // Boot to idle, three sequential cold launches.
    let mut launch_calls = Vec::new();
    let mut boots = Vec::new();
    let mut last = None;
    for _ in 0..3 {
        let t = Instant::now();
        let (kernel, events) = Kernel::launch(&spec, cwd).unwrap();
        launch_calls.push(t.elapsed());
        loop {
            match next(&events, TIMEOUT).expect("event") {
                KernelEvent::Ready { timing, .. } => {
                    eprintln!(
                        "  boot: spawn {:.1?}, sockets {:.0?}, ready {:.0?} (wall {:.0?})",
                        timing.spawn,
                        timing.connect,
                        timing.ready,
                        t.elapsed()
                    );
                }
                KernelEvent::Status(KernelStatus::Idle) => break,
                KernelEvent::Died { reason } => panic!("{reason}"),
                _ => {}
            }
        }
        boots.push(t.elapsed());
        if let Some((k, _)) = last.replace((kernel, events)) {
            let k: Kernel = k;
            k.shutdown_and_wait(Duration::from_secs(5));
        }
    }
    summarize("launch() call", launch_calls);
    summarize("boot to idle", boots);
    let (kernel, events) = last.unwrap();
    let s = Session {
        kernel,
        events,
        _slot: Slot::acquire(),
    };

    // Round trips after warm-up.
    for _ in 0..30 {
        round_trip(&s, "1");
    }
    let rt: Vec<Duration> = (0..300).map(|_| round_trip(&s, "1")).collect();
    let median = summarize("round trip `1`", rt);
    let rt: Vec<Duration> = (0..100).map(|_| round_trip(&s, "print('x')")).collect();
    summarize("round trip `print('x')`", rt);

    // Completion.
    let rt: Vec<Duration> = (0..50)
        .map(|_| {
            let t = Instant::now();
            smol::block_on(s.kernel.complete("import o", 8).recv())
                .unwrap()
                .unwrap();
            t.elapsed()
        })
        .collect();
    summarize("complete('import o')", rt);

    // Burst throughput.
    let t = Instant::now();
    let ids: Vec<String> = (0..1000)
        .map(|i| s.kernel.execute(&format!("{i}")))
        .collect();
    let mut left = ids.len();
    while left > 0 {
        if let KernelEvent::ExecuteReply { .. } = next(&s.events, TIMEOUT).expect("event") {
            left -= 1;
        }
    }
    let burst = t.elapsed();
    s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Idle))
    });
    eprintln!(
        "{:<28} {burst:.0?} total, {:.0} µs/execute",
        "burst of 1000 `i`",
        burst.as_micros() as f64 / 1000.0
    );

    // Restart to idle.
    let mut restarts = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        s.kernel.restart().unwrap();
        s.wait_for(TIMEOUT, |e| {
            matches!(e, KernelEvent::Status(KernelStatus::Idle))
        });
        restarts.push(t.elapsed());
    }
    summarize("restart to idle", restarts);

    let t = Instant::now();
    assert!(s.kernel.shutdown_and_wait(Duration::from_secs(5)));
    eprintln!("{:<28} {:.0?}", "graceful shutdown", t.elapsed());

    // Transport floor: a raw-pyzmq echo kernel without IPython's per-cell work.
    let mut echo = spec.clone();
    echo.name = "echo".into();
    echo.argv = vec![
        spec.argv[0].clone(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/echo_kernel.py").into(),
        "-f".into(),
        "{connection_file}".into(),
    ];
    let t = Instant::now();
    let (kernel, events) = Kernel::launch(&echo, cwd).unwrap();
    let e = Session {
        kernel,
        events,
        _slot: Slot::acquire(),
    };
    e.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Idle))
    });
    eprintln!("{:<28} {:.0?}", "echo kernel boot to idle", t.elapsed());
    for _ in 0..50 {
        round_trip(&e, "1");
    }
    let rt: Vec<Duration> = (0..500).map(|_| round_trip(&e, "1")).collect();
    summarize("echo kernel round trip", rt);
    e.kernel.shutdown_and_wait(Duration::from_secs(5));

    if std::env::var_os("HERSE_ASSERT_TIMING").is_some() {
        assert!(
            median < Duration::from_millis(5),
            "round trip median {median:?} >= 5 ms"
        );
    }
}
