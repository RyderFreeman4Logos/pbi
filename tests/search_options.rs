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
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = safe_test_root("search-options");
        fs::create_dir_all(&root).expect("create fixture root");
        fs::write(
            root.join("fixture.rs"),
            "fn search_option() {}\nfn parity() {}\n",
        )
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

fn safe_test_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp")
        .join(format!("pbi-rs-{label}-{}-{nonce}", std::process::id()))
}

struct ScopeFixture {
    base: PathBuf,
    root: PathBuf,
    probe: PathBuf,
    capture: PathBuf,
    events: PathBuf,
}

impl ScopeFixture {
    fn new() -> Self {
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let base = safe_test_root("search-scope");
        let root = base.join("nested/invocation/root");
        fs::create_dir_all(&root).expect("create nested invocation root");
        let probe = base.join("fake-probe.sh");
        fs::write(
            &probe,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$PWD\" >> \"$PBI_TEST_CAPTURE\"\ncase \"$PBI_TEST_MODE\" in\n  nested) printf 'File: %s/inside.rs, Lines: 1-1\\n' \"$PWD\" ;;\n  boundary) printf 'File: %s/outside-link.rs, Lines: 1-1\\n' \"$PWD\"; printf 'File: ../sibling/sibling.rs, Lines: 1-1\\n' ;;\n  saturated) last=; for arg do last=$arg; done; case \"$last\" in */z-relevant-late.rs|*/z-kept.rs|*/fixture.rs|*/src/lib.rs|*/src) printf 'fallback-hit|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; if [ -d \"$last\" ]; then file=$(find \"$last\" -type f | head -1); printf 'File: %s, Lines: 1-1\\n' \"$file\"; else printf 'File: %s, Lines: 1-1\\n' \"$last\"; fi ;; *.rs) printf 'fallback-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;; *) printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;; esac ;;\n  unrelated) last=; for arg do last=$arg; done; case \"$last\" in */early.rs) printf 'unrelated-file|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s, Lines: 1-1\\n' \"$last\" ;; */src) printf 'fallback-hit|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s/nested/mod.rs, Lines: 1-1\\n' \"$last\" ;; *) printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;; esac ;;\n  deep) last=; for arg do last=$arg; done; case \"$last\" in */src) printf 'fallback-hit|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s/nested/deeper/mod.rs, Lines: 1-1\\n' \"$last\" ;; *) printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;; esac ;;\n  rootdoc) last=; for arg do last=$arg; done; case \"$last\" in */README.md) printf 'fallback-hit|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s, Lines: 1-1\\n' \"$last\" ;; *) printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;; esac ;;\n  filtered) last=; for arg do last=$arg; done; case \"$last\" in *.rs|*.md|*.c|*.py|*/src|*/docs|*/native) printf 'filtered|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; if [ -d \"$last\" ]; then printf 'File: %s/guide.md, Lines: 1-1\\n' \"$last\"; else printf 'File: %s, Lines: 1-1\\n' \"$last\"; fi ;; *) if printf '%s' \"$*\" | grep -q -- '--ignore'; then file=$(find \"$PWD\" -name 'kept.rs' | head -1); if [ -n \"$file\" ]; then printf 'filtered|%s\\n' \"$file\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s, Lines: 1-1\\n' \"$file\"; else printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; fi; else printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; fi ;; esac ;;
  ignored) last=; for arg do last=$arg; done; case \"$last\" in
    *.rs) printf 'fallback-hit|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\"; printf 'File: %s, Lines: 1-1\\n' \"$last\" ;;
    *) printf 'root-miss|%s\\n' \"$last\" >> \"$PBI_TEST_EVENTS\" ;;
  esac ;;\n  decoy) printf 'File: %s/src/decoy.rs, Lines: 1-2\\n' \"$PWD\" ;;\n  overflow) printf 'root-miss|overflow\\n' >> \"$PBI_TEST_EVENTS\" ;;\nesac\n",
        )
        .expect("write scoped fake Probe");
        let mut permissions = fs::metadata(&probe)
            .expect("stat scoped fake Probe")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&probe, permissions).expect("make scoped fake Probe executable");
        let capture = base.join("probe-calls");
        let events = base.join("probe-events");
        Self {
            base,
            root,
            probe,
            capture,
            events,
        }
    }

    fn run(&self, mode: &str) -> Output {
        self.run_args(mode, &["search", SCOPE_QUERY])
    }

    fn run_args(&self, mode: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .env("PBI_RS_PROBE", &self.probe)
            .env("PBI_TEST_CAPTURE", &self.capture)
            .env("PBI_TEST_MODE", mode)
            .env("PBI_TEST_EVENTS", &self.events)
            .args(args)
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

    fn events(&self) -> Vec<String> {
        fs::read_to_string(&self.events)
            .expect("read Probe's result events")
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for ScopeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn dispatch_version_alias_and_no_argument_diagnostic() {
    let fixture = Fixture::new();
    let version = fixture.run(&["--version"], "raw");
    let alias = fixture.run(&["-V"], "raw");
    assert!(alias.status.success());
    assert_eq!(alias.stdout, version.stdout);
    assert!(alias.stderr.is_empty());
    assert!(!fixture.root.join("probe-argv").exists());
}

#[test]
fn dispatch_no_argument_diagnostic() {
    let fixture = Fixture::new();
    let empty = fixture.run(&[], "raw");
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
    assert_eq!(
        empty.stderr,
        b"pbi: question is required; interactive mode is disabled\n"
    );
    assert!(!fixture.root.join("probe-argv").exists());
}

#[test]
fn dispatch_search_help_relays_probe_and_respects_literal_separator() {
    let fixture = Fixture::new();
    for flag in ["--help", "-h"] {
        let output = fixture.run(&["search", flag], "raw");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"raw Probe bytes\n");
        assert_eq!(fixture.argv(), ["search", flag]);
    }
    let output = fixture.run(
        &[
            "search",
            "--bm25",
            "--",
            "--help",
            "-h",
            "-V",
            "--reranker",
            "-r",
        ],
        "raw",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"raw Probe bytes\n");
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("--help -h -V --reranker -r")
    );
    assert_eq!(option_value(&fixture.argv(), "--reranker"), Some("bm25"));
}

