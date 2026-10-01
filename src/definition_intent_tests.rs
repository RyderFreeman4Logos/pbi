use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pbi-rs-definition-{suffix}"));
        fs::create_dir_all(&root).expect("fixture directory");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) -> String {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("fixture directory");
        fs::write(&path, source).expect("fixture source");
        format!("File: {}, Lines: 1-99\n", path.display())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn cited(root: &Path, report: &EvidenceReport) -> String {
    let location = &report.evidence()[0].location;
    fs::read_to_string(root.join(location.path()))
        .expect("read")
        .lines()
        .skip(location.start_line() - 1)
        .take(location.end_line() - location.start_line() + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

fn expect_definition(root: &Path, output: &str, query: &str, needle: &str) {
    let report = verify_probe_evidence(output, root, query, 4)
        .unwrap_or_else(|error| panic!("{query} in {output} produced {error:?}"));
    assert!(report.is_complete(), "{query} was partial: {report:?}");
    let text = cited(root, &report);
    assert!(
        text.contains(needle),
        "{query} cited {text}, expected {needle}"
    );
}

fn expect_identity(
    root: &Path,
    output: &str,
    query: &str,
    line: usize,
    name: &str,
    owner: Option<&str>,
) {
    let report = verify_probe_evidence(output, root, query, 4)
        .unwrap_or_else(|error| panic!("{query} produced {error:?}"));
    assert!(report.is_complete(), "{query} was partial: {report:?}");
    let evidence = &report.evidence()[0];
    assert_eq!(evidence.location().start_line(), line, "{query}");
    assert_eq!(evidence.symbol(), Some(name), "{query}");
    if query.contains("::") {
        let groups = query_groups(query).expect(query);
        let group = groups
            .iter()
            .find(|group| group.symbol.as_deref() == Some(name))
            .expect(query);
        assert_eq!(group.owner.as_deref(), owner, "{query}");
    }
}

fn expect_miss(root: &Path, output: &str, query: &str) {
    let identity = query_groups(query).map(|groups| {
        groups
            .iter()
            .map(|group| {
                (
                    group.symbol.clone(),
                    group.owner.clone(),
                    group.path.clone(),
                )
            })
            .collect::<Vec<_>>()
    });
    match verify_probe_evidence(output, root, query, 4) {
        Err(EvidenceError::NoSourceLocations) => {}
        other => panic!("{query} returned {other:?}; identity={identity:?}"),
    }
}

#[test]
fn definition_query_uses_the_real_declaration() {
    let fixture = Fixture::new();
    let output = fixture.write(
        "src/lib.rs",
        "impl SourceLocation {\n    pub fn display_relative(&self, root: &Path) -> Result<String, EvidenceError> {\n        let _ = root;\n        Ok(String::new())\n    }\n}\n\nfn other() {\n    validate_annotations(unit, location);\n}\n\nfn validate_annotations(unit: &str, location: &str) {\n    let _ = (unit, location);\n}\n",
    );
    for (query, needle) in [
        (
            "where is the implementation of SourceLocation display_relative",
            "fn display_relative",
        ),
        (
            "where is SourceLocation display_relative defined",
            "fn display_relative",
        ),
        (
            "where is fn display_relative defined in src/lib.rs",
            "fn display_relative",
        ),
        (
            "where is the implementation location of validate_annotations",
            "fn validate_annotations(",
        ),
    ] {
        let report = verify_probe_evidence(&output, &fixture.root, query, 4).expect(query);
        assert!(report.is_complete(), "{query}");
        let text = cited(&fixture.root, &report);
        assert!(text.contains(needle), "{query} cited {text}");
        if query.contains("SourceLocation") {
            assert_eq!(
                report.evidence()[0].symbol(),
                Some("display_relative"),
                "{query}"
            );
        }
    }
}

#[test]
fn bare_symbol_still_accepts_a_call_mention() {
    let fixture = Fixture::new();
    let output = fixture.write(
        "src/lib.rs",
        "fn other() {\n    validate_annotations(unit, location);\n}\n",
    );
    let report = verify_probe_evidence(&output, &fixture.root, "validate_annotations", 4)
        .expect("bare symbol");
    assert!(report.is_complete());
    assert!(
        cited(&fixture.root, &report).contains("validate_annotations(unit, location)"),
        "bare symbol lost the call"
    );
}

#[test]
fn supported_language_declarations_are_definitions() {
    let cases = [
        (
            "src/lib.rs",
            "async fn target_func(value: u8) -> u8 {\n    value\n}\n",
            "async fn target_func",
        ),
        (
            "src/empty.rs",
            "fn target_func() {}\n",
            "fn target_func() {}",
        ),
        (
            "src/private.rs",
            "struct Target_Type {\n    value: u8,\n}\n",
            "struct Target_Type",
        ),
        (
            "pkg/target.py",
            "def target_func(value):\n    return value\n",
            "def target_func",
        ),
        (
            "web/target.js",
            "function target_func(value) {\n    return value;\n}\n",
            "function target_func",
        ),
        (
            "native/target.c",
            "int target_func(int value) {\n    return value;\n}\n",
            "int target_func",
        ),
    ];
    for (path, source, needle) in cases {
        let fixture = Fixture::new();
        let output = fixture.write(path, source);
        let symbol = if path.ends_with("private.rs") {
            "Target_Type"
        } else {
            "target_func"
        };
        expect_definition(
            &fixture.root,
            &output,
            &format!("where is {symbol} defined"),
            needle,
        );
    }
}

#[test]
fn ordinary_implementation_prose_is_not_a_definition_request() {
    let fixture = Fixture::new();
    let output = fixture.write(
        "docs/guide.md",
        "The implementation strategy uses bounded batches to reduce memory.\n",
    );
    expect_definition(
        &fixture.root,
        &output,
        "implementation strategy bounded batches",
        "implementation strategy uses bounded batches",
    );
}

#[test]
fn owner_qualified_definition_uses_the_real_declaration() {
    let fixture = Fixture::new();
    let output = fixture.write(
        "src/lib.rs",
        "impl Owner {\n    pub fn target_func(&self) {}\n}\n",
    );
    expect_definition(
        &fixture.root,
        &output,
        "where is Owner::target_func defined in src/lib.rs",
        "fn target_func",
    );
    let space = Fixture::new();
    let space_output = space.write(
        "src/lib.rs",
        "pub struct Owner;\nimpl Owner { pub fn run(&self) {} }\n",
    );
    let report = verify_probe_evidence(&space_output, &space.root, "where is Owner run defined", 4)
        .expect("space member");
    assert!(report.is_complete());
    assert_eq!(report.evidence()[0].symbol(), Some("run"));
    assert_eq!(report.evidence()[0].location().start_line(), 2);
    let parsed = query_groups("where is Owner run defined").expect("parse");
    assert_eq!(parsed[0].owner.as_deref(), Some("Owner"));
    assert_eq!(parsed[0].symbol.as_deref(), Some("run"));
    expect_miss(&space.root, &space_output, "where is Owner missing defined");
}

#[test]
fn definition_identity_rejects_wrong_owner_path_and_parameter() {
    let owner = Fixture::new();
    let owner_output = owner.write(
        "src/lib.rs",
        "impl WrongOwner {\n    fn target_func(&self) {}\n}\n",
    );
    expect_miss(
        &owner.root,
        &owner_output,
        "where is RequestedOwner::target_func defined",
    );

    let path = Fixture::new();
    let path_output = path.write("src/other.rs", "fn target_func() { let _ = 1; }\n");
    expect_miss(
        &path.root,
        &path_output,
        "where is target_func defined in src/requested.rs",
    );

    let parameter = Fixture::new();
    let parameter_output = parameter.write(
        "src/lib.rs",
        "pub fn unrelated(target_func: u8) -> u8 {\n    target_func\n}\n",
    );
    expect_miss(
        &parameter.root,
        &parameter_output,
        "where is fn target_func defined",
    );
}

#[test]
fn definition_identity_rejects_non_production_text() {
    let fenced = Fixture::new();
    let fenced_output = fenced.write(
        "docs/guide.md",
        "Example:\n```rust\npub fn target_func(value: u8) -> u8 {\n    value\n}\n```\n",
    );
    expect_miss(&fenced.root, &fenced_output, "where is target_func defined");

    let comment = Fixture::new();
    let comment_output = comment.write(
        "src/lib.rs",
        "/*\npub fn target_func(implementation: u8) -> u8 {\n    implementation\n}\n*/\nfn other() {}\n",
    );
    expect_miss(
        &comment.root,
        &comment_output,
        "where is the implementation of target_func",
    );

    let raw = Fixture::new();
    let raw_output = raw.write(
        "src/lib.rs",
        "const DOC: &str = r#\"\npub fn target_func(implementation: u8) -> u8 {\n    implementation\n}\n\"#;\n",
    );
    expect_miss(
        &raw.root,
        &raw_output,
        "where is the implementation of target_func",
    );
}

#[test]
fn bare_docs_and_prefix_queries_keep_their_contract() {
    let docs = Fixture::new();
    let docs_output = docs.write(
        "docs/guide.md",
        "See target_func before changing batches.\n",
    );
    expect_definition(&docs.root, &docs_output, "target_func", "target_func");

    let prefix = Fixture::new();
    let prefix_output = prefix.write("src/lib.rs", "fn display_relative(&self) {}\n");
    expect_miss(&prefix.root, &prefix_output, "display_relati");
}

#[test]
fn language_control_case_and_sentence_punctuation_keep_definition_proof() {
    let docs = Fixture::new();
    let docs_output = docs.write("docs/guide.md", "The target_func API is described here.\n");
    for query in [
        "where is target_func defined",
        "Where is target_func defined",
        "where is target_func Defined",
        "where is target_func defined.",
        "definition of target_func",
        "Where is the definition of target_func",
    ] {
        expect_miss(&docs.root, &docs_output, query);
    }

    let call = Fixture::new();
    let call_output = call.write(
        "src/lib.rs",
        "pub fn caller() {\n    let target_func = || ();\n    target_func();\n}\n",
    );
    for query in [
        "Where is target_func defined",
        "where is target_func Defined.",
        "definition of target_func",
    ] {
        expect_miss(&call.root, &call_output, query);
    }

    let declared = Fixture::new();
    let declared_output = declared.write("src/lib.rs", "fn target_func() {}\n");
    for query in ["Where is target_func defined.", "definition of target_func"] {
        expect_definition(
            &declared.root,
            &declared_output,
            query,
            "fn target_func() {}",
        );
        let report =
            verify_probe_evidence(&declared_output, &declared.root, query, 4).expect(query);
        assert_eq!(report.evidence()[0].location.start_line(), 1, "{query}");
    }
    let prose = Fixture::new();
    let prose_output = prose.write(
        "docs/guide.md",
        "The implementation strategy uses bounded batches to reduce memory.\n",
    );
    expect_definition(
        &prose.root,
        &prose_output,
        "implementation strategy bounded batches",
        "implementation strategy uses bounded batches",
    );
}

#[test]
fn uppercase_or_keeps_one_existing_branch() {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", "fn target_func() {}\n");
    let report = verify_probe_evidence(&output, &fixture.root, "target_func OR absent_func", 4)
        .expect("uppercase or");
    assert!(report.is_complete(), "{report:?}");
    assert!(report.missing_targets().is_empty());
    assert_eq!(cited(&fixture.root, &report), "fn target_func() {}");
    let conjunction =
        verify_probe_evidence(&output, &fixture.root, "target_func AND absent_func", 4)
            .expect("and");
    assert!(!conjunction.is_complete());
    assert_eq!(conjunction.missing_targets(), ["absent_func"]);
}

#[test]
fn equivalent_root_contained_paths_name_the_same_file() {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", "fn target_func() { let _ = 1; }\n");
    let absolute = fixture.root.join("src/lib.rs");
    for requested in [
        "./src/lib.rs".to_owned(),
        "src/../src/lib.rs".to_owned(),
        absolute.display().to_string(),
    ] {
        expect_definition(
            &fixture.root,
            &output,
            &format!("where is target_func defined in {requested}"),
            "fn target_func() { let _ = 1; }",
        );
        let report = verify_probe_evidence(
            &output,
            &fixture.root,
            &format!("where is target_func defined in {requested}"),
            4,
        )
        .expect(&requested);
        assert_eq!(report.evidence()[0].location.start_line(), 1, "{requested}");
    }
    expect_miss(
        &fixture.root,
        &output,
        "where is target_func defined in src/missing.rs",
    );
    let outside = fixture
        .root
        .parent()
        .expect("parent")
        .join("outside.rs")
        .display()
        .to_string();
    expect_miss(
        &fixture.root,
        &output,
        &format!("where is target_func defined in {outside}"),
    );
}

struct IdentityCase {
    source: &'static str,
    queries: &'static [(&'static str, Option<(usize, &'static str)>)],
}

fn expect_path_identity(
    root: &Path,
    output: &str,
    query: &str,
    line: usize,
    name: &str,
    owner: Option<&str>,
) {
    let report = verify_probe_evidence(output, root, query, 4).expect(query);
    assert!(report.is_complete(), "{query}");
    let evidence = &report.evidence()[0];
    assert!(
        (evidence.location().start_line()..=evidence.location().end_line()).contains(&line),
        "{query}: {evidence:?}"
    );
    assert_eq!(evidence.symbol(), Some(name), "{query}");
    let groups = query_groups(query).expect(query);
    assert_eq!(groups[0].owner.as_deref(), owner, "{query}");
    assert!(cited(root, &report).contains(name), "{query}");
    let source = fs::read_to_string(root.join(evidence.location().path())).expect("source");
    assert!(
        declaration_identity::declarations(&source)
            .iter()
            .any(|declaration| {
                declaration.line == line
                    && declaration.name == name
                    && owner.is_some_and(|owner| {
                        declaration
                            .owner
                            .as_ref()
                            .is_some_and(|found| found.matches(owner))
                    })
            }),
        "{query}: missing structural identity on line {line}"
    );
}

#[test]
fn definition_owner_path_uses_type_identity_not_impl_module() {
    for path in ["crate::actual::Owner", "super::actual::Owner"] {
        let fixture = Fixture::new();
        let source = format!("pub mod actual {{ pub struct Owner; }}\npub mod elsewhere {{\n impl {path} {{\n  pub fn target_func(&self) {{}}\n }}\n}}\n");
        let output = fixture.write("src/lib.rs", &source);
        expect_path_identity(
            &fixture.root,
            &output,
            "where is actual::Owner::target_func defined",
            4,
            "target_func",
            Some("actual::Owner"),
        );
        for owner in ["elsewhere::Owner", "imaginary::Owner", "actual::WrongOwner"] {
            expect_miss(
                &fixture.root,
                &output,
                &format!("where is {owner}::target_func defined"),
            );
        }
    }
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", "pub mod actual { pub struct Owner; }\nimpl actual::Owner {\n pub fn target_func(&self) {}\n}\n");
    expect_path_identity(
        &fixture.root,
        &output,
        "where is actual::Owner::target_func defined",
        3,
        "target_func",
        Some("actual::Owner"),
    );
}

#[test]
fn definition_owner_path_matches_only_supplied_segment_suffix() {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", "pub mod outer {\n pub mod actual {\n  pub struct Owner;\n  impl self::Owner {\n   pub fn target_func(&self) {}\n  }\n  pub trait Trait {\n   const TARGET_CONST: u8 = 1;\n   type TargetType;\n   fn default_func(&self) {}\n  }\n  impl Trait for Owner {\n   fn trait_func(&self) {}\n  }\n  pub fn direct_func() {}\n }\n}\n");
    for (owner, member, line) in [
        ("outer::actual::Owner", "target_func", 5),
        ("actual::Owner", "target_func", 5),
        ("Owner", "target_func", 5),
        ("Trait", "TARGET_CONST", 8),
        ("actual::Trait", "TargetType", 9),
        ("Trait", "default_func", 10),
        ("Owner", "trait_func", 13),
        ("actual", "direct_func", 15),
    ] {
        let query = format!("where is {owner}::{member} defined");
        expect_path_identity(&fixture.root, &output, &query, line, member, Some(owner));
        for wrong in [format!("imaginary::{owner}"), format!("wrong::{owner}")] {
            expect_miss(
                &fixture.root,
                &output,
                &format!("where is {wrong}::{member} defined"),
            );
        }
    }
    for path in [
        "super::super::Owner",
        "<Owner as Trait>::Assoc",
        "(Owner,)",
        "::external::Owner",
    ] {
        let source = format!("impl {path} {{ fn target_func() {{}} }}");
        let output = fixture.write("src/lib.rs", &source);
        expect_miss(
            &fixture.root,
            &output,
            "where is Owner::target_func defined",
        );
    }
}

#[test]
fn definition_owner_path_normalizes_raw_segments_only() {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", "pub mod r#outer {\n pub mod r#actual {\n  pub struct r#Owner_Name;\n  impl crate::r#outer::r#actual::r#Owner_Name {\n   pub fn r#target_func(&self) {}\n  }\n  pub trait r#Trait {\n   fn r#default_func(&self) {}\n  }\n  pub fn r#direct_func() {}\n }\n}\n");
    for owner in [
        "outer::actual::Owner_Name",
        "Owner_Name",
        "r#outer::r#actual::r#Owner_Name",
    ] {
        expect_path_identity(
            &fixture.root,
            &output,
            &format!("where is {owner}::target_func defined"),
            5,
            "target_func",
            Some(owner),
        );
    }
    expect_path_identity(
        &fixture.root,
        &output,
        "where is outer::actual::Trait::default_func defined",
        8,
        "default_func",
        Some("outer::actual::Trait"),
    );
    expect_path_identity(
        &fixture.root,
        &output,
        "where is outer::actual::direct_func defined",
        10,
        "direct_func",
        Some("outer::actual"),
    );
    for owner in [
        "outer::actual::OwnerName",
        "outer::actual::owner_Name",
        "imaginary::Owner_Name",
    ] {
        expect_miss(
            &fixture.root,
            &output,
            &format!("where is {owner}::target_func defined"),
        );
    }
}

#[test]
fn declaration_identity_uses_the_enclosing_declaration() {
    let cases = [
        IdentityCase { source: "pub struct Owner;\nimpl Owner {}\npub fn target_func() { let _ = 1; }\n", queries: &[("where is Owner::target_func defined", None), ("where is target_func defined", Some((3, "target_func")))] },
        IdentityCase { source: "pub struct Owner;\nimpl Owner {}\npub struct Other<T>(T);\nimpl<T> Other<T> {\n    pub fn target_func(&self) { let _ = 1; }\n}\n", queries: &[("where is Owner::target_func defined", None), ("where is Other::target_func defined", Some((5, "target_func")))] },
        IdentityCase { source: "pub mod first {\n    pub struct Owner;\n    impl Owner {}\n}\npub mod second {\n    pub fn target_func() { let _ = 1; }\n}\n", queries: &[("where is Owner::target_func defined", None), ("where is second::target_func defined", Some((6, "target_func")))] },
        IdentityCase { source: "pub struct Owner;\nimpl Owner {\n    pub fn wrapper() {\n        fn target_func() { let _ = 1; }\n    }\n}\n", queries: &[("where is Owner::target_func defined", None)] },
        IdentityCase { source: "pub const target_const: u8 = 1;\n", queries: &[("where is target_const defined", Some((1, "target_const")))] },
        IdentityCase { source: "pub static target_static: u8 = 1;\n", queries: &[("where is target_static defined", Some((1, "target_static")))] },
        IdentityCase { source: "pub async unsafe fn target_func() { let _ = 1; }\n", queries: &[("where is target_func defined", Some((1, "target_func")))] },
        IdentityCase { source: "pub fn r#target_func() { let _ = 1; }\n", queries: &[("where is target_func defined", Some((1, "target_func")))] },
        IdentityCase { source: "pub /* why */ fn target_func() { let _ = 1; }\n", queries: &[("where is target_func defined", Some((1, "target_func")))] },
        IdentityCase { source: "pub struct Owner;\nimpl Owner { pub fn target_func(&self) { let _ = 1; } }\n", queries: &[("where is target_func defined", Some((2, "target_func"))), ("where is Owner::target_func defined", Some((2, "target_func")))] },
        IdentityCase { source: "pub struct Owner_Name;\nimpl Owner_Name {\n    pub fn target_func(&self) { let _ = 1; }\n}\n", queries: &[("where is OwnerName::target_func defined", None), ("where is Owner_Name::target_func defined", Some((3, "target_func"))), ("where is Owner_Name target_func defined", Some((3, "target_func")))] },
        IdentityCase { source: "pub fn targetfunc() { let _ = 1; }\n", queries: &[("where is target_func defined", None), ("where is targetfunc defined", Some((1, "targetfunc"))), ("where is targetfunc::target_func defined", None)] },
        IdentityCase { source: "pub struct Owner<'a>(&'a str);\nimpl<'a> Owner<'a> {\n    pub fn target_func(&self) { let _ = 1; }\n}\n", queries: &[("where is Owner::target_func defined", Some((3, "target_func")))] },
        IdentityCase { source: "pub trait Trait { fn target_func(&self); }\npub struct Owner;\nimpl Trait for Owner {\n    fn target_func(&self) { let _ = 1; }\n}\n", queries: &[("where is Owner::target_func defined", Some((4, "target_func")))] },
        IdentityCase { source: "pub trait Owner {\n    fn target_func(&self) { let _ = 1; }\n}\n", queries: &[("where is Owner::target_func defined", Some((2, "target_func")))] },
        IdentityCase { source: "pub mod owner_mod {\n    pub fn target_func() { let _ = 1; }\n}\n", queries: &[("where is owner_mod::target_func defined", Some((2, "target_func")))] },
        IdentityCase { source: "pub mod outer {\n    pub struct Owner;\n    impl Owner {\n        pub fn target_func(&self) { let _ = 1; }\n    }\n}\n", queries: &[("where is outer::Owner::target_func defined", Some((4, "target_func")))] },
        IdentityCase { source: "pub mod actual {\n pub struct Owner;\n impl Owner {\n  pub fn target_func(&self) {}\n }\n}\n", queries: &[("where is actual::Owner::target_func defined", Some((4, "target_func"))), ("where is imaginary::Owner::target_func defined", None)] },
        IdentityCase { source: "pub mod first {\n pub struct Owner;\n impl Owner {\n  pub fn target_func(&self) {}\n }\n}\npub mod second {\n pub struct Owner;\n impl Owner {\n  pub fn target_func(&self) {}\n }\n}\n", queries: &[("where is first::Owner::target_func defined", Some((4, "target_func"))), ("where is second::Owner::target_func defined", Some((10, "target_func"))), ("where is third::Owner::target_func defined", None)] },
        IdentityCase { source: "pub struct Owner;\nimpl Owner { pub fn run(&self) {} }\n", queries: &[("where is Owner::run defined", Some((2, "run"))), ("where is Owner missing defined", None)] },
        IdentityCase { source: "/* description */ pub fn target_func() {}\n", queries: &[("where is target_func defined", Some((1, "target_func")))] },
        IdentityCase { source: "#[inline] pub fn target_func() {}\n", queries: &[("where is target_func defined", Some((1, "target_func")))] },
        IdentityCase { source: "pub trait Owner {\n const TARGET_CONST: u8 = 1;\n type TargetType;\n fn target_func(&self);\n}\n", queries: &[("where is Owner::TARGET_CONST defined", Some((2, "TARGET_CONST"))), ("where is Owner::TargetType defined", Some((3, "TargetType"))), ("where is Owner::target_func defined", Some((4, "target_func")))] },
        IdentityCase { source: "pub struct Actual;\nimpl Actual { pub const TARGET_CONST: u8 = 1; }\n", queries: &[("where is Actual::TARGET_CONST defined", Some((2, "TARGET_CONST"))), ("where is Other::TARGET_CONST defined", None)] },
    ];
    for case in cases {
        let fixture = Fixture::new();
        let output = fixture.write("src/lib.rs", case.source);
        for (query, expected) in case.queries {
            match expected {
                None => expect_miss(&fixture.root, &output, query),
                Some((line, name)) => {
                    let owner = query.rsplit_once("::").and_then(|(prefix, member)| {
                        (!member.is_empty()).then(|| {
                            prefix
                                .rsplit_once(" is ")
                                .map(|(_, owner)| owner)
                                .unwrap_or(prefix)
                        })
                    });
                    expect_identity(&fixture.root, &output, query, *line, name, owner)
                }
            }
        }
    }
}
