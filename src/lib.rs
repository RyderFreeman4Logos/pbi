use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub mod semantic;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EVIDENCE_LINES: usize = 4;

/// A verified source path and the exact line span returned to a caller.
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

/// One compact, source-verified answer candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceEvidence {
    location: SourceLocation,
    target: String,
    snippet: String,
    symbol: Option<String>,
    relevance: String,
}

impl SourceEvidence {
    pub fn location(&self) -> &SourceLocation {
        &self.location
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn snippet(&self) -> &str {
        &self.snippet
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn relevance(&self) -> &str {
        &self.relevance
    }
}

/// Verified evidence and explicit coverage state for one deterministic query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceReport {
    complete: bool,
    evidence: Vec<SourceEvidence>,
    missing_targets: Vec<String>,
}

impl EvidenceReport {
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn evidence(&self) -> &[SourceEvidence] {
        &self.evidence
    }

    pub fn missing_targets(&self) -> &[String] {
        &self.missing_targets
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
    order: usize,
}

#[derive(Clone, Debug)]
struct QueryGroup {
    label: String,
    terms: Vec<String>,
    exact_symbols: Vec<String>,
}

#[derive(Clone, Debug)]
struct ScoredEvidence {
    evidence: SourceEvidence,
    score: i32,
    order: usize,
}

/// Parse Probe output, inspect the cited files, and return compact evidence.
///
/// Probe ranges are candidate file hints, not proof. The verifier reads a
/// bounded source file, selects a small local window, excludes generated/test
/// distractors, and records which query groups remain unsupported. No model or
/// repository write is involved.
pub fn verify_probe_evidence(
    probe_output: &str,
    root: &Path,
    query: &str,
    max_results: usize,
) -> Result<EvidenceReport, EvidenceError> {
    if probe_output.trim().is_empty() {
        return Err(EvidenceError::EmptyProbeOutput);
    }
    if max_results == 0 {
        return Err(EvidenceError::NoSourceLocations);
    }
    let root = fs::canonicalize(root).map_err(|_| EvidenceError::SourceUnavailable)?;
    let raw_locations = parse_probe_locations(probe_output)?;
    let groups = query_groups(query);
    if groups.is_empty() {
        return Err(EvidenceError::NoSourceLocations);
    }

    let mut choices: Vec<Vec<ScoredEvidence>> = vec![Vec::new(); groups.len()];
    for raw in raw_locations {
        let Some(path) = resolve_candidate_path(&raw.path, &root) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        if relative.as_os_str().is_empty() || excluded_path(relative) || source_is_too_large(&path)
        {
            continue;
        }
        let test_candidate = test_path(relative);
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = source.lines().collect();
        if lines.is_empty() {
            continue;
        }
        for (group_index, group) in groups.iter().enumerate() {
            let raw_order = raw
                .order
                .saturating_add(raw.start_line)
                .saturating_add(raw.end_line);
            if let Some(choice) = best_window(
                group,
                &groups,
                relative,
                &path,
                &lines,
                raw_order,
                test_candidate,
            ) {
                choices[group_index].push(choice);
            }
        }
    }

    let mut evidence = Vec::new();
    let mut covered = vec![false; groups.len()];
    for (group_index, group_choices) in choices.iter_mut().enumerate() {
        group_choices.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.order.cmp(&right.order))
                .then_with(|| {
                    left.evidence
                        .location
                        .start_line
                        .cmp(&right.evidence.location.start_line)
                })
        });
        let Some(choice) = group_choices.first().cloned() else {
            continue;
        };
        if let Some(existing) = evidence
            .iter()
            .position(|candidate: &SourceEvidence| candidate.location == choice.evidence.location)
        {
            let _ = existing;
            covered[group_index] = true;
            continue;
        }
        if evidence.len() >= max_results {
            continue;
        }
        evidence.push(choice.evidence);
        covered[group_index] = true;
    }

    let missing_targets = groups
        .iter()
        .enumerate()
        .filter(|(index, _)| !covered[*index])
        .map(|(_, group)| group.label.clone())
        .collect::<Vec<_>>();
    if evidence.is_empty() {
        return Err(EvidenceError::NoSourceLocations);
    }
    Ok(EvidenceReport {
        complete: missing_targets.is_empty(),
        evidence,
        missing_targets,
    })
}

