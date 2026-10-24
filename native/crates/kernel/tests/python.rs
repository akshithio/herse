//! Integration tests against the real `python3` kernel (ipykernel).

mod common;

use std::time::{Duration, Instant};

use common::*;
use herse_kernel::{
    HeartbeatConfig, InterruptMode, Kernel, KernelEvent, KernelStatus, LaunchOptions,
};

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn print_streams_stdout_in_documented_order() {
    let s = start();
    let id = s.kernel.execute("print('hi')");
    let mut seen = Vec::new();
    loop {
        let e = next(&s.events, TIMEOUT).expect("event");
        let label = match &e {
            KernelEvent::Status(st) => format!("Status({st:?})"),
            KernelEvent::Iopub {
                msg_type,
                content,
                parent_msg_id,
            } => {
                assert_eq!(parent_msg_id.as_deref(), Some(id.as_str()));
                if msg_type == "status" {
                    format!(
                        "iopub:status:{}",
                        content["execution_state"].as_str().unwrap()
                    )
                } else {
                    format!("iopub:{msg_type}")
                }
            }
            KernelEvent::ExecuteReply { status, .. } => format!("reply:{status}"),
            other => format!("{other:?}"),
        };
        let done = label == "Status(Idle)";
        seen.push(label);
        if done {
            break;
        }
    }
    assert_eq!(
        seen,
        [
            "Status(Busy)",
            "iopub:status:busy",
            "iopub:execute_input",
            "iopub:stream",
            "iopub:status:idle",
            "reply:ok",
            "Status(Idle)"
        ]
    );
    let run = s.run("print('hi')");
    assert_eq!(run.stdout(), "hi\n");
    assert_eq!(run.status, "ok");
}

#[test]
fn expression_gives_execute_result() {
    let s = start();
    let run = s.run("1+1");
    assert_eq!(run.result_text().as_deref(), Some("2"));
    let result = run.of_type("execute_result")[0];
    assert_eq!(result["execution_count"].as_i64(), run.execution_count);
    assert_eq!(run.status, "ok");
}

#[test]
fn exception_gives_error_with_traceback() {
    let s = start();
    let run = s.run("raise ValueError('x')");
    assert_eq!(run.status, "error");
    let err = run.of_type("error");
    assert_eq!(err.len(), 1);
    assert_eq!(err[0]["ename"], "ValueError");
    assert_eq!(err[0]["evalue"], "x");
    let tb = err[0]["traceback"].as_array().unwrap();
    assert!(!tb.is_empty());
    assert!(
        tb.iter()
            .any(|l| l.as_str().unwrap().contains("ValueError"))
    );
}

#[test]
fn display_image_gives_png_display_data() {
    let s = start();
    let run = s.run(&format!(
        "from IPython.display import Image, display\nimport base64\ndisplay(Image(data=base64.b64decode('{PNG_1X1}')))"
    ));
    assert_eq!(run.status, "ok");
    let dd = run.of_type("display_data");
    assert_eq!(dd.len(), 1, "{run:#?}");
    let png = dd[0]["data"]["image/png"].as_str().expect("image/png");
    assert_eq!(png.trim(), PNG_1X1);
}

#[test]
fn input_request_and_reply() {
    let s = start();
    let id = s
        .kernel
        .execute("name = input('name? ')\nprint('hello ' + name)");
    match s.wait_for(TIMEOUT, |e| matches!(e, KernelEvent::InputRequest { .. })) {
        KernelEvent::InputRequest {
            parent_msg_id,
            prompt,
            password,
        } => {
            assert_eq!(parent_msg_id, id);
            assert_eq!(prompt, "name? ");
            assert!(!password);
        }
        _ => unreachable!(),
    }
    s.kernel.input_reply("Ada");
    let run = s.collect(&id, TIMEOUT);
    assert_eq!(run.status, "ok");
    assert!(run.stdout().contains("hello Ada"), "{run:#?}");
}

