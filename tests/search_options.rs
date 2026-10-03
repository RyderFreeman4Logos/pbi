use std::fs;
use std::os::unix::fs::symlink;
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
        Self { root }
    }

    fn run(&self, args: &[&str], _mode: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run pbi-rs")
    }
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
        let events = base.join("probe-events");
        Self { base, root, events }
    }

    fn run(&self, mode: &str) -> Output {
        self.run_args(mode, &["search", SCOPE_QUERY])
    }

    fn run_args(&self, _mode: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run pbi-rs in nested invocation root")
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
fn question_parity_message_rejects_unsupported_chat_tail_before_probe() {
    for tail in [
        vec!["--session", "resume-id"],
        vec!["--session=resume-id"],
        vec!["--session-id", "SID"],
        vec!["--session-id=SID"],
        vec!["--max-iterations", "1"],
        vec!["--max-iterations=1"],
        vec!["--allow-edit"],
        vec!["--enable-bash"],
        vec!["--web"],
        vec!["--trace-remote"],
        vec!["--trace-file", "trace.log"],
        vec!["--images", "shot.png"],
        vec!["--prompt", "extra"],
        vec!["--port", "9"],
        vec!["--debug"],
        vec!["--architecture-file", "arch.md"],
        vec!["--completion-prompt", "done"],
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
fn raw_search_rejects_repeated_safe_flags_before_probe() {
    for tail in [
        vec!["--files-only", "--files-only"],
        vec!["-f", "-f"],
        vec!["--files-only", "-f"],
        vec!["--exact", "--exact"],
        vec!["-e", "-e"],
        vec!["--exact", "-e"],
        vec!["--frequency", "--frequency"],
        vec!["-s", "-s"],
        vec!["--frequency", "-s"],
        vec!["--exclude-filenames", "--exclude-filenames"],
        vec!["-n", "-n"],
        vec!["--exclude-filenames", "-n"],
        vec!["--strict-elastic-syntax", "--strict-elastic-syntax"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["search", "--bm25"];
        args.extend(tail);
        args.extend(["--", "parse_search"]);
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used multiple times"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
    }
}

#[test]
fn safe_flags_without_bm25_and_deferred_flags_stop_before_probe() {
    for args in [
        vec![
            "search",
            "--files-only",
            "--format",
            "json",
            "--",
            "parse_search",
        ],
        vec!["search", "-f", "--", "parse_search"],
        vec!["search", "--exact", "--", "parse_search"],
        vec!["search", "-e", "--", "parse_search"],
        vec!["search", "--frequency", "--", "parse_search"],
        vec!["search", "-s", "--", "parse_search"],
        vec!["search", "--exclude-filenames", "--", "parse_search"],
        vec!["search", "-n", "--", "parse_search"],
        vec!["search", "--strict-elastic-syntax", "--", "parse_search"],
        vec!["search", "--bm25", "--allow-tests", "--", "parse_search"],
        vec!["search", "--bm25", "--no-gitignore", "--", "parse_search"],
        vec!["search", "--bm25", "--no-merge", "--", "parse_search"],
        vec!["search", "--bm25", "--lsp", "--", "parse_search"],
        vec![
            "search",
            "--bm25",
            "--not-a-real-flag",
            "--",
            "parse_search",
        ],
        vec![
            "search",
            "--bm25",
            "--files-only=json",
            "--",
            "parse_search",
        ],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unsupported search option"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
    }
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
        "stdout={stdout} stderr={stderr}",
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
        "directory glob must exclude nested source; stdout={stdout}",
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
fn compact_output_cites_member_line_and_keeps_both_files() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "impl SourceLocation {\n    fn display_relative(&self) {}\n}\n",
    )
    .expect("member");
    let member = fixture.run_args("saturated", &["search", "SourceLocation display_relative"]);
    assert_eq!(
        member.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&member.stderr)
    );
    assert_eq!(member.stdout, b"src/lib.rs:2\n");

    fs::write(fixture.root.join("b.rs"), "fn parse_search() {}\n").expect("b");
    fs::write(fixture.root.join("a.rs"), "fn parse_search() {}\n").expect("a");
    let both = fixture.run_args("files", &["search", "where is parse_search"]);
    assert_eq!(
        both.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );
    assert_eq!(both.stdout, b"a.rs:1\nb.rs:1\n");

    let repeated = ScopeFixture::new();
    fs::write(
        repeated.root.join("twice.rs"),
        "fn parse_search() {}\nfn parse_search() {}\n",
    )
    .expect("twice");
    let twice = repeated.run_args("files", &["search", "where is parse_search"]);
    assert_eq!(
        twice.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&twice.stderr)
    );
    assert_eq!(twice.stdout, b"twice.rs:1\n");
}

#[test]
fn native_search_honors_gitignore_root_cap_and_symlink() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join(".gitignore"), "secret.rs\n").expect("gitignore");
    fs::write(
        fixture.root.join("secret.rs"),
        format!("fn hidden_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("ignored");
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn kept_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("kept");
    let outside = fixture.base.join("outside.rs");
    fs::write(
        &outside,
        format!("fn outside_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("outside");
    symlink(&outside, fixture.root.join("linked.rs")).expect("link");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("kept.rs")
            && !stdout.contains("secret.rs")
            && !stdout.contains("linked.rs"),
        "{stdout}"
    );
    assert!(!fixture.events.is_file());
    for index in 0..16 {
        fs::write(
            fixture.root.join(format!("cap-{index:02}.rs")),
            "fn decoy() {}\n",
        )
        .expect("decoy");
    }
    let capped = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    assert_eq!(capped.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&capped.stderr).contains("bounded target limit"));
}

#[test]
fn native_search_refuses_symlinked_gitignore_outside_root() {
    let fixture = ScopeFixture::new();
    let outside = fixture.base.join("outside-ignore");
    fs::write(&outside, "secret.rs\n").expect("outside ignore file");
    symlink(&outside, fixture.root.join(".gitignore")).expect("linked ignore file");
    fs::write(
        fixture.root.join("secret.rs"),
        format!("fn secret() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not read the repository"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn native_search_respects_nested_gitignore_and_negation() {
    let fixture = ScopeFixture::new();
    let src = fixture.root.join("src");
    fs::create_dir(&src).expect("source directory");
    fs::write(
        fixture.root.join(".gitignore"),
        "src/*.rs\n!src/kept.rs\n!src/hidden.rs\n",
    )
    .expect("root gitignore");
    fs::write(src.join(".gitignore"), "hidden.rs\n").expect("nested gitignore");
    for name in ["kept.rs", "hidden.rs", "blocked.rs"] {
        fs::write(
            src.join(name),
            format!("fn matching() {{ /* {SCOPE_QUERY} */ }}\n"),
        )
        .expect("fixture source");
    }
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout} {:?}", output.stderr);
    assert!(stdout.contains("src/kept.rs"), "{stdout}");
    assert!(
        !stdout.contains("hidden.rs") && !stdout.contains("blocked.rs"),
        "{stdout}"
    );
}

#[test]
fn native_search_finds_checkout_source() {
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["search", "display_relative"])
        .output()
        .expect("search checkout");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("src/lib.rs"));
}

#[test]
fn stage_timing_is_opt_in_bounded_and_content_free() {
    let fixture = Fixture::new();
    let query = "private_stage_query_marker";
    fs::write(
        fixture.root.join("private_stage.rs"),
        format!("fn {query}() {{}}\n"),
    )
    .expect("source");
    let ordinary = fixture.run(&["search", query], "unused");
    assert!(ordinary.status.success());
    assert!(ordinary.stderr.is_empty());

    let traced = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("PBI_RS_STAGE_TIMING", "1")
        .env("LOCAL_ROUTER_API_KEY", "private-credential-canary")
        .current_dir(&fixture.root)
        .args(["search", query])
        .output()
        .expect("traced search");
    assert!(traced.status.success());
    let stderr = String::from_utf8(traced.stderr).expect("UTF-8 telemetry");
    let lines = stderr.lines().collect::<Vec<_>>();
    assert!((2..=8).contains(&lines.len()), "{stderr}");
    assert!(lines.iter().all(|line| line.starts_with("pbi-stage ")));
    assert!(!stderr.contains(query));
    assert!(!stderr.contains("private_stage.rs"));
    assert!(!stderr.contains("private-credential-canary"));
    assert!(!stderr.contains(&fixture.root.to_string_lossy().to_string()));
}

#[test]
fn native_search_refuses_symlinked_nested_gitignore_outside_root() {
    let fixture = ScopeFixture::new();
    let src = fixture.root.join("src");
    fs::create_dir(&src).expect("source directory");
    let outside = fixture.base.join("outside-ignore");
    fs::write(&outside, "secret.rs\n").expect("outside ignore file");
    symlink(&outside, src.join(".gitignore")).expect("linked ignore file");
    fs::write(
        src.join("secret.rs"),
        format!("fn secret() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not read the repository"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}
