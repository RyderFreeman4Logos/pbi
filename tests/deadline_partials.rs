//! Real monotonic deadline oracle, including the release CLI. No test seam in production.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "support/deadline_trace.rs"]
mod deadline_trace;

#[test]
fn search_real_deadline_retains_bounded_locations() {
    for (format, files_only, bytes, tokens) in [
        ("json", false, 100, 20),
        ("xml", false, 100, 20),
        ("plain", false, 100, 20),
        ("json", true, 100, 20),
        ("json", false, 1, 20),
        ("json", false, 100, 1),
    ] {
        partial_output(format, files_only, bytes, tokens);
    }
}

fn partial_output(format: &str, files_only: bool, bytes: usize, tokens: usize) {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "pbi-partial-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    let root = fixture.0.join("repo");
    fs::create_dir_all(&root).unwrap();
    for name in ["a.rs", "b.rs"] {
        fs::write(
            root.join(name),
            "fn unrelated() {}\nfn deadline_marker() {}\n",
        )
        .unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_pbi-rs"));
    command
        .env_clear()
        .env("PBI_RS_ADK_ENABLE", "0")
        .env("PBI_RS_STAGE_TIMING", "1")
        .current_dir(&root)
        .args(["search", "--bm25", "--timeout=1", "deadline_marker"])
        .arg(format!("--format={format}"))
        .arg(format!("--max-bytes={bytes}"))
        .arg(format!("--max-tokens={tokens}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if files_only {
        command.arg("--files-only");
    }
    deadline_trace::trace(&mut command);
    let launched = Instant::now();
    let mut child = Child(command.spawn().unwrap());
    let mut error = String::new();
    let resume = deadline_trace::partial_checkpoint(&mut child, &root, &mut error, launched);
    deadline_trace::resume(&mut child, resume);
    let until = launched + Duration::from_secs(4);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < until, "CLI exceeded cleanup deadline");
        std::thread::sleep(Duration::from_millis(1));
    };
    use std::io::Read;
    let mut stdout = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(
        error.contains("stage_status=deadline") && error.contains("deadline_s=1"),
        "deadline status required"
    );
    assert!(stdout.len() <= bytes);
    let token_count = stdout
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .count();
    assert!(token_count <= tokens);
    let budget_rejected = bytes == 1 || tokens == 1;
    let mut verified = 0;
    if budget_rejected {
        assert!(
            stdout.is_empty(),
            "insufficient caller budget must emit nothing"
        );
    } else {
        assert!(!stdout.is_empty(), "nonempty partial required");
        // Compare the complete serialization, then verify every emitted citation
        // against the actual fixture bytes. Never print child output on failure.
        for name in ["a.rs", "b.rs"] {
            let expected = match (format, files_only) {
                ("json", true) => format!("[\"{name}\"]\n"),
                ("json", false) => format!("[{{\"file\":\"{name}\",\"line\":2}}]\n"),
                ("xml", false) => format!(
                    "<results>\n<hit file=\"{name}\" line=\"2\" end_line=\"2\"/>\n</results>\n"
                ),
                _ => format!("{name}:2\n"),
            };
            if stdout == expected {
                let source = fs::read_to_string(root.join(name)).unwrap();
                assert!(
                    source.lines().nth(1) == Some("fn deadline_marker() {}"),
                    "citation must match source"
                );
                verified += 1;
            }
        }
        assert_eq!(verified, 1, "selected format and location-only projection");
    }
    assert!(
        !error.contains("deadline_marker") && !error.contains(".rs"),
        "private diagnostics forbidden"
    );
    let phase = error
        .lines()
        .find(|line| line.contains("stage=initial_search status=deadline"))
        .expect("partial phase");
    let number = |key: &str| {
        phase
            .split_whitespace()
            .find_map(|part| part.strip_prefix(key))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    assert!(
        number("count=") > 0,
        "custody verification must precede output budgets"
    );
    println!(
        "partial-oracle {}",
        serde_json::json!({
            "format": format, "files_only": files_only, "max_bytes": bytes,
            "max_tokens": tokens, "stdout_bytes": stdout.len(), "tokens": token_count,
            "exit": status.code(), "verified_locations": verified,
            "location_verified": verified > 0, "budget_rejected": budget_rejected,
            "stage": "initial_search", "status": "deadline", "deadline_s": 1,
            "elapsed_ms": number("elapsed_ms="), "remaining_ms": number("remaining_ms="),
            "retained_count": number("count="), "privacy_verified": true
        })
    );
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        // Child::kill tracks reaping and SIGKILL also terminates a stopped child;
        // no raw PID signal is needed (or safe after try_wait returned Some).
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn real_deadline(semantic: bool) {
    let root =
        PathBuf::from(std::env::var_os("TMPDIR").expect("explicit scratch TMPDIR")).join(format!(
            "pbi-deadline-cli-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
    fs::create_dir(&root).expect("fixture");
    let fixture = Fixture(root);
    // Small files provide an observable root-descriptor lifetime, not load-based timing.
    for n in 0..256 {
        fs::write(
            fixture.0.join(format!("source{n}.rs")),
            "fn deadline_marker() {}\n",
        )
        .expect("source");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_pbi-rs"));
    command
        .env_clear()
        .env("PBI_RS_ADK_ENABLE", "0")
        .current_dir(&fixture.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !semantic {
        command.arg("search");
    }
    command.args(["--timeout=1", "deadline_marker"]);
    let mut child = Child(command.spawn().expect("CLI"));
    let pid = child.0.id();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let owns_root = fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| fs::read_link(entry.path()).is_ok_and(|path| path == fixture.0))
        });
        if owns_root {
            break;
        }
        assert!(
            Instant::now() < until,
            "root-descriptor readiness timed out"
        );
        assert!(
            child.0.try_wait().expect("poll").is_none(),
            "CLI completed before readiness; oracle did not run"
        );
        std::thread::yield_now();
    }
    // SAFETY: child remains owned and unreaped; no other process PID is signalled.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
    std::thread::sleep(Duration::from_millis(1100));
    // SAFETY: resume the same owned child after the real deadline has elapsed.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGCONT) }, 0);
    let until = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll") {
            break status;
        }
        assert!(
            Instant::now() < until,
            "CLI did not finish within cleanup margin"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .0
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert_eq!(status.code(), Some(1), "deadline must fail");
    assert!(
        stdout.is_empty(),
        "expired custody must not emit locations or source"
    );
    assert!(
        stderr.contains("stage_status=deadline"),
        "real deadline status absent"
    );
    assert!(stderr.contains("deadline_s=1"), "wrong deadline");
    assert!(!stderr.contains("deadline_marker"), "query leaked");
    assert!(!stderr.contains(".rs"), "path leaked");
    println!(
        "exhausted-oracle {}",
        serde_json::json!({
            "semantic": semantic, "exit": status.code(), "stdout_bytes": stdout.len(),
            "deadline_s": 1, "deadline_status": true, "privacy_verified": true
        })
    );
}
#[test]
fn search_real_deadline_is_fail_closed() {
    real_deadline(false);
}
#[test]
fn semantic_real_deadline_is_fail_closed() {
    real_deadline(true);
}
