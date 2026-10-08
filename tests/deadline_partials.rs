use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn root(label: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp")
        .join(format!("pbi-rs-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&path).expect("fixture");
    path
}

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("PBI_RS_ADK_ENABLE", "0")
        .current_dir(root)
        .args(args)
        .output()
        .expect("pbi")
}

#[test]
fn search_deadline_keeps_verified_locations_without_source() {
    let root = root("deadline-search");
    fs::write(root.join("a_kept.rs"), "fn kept_symbol() {}\n").expect("hit");
    let pad = "fn other() {}\n".repeat(140_000);
    for index in 0..12 {
        fs::write(
            root.join(format!("m_{index:02}.rs")),
            format!("fn kept_symbol() {{}}\n{pad}"),
        )
        .expect("pad");
    }
    let output = run(&root, &["search", "--timeout=1", "kept_symbol"]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stdout.lines().any(|line| line == "a_kept.rs:1"),
        "verified location dropped"
    );
    assert!(!stdout.contains("fn "), "source leaked");
    assert!(stderr.contains("stage=initial_search"), "{stderr}");
    assert!(stderr.contains("stage_status=deadline"), "{stderr}");
    assert!(stderr.contains("deadline_s=1"), "{stderr}");
    assert!(!stderr.contains("kept_symbol"), "query leaked");
    assert!(!stderr.contains(".rs"), "path leaked");
}

#[test]
fn semantic_deadline_keeps_verified_locations_without_answer() {
    let root = root("deadline-semantic");
    fs::write(root.join("a_kept.rs"), "fn kept_symbol() {}\n").expect("hit");
    fs::write(
        root.join("m_more.rs"),
        format!(
            "fn kept_symbol() {{}}\n{}",
            "fn other() {}\n".repeat(140_000)
        ),
    )
    .expect("more");
    let output = run(&root, &["--timeout=1", "where is kept_symbol"]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stdout.lines().any(|line| line == "a_kept.rs:1"),
        "verified location dropped"
    );
    assert!(!stdout.contains("fn "), "source leaked");
    assert!(stderr.contains("stage_status=deadline"), "{stderr}");
    assert!(stderr.contains("deadline_s=1"), "{stderr}");
    assert!(!stderr.contains("kept_symbol"), "query leaked");
    assert!(!stderr.contains(".rs"), "path leaked");
}