#[test]
fn dispatch_reranker_overrides_are_consumed_without_activation() {
    let fixture = Fixture::new();
    for override_args in [
        vec!["--reranker", "hybrid"],
        vec!["-r", "not-a-reranker"],
        vec!["--reranker=ms-marco-minilm-l6"],
        vec!["--reranker="],
        vec!["--reranker", ""],
        vec!["-r", "--bm25"],
    ] {
        for raw in [false, true] {
            let mut args = vec!["search"];
            if raw {
                args.push("--bm25");
            }
            args.extend(override_args.iter().copied());
            args.push("search option parity");
            let output = fixture.run(&args, if raw { "raw" } else { "evidence" });
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let argv = fixture.argv();
            assert_eq!(option_value(&argv, "--reranker"), Some("bm25"));
            assert_eq!(
                argv.iter()
                    .filter(|arg| arg.as_str() == "--reranker")
                    .count(),
                1
            );
            assert_eq!(
                argv.last().map(String::as_str),
                Some("search option parity")
            );
            assert_eq!(argv.contains(&"--dry-run".to_owned()), !raw);
        }
    }
    // Legacy tolerates a missing override operand when the query already exists.
    let output = fixture.run(
        &["search", "search option parity", "--reranker"],
        "evidence",
    );
    assert!(output.status.success());
    for args in [
        vec!["search", "--reranker"],
        vec!["search", "-r", "query"],
        vec!["search", "--reranker="],
        vec!["search", "--reranker", "--", "--bogus"],
    ] {
        fs::remove_file(fixture.root.join("probe-argv")).ok();
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn question_parity_discards_route_overrides_without_activation() {
    for message in [false, true] {
        for overrides in [
            vec![
                "--model-name",
                "unapproved-model",
                "--force-provider",
                "remote",
            ],
            vec!["--model-name=unapproved-model", "--force-provider=remote"],
            vec!["--model-name", "", "--force-provider="],
            vec![
                "--model-name",
                "--model-route",
                "--force-provider",
                "--help",
            ],
            vec!["--force-provider"],
        ] {
            let fixture = Fixture::new();
            let mut args = if message { vec!["--message"] } else { vec![] };
            args.push("search option parity");
            args.extend(overrides);
            let output = fixture.run(&args, "evidence");
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let argv = fixture.argv();
            assert_eq!(
                argv.last().map(String::as_str),
                Some("search option parity")
            );
            assert_eq!(option_value(&argv, "--reranker"), Some("bm25"));
            assert!(!argv
                .iter()
                .any(|arg| arg.contains("unapproved") || arg == "remote"));
            assert_eq!(
                output.stdout,
                b"fixture.rs:1-2\n",
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("semantic_adk_model"));
        }
    }
}

#[test]
fn question_parity_message_rejects_unsupported_chat_tail_before_probe() {
    for tail in [
        vec!["--session", "resume-id"],
        vec!["--session=resume-id"],
        vec!["--max-tokens", "123"],
        vec!["extra", "question"],
        vec!["--model-route", "http://example.invalid", "model", "handle"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["--message", "search option parity"];
        args.extend(tail);
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn question_parity_positional_multiword_literal_and_missing_controls() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["search", "option", "parity", "--model-name=x"],
        "evidence",
    );
    // Use a non-command first word for the actual positional multiword control.
    assert_eq!(output.status.code(), Some(2));
    let output = fixture.run(
        &["option", "search", "parity", "--model-name=x"],
        "evidence",
    );
    assert!(output.status.success());
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("option search parity")
    );
    let output = fixture.run(
        &[
            "search option parity",
            "--",
            "--model-name=literal",
            "--json",
            "--model-route",
        ],
        "evidence",
    );
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("search option parity --model-name=literal --json --model-route")
    );
    assert_ne!(output.status.code(), Some(2));
    for args in [
        vec!["--message", "--json", "--model-name=ignored"],
        vec!["--message", "--model-name", "--force-provider=ignored"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "evidence");
        assert_ne!(output.status.code(), Some(2));
        assert_eq!(fixture.argv().last().map(String::as_str), Some(args[1]));
    }
    for args in [
        vec!["--model-name", "x"],
        vec!["--force-provider="],
        vec!["--message"],
        vec!["--message", "", "--json"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn search_question_is_consumed_without_inference() {
    for raw in [false, true] {
        for question in [
            vec!["--question", "unrelated operand"],
            vec!["--question=--help"],
            vec!["--question", ""],
            vec!["--question="],
        ] {
            let fixture = Fixture::new();
            let mut args = vec!["search"];
            if raw {
                args.push("--bm25");
            }
            args.extend(question);
            args.push("search option parity");
            let output = fixture.run(&args, if raw { "raw" } else { "evidence" });
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let argv = fixture.argv();
            assert_eq!(
                argv.last().map(String::as_str),
                Some("search option parity")
            );
            assert_eq!(option_value(&argv, "--reranker"), Some("bm25"));
            assert!(!argv
                .iter()
                .any(|arg| arg.contains("question") || arg.contains("unrelated")));
        }
    }
    for tail in [
        vec!["--question"],
        vec!["--question", "--help"],
        vec!["--question=x", "--question=y"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["search", "search option parity"];
        args.extend(tail);
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!fixture.root.join("probe-argv").exists());
    }
    let fixture = Fixture::new();
    let output = fixture.run(
        &[
            "search",
            "--bm25",
            "--",
            "--question",
            "--session",
            "literal",
        ],
        "raw",
    );
    assert!(output.status.success());
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("--question --session literal")
    );
}

#[test]
fn search_session_refuses_durable_cache_before_probe() {
    for tail in [
        vec!["--session", "owned-id"],
        vec!["--session=../outside"],
        vec!["--session="],
        vec!["--session"],
        vec!["--session", "--help"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["search", "search option parity"];
        args.extend(tail);
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"pbi-rs: --session requires durable Probe cache writes; search session storage is not supported\n");
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn search_session_ambient_cache_is_not_activated() {
    let fixture = Fixture::new();
    let probe = fixture.root.join("fake-probe.sh");
    fs::write(&probe, "#!/bin/sh\nset -eu\nif [ -n \"${PROBE_SESSION_ID+x}\" ]; then exit 42; fi\nprintf 'raw Probe bytes\\n'\n").expect("write env guard Probe");
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(&fixture.root)
        .env("PBI_RS_PROBE", probe)
        .env("PROBE_SESSION_ID", "user-session")
        .args(["search", "--bm25", "search option parity"])
        .output()
        .expect("run ambient session control");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"raw Probe bytes\n");
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
fn search_format_raw_relays_supported_formats_without_query_pollution() {
    for format in [
        "terminal",
        "markdown",
        "plain",
        "json",
        "xml",
        "color",
        "outline",
        "outline-xml",
    ] {
        for (option, inline) in [
            ("--format", false),
            ("-o", false),
            ("--format=", true),
            ("-o=", true),
            ("-o", true),
        ] {
            let fixture = Fixture::new();
            let joined = format!("{option}{format}");
            let mut args = vec!["search", "--bm25", "search option parity"];
            if inline {
                args.push(&joined);
            } else {
                args.extend([option, format]);
            }
            let output = fixture.run(&args, "raw");
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"raw Probe bytes\n");
            let argv = fixture.argv();
            assert_eq!(option_value(&argv, "--format"), Some(format));
            assert_eq!(
                argv.last().map(String::as_str),
                Some("search option parity")
            );
            assert!(!argv.contains(&"--dry-run".to_owned()));
            assert_eq!(option_value(&argv, "--reranker"), Some("bm25"));
        }
    }
}

#[test]
fn search_format_invalid_duplicates_and_verified_requests_refuse_before_probe() {
    for raw in [false, true] {
        for tail in [
            vec!["--format"],
            vec!["-o"],
            vec!["--format="],
            vec!["-o="],
            vec!["--format", "--"],
            vec!["-o", "--bm25"],
            vec!["--format", ""],
            vec!["--format=JSON"],
            vec!["--format=unknown"],
            vec!["-oxml", "--format=json"],
            vec!["--format=plain", "-o", "json"],
        ] {
            let fixture = Fixture::new();
            let mut args = vec!["search", "search option parity"];
            if raw {
                args.push("--bm25");
            }
            args.extend(tail);
            let output = fixture.run(&args, "raw");
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            assert!(output.stdout.is_empty());
            assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
        }
    }
    // Legacy appends --format plain: installed Probe rejects even an explicit plain.
    for format in ["json", "plain", "outline-xml"] {
        let fixture = Fixture::new();
        let output = fixture.run(
            &["search", "--format", format, "search option parity"],
            "evidence",
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("--format cannot be used multiple times"));
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn search_format_literal_separator_and_default_verified_policy_remain() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["search", "--bm25", "--", "--format", "json", "-oxml"],
        "raw",
    );
    assert!(output.status.success());
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("--format json -oxml")
    );
    assert_eq!(option_value(&fixture.argv(), "--format"), None);
    let output = fixture.run(&["search", "search option parity"], "evidence");
    assert!(output.status.success());
    assert_eq!(option_value(&fixture.argv(), "--format"), Some("plain"));
    assert!(fixture.argv().contains(&"--dry-run".to_owned()));
    assert!(String::from_utf8_lossy(&output.stdout).contains("fixture.rs:1"));
    let output = fixture.run(&["search", "--format=json", "--help"], "raw");
    assert!(output.status.success());
    assert_eq!(fixture.argv(), ["search", "--format=json", "--help"]);
    for args in [
        vec!["search", "--bm25", "--format=json"],
        vec!["search", "--help", "--format=unknown"],
        vec!["search", "--help", "--format=json", "-oxml"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2));
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn search_budget_values_preserve_native_numbers_and_formatting() {
    for raw in [false, true] {
        for values in [vec!["0", "+80", "000"], vec!["80", "20", "30"]] {
            for inline in [false, true] {
                let fixture = Fixture::new();
                let mut args = vec!["search".to_owned()];
                if raw {
                    args.push("--bm25".to_owned());
                }
                for (option, value) in ["--max-bytes", "--max-tokens", "--merge-threshold"]
                    .into_iter()
                    .zip(&values)
                {
                    if inline {
                        args.push(format!("{option}={value}"));
                    } else {
                        args.extend([option.to_owned(), (*value).to_owned()]);
                    }
                }
                args.push("search option parity".to_owned());
                let args: Vec<_> = args.iter().map(String::as_str).collect();
                let output = fixture.run(&args, if raw { "raw" } else { "evidence" });
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let argv = fixture.argv();
                for (option, value) in ["--max-bytes", "--max-tokens", "--merge-threshold"]
                    .into_iter()
                    .zip(&values)
                {
                    assert_eq!(option_value(&argv, option), Some(*value));
                }
                assert_eq!(option_value(&argv, "--reranker"), Some("bm25"));
                assert_eq!(
                    option_value(&argv, "--format"),
                    if raw { None } else { Some("plain") }
                );
                assert_eq!(argv.contains(&"--dry-run".to_owned()), !raw);
                if raw {
                    assert_eq!(output.stdout, b"raw Probe bytes\n");
                } else {
                    assert_eq!(output.stdout, b"fixture.rs:1-2\n");
                }
            }
        }
    }
}

#[test]
fn search_budget_invalid_and_duplicate_operands_refuse_before_probe() {
    for option in ["--max-bytes", "--max-tokens", "--merge-threshold"] {
        for tail in [
            vec![option.to_owned()],
            vec![format!("{option}=")],
            vec![option.to_owned(), "--".to_owned()],
            vec![option.to_owned(), "--help".to_owned()],
            vec![option.to_owned(), "".to_owned()],
            vec![format!("{option}=-1")],
            vec![format!("{option}=1.5")],
            vec![format!("{option}= 1")],
            vec![format!("{option}=18446744073709551616")],
            vec![option.to_owned(), "0".to_owned(), format!("{option}=1")],
        ] {
            let fixture = Fixture::new();
            let mut args = vec!["search", "search option parity"];
            args.extend(tail.iter().map(String::as_str));
            let output = fixture.run(&args, "evidence");
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            assert!(output.stdout.is_empty());
            assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
        }
        let fixture = Fixture::new();
        let output = fixture.run(&["search", "--bm25", "--", option, "0"], "raw");
        assert!(output.status.success());
        assert_eq!(fixture.argv().last(), Some(&format!("{option} 0")));
        assert_eq!(option_value(&fixture.argv(), option), None);
    }
}

#[test]
fn search_budget_miss_does_not_restart_global_limits_in_fallback() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("z-relevant-late.rs"),
        format!("fn late_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write candidate source");
    for option in ["--max-bytes=0", "--max-tokens=0"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&fixture.root)
            .env("PBI_RS_PROBE", &fixture.probe)
            .env("PBI_TEST_CAPTURE", &fixture.capture)
            .env("PBI_TEST_MODE", "saturated")
            .env("PBI_TEST_EVENTS", &fixture.events)
            .args(["search", option, SCOPE_QUERY])
            .output()
            .expect("run budgeted root miss");
        assert_eq!(output.status.code(), Some(1), "{option}");
        assert!(output.stdout.is_empty(), "{option}");
        assert_eq!(output.stderr, b"pbi-rs: probe returned no output\n");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(&fixture.root)
        .env("PBI_RS_PROBE", &fixture.probe)
        .env("PBI_TEST_CAPTURE", &fixture.capture)
        .env("PBI_TEST_MODE", "saturated")
        .env("PBI_TEST_EVENTS", &fixture.events)
        .args(["search", "--merge-threshold=0", SCOPE_QUERY])
        .output()
        .expect("run zero merge distance");
    assert!(
        output.status.success(),
        "merge distance zero is not an output budget: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls().len(), 4);
}

#[test]
fn search_budget_values_cannot_raise_wrapper_output_cap() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("fake-probe.sh"),
        format!("#!/bin/sh\nprintf '%s' '{}'\n", "x".repeat(33 * 1024)),
    )
    .expect("write excessive-output Probe");
    for raw in [false, true] {
        let mut args = vec![
            "search",
            "--max-bytes=18446744073709551615",
            "--max-tokens=18446744073709551615",
            "--merge-threshold=18446744073709551615",
            "search option parity",
        ];
        if raw {
            args.push("--bm25");
            args.push("--format=json");
        }
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"pbi-rs: Probe output exceeded the bounded limit\n"
        );
    }
}

#[test]
fn scoped_search_shorthand_normalizes_verified_query_but_not_raw_query() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search", "search_option:parity"], "evidence");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("search_option parity")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("fixture.rs:1"));

    let output = fixture.run(&["search", "--bm25", "search_option:parity"], "raw");
    assert!(output.status.success());
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("search_option:parity")
    );
    assert_eq!(output.stdout, b"raw Probe bytes\n");
}