#[test]
fn getpass_is_a_password_request() {
    let s = start();
    let id = s
        .kernel
        .execute("import getpass\npw = getpass.getpass('pw: ')\nprint(len(pw))");
    match s.wait_for(TIMEOUT, |e| matches!(e, KernelEvent::InputRequest { .. })) {
        KernelEvent::InputRequest { password, .. } => assert!(password),
        _ => unreachable!(),
    }
    s.kernel.input_reply("secret");
    assert_eq!(s.collect(&id, TIMEOUT).stdout(), "6\n");
}

fn assert_interrupts(s: &Session) {
    // The first traceback in a fresh kernel lazily imports IPython's
    // formatting machinery (hundreds of ms); keep that out of the measurement.
    assert_eq!(s.run("1/0").status, "error");
    let id = s.kernel.execute("import time\ntime.sleep(30)");
    s.wait_for(
        TIMEOUT,
        |e| matches!(e, KernelEvent::Iopub { msg_type, .. } if msg_type == "execute_input"),
    );
    std::thread::sleep(Duration::from_millis(200)); // inside time.sleep
    let t = Instant::now();
    s.kernel.interrupt().unwrap();
    let run = s.collect(&id, TIMEOUT);
    let took = t.elapsed();
    assert_eq!(run.status, "error");
    assert_eq!(run.of_type("error")[0]["ename"], "KeyboardInterrupt");
    assert!(took < Duration::from_secs(1), "interrupt took {took:?}");
    eprintln!("interrupt -> KeyboardInterrupt reply in {took:?}");
    // The kernel is usable afterwards.
    assert_eq!(s.run("40+2").result_text().as_deref(), Some("42"));
}

#[test]
fn interrupt_sleep_with_signal() {
    let s = start();
    assert_eq!(s.kernel.spec().interrupt_mode, InterruptMode::Signal);
    assert_interrupts(&s);
}

#[test]
fn interrupt_sleep_with_message() {
    let slot = Slot::acquire();
    let mut spec = python3();
    spec.interrupt_mode = InterruptMode::Message;
    let (kernel, events) = Kernel::launch(&spec, std::path::Path::new(".")).unwrap();
    let s = Session {
        kernel,
        events,
        _slot: slot,
    };
    s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Idle))
    });
    assert_interrupts(&s);
}

#[test]
fn restart_clears_state() {
    let s = start();
    let pid = s.kernel.pid().unwrap();
    let session = s.kernel.session_id().to_owned();
    assert_eq!(s.run("x = 1").status, "ok");
    assert_eq!(s.run("x").result_text().as_deref(), Some("1"));
    let t = Instant::now();
    s.kernel.restart().unwrap();
    // Queued before the new kernel is ready: must not be dropped.
    let id = s.kernel.execute("x");
    s.wait_for(TIMEOUT, |e| {
        matches!(e, KernelEvent::Status(KernelStatus::Restarting))
    });
    let timing = match s.wait_for(TIMEOUT, |e| matches!(e, KernelEvent::Ready { .. })) {
        KernelEvent::Ready { timing, .. } => timing,
        _ => unreachable!(),
    };
    eprintln!(
        "restart to ready: {:?} (new process boot {:?})",
        t.elapsed(),
        timing.ready
    );
    let run = s.collect(&id, TIMEOUT);
    assert_eq!(run.status, "error");
    assert_eq!(run.of_type("error")[0]["ename"], "NameError");
    assert_eq!(run.execution_count, Some(1), "execution count restarts");
    assert_ne!(s.kernel.pid().unwrap(), pid);
    assert_eq!(s.kernel.session_id(), session);
    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(pid)),
        "old kernel still alive"
    );
}

#[test]
fn restart_aborts_running_execution() {
    let s = start();
    let id = s.kernel.execute("import time\ntime.sleep(30)");
    s.wait_for(
        TIMEOUT,
        |e| matches!(e, KernelEvent::Iopub { msg_type, .. } if msg_type == "execute_input"),
    );
    s.kernel.restart().unwrap();
    let run = s.collect(&id, TIMEOUT);
    assert_eq!(run.status, "aborted");
    s.kernel.wait_ready(TIMEOUT).unwrap();
    assert_eq!(s.run("1").result_text().as_deref(), Some("1"));
}

