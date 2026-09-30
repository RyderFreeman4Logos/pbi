use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
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

const SCOPE_QUERY: &str = "compression publication cache assembly";

struct ScopeFixture {
    base: PathBuf,
    root: PathBuf,
    probe: PathBuf,
    capture: PathBuf,
}

impl ScopeFixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "pbi-rs-search-scope-{}-{nonce}",
            std::process::id()
        ));
        let root = base.join("nested/invocation/root");
        fs::create_dir_all(&root).expect("create nested invocation root");
        let probe = base.join("fake-probe.sh");
        fs::write(
            &probe,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$PWD\" >> \"$PBI_TEST_CAPTURE\"\ncase \"$PBI_TEST_MODE\" in\n  nested) printf 'File: %s/inside.rs, Lines: 1-1\\n' \"$PWD\" ;;\n  boundary) printf 'File: %s/outside-link.rs, Lines: 1-1\\n' \"$PWD\"; printf 'File: ../sibling/sibling.rs, Lines: 1-1\\n' ;;\n  saturated) last=; for arg do last=$arg; done; case \"$last\" in */z-relevant-late.rs) printf 'File: %s/z-relevant-late.rs, Lines: 1-1\\n' \"$PWD\" ;; esac ;;\nesac\n",
        )
        .expect("write scoped fake Probe");
        let mut permissions = fs::metadata(&probe)
            .expect("stat scoped fake Probe")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&probe, permissions).expect("make scoped fake Probe executable");
        let capture = base.join("probe-calls");
        Self {
            base,
            root,
            probe,
            capture,
        }
    }

    fn run(&self, mode: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .env("PBI_RS_PROBE", &self.probe)
            .env("PBI_TEST_CAPTURE", &self.capture)
            .env("PBI_TEST_MODE", mode)
            .args(["search", SCOPE_QUERY])
            .output()
            .expect("run pbi-rs in nested invocation root")
    }

    fn calls(&self) -> Vec<PathBuf> {
        fs::read_to_string(&self.capture)
            .expect("read Probe working-directory log")
            .lines()
            .map(PathBuf::from)
            .collect()
    }
}

impl Drop for ScopeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
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

#[test]
fn probe_scope_uses_nested_invocation_root() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("inside.rs"),
        format!("fn scoped_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write in-scope source");
    let sibling = fixture
        .root
        .parent()
        .expect("invocation parent")
        .join("sibling.rs");
    fs::write(
        &sibling,
        format!("fn sibling_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write sibling source");

    let output = fixture.run("nested");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("inside.rs"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sibling.rs"));
    assert_eq!(
        fixture.calls(),
        vec![fixture.root.canonicalize().expect("canonical root")]
    );
}

#[test]
fn probe_scope_rejects_sibling_and_outside_symlink_candidates() {
    let fixture = ScopeFixture::new();
    let sibling_dir = fixture
        .root
        .parent()
        .expect("invocation parent")
        .join("sibling");
    fs::create_dir_all(&sibling_dir).expect("create sibling source directory");
    fs::write(
        sibling_dir.join("sibling.rs"),
        format!("fn sibling_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write sibling source");

    let outside_dir = fixture
        .root
        .parent()
        .and_then(|parent| parent.parent())
        .expect("outside parent")
        .join("outside");
    fs::create_dir_all(&outside_dir).expect("create outside source directory");
    let outside = outside_dir.join("outside.rs");
    fs::write(
        &outside,
        format!("fn outside_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write outside source");
    symlink(&outside, fixture.root.join("outside-link.rs")).expect("link outside source");

    let output = fixture.run("boundary");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "pbi: no source locations found"
    );
    assert_eq!(
        fixture.calls(),
        vec![fixture.root.canonicalize().expect("canonical root")]
    );
}

#[test]
fn probe_scope_fails_closed_after_root_miss_with_late_17th_match() {
    let fixture = ScopeFixture::new();
    for index in 0..16 {
        fs::write(
            fixture.root.join(format!("candidate-{index:02}.rs")),
            "fn decoy() {}\n",
        )
        .expect("write early candidate");
    }
    let late_match = fixture.root.join("z-relevant-late.rs");
    fs::write(
        &late_match,
        format!("fn late_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write relevant seventeenth candidate");
    let mut candidates = fs::read_dir(&fixture.root)
        .expect("read candidate files")
        .map(|entry| entry.expect("candidate entry").path())
        .collect::<Vec<_>>();
    candidates.sort();
    assert_eq!(candidates.len(), 17);
    assert_eq!(candidates.last(), Some(&late_match));

    let output = fixture.run("saturated");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "pbi-rs: Probe scope exceeded the bounded target limit"
    );
    assert_eq!(
        fixture.calls(),
        vec![fixture.root.canonicalize().expect("canonical root")]
    );
}
