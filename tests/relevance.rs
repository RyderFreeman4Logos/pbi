use pbi_rs::{verify_probe_evidence, EvidenceError};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("pbi-rs-relevance-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("fixture directory");
        Self(root)
    }
    fn check(&self, name: &str, source: &str, positive: bool) -> bool {
        let path = self.0.join(format!("{name}.rs"));
        fs::write(&path, source).expect("fixture source");
        let report = verify_probe_evidence(
            &format!("File: {}, Lines: 1-99\n", path.display()),
            &self.0,
            "unknown field handling",
            8,
        );
        match report {
            Ok(report) => {
                positive
                    && report.is_complete()
                    && report.evidence().iter().all(|e| {
                        let start = e.location().start_line();
                        let end = e.location().end_line();
                        end - start < 4
                            && e.snippet()
                                == source
                                    .lines()
                                    .skip(start - 1)
                                    .take(end - start + 1)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                    })
            }
            Err(EvidenceError::NoSourceLocations) => !positive,
            Err(_) => false,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Sources and expectations copied exactly from the frozen e80aa14 review oracle.
#[test]
fn immutable_eleven_case_relevance_oracle() {
    let fixture = Fixture::new();
    let cases = [
        (
            "serde",
            true,
            r####"use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config { enabled: bool }
fn decode(input: &str) -> Result<Config, serde_json::Error> { serde_json::from_str(input) }
"####,
        ),
        (
            "alternate",
            true,
            r####"fn retain<T>(known_fields: &std::collections::HashSet<String>, key: String, value: T) -> Vec<(String, T)> {
    let mut other_values = Vec::new();
    if known_fields.contains(&key) { drop(value); } else { other_values.push((key, value)); }
    other_values
}
"####,
        ),
        (
            "fallback",
            true,
            r####"#[derive(Debug)]
enum FieldError { UnknownField(String) }
fn parse_field(key: &str) -> Result<(), FieldError> {
    match key {
        "id" => Ok(()),
        other => Err(FieldError::UnknownField(other.to_owned())),
    }
}
"####,
        ),
        (
            "full_line_comment",
            false,
            r####"// match key { other => Err(FieldError::UnknownField(other)) }?
"####,
        ),
        (
            "inline_comment",
            false,
            r####"fn unrelated() {} // match key { other => Err(FieldError::UnknownField(other)) }?
"####,
        ),
        (
            "block_comment",
            false,
            r####"/* match key { other => Err(FieldError::UnknownField(other)) }? */
"####,
        ),
        (
            "literal",
            false,
            r####"const NOTE: &str = "match key { other => Err(FieldError::UnknownField(other)) }?";
"####,
        ),
        (
            "duplicate_membership",
            false,
            r####"fn duplicate_field(key: &str, seen: &mut std::collections::HashSet<String>) -> Result<(), &'static str> {
    if seen.contains(key) { return Err("duplicate field"); } else { seen.insert(key.to_owned()); }
    Ok(())
}
"####,
        ),
        (
            "unrelated_membership",
            false,
            r####"fn field_keys(key: i32, selected: &std::collections::HashSet<i32>, pending: &mut Vec<i32>) {
    if selected.contains(&key) { drop(key); } else { pending.push(key); }
}
"####,
        ),
        (
            "literal_reject",
            true,
            r####"fn parse_field(key: &str) -> Result<(), &'static str> {
    if key != "id" { return Err("unknown field"); }
    Ok(())
}
"####,
        ),
        (
            "predicate_capture",
            true,
            r####"fn visit(key: String, value: i32, extensions: &mut Vec<(String, i32)>) {
    if known(&key) { drop(value); } else { extensions.push((key, value)); }
}
fn known(key: &str) -> bool { key == "id" }
"####,
        ),
    ];
    assert_eq!(cases.len(), 11);
    let failed = cases
        .iter()
        .filter_map(|(name, positive, source)| {
            (!fixture.check(name, source, *positive)).then_some(*name)
        })
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "immutable oracle failed: {failed:?}");
}

