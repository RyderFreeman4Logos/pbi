//! Real monotonic deadline oracle, including the release CLI. No test seam in production.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: this PID belongs to the unreaped child owned by this guard.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGCONT);
        }
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
}
#[test]
fn search_real_deadline_is_fail_closed() {
    real_deadline(false);
}
#[test]
fn semantic_real_deadline_is_fail_closed() {
    real_deadline(true);
}
