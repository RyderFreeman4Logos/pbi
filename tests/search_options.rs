use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pbi-rs-search-options-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create fixture root");
        fs::write(root.join("fixture.rs"), "fn search_option_parity() {}\n")
            .expect("write fixture source");
        let probe = root.join("fake-probe.sh");
        fs::write(
            &probe,
            "#!/bin/sh\nset -eu\nprintf '%s\\000' \"$@\" > \"$PBI_TEST_CAPTURE\"\ncase \"$PBI_TEST_MODE\" in\n  raw) printf 'raw Probe bytes\\n'; exit 0 ;;\n  timeout) exec /bin/sleep 30 ;;\nesac\nprintf 'File: %s/fixture.rs, Lines: 1-1\\n' \"$(pwd)\"\n",
        )
        .expect("write fake Probe");
        let mut permissions = fs::metadata(&probe).expect("stat fake Probe").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&probe, permissions).expect("make fake Probe executable");
        Self { root }
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        let probe = self.root.join("fake-probe.sh");
        let capture = self.root.join("probe-argv");
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .env("PBI_RS_PROBE", probe)
            .env("PBI_TEST_CAPTURE", capture)
            .env("PBI_TEST_MODE", mode)
            .args(args)
            .output()
            .expect("run pbi-rs")
    }

    fn argv(&self) -> Vec<String> {
        fs::read(self.root.join("probe-argv"))
            .expect("read captured Probe argv")
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8(part.to_vec()).expect("argv is UTF-8"))
            .collect()
    }

    fn assert_search_options(&self, timeout: &str, max_results: &str) {
        let args = self.argv();
        assert_eq!(option_value(&args, "--timeout"), Some(timeout));
        assert_eq!(option_value(&args, "--max-results"), Some(max_results));
    }
}

fn option_value<'a>(args: &'a [String], option: &str) -> Option<&'a str> {
    let mut index = 0;
    let mut value = None;
    while index < args.len() {
        if args[index] == option {
            value = args.get(index + 1).map(String::as_str);
            index += 2;
        } else {
            if let Some((name, inline_value)) = args[index].split_once('=') {
                if name == option {
                    value = Some(inline_value);
                }
            }
            index += 1;
        }
    }
    value
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn default_search_options_reach_probe() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search", "search option parity"], "evidence");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_search_options("540", "8");
}

#[test]
fn explicit_search_options_reach_probe_with_effective_values() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &[
            "search",
            "--max-results=3",
            "--timeout=17",
            "search option parity",
        ],
        "evidence",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_search_options("17", "3");

    let output = fixture.run(
        &[
            "search",
            "--max-results",
            "3",
            "--timeout",
            "17",
            "search option parity",
        ],
        "evidence",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_search_options("17", "3");
}

#[test]
fn invalid_or_missing_numeric_options_stop_before_probe() {
    let fixture = Fixture::new();
    for (args, expected_error) in [
        (&["search", "--timeout"][..], "--timeout requires a value"),
        (
            &["search", "--max-results"][..],
            "--max-results requires a value",
        ),
        (
            &["search", "--timeout=fast", "search option parity"][..],
            "--timeout must be a non-negative integer",
        ),
        (
            &["search", "--max-results=0", "search option parity"][..],
            "--max-results must be a positive integer",
        ),
    ] {
        let output = fixture.run(args, "evidence");
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn raw_bm25_search_keeps_probe_stream_and_skips_formatting_options() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search", "--bm25", "search option parity"], "raw");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"raw Probe bytes\n");
    fixture.assert_search_options("540", "8");
}

#[test]
fn total_probe_deadline_bounds_long_backend_timeout() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.run(
        &[
            "search",
            "--timeout=999",
            "--max-results",
            "8",
            "search option parity",
        ],
        "timeout",
    );
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(124));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Probe query exceeded bounded deadline")
    );
    assert!(
        elapsed >= Duration::from_secs(7),
        "deadline fired too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(12),
        "deadline was not enforced: {elapsed:?}"
    );
    fixture.assert_search_options("999", "8");
}