#[test]
fn external_kill_emits_died_and_restart_revives() {
    let s = start();
    let pid = s.kernel.pid().unwrap();
    let pending = s.kernel.execute("import time\ntime.sleep(30)");
    s.wait_for(
        TIMEOUT,
        |e| matches!(e, KernelEvent::Iopub { msg_type, .. } if msg_type == "execute_input"),
    );
    let t = Instant::now();
    signal(pid, libc::SIGKILL);
    let mut saw_dead = false;
    let reason = loop {
        match next(&s.events, TIMEOUT).expect("event") {
            KernelEvent::Status(KernelStatus::Dead) => saw_dead = true,
            KernelEvent::Died { reason } => break reason,
            _ => {}
        }
    };
    let took = t.elapsed();
    eprintln!("external SIGKILL -> Died in {took:?}: {reason}");
    assert!(saw_dead, "Status(Dead) precedes Died");
    assert!(reason.contains("SIGKILL"), "{reason}");
    assert!(took < Duration::from_secs(1));
    // The in-flight execution is aborted.
    assert_eq!(s.collect(&pending, TIMEOUT).status, "aborted");
    assert_eq!(s.kernel.status(), KernelStatus::Dead);
    assert!(s.kernel.wait_ready(Duration::from_secs(1)).is_err());
    // Executing while dead aborts immediately.
    assert_eq!(s.run("1").status, "aborted");
    s.kernel.restart().unwrap();
    s.kernel.wait_ready(TIMEOUT).unwrap();
    assert_eq!(s.run("2+2").result_text().as_deref(), Some("4"));
}

#[test]
fn hung_kernel_detected_by_heartbeat() {
    let s = start_with(LaunchOptions {
        heartbeat: Some(HeartbeatConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(200),
            max_missed: 3,
        }),
        ..Default::default()
    });
    let pid = s.kernel.pid().unwrap();
    // A busy kernel still answers heartbeats (ipykernel echoes without the GIL).
    let busy = s
        .kernel
        .execute("import time\nt = time.time()\nwhile time.time() - t < 1.5: pass");
    let run = s.collect(&busy, TIMEOUT);
    assert_eq!(run.status, "ok");
    // A stopped one doesn't.
    signal(pid, libc::SIGSTOP);
    let t = Instant::now();
    let reason = match s.wait_for(Duration::from_secs(10), |e| {
        matches!(e, KernelEvent::Died { .. })
    }) {
        KernelEvent::Died { reason } => reason,
        _ => unreachable!(),
    };
    eprintln!("SIGSTOP -> Died in {:?}: {reason}", t.elapsed());
    assert!(reason.contains("heartbeat"), "{reason}");
    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(pid)),
        "hung kernel was not killed"
    );
}

#[test]
fn complete_import_o() {
    let s = start();
    let reply = smol::block_on(s.kernel.complete("import o", 8).recv())
        .unwrap()
        .unwrap();
    assert!(
        reply.matches.iter().any(|m| m == "os"),
        "{:?}",
        reply.matches
    );
    assert_eq!((reply.cursor_start, reply.cursor_end), (7, 8));
    assert_eq!(reply.byte_range("import o"), 7..8);
}

#[test]
fn inspect_print() {
    let s = start();
    let reply = smol::block_on(s.kernel.inspect("print", 5).recv())
        .unwrap()
        .unwrap();
    assert!(reply.found);
    let text = reply.text().expect("text/plain");
    assert!(text.contains("print"), "{text}");
}

