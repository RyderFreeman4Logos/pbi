use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The compact location format consumed by callers and by the old pbi wrapper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    path: PathBuf,
    start_line: usize,
    end_line: usize,
}

impl SourceLocation {
    pub fn new(path: PathBuf, start_line: usize, end_line: usize) -> Self {
        Self {
            path,
            start_line,
            end_line,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn start_line(&self) -> usize {
        self.start_line
    }

    pub fn end_line(&self) -> usize {
        self.end_line
    }

    pub fn display_relative(&self, root: &Path) -> Result<String, EvidenceError> {
        let relative = self
            .path
            .strip_prefix(root)
            .map_err(|_| EvidenceError::SourceOutsideRoot)?;
        if relative.as_os_str().is_empty() {
            return Err(EvidenceError::SourceOutsideRoot);
        }
        let mut value = relative.to_string_lossy().into_owned();
        if self.start_line == self.end_line {
            value.push_str(&format!(":{}", self.start_line));
        } else {
            value.push_str(&format!(":{}-{}", self.start_line, self.end_line));
        }
        Ok(value)
    }
}

/// Privacy-safe failures from deterministic source verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceError {
    EmptyProbeOutput,
    NoSourceLocations,
    SourceOutsideRoot,
    InvalidLineRange,
    SourceUnavailable,
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyProbeOutput => "probe returned no output",
            Self::NoSourceLocations => "no source locations found",
            Self::SourceOutsideRoot => "source location crossed the repository boundary",
            Self::InvalidLineRange => "probe returned an invalid source line range",
            Self::SourceUnavailable => "source evidence could not be read",
        })
    }
}

impl std::error::Error for EvidenceError {}

#[derive(Clone, Debug)]
struct RawLocation {
    path: PathBuf,
    start_line: usize,
    end_line: usize,
}