#[test]
fn lexical_boundaries_and_executable_controls() {
    let fixture = Fixture::new();
    let example = "match key { other => Err(FieldError::UnknownField(other)) }?";
    let sources = [
        format!("/* opening\npadding\npadding\npadding\n{example}\nclosing */\n"),
        format!("/* outer\n/* inner */\npadding\npadding\n{example}\n*/\n"),
        format!(
            "const NOTE: &str = r###\"opening\npadding\npadding\npadding\n{example} \"##\n\"###;\n"
        ),
        format!("const NOTE: &str = \"opening\npadding\npadding\npadding\n{example}\n\";\n"),
        format!("const NOTE: &str = \"escaped \\\" {example}\";\n"),
    ];
    let positive = "fn parse_field(key: &str) -> Result<(), &'static str> {\n    if key != \"id\" { return Err(\"unknown field\"); }\n    Ok(())\n}\n";
    let mut failed = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        if !fixture.check(&format!("lexical_{index}"), source, false) {
            failed.push(format!("decoy {index}"));
        }
        if !fixture.check(
            &format!("executable_{index}"),
            &format!("{source}{positive}"),
            true,
        ) {
            failed.push(format!("executable {index}"));
        }
    }
    assert!(
        failed.is_empty(),
        "lexical boundary cases failed: {failed:?}"
    );
}

#[test]
fn field_roles_are_not_membership_history_or_unrelated_mutation() {
    let fixture = Fixture::new();
    let cases = [
        ("utf8_code_boundary", false, "fn unrelated() { ℘(); }\n"),
        (
            "unrelated_mutation_argument",
            false,
            r#"fn visit(key: String, schema: &Set<String>, pending: &mut Vec<i32>) {
    if schema.contains(&key) { drop(key); } else { (pending.push(7), drop(key)); }
}
"#,
        ),
        (
            "readonly_history",
            false,
            r#"fn visit(key: String, prior_fields: &Set<String>, leftovers: &mut Vec<String>) {
    if prior_fields.contains(&key) { return Err("duplicate field"); } else { leftovers.push(key); }
}
"#,
        ),
        (
            "unrelated_error_type",
            false,
            r#"enum FieldError { UnknownField(String) }
fn visit(key: i32) -> Result<(), FieldError> {
    if key == 7 { return Err(other_error()); }
}
"#,
        ),
        (
            "unrelated_predicate_comparison",
            false,
            r#"fn visit(key: String, extras: &mut Vec<String>) {
    if selected(&key) { drop(key); } else { extras.push(key); }
}
fn selected(key: &str) -> bool { key.len() > 3 && mode() == "id" }
"#,
        ),
        (
            "field_history",
            false,
            r#"fn visit(key: String, field_history: &mut Set<String>, extras: &mut Vec<String>) {
    if field_history.contains(&key) { return Err("duplicate field"); } else { field_history.insert(key.clone()); extras.push(key); }
}
"#,
        ),
        (
            "unrelated_payload",
            false,
            r#"fn visit(key: String, schema_fields: &Set<String>, pending: &mut Vec<i32>) {
    if schema_fields.contains(&key) { drop(key); } else { pending.push(7); }
}
"#,
        ),
        (
            "unrelated_predicate",
            false,
            r#"fn visit(key: String, extras: &mut Vec<String>) {
    if selected(&key) { drop(key); } else { extras.push(key); }
}
fn selected(key: &str) -> bool { key.len() > 3 }
"#,
        ),
        (
            "renamed_schema",
            true,
            r#"fn visit(name: String, schema: &Set<String>, leftovers: &mut Vec<String>) {
    if schema.contains(&name) { drop(name); } else { leftovers.push(name); }
}
"#,
        ),
        (
            "negative_schema",
            true,
            r#"fn visit(name: String, schema: &Set<String>, leftovers: &mut Vec<String>) {
    if !schema.contains(&name) { leftovers.push(name); }
}
"#,
        ),
        (
            "renamed_predicate",
            true,
            r#"fn visit(name: String, leftovers: &mut Vec<String>) {
    if accepts(&name) { drop(name); } else { leftovers.push(name); }
}
fn accepts(name: &str) -> bool { name == "id" }
"#,
        ),
        (
            "diagnostic_example",
            false,
            r#"fn unrelated() { log("unknown field: match key { other => Err(FieldError::UnknownField(other)) }?"); }
"#,
        ),
    ];
    let failed = cases
        .iter()
        .filter_map(|(name, positive, source)| {
            (!fixture.check(name, source, *positive)).then_some(*name)
        })
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "field-role cases failed: {failed:?}");
}
