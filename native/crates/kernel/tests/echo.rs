//! Protocol edge cases against a raw-pyzmq fake kernel (tests/fixtures/echo_kernel.py).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use herse_kernel::{InterruptMode, Kernel, KernelEvent, KernelSpec, KernelStatus, LaunchOptions};

fn echo_spec(flags: &[&str]) -> KernelSpec {
    let mut argv = vec![
        python3().argv[0].clone(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/echo_kernel.py").to_owned(),
    ];
    argv.extend(flags.iter().map(|f| f.to_string()));
    argv.extend(["-f".to_owned(), "{connection_file}".to_owned()]);
    KernelSpec {
        name: "echo".into(),
        display_name: "Echo".into(),
        language: "echo".into(),
        argv,
        env: Default::default(),
        resource_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        interrupt_mode: InterruptMode::Signal,
        metadata: Default::default(),
    }
}

fn launch_spec(spec: &KernelSpec, opts: LaunchOptions) -> Session {
    let slot = Slot::acquire();
    let (kernel, events) = Kernel::launch_with(spec, Path::new("."), opts).expect("launch");
    Session {
        kernel,
        events,
        _slot: slot,
    }
}

fn died_reason(s: &Session, timeout: Duration) -> String {
    match s.wait_for(timeout, |e| {
        matches!(
            e,
            KernelEvent::Died { .. } | KernelEvent::Status(KernelStatus::Idle)
        )
    }) {
        KernelEvent::Died { reason } => reason,
        other => panic!("expected Died, got {other:?}"),
    }
}

#[test]
fn reply_waits_for_idle_but_not_forever() {
    let s = launch_spec(&echo_spec(&[]), LaunchOptions::default());
    s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Idle))
    });
    assert_eq!(s.kernel.info().unwrap().implementation, "echo");

    // Lost idle, nothing after it: the reply is delivered after the grace period.
    let t = Instant::now();
    let run = s.run("skip-idle");
    assert_eq!(run.status, "ok");
    assert_eq!(run.result_text().as_deref(), Some("skip-idle"));
    let waited = t.elapsed();
    assert!(
        waited >= Duration::from_millis(2500) && waited < Duration::from_secs(10),
        "{waited:?}"
    );

    // Lost idle followed by another execution: delivered as soon as the next
    // execution's IOPub traffic shows the first one is over, after all of its
    // own outputs, and before the next reply.
    let t = Instant::now();
    let a = s.kernel.execute("skip-idle");
    let b = s.kernel.execute("2");
    let mut order = Vec::new();
    while order.last().map(String::as_str) != Some("reply b") {
        match next(&s.events, TIMEOUT).expect("event") {
            KernelEvent::ExecuteReply { parent_msg_id, .. } => order.push(if parent_msg_id == a {
                "reply a".into()
            } else {
                "reply b".into()
            }),
            KernelEvent::Iopub {
                parent_msg_id: Some(p),
                msg_type,
                ..
            } if msg_type == "execute_result" => order.push(if p == a {
                "a result".into()
            } else {
                assert_eq!(p, b);
                "b result".into()
            }),
            _ => {}
        }
    }
    let pos = |what: &str| {
        order
            .iter()
            .position(|o| o == what)
            .unwrap_or_else(|| panic!("{what} missing: {order:?}"))
    };
    assert!(pos("a result") < pos("reply a"), "{order:?}");
    assert!(pos("reply a") < pos("reply b"), "{order:?}");
    assert!(pos("b result") < pos("reply b"), "{order:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn port_collision_relaunches_with_fresh_ports() {
    let marker = std::env::temp_dir().join(format!("herse-collide-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let echo = echo_spec(&[]);
    let script = format!(
        "if [ ! -e '{m}' ]; then touch '{m}'; echo 'zmq.error.ZMQError: Address already in use (addr=tcp://127.0.0.1:1)' >&2; exit 1; fi; exec '{py}' '{k}' -f \"$0\"",
        m = marker.display(),
        py = echo.argv[0],
        k = echo.argv[1],
    );
    let spec = KernelSpec {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            script,
            "{connection_file}".into(),
        ],
        ..echo
    };
    let s = launch_spec(&spec, LaunchOptions::default());
    let timing = match s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Ready { .. } | KernelEvent::Died { .. })
    }) {
        KernelEvent::Ready { timing, .. } => timing,
        other => panic!("{other:?}"),
    };
    assert_eq!(timing.attempt, 2);
    assert_eq!(s.run("ok").result_text().as_deref(), Some("ok"));
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn badly_signed_kernel_never_becomes_ready() {
    let s = launch_spec(
        &echo_spec(&["--bad-key"]),
        LaunchOptions {
            startup_timeout: Duration::from_secs(3),
            ..Default::default()
        },
    );
    let pid = s.kernel.pid().unwrap();
    let reason = died_reason(&s, TIMEOUT);
    assert!(reason.contains("did not become ready"), "{reason}");
    assert!(wait_until(Duration::from_secs(3), || !process_alive(pid)));
}

