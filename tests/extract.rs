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
