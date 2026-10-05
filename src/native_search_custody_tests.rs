//! Synthetic SSD-only hostile substitutions at the production read boundary.
use super::*;
use std::cell::RefCell;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

thread_local! {
    static AFTER_METADATA: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    static AFTER_POLICY: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}
pub(super) fn after_metadata() {
    let hook = AFTER_METADATA.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
pub(super) fn after_policy_validation() {
    let hook = AFTER_POLICY.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
fn substitute(hook: impl FnOnce() + 'static) {
    AFTER_METADATA.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Fixture {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp")
        .join(format!("pbi-327-{}-{nonce}", std::process::id()));
    fs::create_dir_all(path.join("root/dir")).expect("fixture");
    Fixture(path)
}
fn limits() -> SearchLimits {
    SearchLimits {
        deadline: Instant::now() + Duration::from_secs(8),
        max_results: 8,
        language: None,
        ignores: Vec::new(),
    }
}
#[test]
fn walk_reuses_the_retained_parent_for_leaf_admission() {
    let fixture = fixture();
    let root = fixture.0.join("root");
    fs::remove_dir(root.join("dir")).expect("remove unused fixture sibling");
    let relative = PathBuf::from("child/".repeat(16)).join("visible.rs");
    fs::create_dir_all(root.join(relative.parent().expect("parent"))).expect("parents");
    fs::write(root.join(&relative), "fn visible() {}\n").expect("source");
    crate::extract::SOURCE_COMPONENT_OPENS.with(|count| count.set(0));
    let files = walk(&root, fs::metadata(&root).expect("root").dev(), &limits()).expect("walk");
    assert_eq!(files, vec![root.join(relative)]);
    assert_eq!(
        crate::extract::SOURCE_COMPONENT_OPENS.with(|count| count.get()),
        153,
        "each entry must open its parent ancestry once, then its leaf through that held parent"
    );
}

#[test]
fn absent_ancestor_policies_do_not_create_quadratic_match_work() {
    let fixture = fixture();
    let root = fixture.0.join("root");
    fs::write(root.join(".gitignore"), "ignored.rs\n").expect("policy");
    let relative = PathBuf::from("child/".repeat(16)).join("visible.rs");
    fs::create_dir_all(root.join(relative.parent().expect("parent"))).expect("parents");
    fs::write(root.join(&relative), "fn visible() {}\n").expect("source");
    let device = fs::metadata(&root).expect("root").dev();
    let (_, directories) = open_source(
        open_root(&root).expect("owner"),
        &relative,
        device,
        &limits(),
        false,
    )
    .expect("source owners");
    let compiler = std::sync::Mutex::new(crate::extract::PolicyCompiler::default());
    crate::extract::POLICY_MATCH_VISITS.with(|count| count.set(0));
    assert!(policy_admitted(
        &root,
        &relative,
        &directories,
        device,
        &limits(),
        false,
        &compiler
    )
    .expect("policy"));
    assert!(
        crate::extract::POLICY_MATCH_VISITS.with(|count| count.get()) <= 2 * 17,
        "matching must visit present policies, not every absent ancestor slot"
    );
}

#[test]
fn fresh_policy_matching_reuses_derived_ancestor_results() {
    let fixture = fixture();
    let root = fixture.0.join("root");
    fs::write(root.join(".gitignore"), "ignored.rs\n").expect("policy");
    let relative = PathBuf::from("child/".repeat(64)).join("visible.rs");
    fs::create_dir_all(root.join(relative.parent().expect("parent"))).expect("parents");
    fs::write(root.join(&relative), "fn review_marker() {}\n").expect("source");
    crate::extract::POLICY_MATCH_VISITS.with(|count| count.set(0));
    let files = walk(&root, fs::metadata(&root).expect("root").dev(), &limits()).expect("walk");
    assert_eq!(files, vec![root.join(&relative)]);
    let calls = crate::extract::POLICY_MATCH_VISITS.with(|count| count.get());
    assert!(
        calls <= 2 * 67,
        "fresh identical policy bytes must not rematch every ancestor per entry: {calls}"
    );
    eprintln!("actual matcher calls={calls}");
    let device = fs::metadata(&root).expect("root").dev();
    let (_, directories) = open_source(
        open_root(&root).expect("owner"),
        &relative,
        device,
        &limits(),
        false,
    )
    .expect("owners");
    let compiler = std::sync::Mutex::new(crate::extract::PolicyCompiler::default());
    let admitted = || {
        policy_admitted(
            &root,
            &relative,
            &directories,
            device,
            &limits(),
            false,
            &compiler,
        )
        .expect("fresh admission")
    };
    assert!(admitted());
    fs::write(root.join(".gitignore"), "visible.rs\n").expect("changed policy");
    assert!(
        !admitted(),
        "changed freshly read bytes invalidate derived matches"
    );
    fs::write(root.join(".ignore"), "!visible.rs\n").expect("absent to present");
    assert!(admitted(), ".ignore must outrank .gitignore");
    fs::remove_file(root.join(".ignore")).expect("present to absent");
    assert!(!admitted());
    fs::write(root.join(".gitignore"), "ignored.rs\n").expect("restore policy");
    assert!(admitted());
    fs::write(root.join("child/.ignore"), "visible.rs\n").expect("new ancestor denial");
    assert!(
        !admitted(),
        "a previously absent ancestor policy must still deny"
    );
}

#[test]
fn unchanged_policy_bytes_compile_once_per_search() {
    let fixture = fixture();
    let root = fixture.0.join("root");
    fs::write(root.join(".gitignore"), "ignored.rs\n").expect("policy");
    fs::write(root.join("visible.rs"), "fn review_marker() {}\n").expect("source");
    crate::extract::POLICY_BUILDS.with(|count| count.set(0));
    let hits = search_repository(&root, "review_marker", &limits()).expect("search");
    assert!(!hits.is_empty());
    assert_eq!(
        crate::extract::POLICY_BUILDS.with(|count| count.get()),
        1,
        "freshly reread unchanged policy bytes must not recompile per entry/read"
    );
}

#[test]
fn ancestor_symlink_substitution_is_denied_in_all_readers() {
    let mut denied = Vec::new();
    for mode in 0..3 {
        let fixture = fixture();
        let root = fixture.0.join("root");
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).expect("outside");
        fs::write(
            root.join("dir/synthetic_candidate.rs"),
            "fn ordinary() {}\n",
        )
        .expect("source");
        fs::write(
            outside.join("synthetic_candidate.rs"),
            "fn synthetic_external_marker() {}\n",
        )
        .expect("external");
        let original = root.join("dir");
        let held = fixture.0.join("held");
        substitute(move || {
            fs::rename(&original, &held).expect("retain original");
            symlink(&outside, &original).expect("substitute ancestor");
        });
        let refused = match mode {
            0 => search_repository(&root, "synthetic_external_marker", &limits()).is_err(),
            1 => search_raw_repository(
                &root,
                "synthetic_external_marker",
                &limits(),
                &RawSearchOptions {
                    exact: false,
                    stem: false,
                    exclude_filenames: false,
                    merge_threshold: 2,
                    strict: None,
                },
            )
            .is_err(),
            _ => candidate_symbols(&root, "synthetic_candidate", &limits()).is_err(),
        };
        denied.push(refused);
    }
    assert_eq!(
        denied,
        vec![true; 3],
        "all shared readers must reject redirected ancestors"
    );
}
#[test]
fn retained_root_and_current_policy_are_required() {
    for replace_root in [false, true] {
        let fixture = fixture();
        let root = fixture.0.join("root");
        fs::write(
            root.join("dir/synthetic_candidate.rs"),
            "fn ordinary() {}\n",
        )
        .expect("source");
        let named = root.clone();
        let held = fixture.0.join("held");
        substitute(move || {
            if replace_root {
                fs::rename(&named, &held).expect("retain root");
                fs::create_dir_all(named.join("dir")).expect("replacement root");
                fs::write(
                    named.join("dir/synthetic_candidate.rs"),
                    "fn replacement() {}\n",
                )
                .expect("replacement");
            } else {
                fs::write(named.join("dir/.ignore"), "synthetic_candidate.rs\n")
                    .expect("new denial");
            }
        });
        assert!(candidate_symbols(&root, "synthetic_candidate", &limits()).is_err());
    }
}

#[test]
fn candidate_reread_keeps_the_two_mib_cap() {
    let fixture = fixture();
    let root = fixture.0.join("root");
    let path = root.join("dir/synthetic_candidate.rs");
    fs::write(&path, "fn ordinary() {}\n").expect("source");
    substitute(move || {
        let mut bytes = b"fn oversized() {}\n".to_vec();
        bytes.resize(MAX_FILE_BYTES as usize + 1, b' ');
        fs::write(&path, bytes).expect("growth");
    });
    assert!(candidate_symbols(&root, "synthetic_candidate", &limits())
        .expect("bounded skip")
        .is_empty());
}

#[test]
fn policy_symlink_is_not_authority() {
    for policy in [".gitignore", ".ignore"] {
        let fixture = fixture();
        let root = fixture.0.join("root");
        fs::write(root.join("ordinary.rs"), "fn ordinary() {}\n").expect("source");
        let outside = fixture.0.join("external-policy");
        fs::write(&outside, "ordinary.rs\n").expect("synthetic policy");
        symlink(outside, root.join(policy)).expect("policy link");
        assert!(
            matches!(
                walk(
                    &root,
                    fs::metadata(&root).expect("metadata").dev(),
                    &limits()
                ),
                Err(SearchFailure::Unavailable)
            ),
            "linked policy must fail closed"
        );
    }
}
fn unreadable_policy_entry(policy: &str, directory: bool) {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let root = fixture.0.join("root");
    fs::write(root.join("visible.rs"), "fn review_marker() {}\n").expect("visible");
    let denied = root.join(if directory { "ignored" } else { "ignored.rs" });
    if directory {
        fs::create_dir(&denied).expect("directory");
    } else {
        fs::write(&denied, "fn private_marker() {}\n").expect("ignored source");
    }
    fs::write(
        root.join(policy),
        if directory {
            "ignored/\n"
        } else {
            "ignored.rs\n"
        },
    )
    .expect("policy");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).expect("unreadable");
    let raw = RawSearchOptions {
        exact: false,
        stem: false,
        exclude_filenames: false,
        merge_threshold: 2,
        strict: None,
    };
    let admitted = vec![
        search_repository(&root, "review_marker", &limits()).is_ok_and(|hits| !hits.is_empty()),
        search_raw_repository(&root, "review_marker", &limits(), &raw)
            .is_ok_and(|hits| !hits.0.is_empty()),
        candidate_symbols(&root, "visible_marker", &limits()).is_ok_and(|hits| !hits.is_empty()),
        crate::extract::extract(&root, Path::new("visible.rs"), 1, &limits(), 4096)
            .is_ok_and(|text| text.contains("review_marker")),
    ];
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).expect("restore");
    assert_eq!(
        admitted,
        vec![true; 4],
        "excluded entry must not abort shared readers"
    );
}
#[test]
fn inherited_policy_precedence_and_unreadable_admission_stay_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let root = fixture.0.join("root");
    let source = root.join("dir/visible.rs");
    fs::write(&source, "fn review_marker() {}\n").expect("source");
    fs::write(root.join(".gitignore"), "*.rs\n").expect("root policy");
    fs::write(root.join("dir/.gitignore"), "!visible.rs\n").expect("nearest policy");
    assert!(!search_repository(&root, "review_marker", &limits())
        .expect("nested negation")
        .is_empty());
    fs::write(root.join(".ignore"), "*.rs\n").expect("higher class");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o0)).expect("unreadable");
    let denied = search_repository(&root, "review_marker", &limits());
    fs::write(root.join("dir/.ignore"), "!visible.rs\n").expect("nearest higher class");
    let admitted_unreadable = search_repository(&root, "review_marker", &limits());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("restore");
    assert!(denied.expect("excluded unreadable").is_empty());
    assert!(matches!(
        admitted_unreadable,
        Err(SearchFailure::Unavailable)
    ));
    assert!(!search_repository(&root, "review_marker", &limits())
        .expect("nearest negation")
        .is_empty());
}
#[test]
fn ignored_unreadable_gitignore_file_preserves_shared_readers() {
    unreadable_policy_entry(".gitignore", false);
}
#[test]
fn ignored_unreadable_gitignore_directory_preserves_shared_readers() {
    unreadable_policy_entry(".gitignore", true);
}
#[test]
fn ignored_unreadable_ignore_file_preserves_shared_readers() {
    unreadable_policy_entry(".ignore", false);
}
#[test]
fn ignored_unreadable_ignore_directory_preserves_shared_readers() {
    unreadable_policy_entry(".ignore", true);
}
// The outer process owns a strict deadline and reaps the child even on RED.
// Mutation is synchronous at the metadata/open seam; no race sleeps are used.
#[test]
fn fifo_substitution_and_policy_opens_are_bounded() {
    if let Ok(kind) = std::env::var("PBI_327_FIFO_CHILD") {
        let root = PathBuf::from(std::env::var_os("PBI_327_FIFO_ROOT").expect("root"));
        if kind == "policy" {
            let policy = root.join(".gitignore");
            AFTER_POLICY.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    fs::remove_file(&policy).expect("remove validated policy");
                    let name = std::ffi::CString::new(policy.as_os_str().as_bytes()).expect("path");
                    // SAFETY: owned synthetic NUL-terminated path.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }))
            });
            assert!(walk(
                &root,
                fs::metadata(&root).expect("metadata").dev(),
                &limits()
            )
            .is_err());
        } else {
            let path = root.join("dir/synthetic_candidate.rs");
            substitute(move || {
                fs::remove_file(&path).expect("remove regular leaf");
                let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
                // SAFETY: NUL-terminated synthetic path, no borrowed memory escapes.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            });
            let result = if kind == "candidate" {
                candidate_symbols(&root, "synthetic_candidate", &limits()).map(|_| ())
            } else {
                search_repository(&root, "ordinary", &limits()).map(|_| ())
            };
            assert!(result.is_err(), "FIFO must be rejected");
        }
        return;
    }
    let mut completed = Vec::new();
    for kind in ["source", "candidate", "policy"] {
        let fixture = fixture();
        let root = fixture.0.join("root");
        fs::write(
            root.join("dir/synthetic_candidate.rs"),
            "fn ordinary() {}\n",
        )
        .expect("source");
        if kind == "policy" {
            fs::write(root.join(".gitignore"), "").expect("initial regular policy");
        }
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "native_search::issue_327_tests::fifo_substitution_and_policy_opens_are_bounded",
            ])
            .env("PBI_327_FIFO_CHILD", kind)
            .env("PBI_327_FIFO_ROOT", &root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("child");
        let deadline = Instant::now() + Duration::from_secs(3);
        let success = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status.success();
            }
            if Instant::now() >= deadline {
                child.kill().expect("bounded kill");
                child.wait().expect("reap");
                break false;
            }
            std::thread::yield_now();
        };
        use std::os::unix::fs::FileTypeExt;
        let substituted = if kind == "policy" {
            root.join(".gitignore")
        } else {
            root.join("dir/synthetic_candidate.rs")
        };
        assert!(
            fs::symlink_metadata(substituted)
                .expect("mutation witness")
                .file_type()
                .is_fifo(),
            "the deterministic substitution must actually have executed"
        );
        completed.push(success);
    }
    assert_eq!(
        completed,
        vec![true; 3],
        "source/candidate/policy FIFO opens must finish and reject"
    );
}
