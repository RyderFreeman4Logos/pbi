use super::*;
use std::os::unix::process::ExitStatusExt;

// libtest has its own main. Only this exact test is selected in the owned
// same-image child; protocol fd 3 is independent of libtest's stdout.
#[test]
fn worker_entry() {
    if std::env::args().any(|arg| arg == "--exact") {
        worker_exit();
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn spans_reject_malformed_untrusted_responses() {
    let source = "é/'x'/";
    for spans in [
        vec![(1, 2)],
        vec![(0, 99)],
        vec![(2, 2)],
        vec![(2, 5), (4, 6)],
    ] {
        assert!(decode(source, &encode(&spans)).is_err());
    }
    let valid = encode(&[(2, 7)]);
    assert_eq!(decode(source, &valid).expect("valid byte span"), [(2, 7)]);
    for bytes in [
        b"[]".to_vec(),
        valid[..7].to_vec(),
        [valid.clone(), vec![0]].concat(),
    ] {
        assert!(decode(source, &bytes).is_err());
    }
}

#[test]
fn worker_projection_preserves_utf8_and_masks_only_literals() {
    let source = "// café\r\nconst r = /[\"'] hidden/; const s = 'é'; const t = `hidden ${1}`;\r\nfunction real() {return 1 / 2;}\n";
    let code = project(source, false, deadline()).expect("contained grammar");
    assert_eq!(code.len(), source.len());
    assert!(!code.contains("hidden"));
    assert!(!code.contains("café"));
    assert!(code.contains("function real() {return 1 / 2;}"));
    assert_eq!(
        source.match_indices(['\r', '\n']).collect::<Vec<_>>(),
        code.match_indices(['\r', '\n']).collect::<Vec<_>>()
    );
    assert!(project(
        "const r = /unterminated\nfunction visible() {}",
        false,
        deadline()
    )
    .is_err());
    assert!(project(&" ".repeat(MAX_SOURCE + 1), false, deadline()).is_err());
    assert!(project("1;", false, Instant::now()).is_err());
}

#[test]
fn actual_cap_sized_recursive_parser_aborts_are_reaped() {
    for source in [
        format!(
            "{}1{};",
            "(".repeat((MAX_SOURCE - 2) / 2),
            ")".repeat((MAX_SOURCE - 2) / 2)
        ),
        format!("{}1;", "!".repeat(MAX_SOURCE - 2)),
    ] {
        assert_eq!(source.len(), MAX_SOURCE);
        let output = exchange(&source, 0, deadline()).expect("supervised parser exit");
        assert_eq!(output.status.signal(), Some(libc::SIGABRT));
        assert!(output.bytes.is_empty());
        assert_reaped(output.pid);
        assert!(project("function still_alive() {}", false, deadline()).is_ok());
    }
}

#[test]
fn cap_sized_flat_source_keeps_real_function() {
    let mut source = "var x = /[\"'] hidden/;\n".repeat(MAX_SOURCE / 24);
    source.truncate(source.rfind(';').expect("statement") + 1);
    source.push_str("\nfunction real() {}\n");
    source.push_str(&" ".repeat(MAX_SOURCE - source.len()));
    let now = Instant::now();
    let code = project(&source, false, deadline()).expect("cap-sized grammar");
    assert_eq!(code.len(), MAX_SOURCE);
    assert!(code.contains("function real()"));
    assert!(!code.contains("hidden"));
    eprintln!(
        "flat_source_bytes={} elapsed_ms={}",
        source.len(),
        now.elapsed().as_millis()
    );
}

#[test]
fn backpressure_timeout_oversize_invalid_output_and_panics_fail_closed() {
    for mode in [16, 17, 18, 19, 20] {
        let now = Instant::now();
        let source = " ".repeat(MAX_SOURCE);
        let budget = if mode == 16 {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(2)
        };
        let result = exchange(&source, mode, now + budget);
        assert!(now.elapsed() < Duration::from_secs(3));
        LAST_CHILD.with(|pid| assert_reaped(pid.get()));
        match mode {
            16 => assert!(matches!(result, Err(WorkerFailure::Deadline))),
            17 => assert!(matches!(result, Err(WorkerFailure::OutputLimit))),
            18 => {
                let output = result.expect("invalid response completed");
                assert!(output.status.success());
                assert!(!output.bytes.is_empty());
                assert!(decode(&source, &output.bytes).is_err());
            }
            19 => assert_eq!(result.expect("panic completed").status.code(), Some(101)),
            20 => assert_eq!(
                result.expect("abort completed").status.signal(),
                Some(libc::SIGABRT)
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn drop_and_unwind_kill_drain_and_reap() {
    for panic in [false, true] {
        let mut pid = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let child = OwnedWorker::spawn().expect("worker");
            pid = child.child.id();
            if panic {
                panic!("synthetic parent unwind");
            }
            drop(child);
        }));
        assert_eq!(result.is_err(), panic);
        assert_reaped(pid);
    }
}

#[test]
fn worker_limits_and_capabilities_are_enforced() {
    let output = exchange("", 21, deadline()).expect("resource witness");
    assert!(output.status.success());
    assert_eq!(output.bytes, b"bounded");
    assert_reaped(output.pid);
}

fn assert_reaped(pid: u32) {
    let mut status = 0;
    // SAFETY: WNOHANG only queries the exact child returned by our spawn.
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    // SAFETY: signal zero observes liveness without signaling another process.
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

pub(super) fn fixture(mode: u8, output: &mut File) -> bool {
    match mode {
        0 | 1 => return false,
        16 => loop {
            std::hint::spin_loop();
        },
        17 => {
            let _ = output.write_all(&vec![0; MAX_OUTPUT + 1]);
        }
        18 => {
            let _ = output.write_all(&encode(&[(1, u32::MAX)]));
        }
        19 => panic!("synthetic worker panic"),
        20 => std::process::abort(),
        21 => {
            assert!(std::env::vars_os().next().is_none());
            for (resource, expected) in LIMITS {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // SAFETY: pointer names initialized owned storage.
                assert_eq!(unsafe { libc::getrlimit(resource, &mut limit) }, 0);
                assert_eq!(limit.rlim_cur, expected);
                assert_eq!(limit.rlim_max, expected);
            }
            // File/network/exec syscalls are denied, not a filesystem namespace.
            assert!(File::open("/etc/passwd").is_err());
            // SAFETY: no pointer arguments; a successful descriptor is closed.
            let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            assert_eq!(socket, -1);
            output.write_all(b"bounded").expect("witness");
        }
        _ => panic!("unknown test mode"),
    }
    true
}