/// Compatibility view for callers that only need verified exact locations.
pub fn verify_probe_locations(
    probe_output: &str,
    root: &Path,
    query: &str,
    max_results: usize,
) -> Result<Vec<SourceLocation>, EvidenceError> {
    let report = verify_probe_evidence(probe_output, root, query, max_results)?;
    let root = fs::canonicalize(root).map_err(|_| EvidenceError::SourceUnavailable)?;
    let ranges = parse_probe_locations(probe_output)?;
    let locations = report
        .evidence
        .into_iter()
        .filter(|evidence| {
            ranges.iter().any(|raw| {
                resolve_candidate_path(&raw.path, &root).is_some_and(|path| {
                    path == evidence.location.path
                        && evidence.location.start_line >= raw.start_line
                        && evidence.location.end_line <= raw.end_line
                })
            })
        })
        .map(|evidence| evidence.location)
        .collect::<Vec<_>>();
    if locations.is_empty() {
        Err(EvidenceError::NoSourceLocations)
    } else {
        Ok(locations)
    }
}

fn parse_probe_locations(probe_output: &str) -> Result<Vec<RawLocation>, EvidenceError> {
    let mut raw_locations = Vec::new();
    let mut malformed_range = false;
    for (order, line) in probe_output.lines().enumerate() {
        let Some(rest) = line.trim().strip_prefix("File: ") else {
            continue;
        };
        let Some((path_text, range_text)) = rest.rsplit_once(", Lines: ") else {
            continue;
        };
        let range_text = range_text.trim();
        let (start_text, end_text) = range_text
            .split_once('-')
            .unwrap_or((range_text, range_text));
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
            order,
        });
    }
    if raw_locations.is_empty() {
        return if malformed_range {
            Err(EvidenceError::InvalidLineRange)
        } else {
            Err(EvidenceError::NoSourceLocations)
        };
    }
    Ok(raw_locations)
}

fn resolve_candidate_path(raw: &Path, root: &Path) -> Option<PathBuf> {
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let path = fs::canonicalize(candidate).ok()?;
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() || excluded_path(relative) {
        return None;
    }
    Some(path)
}

fn source_is_too_large(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.len() > MAX_SOURCE_BYTES)
        .unwrap_or(true)
}

fn excluded_path(relative: &Path) -> bool {
    relative.components().any(|component| match component {
        Component::Normal(name) => {
            let Some(name) = name.to_str() else {
                return true;
            };
            name == "drafts"
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
                || name == ".git"
                || name == ".env"
                || name.starts_with(".env.")
        }
        _ => false,
    })
}

fn test_path(relative: &Path) -> bool {
    relative.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_lowercase();
            name == "test"
                || name == "tests"
                || name == "__tests__"
                || name.starts_with("test_")
                || name.ends_with("_test.rs")
                || name.ends_with("_tests.rs")
                || name.contains(".test.")
        }
        _ => false,
    })
}

