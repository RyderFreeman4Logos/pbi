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
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture() -> (Fixture, String) {
    let fixture = Fixture::new();
    let path = fixture.root.join("src/lib.rs");
    let source = [
        "# docs\n",
        "GREEN verified display_relative at src/lib.rs\n",
        "fn display_relative() {}\n",
        "\n",
        "[lib]\n",
        "path = \"src/lib.rs\"\n",
        "\n",
        "impl SourceLocation {\n",
        "    fn display_relative(&self, root: &Path) -> Result<String, EvidenceError> {\n",
        "        let _ = root;\n",
        "        Ok(String::new())\n",
        "    }\n",
        "}\n",
        "\n",
        "fn other() {\n",
        "    validate_annotations(unit, location);\n",
        "}\n",
        "\n",
        "fn validate_annotations(unit: &str, location: &str) {\n",
        "    let _ = (unit, location);\n",
        "}\n",
    ]
    .concat();
    fs::write(&path, &source).expect("fixture source");
    let output = format!("File: {}, Lines: 1-99\n", path.display());
    (fixture, output)
}

fn cited(root: &Path, report: &EvidenceReport) -> String {
    let location = &report.evidence()[0].location;
    fs::read_to_string(root.join("src/lib.rs"))
        .expect("read")
        .lines()
        .skip(location.start_line() - 1)
        .take(location.end_line() - location.start_line() + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn definition_query_skips_docs_config_and_calls() {
    let (fixture, output) = fixture();
    let queries = [
        "where is the implementation of SourceLocation display_relative",
        "where is SourceLocation display_relative defined",
        "where is fn display_relative defined in src/lib.rs",
        "where is the implementation location of validate_annotations",
    ];
    for query in queries {
        let report = verify_probe_evidence(&output, &fixture.root, query, 4).expect(query);
        assert!(report.is_complete(), "{query}");
        let text = cited(&fixture.root, &report);
        let wants_member = query.contains("SourceLocation");
        let wants_validate = query.contains("validate_annotations");
        assert!(
            (!wants_member
                || (text.contains("impl SourceLocation") && text.contains("fn display_relative")))
                && (!wants_validate || text.contains("fn validate_annotations(")),
            "{query} cited {text}"
        );
        assert!(
            !text.contains("GREEN verified")
                && !text.contains("path =")
                && !text.contains("fn display_relative() {}"),
            "{query} cited decoy {text}"
        );
    }
}

#[test]
fn bare_symbol_still_accepts_a_call_mention() {
    let (fixture, output) = fixture();
    let report = verify_probe_evidence(&output, &fixture.root, "validate_annotations", 4)
        .expect("bare symbol");
    assert!(report.is_complete());
    let text = cited(&fixture.root, &report);
    assert!(
        text.contains("validate_annotations(unit, location)"),
        "bare symbol lost the call: {text}"
    );
}