/// Parse and source-check Probe's plain search result without invoking a model.
///
/// A candidate is accepted only when its file is inside `root` and the reported
/// source span contains enough distinctive query evidence. This deliberately
/// rejects bare location stamps and unrelated high-ranking matches.
pub fn verify_probe_locations(
    probe_output: &str,
    root: &Path,
    query: &str,
    max_results: usize,
) -> Result<Vec<SourceLocation>, EvidenceError> {
    if probe_output.trim().is_empty() {
        return Err(EvidenceError::EmptyProbeOutput);
    }
    if max_results == 0 {
        return Err(EvidenceError::NoSourceLocations);
    }
    let root = fs::canonicalize(root).map_err(|_| EvidenceError::SourceUnavailable)?;
    let mut raw_locations = Vec::new();
    let mut malformed_range = false;
    for line in probe_output.lines() {
        let Some(rest) = line.trim().strip_prefix("File: ") else {
            continue;
        };
        let Some((path_text, range_text)) = rest.rsplit_once(", Lines: ") else {
            continue;
        };
        let Some((start_text, end_text)) = range_text.trim().split_once('-') else {
            malformed_range = true;
            continue;
        };
        let Ok(start_line) = start_text.trim().parse::<usize>() else {
            malformed_range = true;
            continue;
        };
        let Ok(end_line) = end_text.trim().parse::<usize>() else {
            malformed_range = true;
            continue;
        };
        if start_line == 0 || end_line < start_line {
            malformed_range = true;
            continue;
        }
        raw_locations.push(RawLocation {
            path: PathBuf::from(path_text.trim()),
            start_line,
            end_line,
        });
    }
    if raw_locations.is_empty() {
        return if malformed_range {
            Err(EvidenceError::InvalidLineRange)
        } else {
            Err(EvidenceError::NoSourceLocations)
        };
    }

    let terms = distinctive_terms(query);
    let compact_query = compact_alphanumeric(query);
    let compound_query = query_compound_symbol(query);
    let mut locations = Vec::new();
    for raw in raw_locations {
        let Ok(path) = fs::canonicalize(&raw.path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        if relative.as_os_str().is_empty() || excluded_path(relative) {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let source_lines: Vec<&str> = source.lines().collect();
        if raw.start_line > source_lines.len() {
            continue;
        }
        let end_line = raw.end_line.min(source_lines.len());
        let span = source_lines[raw.start_line - 1..end_line].join("\n");
        if !has_distinctive_evidence(&span, &terms, &compact_query, compound_query.as_deref()) {
            continue;
        }
        let location = SourceLocation::new(path, raw.start_line, end_line);
        if !locations
            .iter()
            .any(|seen: &SourceLocation| seen == &location)
        {
            locations.push(location);
        }
        if locations.len() == max_results {
            break;
        }
    }
    if locations.is_empty() {
        Err(EvidenceError::NoSourceLocations)
    } else {
        Ok(locations)
    }
}

fn excluded_path(relative: &Path) -> bool {
    relative.components().any(|component| match component {
        Component::Normal(name) => matches!(
            name.to_str(),
            Some("drafts") | Some("target") | Some("node_modules") | Some("__pycache__")
        ),
        _ => false,
    })
}

fn distinctive_terms(query: &str) -> Vec<String> {
    let stop_words = [
        "a", "an", "and", "are", "at", "be", "for", "from", "how", "is", "of", "or", "the", "to",
        "what", "where", "which", "who", "why",
    ];
    let mut terms = Vec::new();
    let mut current = String::new();
    for character in query.chars() {
        if character.is_alphanumeric() {
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            if current.len() >= 4
                && !stop_words.contains(&current.as_str())
                && !terms.contains(&current)
            {
                terms.push(current.clone());
            }
            current.clear();
        }
    }
    if current.len() >= 4 && !stop_words.contains(&current.as_str()) && !terms.contains(&current) {
        terms.push(current);
    }
    terms
}

fn compact_alphanumeric(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn has_distinctive_evidence(
    span: &str,
    terms: &[String],
    compact_query: &str,
    compound_query: Option<&str>,
) -> bool {
    let lower_span = span.to_lowercase();
    if let Some(compound) = compound_query {
        return compact_alphanumeric(&lower_span).contains(compound);
    }
    let matching_terms = terms
        .iter()
        .filter(|term| lower_span.contains(term.as_str()))
        .count();
    if terms.len() <= 1 {
        return matching_terms > 0
            || (!compact_query.is_empty() && compact_alphanumeric(span).contains(compact_query));
    }
    matching_terms >= 2
        || (!compact_query.is_empty() && compact_alphanumeric(span).contains(compact_query))
}

fn query_compound_symbol(query: &str) -> Option<String> {
    query.split_whitespace().find_map(|token| {
        if token.contains('_') {
            let compact = compact_alphanumeric(token);
            (!compact.is_empty()).then_some(compact)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("pbi-rs-evidence-{suffix}"));
            fs::create_dir_all(root.join("src")).expect("fixture directory");
            fs::write(
                root.join("src/lib.rs"),
                "// compression publication and cache key assembly\npub fn answer() {}\n",
            )
            .expect("fixture source");
            Self { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn source_evidence_accepts_distinctive_span() {
        let fixture = Fixture::new();
        let output = format!(
            "Pattern: cache key\nPath: {}\nFile: {}, Lines: 1-1\n",
            fixture.root.display(),
            fixture.root.join("src/lib.rs").display()
        );
        let locations = verify_probe_locations(
            &output,
            &fixture.root,
            "where is compression publication and cache key assembly",
            8,
        )
        .expect("verified source");
        assert_eq!(locations[0].start_line(), 1);
        assert_eq!(
            locations[0]
                .display_relative(&fs::canonicalize(&fixture.root).unwrap())
                .unwrap(),
            "src/lib.rs:1"
        );
    }

    #[test]
    fn source_evidence_rejects_bare_stamp_and_unrelated_span() {
        let fixture = Fixture::new();
        let output = format!(
            "Path: {}\nFile: {}, Lines: 2-2\n",
            fixture.root.display(),
            fixture.root.join("src/lib.rs").display()
        );
        assert_eq!(
            verify_probe_locations(
                &output,
                &fixture.root,
                "compression publication cache assembly",
                8
            ),
            Err(EvidenceError::NoSourceLocations)
        );
    }

    #[test]
    fn source_evidence_rejects_partial_compound_symbol_match() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root.join("src/lib.rs"),
            "fn probe_api_error_diagnostic() {}\n",
        )
        .expect("fixture source");
        let output = format!(
            "File: {}, Lines: 1-1\n",
            fixture.root.join("src/lib.rs").display()
        );
        assert_eq!(
            verify_probe_locations(&output, &fixture.root, "probe_json_error", 8),
            Err(EvidenceError::NoSourceLocations)
        );
    }

    #[test]
    fn source_evidence_accepts_exact_compound_symbol_match() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root.join("src/lib.rs"),
            "fn probe_json_error() {}\n",
        )
        .expect("fixture source");
        let output = format!(
            "File: {}, Lines: 1-1\n",
            fixture.root.join("src/lib.rs").display()
        );
        let locations = verify_probe_locations(&output, &fixture.root, "probe_json_error", 8)
            .expect("verified source");
        assert_eq!(locations[0].start_line(), 1);
    }

    #[test]
    fn source_evidence_rejects_locations_outside_root() {
        let fixture = Fixture::new();
        let outside = fixture.root.parent().expect("parent").join("outside.rs");
        fs::write(&outside, "compression publication cache assembly\n").expect("outside source");
        let output = format!("File: {}, Lines: 1-1\n", outside.display());
        assert_eq!(
            verify_probe_locations(
                &output,
                &fixture.root,
                "compression publication cache assembly",
                8
            ),
            Err(EvidenceError::NoSourceLocations)
        );
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn source_evidence_preserves_probe_order_and_limit() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root.join("src/second.rs"),
            "compression publication cache assembly\n",
        )
        .expect("second source");
        let output = format!(
            "File: {}, Lines: 1-1\nFile: {}, Lines: 1-1\n",
            fixture.root.join("src/lib.rs").display(),
            fixture.root.join("src/second.rs").display()
        );
        let locations = verify_probe_locations(
            &output,
            &fixture.root,
            "compression publication cache assembly",
            1,
        )
        .expect("verified source");
        assert_eq!(locations.len(), 1);
        assert!(locations[0].path().ends_with("src/lib.rs"));
    }
}