#[test]
fn is_complete_and_kernel_info() {
    let s = start();
    let r = smol::block_on(s.kernel.is_complete("for i in range(3):").recv())
        .unwrap()
        .unwrap();
    assert_eq!(r.status, "incomplete");
    assert!(!r.indent.is_empty());
    let r = smol::block_on(s.kernel.is_complete("x = 1").recv())
        .unwrap()
        .unwrap();
    assert_eq!(r.status, "complete");

    let info = s.kernel.info().expect("info after ready");
    assert_eq!(info.implementation, "ipython");
    assert_eq!(info.language_info.name, "python");
    assert_eq!(info.language_info.file_extension.as_deref(), Some(".py"));
    assert_eq!(
        info.language_info.pygments_lexer.as_deref(),
        Some("ipython3")
    );
    assert_eq!(info.language_info.codemirror_mode_name(), Some("ipython"));
    assert!(info.protocol_version.starts_with('5'));
    assert!(s.kernel.timing().unwrap().ready > Duration::ZERO);

    let history = smol::block_on(
        s.kernel.request("history_request", serde_json::json!({"hist_access_type": "tail", "n": 1, "output": false, "raw": true})).recv(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(history.msg_type, "history_reply");
}

#[test]
fn burst_of_1000_keeps_order_and_loses_nothing() {
    let s = start();
    let t = Instant::now();
    let ids: Vec<String> = (0..1000)
        .map(|i| s.kernel.execute(&format!("print({i})")))
        .collect();
    let mut outputs: std::collections::HashMap<String, String> = Default::default();
    let mut replies = Vec::new();
    let mut busy_flips = 0;
    while replies.len() < ids.len() {
        match next(&s.events, TIMEOUT).expect("event") {
            KernelEvent::Iopub {
                parent_msg_id: Some(p),
                msg_type,
                content,
            } if msg_type == "stream" => {
                outputs
                    .entry(p)
                    .or_default()
                    .push_str(content["text"].as_str().unwrap());
            }
            KernelEvent::ExecuteReply {
                parent_msg_id,
                status,
                execution_count,
            } => {
                assert_eq!(status, "ok");
                replies.push((parent_msg_id, execution_count.unwrap()));
            }
            KernelEvent::Status(KernelStatus::Busy) => busy_flips += 1,
            KernelEvent::Died { reason } => panic!("{reason}"),
            _ => {}
        }
    }
    let elapsed = t.elapsed();
    let got: Vec<&String> = replies.iter().map(|(id, _)| id).collect();
    let want: Vec<&String> = ids.iter().collect();
    assert_eq!(got, want, "replies out of order");
    assert!(
        replies.windows(2).all(|w| w[1].1 == w[0].1 + 1),
        "execution counts not consecutive"
    );
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            outputs.get(id).map(String::as_str),
            Some(format!("{i}\n").as_str()),
            "output of #{i}"
        );
    }
    assert_eq!(busy_flips, 1, "Busy/Idle is coalesced over a burst");
    eprintln!(
        "1000 executes: {elapsed:?} ({:.0} µs each)",
        elapsed.as_micros() as f64 / 1000.0
    );
}

#[test]
fn executes_before_ready_are_queued_not_dropped() {
    let s = launch_with(LaunchOptions::default());
    let first = s.kernel.execute("print('first')");
    let second = s.kernel.execute("print('second')");
    assert_eq!(s.collect(&first, TIMEOUT).stdout(), "first\n");
    assert_eq!(s.collect(&second, TIMEOUT).stdout(), "second\n");
}

#[test]
fn interrupt_before_ready_aborts_queue() {
    let s = launch_with(LaunchOptions::default());
    let id = s.kernel.execute("print('never')");
    s.kernel.interrupt().unwrap();
    let run = s.collect(&id, TIMEOUT);
    assert_eq!(run.status, "aborted");
    assert!(run.iopub.is_empty());
    s.kernel.wait_ready(TIMEOUT).unwrap();
}

#[test]
fn stop_on_error_aborts_queued_cells() {
    let s = start();
    let a = s.kernel.execute("raise RuntimeError('boom')");
    let b = s.kernel.execute("print('should not run')");
    assert_eq!(s.collect(&a, TIMEOUT).status, "error");
    let run_b = s.collect(&b, TIMEOUT);
    assert_eq!(run_b.status, "aborted");
    assert_eq!(run_b.stdout(), "");
}