#[test]
fn search_filters_preserve_operands_safety_and_literal_separator() {
    let fixture = Fixture::new();
    for filters in [
        vec!["--language", "rust", "--ignore", "*.py", "-i", "!drafts/**"],
        vec!["-l", "rust", "--ignore=*.py", "--ignore=!drafts/**"],
    ] {
        for raw in [false, true] {
            let mut args = vec!["search"];
            args.extend(filters.iter().copied());
            if raw {
                args.push("--bm25");
            }
            args.push("search option parity");
            let output = fixture.run(&args, if raw { "raw" } else { "evidence" });
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let argv = fixture.argv();
            assert_eq!(option_value(&argv, "--language"), Some("rust"));
            let ignores: Vec<_> = argv
                .iter()
                .enumerate()
                .filter_map(|(index, arg)| {
                    if arg == "--ignore" {
                        argv.get(index + 1).map(String::as_str)
                    } else {
                        arg.strip_prefix("--ignore=")
                    }
                })
                .collect();
            assert_eq!(
                ignores,
                [
                    "*.py",
                    "!drafts/**",
                    ".git",
                    "target",
                    "drafts",
                    "node_modules",
                    "__pycache__"
                ]
            );
            assert_eq!(
                argv.last().map(String::as_str),
                Some("search option parity")
            );
            if raw {
                assert_eq!(output.stdout, b"raw Probe bytes\n");
                assert!(!argv.contains(&"--format".to_owned()));
            }
        }
    }
    let output = fixture.run(
        &[
            "search",
            "--bm25",
            "--",
            "--language",
            "--ignore",
            "--help",
            "-h",
            "--model-route",
        ],
        "raw",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"raw Probe bytes\n");
    assert_eq!(
        fixture.argv().last().map(String::as_str),
        Some("--language --ignore --help -h --model-route")
    );
}