fn query_groups(query: &str) -> Vec<QueryGroup> {
    let mut groups = Vec::<Vec<String>>::new();
    let mut current = Vec::new();
    for token in raw_query_tokens(query) {
        if token == "and" || token == "or" {
            if !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
        } else if !query_stop_word(&token) {
            current.push(token);
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
        .into_iter()
        .filter_map(|tokens| {
            let mut terms = Vec::new();
            let mut exact_symbols = Vec::new();
            for token in &tokens {
                if token.len() >= 3 && !terms.contains(token) {
                    terms.push(token.clone());
                }
                if token.contains('_') || token.contains("::") {
                    exact_symbols.push(compact_alphanumeric(token));
                }
            }
            (!terms.is_empty()).then(|| QueryGroup {
                label: tokens.join(" "),
                terms,
                exact_symbols,
            })
        })
        .collect()
}

fn raw_query_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in value.chars() {
        if character.is_alphanumeric() || character == '_' || character == ':' {
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn query_stop_word(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "at"
            | "be"
            | "does"
            | "for"
            | "from"
            | "how"
            | "is"
            | "of"
            | "or"
            | "the"
            | "to"
            | "what"
            | "where"
            | "which"
            | "who"
            | "why"
    )
}

fn compact_alphanumeric(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn tokenized(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous = None;
    for character in value.chars() {
        let boundary = character.is_uppercase()
            && previous.is_some_and(|previous: char| previous.is_lowercase());
        if !character.is_alphanumeric() || boundary {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
            if character.is_alphanumeric() {
                current.push(character);
            }
        } else {
            current.push(character);
        }
        previous = Some(character);
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens
}

fn token_matches(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left = left.as_bytes();
    let right = right.as_bytes();
    let common = left
        .iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count();
    common >= 4 && common * 5 >= left.len().min(right.len()) * 4
}

fn matching_terms(terms: &[String], text: &str) -> Vec<String> {
    let source_tokens = tokenized(text);
    terms
        .iter()
        .filter(|term| source_tokens.iter().any(|token| token_matches(term, token)))
        .cloned()
        .collect()
}

fn requested_features(group: &QueryGroup) -> Vec<&'static str> {
    let mut features = Vec::new();
    let add = |features: &mut Vec<&'static str>, feature| {
        if !features.contains(&feature) {
            features.push(feature);
        }
    };
    for term in &group.terms {
        if ["error", "err", "failure", "fail", "exception"]
            .iter()
            .any(|word| token_matches(term, word))
        {
            add(&mut features, "error");
        }
        if [
            "parse",
            "parser",
            "conversion",
            "convert",
            "decode",
            "deserialize",
            "serialize",
        ]
        .iter()
        .any(|word| token_matches(term, word))
        {
            add(&mut features, "conversion");
        }
        if [
            "unknown", "field", "fields", "key", "keys", "handling", "handle",
        ]
        .iter()
        .any(|word| token_matches(term, word))
        {
            add(&mut features, "data");
        }
    }
    features
}

fn window_features(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    let mut features = Vec::new();
    let add = |features: &mut Vec<&'static str>, feature| {
        if !features.contains(&feature) {
            features.push(feature);
        }
    };
    let tokens = tokenized(&lower);
    if tokens.iter().any(|token| {
        ["error", "err", "failure", "fail", "exception", "invalid"]
            .iter()
            .any(|word| token_matches(token, word))
    }) || lower.contains("map_err")
    {
        add(&mut features, "error");
    }
    if tokens.iter().any(|token| {
        [
            "parse",
            "convert",
            "decode",
            "deserialize",
            "serialize",
            "map",
        ]
        .iter()
        .any(|word| token_matches(token, word))
    }) || lower.contains("map_err")
        || lower.contains("?")
    {
        add(&mut features, "conversion");
    }
    if tokens.iter().any(|token| {
        [
            "unknown",
            "field",
            "fields",
            "key",
            "keys",
            "extension",
            "extensions",
        ]
        .iter()
        .any(|word| token_matches(token, word))
    }) || lower.contains("if ")
        || lower.contains("else")
        || lower.contains("match ")
        || lower.contains(".push")
        || lower.contains(".insert")
        || lower.contains("next_value")
    {
        add(&mut features, "data");
    }
    features
}

fn call_markers(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut markers = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if !(characters[index].is_alphanumeric() || characters[index] == '_') {
            index += 1;
            continue;
        }
        let start = index;
        while index < characters.len()
            && (characters[index].is_alphanumeric()
                || characters[index] == '_'
                || characters[index] == '.'
                || characters[index] == ':')
        {
            index += 1;
        }
        let name: String = characters[start..index]
            .iter()
            .collect::<String>()
            .trim_end_matches(':')
            .to_owned();
        let mut lookahead = index;
        while lookahead < characters.len() && characters[lookahead].is_whitespace() {
            lookahead += 1;
        }
        if lookahead < characters.len() && characters[lookahead] == '<' {
            let mut depth = 0usize;
            while lookahead < characters.len() {
                match characters[lookahead] {
                    '<' => depth += 1,
                    '>' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            lookahead += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                lookahead += 1;
            }
            while lookahead < characters.len() && characters[lookahead].is_whitespace() {
                lookahead += 1;
            }
        }
        let previous_word = characters[..start]
            .iter()
            .rev()
            .skip_while(|character| character.is_whitespace())
            .take_while(|character| character.is_alphanumeric() || **character == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        if lookahead < characters.len()
            && characters[lookahead] == '('
            && !matches!(
                name.as_str(),
                "if" | "for" | "while" | "match" | "fn" | "struct"
            )
            && previous_word != "fn"
            && !markers.contains(&name)
        {
            markers.push(name);
        }
    }
    markers
}

fn line_markers(text: &str) -> Vec<String> {
    if text.trim_start().starts_with('#') {
        return Vec::new();
    }
    let mut markers = call_markers(text);
    if text.contains('?') {
        markers.push("error propagation".to_owned());
    }
    if text.contains("=>") || text.contains(" else") || text.trim_start().starts_with("if ") {
        markers.push("branch".to_owned());
    }
    if text.contains('=')
        && !text.contains("==")
        && !text.contains("=>")
        && !text.trim_start().starts_with("#")
        && !markers.contains(&"assignment".to_owned())
    {
        markers.push("assignment".to_owned());
    }
    markers
}

fn behavior_query(group: &QueryGroup) -> bool {
    group.terms.iter().any(|term| {
        [
            "error",
            "parser",
            "parse",
            "conversion",
            "convert",
            "decode",
            "unknown",
            "field",
            "handling",
            "handle",
            "deserialize",
            "serialize",
            "reject",
            "accept",
        ]
        .iter()
        .any(|word| token_matches(term, word))
    })
}

fn exact_symbol_in_lines(
    group: &QueryGroup,
    lines: &[&str],
    start: usize,
    end: usize,
) -> Option<String> {
    let candidates = lines[start..end]
        .iter()
        .flat_map(|line| raw_identifiers(line))
        .collect::<Vec<_>>();
    group.exact_symbols.iter().find_map(|expected| {
        candidates
            .iter()
            .find(|candidate| compact_alphanumeric(candidate) == *expected)
            .cloned()
    })
}

fn raw_identifiers(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' {
            current.push(character);
        } else if !current.is_empty() {
            values.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        values.push(current);
    }
    values
}

fn best_window(
    group: &QueryGroup,
    all_groups: &[QueryGroup],
    relative: &Path,
    path: &Path,
    lines: &[&str],
    order: usize,
    test_candidate: bool,
) -> Option<ScoredEvidence> {
    let all_terms = all_groups
        .iter()
        .flat_map(|group| group.terms.iter().cloned())
        .collect::<Vec<_>>();
    let path_text = relative.to_string_lossy();
    let requested = requested_features(group);
    let behavioral = behavior_query(group);
    let context_terms = all_groups
        .iter()
        .flat_map(|candidate| candidate.terms.iter().cloned())
        .filter(|term| !group.terms.contains(term))
        .collect::<Vec<_>>();
    let mut best: Option<ScoredEvidence> = None;
    for start in 0..lines.len() {
        for length in 1..=MAX_EVIDENCE_LINES.min(lines.len() - start) {
            let end = start + length;
            let text = lines[start..end].join("\n");
            let group_matches = matching_terms(&group.terms, &text);
            let path_matches = matching_terms(&group.terms, &path_text);
            let path_context = matching_terms(&context_terms, &path_text);
            let context_matches = matching_terms(&all_terms, &format!("{path_text}\n{text}"));
            let features = window_features(&text);
            let overlap = requested
                .iter()
                .filter(|feature| features.contains(feature))
                .count();
            let markers = lines[start..end]
                .iter()
                .flat_map(|line| line_markers(line))
                .fold(Vec::new(), |mut markers, marker| {
                    if !markers.contains(&marker) {
                        markers.push(marker);
                    }
                    markers
                });
            let exact_symbol = exact_symbol_in_lines(group, lines, start, end);
            let test_context = test_window(lines, start, end);
            if exact_symbol.is_none() && lexical_harness_window(&text) {
                continue;
            }
            let actionable = markers
                .iter()
                .any(|marker| marker != "assignment" && marker != "branch");
            let setup_lines = lines[start..end]
                .iter()
                .filter(|line| line.trim_start().starts_with("let "))
                .count();
            let direct = group_matches.len();
            let simple_lexical = !behavioral && direct >= 2;
            let exact = exact_symbol.is_some();
            if !exact
                && ((behavioral && (!actionable || (direct == 0 && overlap == 0)))
                    || (!behavioral && !simple_lexical))
            {
                continue;
            }
            if direct == 0 && overlap == 0 && !exact {
                continue;
            }
            let score = (direct as i32 * 12)
                + (path_matches.len() as i32 * 3)
                + (path_context.len() as i32 * 10)
                + (overlap as i32 * 20)
                + (context_matches.len() as i32 * 2)
                + (markers.len() as i32 * 3)
                - (setup_lines as i32 * 3)
                - if test_candidate || test_context {
                    16
                } else {
                    0
                }
                + if exact { 100 } else { 0 }
                - length as i32;
            let location = SourceLocation::new(path.to_path_buf(), start + 1, end);
            let symbol = exact_symbol
                .or_else(|| markers.iter().find(|marker| useful_symbol(marker)).cloned());
            let mut relevance_parts = Vec::new();
            if !group_matches.is_empty() {
                relevance_parts.push(format!("terms={}", group_matches.join(",")));
            } else if !path_matches.is_empty() {
                relevance_parts.push(format!("path_terms={}", path_matches.join(",")));
            }
            if !markers.is_empty() {
                relevance_parts.push(format!(
                    "code={}",
                    markers
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            if test_candidate || test_context {
                relevance_parts.push("context=test".to_owned());
            }
            if relevance_parts.is_empty() {
                relevance_parts.push("exact symbol".to_owned());
            }
            let candidate = ScoredEvidence {
                evidence: SourceEvidence {
                    location,
                    target: group.label.clone(),
                    snippet: text,
                    symbol,
                    relevance: relevance_parts.join(" "),
                },
                score,
                order,
            };
            if match best.as_ref() {
                None => true,
                Some(current) => candidate.score > current.score,
            } {
                best = Some(candidate);
            }
        }
    }
    best
}

fn test_window(lines: &[&str], start: usize, end: usize) -> bool {
    let context_start = start.saturating_sub(64);
    lines[context_start..end].iter().any(|line| {
        let lower = line.to_lowercase();
        lower.contains("#[test")
            || lower.contains("cfg(test")
            || lower.contains("mod tests")
            || (lower.contains("fn test_") && lower.contains('('))
    })
}

fn useful_symbol(marker: &str) -> bool {
    marker.chars().any(|character| character.is_alphanumeric())
        && marker != "assignment"
        && marker != "branch"
        && marker != "error propagation"
        && marker != "Some"
        && marker != "Ok"
        && marker != "Err"
        && marker != "format"
        && marker != "Vec::new"
}

fn lexical_harness_window(text: &str) -> bool {
    let lower = text.to_lowercase();
    (lower.contains("search") || lower.contains("grep") || lower.contains("probe"))
        && (text.contains('"') || text.contains('\''))
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

    #[test]
    fn evidence_compacts_broad_ranges_and_covers_distinct_targets() {
        let fixture = Fixture::new();
        let parser = fixture.root.join("src/parser.rs");
        let permissive = fixture.root.join("src/permissive.rs");
        let test_file = fixture.root.join("src/parser_tests.rs");
        let decoy = fixture.root.join("src/unknown_field.rs");
        fs::write(
            &parser,
            "fn decode(line: &str) {\n    let decoded = parse_jsonl_value(line).map_err(|error| {\n        Error::new(JsonlDecodeError { source: error })\n    })?;\n    use_value(decoded);\n}\n",
        )
        .expect("parser source");
        fs::write(
            &permissive,
            "fn visit(map: &mut Map) {\n    if known(&key) {\n        map.next_value::<IgnoredAny>()?;\n    } else {\n        extensions.push((key, map.next_value()?));\n    }\n}\n",
        )
        .expect("unknown-field source");
        fs::write(
            &test_file,
            "let fixture = \"JSONL parser error conversion unknown-field handling\";\n",
        )
        .expect("test source");
        fs::write(
            &decoy,
            "#[serde(deny_unknown_fields)]\nfn reject_unknown_field() {}\n",
        )
        .expect("decoy source");
        let output = format!(
            "File: {}, Lines: 1-99\nFile: {}, Lines: 1-99\nFile: {}, Lines: 1-99\nFile: {}, Lines: 1-99\n",
            parser.display(),
            permissive.display(),
            test_file.display(),
            decoy.display()
        );
        let report = verify_probe_evidence(
            &output,
            &fixture.root,
            "where is JSONL parser error conversion and unknown-field handling?",
            8,
        )
        .expect("verified evidence");
        assert!(report.is_complete());
        assert_eq!(report.evidence().len(), 2);
        assert!(report.evidence().iter().any(|evidence| evidence
            .location()
            .path()
            .ends_with("src/parser.rs")
            && evidence.snippet().contains("map_err")
            && evidence.location().end_line() - evidence.location().start_line() < 4));
        assert!(report.evidence().iter().any(|evidence| {
            evidence.location().path().ends_with("src/permissive.rs")
                && evidence.snippet().contains("extensions.push")
        }));
        assert!(!report
            .evidence()
            .iter()
            .any(|evidence| evidence.location().path().ends_with("parser_tests.rs")));
        assert!(!report
            .evidence()
            .iter()
            .any(|evidence| evidence.location().path().ends_with("unknown_field.rs")));
    }

    #[test]
    fn evidence_reports_missing_target_without_claiming_complete() {
        let fixture = Fixture::new();
        let parser = fixture.root.join("src/parser.rs");
        fs::write(
            &parser,
            "fn decode(line: &str) {\n    parse_jsonl_value(line).map_err(convert_error)?;\n}\n",
        )
        .expect("parser source");
        let output = format!("File: {}, Lines: 1-99\n", parser.display());
        let report = verify_probe_evidence(
            &output,
            &fixture.root,
            "where is JSONL parser error conversion and unknown-field handling?",
            8,
        )
        .expect("partial evidence");
        assert!(!report.is_complete());
        assert_eq!(report.evidence().len(), 1);
        assert_eq!(report.missing_targets().len(), 1);
    }

    #[test]
    fn evidence_exposes_an_exact_symbol_when_present() {
        let fixture = Fixture::new();
        let source = fixture.root.join("src/errors.rs");
        fs::write(
            &source,
            "fn probe_json_error(input: &str) {\n    convert(input);\n}\n",
        )
        .expect("symbol source");
        let output = format!("File: {}, Lines: 1-99\n", source.display());
        let report = verify_probe_evidence(&output, &fixture.root, "where is probe_json_error?", 8)
            .expect("symbol evidence");
        assert!(report.is_complete());
        assert_eq!(report.evidence()[0].symbol(), Some("probe_json_error"));
    }

    #[cfg(unix)]
    #[test]
    fn evidence_rejects_traversal_and_symlink_boundary_candidates() {
        let fixture = Fixture::new();
        let outside = fixture
            .root
            .parent()
            .expect("parent")
            .join("outside-link.rs");
        fs::write(&outside, "compression publication cache assembly\n").expect("outside source");
        let link = fixture.root.join("src/link.rs");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let symlink_output = format!("File: {}, Lines: 1-1\n", link.display());
        let traversal_output = format!(
            "File: ../{}, Lines: 1-1\n",
            outside.file_name().unwrap().display()
        );
        for output in [symlink_output, traversal_output] {
            assert_eq!(
                verify_probe_evidence(
                    &output,
                    &fixture.root,
                    "compression publication cache assembly",
                    8,
                ),
                Err(EvidenceError::NoSourceLocations)
            );
        }
        let _ = fs::remove_file(&outside);
    }
}