#[test]
fn payload_for_help() {
    let s = start();
    let id = s.kernel.execute("len?");
    let payload = match s.wait_for(TIMEOUT, |e| matches!(e, KernelEvent::ExecutePayload { .. })) {
        KernelEvent::ExecutePayload {
            parent_msg_id,
            payload,
        } => {
            assert_eq!(parent_msg_id, id);
            payload
        }
        _ => unreachable!(),
    };
    assert_eq!(payload[0]["source"], "page");
    assert_eq!(s.collect(&id, TIMEOUT).status, "ok");
}

#[test]
fn shutdown_kills_process_and_closes_channel() {
    let s = start();
    let pid = s.kernel.pid().unwrap();
    let file = s.kernel.connection_file();
    assert!(file.exists());
    let t = Instant::now();
    assert!(s.kernel.shutdown_and_wait(Duration::from_secs(5)));
    eprintln!("graceful shutdown: {:?}", t.elapsed());
    assert!(!process_alive(pid));
    assert!(!file.exists(), "connection file removed");
    assert_eq!(s.kernel.status(), KernelStatus::Dead);
    // Channel drains to Status(Dead) then closes.
    let mut last = None;
    while let Some(e) = next(&s.events, Duration::from_secs(5)) {
        last = Some(e);
    }
    assert!(
        matches!(last, Some(KernelEvent::Status(KernelStatus::Dead))),
        "{last:?}"
    );
    assert!(s.events.is_closed());
    assert!(s.kernel.restart().is_err());
}

#[test]
fn dropping_last_handle_shuts_down() {
    let s = start();
    let pid = s.kernel.pid().unwrap();
    let Session {
        kernel,
        events,
        _slot,
    } = s;
    let clone = kernel.clone();
    drop(kernel);
    assert!(process_alive(pid), "a clone is still alive");
    drop(clone);
    assert!(
        wait_until(Duration::from_secs(5), || !process_alive(pid)),
        "kernel outlived its handles"
    );
    drop(events);
}

#[test]
fn launch_failure_is_an_error_and_startup_crash_is_died() {
    let mut spec = python3();
    spec.argv[0] = "/nonexistent/python".into();
    assert!(Kernel::launch(&spec, std::path::Path::new(".")).is_err());

    let mut spec = python3();
    spec.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "echo 'ModuleNotFoundError: no ipykernel' >&2; exit 3".into(),
    ];
    let (kernel, events) = Kernel::launch(&spec, std::path::Path::new(".")).unwrap();
    let reason = loop {
        if let KernelEvent::Died { reason } = next(&events, TIMEOUT).expect("event") {
            break reason;
        }
    };
    assert!(
        reason.contains("exit code 3") && reason.contains("no ipykernel"),
        "{reason}"
    );
    assert!(kernel.wait_ready(Duration::from_secs(1)).is_err());
}

#[test]
fn concurrent_kernels_are_independent() {
    let a = start();
    let b = start();
    assert_eq!(a.run("v = 'a'").status, "ok");
    assert_eq!(b.run("v = 'b'").status, "ok");
    assert_eq!(a.run("v").result_text().as_deref(), Some("'a'"));
    assert_eq!(b.run("v").result_text().as_deref(), Some("'b'"));
    assert_ne!(a.kernel.pid(), b.kernel.pid());
}

#[test]
fn large_outputs_arrive_intact() {
    let s = start();
    let t = Instant::now();
    let run = s.run(
        "import base64, os\n\
         print('x' * 5_000_000)\n\
         from IPython.display import display\n\
         display({'image/png': base64.b64encode(os.urandom(3_000_000)).decode()}, raw=True)\n\
         for i in range(20000): print(i)",
    );
    assert_eq!(run.status, "ok");
    let stdout = run.stdout();
    assert!(stdout.starts_with(&"x".repeat(5_000_000)));
    let tail: Vec<&str> = stdout[5_000_001..].lines().collect();
    assert_eq!(tail.len(), 20000);
    assert!(
        tail.iter()
            .enumerate()
            .all(|(i, l)| l.parse::<usize>() == Ok(i))
    );
    let png = run.of_type("display_data")[0]["data"]["image/png"]
        .as_str()
        .unwrap()
        .len();
    assert_eq!(png, 4_000_000);
    eprintln!("~9 MB of output in {:?}", t.elapsed());
}