#[test]
fn invalid_filter_operands_stop_before_probe() {
    for args in [
        vec!["search", "--language"],
        vec!["search", "-l"],
        vec!["search", "--ignore"],
        vec!["search", "-i"],
        vec!["search", "--language", "--ignore", "query"],
        vec!["search", "--ignore", "--", "query"],
        vec!["search", "--language=", "query"],
        vec!["search", "--ignore=", "query"],
        vec!["search", "--language", "not-a-language", "query"],
        vec!["search", "-l", "rust", "-l", "python", "query"],
        vec!["search", "--format", "json", "query"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!fixture.root.join("probe-argv").exists());
    }
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
            "--max-bytes=18446744073709551615",
            "--max-tokens=0",
            "--merge-threshold=18446744073709551615",
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
    assert_eq!(fixture.calls().len(), 16);
    assert!(fixture.calls().windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn probe_scope_returns_late_under_cap_match_after_root_miss() {
    let fixture = ScopeFixture::new();
    for name in ["a-decoy.rs", "m-decoy.rs"] {
        fs::write(fixture.root.join(name), "fn decoy() {}\n").expect("write decoy source");
    }
    let late_match = fixture.root.join("z-relevant-late.rs");
    fs::write(
        &late_match,
        format!("fn late_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write relevant late candidate");
    let mut candidates = fs::read_dir(&fixture.root)
        .expect("read candidate files")
        .map(|entry| entry.expect("candidate entry").path())
        .collect::<Vec<_>>();
    candidates.sort();
    assert_eq!(candidates.len(), 3);
    assert_eq!(candidates.last(), Some(&late_match));

    let output = fixture.run("saturated");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"z-relevant-late.rs:1\n");
    assert!(!output
        .stdout
        .windows(9)
        .any(|window| window == b"a-decoy.rs"));

    let root = fixture
        .root
        .canonicalize()
        .expect("canonical invocation root");
    assert_eq!(fixture.calls(), vec![root; candidates.len() + 1]);
    let canonical_late_match = late_match.canonicalize().expect("canonical late match");
    let mut expected_events = vec![format!("root-miss|{SCOPE_QUERY}")];
    expected_events.extend(candidates.iter().map(|path| {
        let canonical_path = path.canonicalize().expect("canonical candidate path");
        let result = if canonical_path == canonical_late_match {
            "fallback-hit"
        } else {
            "fallback-miss"
        };
        format!("{result}|{}", canonical_path.display())
    }));
    assert_eq!(fixture.events(), expected_events);
}

#[test]
fn root_miss_searches_source_before_instruction_directories() {
    let fixture = ScopeFixture::new();
    for name in [".agents", ".claude", ".codex"] {
        fs::create_dir(fixture.root.join(name)).expect("create instruction directory");
    }
    fs::create_dir(fixture.root.join("src")).expect("create source directory");
    fs::write(
        fixture.root.join("src/lib.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write genuine source");
    let same_device = fixture.root.join("notes");
    fs::create_dir(&same_device).expect("create same-device directory");
    symlink(&same_device, fixture.root.join("same-device-link")).expect("same-device link");
    let output = fixture.run("saturated");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = fixture.events();
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={} events={events:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("src/lib.rs:1"), "{stdout}");
    let hit = events
        .iter()
        .find(|event| event.starts_with("fallback-hit|"))
        .expect("source fallback hit");
    assert!(hit.ends_with("/src"), "{hit}");
    assert!(events.iter().all(|event| !event.contains("/.agents")
        && !event.contains("/.claude")
        && !event.contains("/.codex")
        && !event.contains("same-device-link")));
}

#[test]
fn root_scope_depth_two_source_directory() {
    let fixture = ScopeFixture::new();
    fs::create_dir_all(fixture.root.join("src/nested")).expect("create nested source directory");
    fs::write(
        fixture.root.join("src/nested/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write depth-two source");
    let output = fixture.run("saturated");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("src/nested/mod.rs:1"), "{stdout}");
    let hit = fixture
        .events()
        .into_iter()
        .find(|event| event.starts_with("fallback-hit|"))
        .expect("nested directory fallback hit");
    assert!(hit.ends_with("/src"), "{hit}");
}

#[test]
fn root_scope_root_documents_consume_budget() {
    let fixture = ScopeFixture::new();
    for index in 0..17 {
        fs::write(
            fixture.root.join(format!("noise-{index:02}.md")),
            "not source\n",
        )
        .expect("write root noise");
    }
    fs::create_dir_all(fixture.root.join("src/nested")).expect("create nested source directory");
    fs::write(
        fixture.root.join("src/nested/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write depth-two source after noise");
    let output = fixture.run("saturated");
    assert!(
        !output.status.success(),
        "root documents must consume the target budget"
    );
}

#[test]
fn root_scope_unrelated_file_does_not_stop() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("early.rs"),
        "fn early() { /* unrelated */ }\n",
    )
    .expect("write early unrelated source");
    fs::create_dir_all(fixture.root.join("src/nested")).expect("create nested source directory");
    fs::write(
        fixture.root.join("src/nested/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write late relevant source");
    let output = fixture.run("unrelated");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("src/nested/mod.rs:1"), "{stdout}");
    assert!(!stdout.contains("early.rs"), "{stdout}");
    let events = fixture.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("unrelated-file|") && event.ends_with("/early.rs")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("fallback-hit|") && event.ends_with("/src")),
        "{events:?}"
    );
}