#[test]
fn kernel_that_never_listens_times_out_and_is_killed() {
    let mut spec = echo_spec(&[]);
    spec.argv = vec!["/bin/sleep".into(), "30".into()];
    let s = launch_spec(
        &spec,
        LaunchOptions {
            startup_timeout: Duration::from_secs(1),
            ..Default::default()
        },
    );
    let pid = s.kernel.pid().unwrap();
    let t = Instant::now();
    let reason = died_reason(&s, TIMEOUT);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(
        reason.contains("listening") || reason.contains("ready"),
        "{reason}"
    );
    assert!(wait_until(Duration::from_secs(3), || !process_alive(pid)));
}

#[test]
fn shutdown_during_startup() {
    let s = launch_spec(&echo_spec(&[]), LaunchOptions::default());
    let pid = s.kernel.pid().unwrap();
    let queued = s.kernel.execute("never");
    s.kernel.shutdown();
    let run = s.collect(&queued, TIMEOUT);
    assert_eq!(run.status, "aborted");
    assert!(wait_until(Duration::from_secs(5), || !process_alive(pid)));
    assert!(wait_until(Duration::from_secs(5), || s.events.is_closed()
        || s.kernel.status() == KernelStatus::Dead));
}

#[test]
fn many_restarts_in_a_row() {
    let s = launch_spec(&echo_spec(&[]), LaunchOptions::default());
    s.kernel.wait_ready(TIMEOUT).unwrap();
    let mut pids = vec![s.kernel.pid().unwrap()];
    for _ in 0..3 {
        s.kernel.restart().unwrap();
    }
    s.kernel.wait_ready(TIMEOUT).unwrap();
    pids.push(s.kernel.pid().unwrap());
    assert_eq!(s.run("after").result_text().as_deref(), Some("after"));
    // Only the newest process survives.
    assert!(wait_until(Duration::from_secs(5), || !process_alive(
        pids[0]
    )));
    assert!(process_alive(pids[1]));
}

/// Re-runs this test binary as a fake "app" that launches a kernel, then
/// SIGKILLs that app: no destructor or atexit runs, so only the watchdog can
/// clean up. (The echo kernel has no parent poller of its own.)
#[test]
fn app_crash_does_not_orphan_kernels() {
    use std::io::BufRead;
    if std::env::var_os("HERSE_ORPHAN_CHILD").is_some() {
        let s = launch_spec(&echo_spec(&[]), LaunchOptions::default());
        s.kernel.wait_ready(TIMEOUT).unwrap();
        let pid = s.kernel.pid().unwrap();
        // Own process group, so the terminal's Ctrl-C (sent to the app's group) misses it.
        let (kernel_pgid, app_pgid) = unsafe { (libc::getpgid(pid as i32), libc::getpgid(0)) };
        assert_eq!(kernel_pgid, pid as i32);
        assert_ne!(kernel_pgid, app_pgid);
        println!("\nKERNEL_PID={pid}");
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let mut app = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app_crash_does_not_orphan_kernels",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("HERSE_ORPHAN_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = std::io::BufReader::new(app.stdout.take().unwrap());
    let pid: u32 = stdout
        .lines()
        .map_while(Result::ok)
        .find_map(|l| {
            l.split("KERNEL_PID=")
                .nth(1)
                .map(|p| p.trim().parse().unwrap())
        })
        .expect("child reported its kernel pid");
    assert!(process_alive(pid));
    app.kill().unwrap(); // SIGKILL
    app.wait().unwrap();
    let t = Instant::now();
    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(pid)),
        "kernel orphaned"
    );
    eprintln!(
        "orphaned kernel killed {:?} after the app died",
        t.elapsed()
    );
}
