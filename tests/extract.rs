use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "extract-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
        fs::create_dir_all(&root).expect("fixture");
        let root = fs::canonicalize(root).expect("canonical fixture");
        fs::write(root.join("fixture.rs"), source).expect("source");
        Self(root)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .env("PBI_RS_ADK_ENABLE", "0")
            .current_dir(&self.0)
            .args(args)
            .output()
            .expect("extract")
    }
    fn extract(&self, position: &str) -> String {
        let result = self.run(&["extract", position]);
        assert!(
            result.status.success(),
            "rc={:?} stderr={}",
            result.status.code(),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stderr.is_empty());
        String::from_utf8(result.stdout).expect("UTF-8")
    }
    fn refused(&self, position: &str) {
        let result = self.run(&["extract", position]);
        assert!(!result.status.success(), "unexpected success {position}");
        assert!(result.stdout.is_empty(), "no partial source on failure");
        assert!(
            !String::from_utf8_lossy(&result.stderr).contains("fixture-secret"),
            "private content in error"
        );
        assert!(
            !String::from_utf8_lossy(&result.stderr).contains(position),
            "private path in error"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCE: &str = "// outside\n\n/// Unicode café docs\n#[inline]\npub fn chosen() {\n    // a } brace in a comment\n    let text = \"é }\";\n    fn nested() {\n        let _ = 7;\n    }\n    nested();\n}\nfn next() {}\n";

#[test]
fn extract_complete_rust_function_and_nested_boundaries() {
    let fixture = Fixture::new(SOURCE);
    let expected = "File: fixture.rs, Lines: 3-12\nBlock: complete\n\n".to_owned()
        + &SOURCE
            .lines()
            .skip(2)
            .take(10)
            .collect::<Vec<_>>()
            .join("\n")
        + "\n";
    assert_eq!(fixture.extract("fixture.rs:7"), expected);
    assert_eq!(fixture.extract("fixture.rs:4"), expected);
    assert_eq!(fixture.extract("fixture.rs:12"), expected);
    assert_eq!(fixture.extract("fixture.rs:9"), "File: fixture.rs, Lines: 8-10\nBlock: complete\n\n    fn nested() {\n        let _ = 7;\n    }\n");
}

#[test]
fn extract_parser_prefix_coordinates_remain_raw_and_exact() {
    for prefix in [
        "",
        "\u{feff}",
        "#!/usr/bin/env rust-script\n",
        "\u{feff}#!/usr/bin/env rust-script\n",
    ] {
        for newline in ["\n", "\r\n"] {
            let source = format!("{prefix}{SOURCE}fn tiny() {{}} fn neighbor() {{}}\n")
                .replace('\n', newline);
            let fixture = Fixture::new(&source);
            let shift = usize::from(prefix.contains('\n'));
            for line in [4, 7, 12] {
                let output = fixture.extract(&format!("fixture.rs:{}", line + shift));
                let body = source
                    .lines()
                    .skip(2 + shift)
                    .take(10)
                    .collect::<Vec<_>>()
                    .join(newline);
                assert_eq!(
                    output,
                    format!(
                        "File: fixture.rs, Lines: {}-{}\nBlock: complete\n\n{body}\n",
                        3 + shift,
                        12 + shift
                    )
                );
            }
            let output = fixture.extract(&format!("fixture.rs:{}", 9 + shift));
            assert_eq!(output, format!("File: fixture.rs, Lines: {}-{}\nBlock: complete\n\n    fn nested() {{{newline}        let _ = 7;{newline}    }}\n", 8 + shift, 10 + shift));
            let output = fixture.extract(&format!("fixture.rs:{}", 14 + shift));
            assert_eq!(
                output,
                format!(
                    "File: fixture.rs, Lines: {0}-{0}\nBlock: complete\n\nfn tiny() {{}}\n",
                    14 + shift
                )
            );
        }
    }
}

#[test]
fn extract_foreign_items_are_smallest_enclosing_declarations() {
    let source = "unsafe extern \"C\" {\n    /// café docs\n    #[link_name = \"external\"]\n    fn chosen(arg: u32);\n    static VALUE: u32;\n    type Opaque;\n    foreign_macro!();\n    fn neighbor();\n}\n";
    let fixture = Fixture::new(source);
    for (line, first, last) in [
        (2, 2, 4),
        (3, 2, 4),
        (4, 2, 4),
        (5, 5, 5),
        (6, 6, 6),
        (7, 7, 7),
    ] {
        let body = source
            .lines()
            .skip(first - 1)
            .take(last - first + 1)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            fixture.extract(&format!("fixture.rs:{line}")),
            format!("File: fixture.rs, Lines: {first}-{last}\nBlock: complete\n\n{body}\n")
        );
    }
}

#[test]
fn extract_escapes_unsafe_controls_before_enforcing_output_cap() {
    let controls = "\u{1b}\u{7}\u{8}\u{c}\u{d}\u{7f}\u{85}\u{9b}";
    let fixture = Fixture::new(&format!("fn chosen() {{\n\t// {controls}\n}}\n"));
    let output = fixture.extract("fixture.rs:2");
    assert!(
        !output
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')),
        "unsafe control predicate"
    );
    assert!(output.contains("\n\t// "));
    for ch in controls.chars() {
        assert!(output.contains(&format!("\\u{{{:x}}}", ch as u32)));
    }
    fs::write(
        fixture.0.join("fixture.rs"),
        format!("fn chosen() {{\n\t// {}\n}}\n", controls.repeat(10)),
    )
    .expect("source");
    let result = fixture.run(&["extract", "fixture.rs:2", "--max-bytes", "128"]);
    assert!(result.status.success());
    assert!(result.stdout.len() <= 128);
    let output = String::from_utf8(result.stdout).expect("UTF-8");
    assert!(output.contains("Block: truncated"));
    assert!(output.ends_with("\n[truncated]\n"));
    assert!(!output
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')));
}

#[test]
fn extract_methods_types_and_same_line_items() {
    let fixture = Fixture::new("struct Owner {\n    value: u32,\n}\nimpl Owner {\n    /// method\n    pub fn get(&self) -> u32 {\n        self.value\n    }\n}\nfn first() {} fn second() {}\n");
    assert_eq!(
        fixture.extract("fixture.rs:2"),
        "File: fixture.rs, Lines: 1-3\nBlock: complete\n\nstruct Owner {\n    value: u32,\n}\n"
    );
    assert_eq!(fixture.extract("fixture.rs:7"), "File: fixture.rs, Lines: 5-8\nBlock: complete\n\n    /// method\n    pub fn get(&self) -> u32 {\n        self.value\n    }\n");
    assert!(!fixture.extract("fixture.rs:10").contains("fn second"));
}

#[test]
fn extract_outside_and_non_rust_are_approximate() {
    let fixture = Fixture::new(SOURCE);
    let outside = fixture.extract("fixture.rs:1");
    assert!(outside.contains("Block: approximate"));
    assert!(outside.contains("Lines: 1-4"));
    fs::write(fixture.0.join("plain.py"), "a\nb\nc\nd\ne\nf\n").expect("plain");
    assert_eq!(
        fixture.extract("plain.py:3"),
        "File: plain.py, Lines: 2-5\nBlock: approximate\n\nb\nc\nd\ne\n"
    );
}

#[test]
fn extract_truncates_explicitly_at_byte_cap_and_utf8_boundary() {
    let fixture = Fixture::new(&format!(
        "fn huge() {{\n    let _ = \"{}\";\n}}\n",
        "é".repeat(30_000)
    ));
    let output = fixture.extract("fixture.rs:2");
    assert!(output.len() <= 32 * 1024);
    assert!(output.contains("Block: truncated"));
    assert!(output.ends_with("\n[truncated]\n"));
    let result = fixture.run(&["extract", "fixture.rs:2", "--max-bytes", "128"]);
    assert!(result.status.success());
    assert!(result.stdout.len() <= 128);
    assert!(String::from_utf8(result.stdout)
        .expect("UTF-8")
        .ends_with("\n[truncated]\n"));
}

#[test]
fn extract_invalid_positions_and_usage_fail_closed() {
    let fixture = Fixture::new(SOURCE);
    for position in [
        "fixture.rs",
        "fixture.rs:0",
        "fixture.rs:-1",
        "fixture.rs:x",
        "fixture.rs:999",
        "fixture.rs:184467440737095516160",
        "missing.rs:1",
        "fixture.rs:1:2",
    ] {
        fixture.refused(position);
    }
    for args in [
        vec!["extract"],
        vec!["extract", "fixture.rs:1", "extra"],
        vec!["extract", "fixture.rs:1", "--max-bytes", "0"],
        vec!["extract", "fixture.rs:1", "--timeout", "x"],
        vec!["extract", "fixture.rs:1", "--json"],
    ] {
        let result = fixture.run(&args);
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn timeout_validation_rejects_overflow_and_duplicates_without_panics() {
    let fixture = Fixture::new(SOURCE);
    let huge = "18446744073709551615";
    let overflow = "18446744073709551616";
    let canary = "private-timeout-query-canary";
    for prefix in [
        vec!["extract", "private-timeout-path.rs:6"],
        vec!["search", canary],
        vec!["--message", canary],
        vec![canary],
    ] {
        for options in [
            vec!["--timeout", huge],
            vec!["--timeout", huge, "--timeout", "1"],
            vec!["--timeout", "1", "--timeout", huge],
            vec!["--timeout=18446744073709551615"],
            vec!["--timeout=18446744073709551615", "--timeout=1"],
            vec!["--timeout=1", "--timeout=18446744073709551615"],
            vec!["--timeout", huge, "--timeout=1"],
            vec!["--timeout=1", "--timeout", huge],
            vec!["--timeout=18446744073709551615", "--timeout", "1"],
            vec!["--timeout", "1", "--timeout=18446744073709551615"],
            vec!["--timeout", overflow],
            vec!["--timeout=18446744073709551616"],
            vec!["--timeout", canary],
            vec!["--timeout=private-timeout-query-canary"],
            vec!["--timeout", huge, "--max-bytes", "0"],
        ] {
            let mut arguments = prefix.clone();
            arguments.extend(&options);
            let result = fixture.run(&arguments);
            assert_eq!(result.status.code(), Some(2), "usage status required");
            assert!(result.stdout.is_empty(), "usage must not emit source");
            let error = String::from_utf8(result.stderr).expect("UTF-8 error");
            assert!(!error.contains("panicked"), "no panic diagnostics");
            assert!(!error.contains(canary), "query privacy");
            assert!(!error.contains("private-timeout-path"), "path privacy");
            assert!(error.starts_with("pbi-rs: "), "static usage diagnostic");
            if prefix[0] == "extract" && options.len() == 4 {
                assert!(
                    error.starts_with("pbi-rs: invalid extract options\n")
                        || error.starts_with("pbi-rs: invalid extract byte cap\n")
                );
            }
        }
    }
}

#[test]
fn extract_paths_ignores_symlinks_and_nonfiles_fail_closed() {
    let fixture = Fixture::new(SOURCE);
    fs::write(
        fixture.0.join(".gitignore"),
        "ignored.rs\n!.env\n!dir/.env.local\n!.private/\n!.private/**\n",
    )
    .expect("ignore");
    fs::write(fixture.0.join("ignored.rs"), SOURCE).expect("ignored");
    fs::write(fixture.0.join(".env"), "fixture-secret").expect("private fixture");
    fs::create_dir(fixture.0.join("dir")).expect("dir");
    symlink("fixture.rs", fixture.0.join("link.rs")).expect("link");
    symlink("dir", fixture.0.join("linked-dir")).expect("dir link");
    fs::write(fixture.0.join("dir/file.rs"), SOURCE).expect("nested");
    fs::write(fixture.0.join("dir/.gitignore"), "nested_ignored.rs\n").expect("nested ignore");
    fs::write(fixture.0.join("dir/nested_ignored.rs"), SOURCE).expect("nested ignored");
    fs::write(fixture.0.join("dir/.env.local"), "fixture-secret").expect("nested private");
    fs::create_dir(fixture.0.join(".private")).expect("hidden dir");
    fs::write(fixture.0.join(".private/file.rs"), SOURCE).expect("hidden source");
    assert!(fixture.extract("dir/file.rs:7").contains("Block: complete"));
    assert!(fixture
        .extract("./fixture.rs:7")
        .contains("Block: complete"));
    for position in [
        "../fixture.rs:1",
        "dir/../fixture.rs:1",
        ".env:1",
        "dir/.env.local:1",
        ".private/file.rs:1",
        "dir/nested_ignored.rs:1",
        "ignored.rs:1",
        "link.rs:1",
        "linked-dir/file.rs:1",
        "dir:1",
        "/etc/passwd:1",
    ] {
        fixture.refused(position);
    }
    let absolute = format!("{}:7", fixture.0.join("fixture.rs").display());
    assert!(fixture.extract(&absolute).contains("Block: complete"));
    let fifo = fixture.0.join("fifo.rs");
    let status = Command::new("mkfifo").arg(&fifo).status().expect("fifo");
    assert!(status.success());
    fixture.refused("fifo.rs:1");
}

#[test]
fn extract_retained_policy_preserves_precedence_and_static_controls() {
    let fixture = Fixture::new(SOURCE);
    fs::create_dir(fixture.0.join("dir")).expect("dir");
    fs::write(fixture.0.join("dir/file.rs"), SOURCE).expect("source");
    fs::write(fixture.0.join(".gitignore"), "*.rs\n!fixture.rs\n").expect("root policy");
    fs::write(fixture.0.join("dir/.gitignore"), "!file.rs\n").expect("nested policy");
    assert!(fixture.extract("dir/file.rs:7").contains("Block: complete"));
    fs::write(fixture.0.join(".ignore"), "dir/file.rs\n").expect("ignore precedence");
    fixture.refused("dir/file.rs:7");
    fs::write(fixture.0.join("dir/.ignore"), "!file.rs\n").expect("nested override");
    assert!(fixture.extract("dir/file.rs:7").contains("Block: complete"));
    fs::write(fixture.0.join(".ignore"), "dir/\n").expect("ancestor denial");
    fixture.refused("dir/file.rs:7");
}

#[test]
fn extract_large_binary_files_and_expired_deadline_fail_closed() {
    let fixture = Fixture::new(SOURCE);
    fs::write(
        fixture.0.join("oversize.rs"),
        vec![b'a'; 2 * 1024 * 1024 + 1],
    )
    .expect("oversize");
    fs::write(fixture.0.join("binary.rs"), [0xff, 0xfe]).expect("binary");
    fixture.refused("oversize.rs:1");
    fixture.refused("binary.rs:1");
    let result = fixture.run(&["extract", "fixture.rs:7", "--timeout", "0"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}

#[test]
fn extract_cross_device_path_is_refused() {
    let fixture = Fixture::new(SOURCE);
    // Linux procfs is a different device and cannot be admitted as this root's source.
    fixture.refused("/proc/self/status:1");
    symlink("/proc", fixture.0.join("proc")).expect("proc link");
    fixture.refused("proc/self/status:1");
}

#[test]
fn symbols_rust_top_level_items_and_impl_methods_have_lines() {
    let fixture = Fixture::new("placeholder");
    fs::write(
        fixture.0.join("fixture.rs"),
        "fn first() {}\nstruct Owner;\nimpl Owner {\n    fn method(&self) {}\n}\n",
    )
    .expect("source");
    let result = fixture.run(&["symbols", "fixture.rs"]);
    assert!(result.status.success(), "Rust symbols command must succeed");
    assert!(result.stderr.is_empty(), "no stderr");
    assert_eq!(
        String::from_utf8(result.stdout).expect("UTF-8"),
        "1: fn first\n2: struct Owner\n4: fn method\n"
    );
}

#[test]
fn symbols_python_defs_and_classes_have_lines() {
    let fixture = Fixture::new("placeholder");
    fs::write(
        fixture.0.join("fixture.py"),
        "def first():\n    pass\nclass Owner:\n    pass\n",
    )
    .expect("source");
    let result = fixture.run(&["symbols", "fixture.py"]);
    assert!(
        result.status.success(),
        "Python symbols command must succeed"
    );
    assert!(result.stderr.is_empty(), "no stderr");
    assert_eq!(
        String::from_utf8(result.stdout).expect("UTF-8"),
        "1: def first\n3: class Owner\n"
    );
}

#[test]
fn symbols_go_functions_and_types_have_lines() {
    let fixture = Fixture::new("placeholder");
    fs::write(
        fixture.0.join("fixture.go"),
        "package sample\nfunc first() {}\ntype Owner struct{}\n",
    )
    .expect("source");
    let result = fixture.run(&["symbols", "fixture.go"]);
    assert!(result.status.success(), "Go symbols command must succeed");
    assert!(result.stderr.is_empty(), "no stderr");
    assert_eq!(
        String::from_utf8(result.stdout).expect("UTF-8"),
        "2: func first\n3: type Owner\n"
    );
}

#[test]
fn symbols_python_identifier_preserves_combining_mark() {
    let fixture = Fixture::new("placeholder");
    fs::write(fixture.0.join("fixture.py"), "def cafe\u{301}(): pass\n").expect("source");
    let result = fixture.run(&["symbols", "fixture.py"]);
    assert!(
        result.status.success(),
        "Python symbols command must succeed"
    );
    assert!(result.stderr.is_empty(), "no stderr");
    let actual = String::from_utf8(result.stdout).expect("UTF-8");
    assert!(
        actual == "1: def cafe\u{301}\n",
        "Python scanner must preserve the full Unicode identifier"
    );
}

#[test]
fn symbols_go_compact_receiver_and_prefix_boundary() {
    let fixture = Fixture::new("placeholder");
    fs::write(
        fixture.0.join("fixture.go"),
        "package fixture\ntype Owner struct{}\nfunc(o *Owner) Method() {}\nfunc (o *Owner) Spaced() {}\nfuncion() {}\n",
    )
    .expect("source");
    let result = fixture.run(&["symbols", "fixture.go"]);
    assert!(result.status.success(), "Go symbols command must succeed");
    assert!(result.stderr.is_empty(), "no stderr");
    let actual = String::from_utf8(result.stdout).expect("UTF-8");
    assert!(
        actual == "2: type Owner\n3: func Method\n4: func Spaced\n",
        "Go scanner must accept compact receiver syntax and reject keyword prefixes"
    );
}

#[test]
fn symbols_missing_non_utf8_ignored_hidden_links_and_fifo_fail_closed() {
    let fixture = Fixture::new("fn allowed() {}\n");
    for path in ["missing.rs", "binary.rs"] {
        if path == "binary.rs" {
            fs::write(fixture.0.join(path), [0xff, 0xfe]).expect("binary source");
        }
        let result = fixture.run(&["symbols", path]);
        assert!(!result.status.success(), "invalid source must fail");
        assert!(result.stdout.is_empty(), "no partial output on failure");
        assert!(!String::from_utf8_lossy(&result.stderr).contains(path));
    }
    fs::write(fixture.0.join(".private.rs"), "fn private_canary() {}\n").expect("hidden");
    fs::write(fixture.0.join(".ignore"), "!.private.rs\n").expect("negation");
    let hidden = fixture.run(&["symbols", ".private.rs"]);
    assert!(
        !hidden.status.success(),
        "hidden hard-deny must beat negation"
    );
    assert!(hidden.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&hidden.stderr).contains("private_canary"));
    fs::write(fixture.0.join("ignored.rs"), "fn ignored() {}\n").expect("ignored");
    fs::write(fixture.0.join(".gitignore"), "ignored.rs\n").expect("policy");
    let ignored = fixture.run(&["symbols", "ignored.rs"]);
    assert!(!ignored.status.success(), "ignored sources must fail");
    assert!(ignored.stdout.is_empty());
    symlink("fixture.rs", fixture.0.join("alias.rs")).expect("source link");
    let linked = fixture.run(&["symbols", "alias.rs"]);
    assert!(!linked.status.success(), "source symlinks must fail");
    assert!(linked.stdout.is_empty());
    let outside = fixture.run(&["symbols", "../outside.rs"]);
    assert!(!outside.status.success(), "paths outside root must fail");
    assert!(outside.stdout.is_empty());
    let fifo = fixture.0.join("fifo.rs");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("fifo")
        .success());
    let special = fixture.run(&["symbols", "fifo.rs"]);
    assert!(!special.status.success(), "nonregular sources must fail");
    assert!(special.stdout.is_empty());
}

#[test]
fn symbols_count_and_output_bytes_are_bounded() {
    let fixture = Fixture::new("placeholder");
    let many = (0..300)
        .map(|index| format!("def f{index}():\n    pass\n"))
        .collect::<String>();
    fs::write(fixture.0.join("many.py"), many).expect("many symbols");
    let count = fixture.run(&["symbols", "many.py"]);
    assert!(count.status.success(), "bounded listing must succeed");
    let count = String::from_utf8(count.stdout).expect("UTF-8");
    assert!(count.ends_with("[truncated]\n"));
    assert_eq!(count.lines().count(), 257);
    let long = (0..300)
        .map(|index| format!("def {}_{index}():\n    pass\n", "x".repeat(200)))
        .collect::<String>();
    fs::write(fixture.0.join("long.py"), long).expect("long names");
    let bytes = fixture.run(&["symbols", "long.py"]);
    assert!(bytes.status.success(), "output must truncate successfully");
    assert!(bytes.stdout.len() <= 32 * 1024);
    assert!(String::from_utf8(bytes.stdout)
        .unwrap_or_else(|_| panic!("symbols output must remain UTF-8"))
        .ends_with("[truncated]\n"));
}