#[test]
fn root_scope_depth_three_source_directory() {
    let fixture = ScopeFixture::new();
    fs::create_dir_all(fixture.root.join("src/nested/deeper")).expect("depth three");
    fs::write(
        fixture.root.join("src/nested/deeper/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write depth-three source");
    let output = fixture.run("deep");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("src/nested/deeper/mod.rs:1"), "{stdout}");
}

#[test]
fn root_scope_keeps_root_docs_and_config() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("README.md"),
        format!("display_relative lives here /* {SCOPE_QUERY} */\n"),
    )
    .expect("write root doc");
    fs::write(fixture.root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("config");
    let output = fixture.run("rootdoc");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("README.md:1"), "{stdout}");
}

#[test]
fn root_scope_passes_ignore_language_and_budget() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("kept");
    let output = fixture.run_args(
        "filtered",
        &[
            "search",
            "--language",
            "rust",
            "--ignore",
            "*.md",
            "--max-bytes",
            "4096",
            SCOPE_QUERY,
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("kept.rs:1"), "{stdout}");
}

#[test]
fn root_scope_user_ignore_skips_fallback() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("chosen.rs"), "fn display_relative() {}\n").expect("rs");
    fs::write(
        fixture.root.join("chosen.py"),
        "def display_relative(): pass\n",
    )
    .expect("py");
    let output = fixture.run_args(
        "ignored",
        &["search", "--ignore=*.rs", "--ignore=*.py", SCOPE_QUERY],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty() && !stdout.contains("Coverage: complete"),
        "stdout={stdout} stderr={stderr} events={:?}",
        fixture.events()
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr.trim(), "pbi: no source locations found");
}

