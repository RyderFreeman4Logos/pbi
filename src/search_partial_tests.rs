use super::*;
use std::cell::RefCell;

thread_local! {
    static AFTER_FILE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}
pub(super) fn after_file() -> Result<(), SearchFailure> {
    let hook = AFTER_FILE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
        return Err(SearchFailure::Deadline);
    }
    Ok(())
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pbi-deadline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(path.join("nested")).expect("fixture");
        fs::write(
            path.join("nested/a.rs"),
            "fn decoy() {}\nfn deadline_marker() {}\n",
        )
        .expect("source");
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        AFTER_FILE.with(|slot| slot.borrow_mut().take());
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn limits() -> SearchLimits {
    SearchLimits {
        deadline: Instant::now() + Duration::from_secs(8),
        max_results: 8,
        language: None,
        ignores: Vec::new(),
    }
}
fn search(root: &Path, mode: usize, limits: &SearchLimits) -> Result<(), SearchFailure> {
    match mode {
        0 => search_repository(root, "deadline_marker", limits).map(|_| ()),
        1 => search_regex_repository(
            root,
            &regex::Regex::new("deadline_marker").expect("regex"),
            limits,
        )
        .map(|_| ()),
        _ => search_raw_repository(
            root,
            "deadline_marker",
            limits,
            &RawSearchOptions {
                compact: true,
                exact: false,
                stem: false,
                exclude_filenames: false,
                merge_threshold: 0,
                strict: None,
            },
        )
        .map(|_| ()),
    }
}
#[test]
fn deadline_retains_verified_identity_in_all_search_paths() {
    for mode in 0..3 {
        let fixture = Fixture::new();
        AFTER_FILE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| {})));
        let Err(SearchFailure::PartialDeadline(locations)) = search(&fixture.0, mode, &limits())
        else {
            panic!("missing explicit partial outcome, mode={mode}")
        };
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].file, "nested/a.rs");
        assert_eq!(locations[0].line, 2);
        assert!(
            search(&fixture.0, mode, &limits()).is_ok(),
            "prior outcome contaminated invocation"
        );
    }
}
#[test]
fn deadline_rejects_changed_bytes_policy_leaf_ancestor_and_root() {
    for mode in 0..3 {
        for mutation in 0..6 {
            let fixture = Fixture::new();
            let root = fixture.0.clone();
            AFTER_FILE.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || match mutation {
                    0 => fs::write(root.join("nested/a.rs"), "fn changed_marker() {}\n")
                        .expect("change"),
                    1 => fs::write(root.join(".ignore"), "nested/\n").expect("policy"),
                    2 => {
                        fs::rename(root.join("nested/a.rs"), root.join("saved.rs")).expect("move");
                        fs::write(
                            root.join("nested/a.rs"),
                            "fn decoy() {}\nfn deadline_marker() {}\n",
                        )
                        .expect("replacement");
                    }
                    3 => {
                        fs::rename(root.join("nested"), root.join("saved")).expect("move");
                        fs::create_dir(root.join("nested")).expect("replacement");
                        fs::write(
                            root.join("nested/a.rs"),
                            "fn decoy() {}\nfn deadline_marker() {}\n",
                        )
                        .expect("source");
                    }
                    4 => fs::write(root.join("nested/.gitignore"), "a.rs\n").expect("policy"),
                    _ => {
                        let moved = root.with_extension("moved");
                        fs::rename(&root, &moved).expect("move root");
                        fs::create_dir(&root).expect("replacement");
                        fs::remove_dir_all(moved).expect("cleanup held root");
                    }
                }))
            });
            assert!(
                matches!(
                    search(&fixture.0, mode, &limits()),
                    Err(SearchFailure::Deadline)
                ),
                "unsafe partial escaped: mode={mode}, mutation={mutation}"
            );
        }
    }
}
#[test]
fn deadline_walk_and_expired_verification_are_fail_closed() {
    for mode in 0..3 {
        let fixture = Fixture::new();
        let mut limits = limits();
        limits.deadline = Instant::now();
        assert!(matches!(
            search(&fixture.0, mode, &limits),
            Err(SearchFailure::Deadline)
        ));
    }
}
#[test]
fn limit_never_publishes_retained_locations() {
    let fixture = Fixture::new();
    let limits = limits();
    let result: Result<(), SearchFailure> = run(&fixture.0, &limits, |scan, progress| {
        let scope = open_root_scope(&fixture.0)?;
        let source = read_owned_source(
            &fixture.0,
            &scope.file,
            &fixture.0.join("nested/a.rs"),
            scope.device,
            scan,
            &scope.compiler,
        )?
        .expect("owned");
        progress.retain(source, [2].into_iter(), scan)?;
        Err(SearchFailure::Limit)
    });
    assert!(matches!(result, Err(SearchFailure::Limit)));
}