#[test]
fn root_scope_rejects_early_display_stem() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/decoy.rs"),
        "fn show(path: &std::path::Path) {\n    let _ = path.display();\n}\n",
    )
    .expect("decoy");
    let output = fixture.run_args("decoy", &["search", "SourceLocation display_relative"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("Coverage: complete"),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(!stdout.contains("decoy.rs"), "stdout={stdout}");
}

#[test]
fn root_scope_overflow_is_not_complete() {
    let fixture = ScopeFixture::new();
    for index in 0..17 {
        fs::create_dir_all(fixture.root.join(format!("pkg-{index:02}/src"))).expect("pkg");
        fs::write(
            fixture.root.join(format!("pkg-{index:02}/src/lib.rs")),
            "fn other() {}\n",
        )
        .expect("src");
    }
    fs::create_dir_all(fixture.root.join("pkg-zz/src")).expect("late");
    fs::write(
        fixture.root.join("pkg-zz/src/lib.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("late source");
    let output = fixture.run("overflow");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success() || stdout.contains("Coverage: incomplete"),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("Coverage: complete"), "{stdout}");
}

#[test]
fn root_boundary_directory_glob_excludes_nested_source() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    fs::write(fixture.root.join(".gitignore"), "src/hidden.rs\n").expect("gitignore");
    fs::write(
        fixture.root.join("src/hidden.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("hidden");
    let output = fixture.run_args("filtered", &["search", "--ignore=src/**", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("src/lib.rs") && !stdout.contains("Coverage: complete"),
        "directory glob must exclude nested source; stdout={stdout} events={:?}",
        fixture.events()
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn root_boundary_fifo_does_not_block_planning() {
    let fixture = ScopeFixture::new();
    let fifo = fixture.root.join("events.pipe");
    std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let started = Instant::now();
    let output = fixture.run_args("filtered", &["search", "--timeout=1", SCOPE_QUERY]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "planning opened a fifo and blocked"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("kept.rs"),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn root_boundary_keeps_nested_docs_and_supported_languages() {
    let fixture = ScopeFixture::new();
    fs::create_dir_all(fixture.root.join("docs")).expect("docs");
    fs::write(
        fixture.root.join("docs/guide.md"),
        format!("witness_symbol reference documentation /* {SCOPE_QUERY} */\n"),
    )
    .expect("doc");
    fs::create_dir_all(fixture.root.join("native")).expect("native");
    fs::write(
        fixture.root.join("native/main.c"),
        format!("void witness_symbol(void) {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("c");
    let output = fixture.run_args("filtered", &["search", "witness_symbol"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("docs/guide.md") || stdout.contains("guide.md"),
        "nested docs and supported languages were dropped; stdout={stdout} events={:?}",
        fixture.events()
    );
    assert!(
        fixture
            .events()
            .iter()
            .any(|event| event.ends_with("/docs") || event.ends_with("/native")),
        "{:?}",
        fixture.events()
    );
}

#[test]
fn root_boundary_counts_targets_and_bytes_across_the_request() {
    let fixture = ScopeFixture::new();
    for index in 0..18 {
        fs::write(
            fixture.root.join(format!("note-{index:02}.rs")),
            "fn unrelated() {}\n",
        )
        .expect("note");
    }
    fs::write(
        fixture.root.join("z-kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let output = fixture.run("filtered");
    let events = fixture.events();
    assert!(
        !output.status.success(),
        "eighteen root documents plus source stayed inside the 16-target ledger; stdout={} events={events:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn root_boundary_and_query_keeps_searching_until_both_targets() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("a.rs"), "fn alpha_function() {}\n").expect("a");
    fs::write(fixture.root.join("z.rs"), "fn beta_function() {}\n").expect("z");
    let output = fixture.run_args("filtered", &["search", "alpha_function and beta_function"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = fixture.events();
    assert!(
        events.iter().any(|event| event.contains("a.rs"))
            && events.iter().any(|event| event.contains("z.rs")),
        "AND stopped after the first partial file; events={events:?} stdout={stdout}"
    );
    assert!(
        stdout.contains("a.rs:1") && stdout.contains("z.rs:1"),
        "{stdout}"
    );
}

#[test]
fn root_boundary_rejects_absent_member_and_symbol_prefix() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "impl SourceLocation {\n    fn new() {}\n    fn display_relative(&self) {}\n}\n",
    )
    .expect("lib");
    let member = fixture.run_args("filtered", &["search", "SourceLocation nonexistent_member"]);
    let member_out = String::from_utf8_lossy(&member.stdout);
    assert!(
        !member_out.contains("Coverage: complete"),
        "absent member accepted via owner; stdout={member_out}"
    );
    let prefix = fixture.run_args("filtered", &["search", "display_relati"]);
    let prefix_out = String::from_utf8_lossy(&prefix.stdout);
    assert!(
        !prefix_out.contains("Coverage: complete"),
        "symbol prefix accepted as exact; stdout={prefix_out}"
    );
}

#[test]
fn root_boundary_numeric_zero_spellings_share_fallback() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let mut codes = Vec::new();
    for spelling in ["0", "+0", "000"] {
        let output = fixture.run_args(
            "filtered",
            &[
                "search",
                &format!("--merge-threshold={spelling}"),
                SCOPE_QUERY,
            ],
        );
        codes.push(output.status.code());
    }
    assert!(
        codes.windows(2).all(|pair| pair[0] == pair[1]),
        "numeric spellings changed fallback: {codes:?}"
    );
}

#[test]
fn default_question_and_search_print_compact_relative_locations() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(fixture.root.join("src/chosen.rs"), "fn parse_search() {}\n").expect("chosen");
    fs::write(
        fixture.root.join("fake-probe.sh"),
        "#!/bin/sh\nset -eu\nprintf '%s\\000' \"$@\" > \"$PBI_TEST_CAPTURE\"\ncase \"$PBI_TEST_MODE\" in\n  raw) printf 'File: %s/src/chosen.rs\\nLines: 1-1\\nraw Probe bytes\\n' \"$(pwd)\"; exit 0 ;;\n  miss) printf 'No results found.\\n'; exit 0 ;;\nesac\nprintf 'File: %s/src/chosen.rs, Lines: 1-1\\n' \"$(pwd)\"\n",
    )
    .expect("rewrite probe");
    let hit = fixture.run(&["where is parse_search"], "evidence");
    assert_eq!(
        hit.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&hit.stderr)
    );
    assert_eq!(hit.stdout, b"src/chosen.rs:1\n");
    assert!(hit.stderr.is_empty());
    let json_hit = fixture.run(&["--json", "where is parse_search"], "evidence");
    assert_eq!(json_hit.status.code(), Some(0));
    assert_eq!(json_hit.stdout, hit.stdout);
    let search_hit = fixture.run(&["search", "where is parse_search"], "evidence");
    assert_eq!(search_hit.status.code(), Some(0));
    assert_eq!(search_hit.stdout, b"src/chosen.rs:1\n");
    let miss = fixture.run(&["where is absent_symbol"], "miss");
    assert_eq!(miss.status.code(), Some(1));
    assert!(miss.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&miss.stderr).trim(),
        "pbi: no source locations found"
    );
    let raw = fixture.run(&["search", "--bm25", "where is parse_search"], "raw");
    assert_eq!(raw.status.code(), Some(0));
    assert!(raw.stdout.windows(6).any(|window| window == b"File: "));
    assert!(raw.stdout.windows(9).any(|window| window == b"chosen.rs"));
    assert!(!raw.stdout.starts_with(b"src/chosen.rs:1\n"));
}

#[test]
fn compact_output_keeps_qualified_owner_and_root_boundary() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "impl SourceLocation {\n    fn display_relative(&self) {}\n    fn other(&self) {}\n}\n",
    )
    .expect("lib");
    let positive = fixture.run_args("saturated", &["search", "SourceLocation display_relative"]);
    assert_eq!(
        positive.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&positive.stderr)
    );
    assert_eq!(positive.stdout, b"src/lib.rs:1-2\n");
    let negative = fixture.run_args("saturated", &["search", "SourceLocation absent_member"]);
    assert_eq!(negative.status.code(), Some(1));
    assert!(negative.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&negative.stderr).trim(),
        "pbi: no source locations found"
    );

    let sibling_dir = fixture.root.parent().expect("parent").join("sibling");
    fs::create_dir_all(&sibling_dir).expect("sibling dir");
    fs::write(sibling_dir.join("sibling.rs"), "fn parse_search() {}\n").expect("sibling");
    symlink(
        sibling_dir.join("sibling.rs"),
        fixture.root.join("outside-link.rs"),
    )
    .expect("outside link");
    let boundary = fixture.run_args("boundary", &["where is parse_search"]);
    assert_eq!(boundary.status.code(), Some(1));
    assert!(boundary.stdout.is_empty());
    assert!(!boundary
        .stdout
        .windows(10)
        .any(|window| window == b"sibling.rs"));
    assert_eq!(
        String::from_utf8_lossy(&boundary.stderr).trim(),
        "pbi: no source locations found"
    );
}
