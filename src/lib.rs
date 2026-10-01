#[cfg(test)]
#[path = "definition_intent_tests.rs"]
mod definition_intent_tests;
mod relevance_scope;
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
    any_of: bool,
    definition: bool,
    symbol: Option<String>,
    owner: Option<String>,
    path: Option<String>,
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
    let Some(groups) = query_groups(query) else {
        return Err(EvidenceError::NoSourceLocations);
    };
    if groups.is_empty() {
        return Err(EvidenceError::NoSourceLocations);
    }
    let any_of = groups[0].any_of;

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
                &root,
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

    let missing_targets = if any_of && covered.iter().any(|covered| *covered) {
        Vec::new()
    } else {
        groups
            .iter()
            .enumerate()
            .filter(|(index, _)| !covered[*index])
            .map(|(_, group)| group.label.clone())
            .collect::<Vec<_>>()
    };
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

fn query_groups(query: &str) -> Option<Vec<QueryGroup>> {
    if query
        .chars()
        .any(|character| matches!(character, '(' | ')' | '&' | '|' | '!'))
    {
        return None;
    }
    let mut groups = Vec::<Vec<String>>::new();
    let mut current = Vec::new();
    let mut disjunction = None;
    for token in raw_query_tokens(query) {
        if token.to_lowercase() == "not" || token.to_lowercase() == "xor" {
            return None;
        }
        if token.to_lowercase() == "and" || token.to_lowercase() == "or" {
            if current.is_empty() {
                return None;
            }
            let is_or = token.eq_ignore_ascii_case("or");
            if disjunction.is_some_and(|previous| previous != is_or) {
                return None;
            }
            disjunction = Some(is_or);
            groups.push(std::mem::take(&mut current));
        } else {
            current.push(token);
        }
    }
    if current.is_empty() {
        return None;
    }
    groups.push(current);
    let any_of = disjunction == Some(true);
    let groups = groups
        .into_iter()
        .map(|tokens| {
            let mut terms = Vec::new();
            let mut exact_symbols = Vec::new();
            for token in &tokens {
                let term = token.to_lowercase();
                if term.len() >= 3
                    && !query_stop_word(&term)
                    && !terms.contains(&term)
                    && !term.contains('/')
                    && !term.contains('.')
                {
                    terms.push(term);
                }
                let pieces: Vec<&str> = if token.contains("::") {
                    token
                        .split("::")
                        .filter(|piece| !piece.is_empty())
                        .collect()
                } else {
                    vec![token.as_str()]
                };
                for piece in pieces {
                    if piece.contains('_') {
                        exact_symbols.push(compact_alphanumeric(piece));
                    }
                }
            }
            let definition = definition_request(&tokens);
            let (symbol, owner, path) = if definition {
                definition_identity(&tokens)
            } else {
                (None, None, None)
            };
            (!terms.is_empty()).then(|| QueryGroup {
                label: tokens
                    .iter()
                    .filter(|token| !query_stop_word(&token.to_lowercase()))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
                terms,
                exact_symbols,
                any_of,
                definition,
                symbol,
                owner,
                path,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(groups)
}

fn definition_request(tokens: &[String]) -> bool {
    let control = |token: &str| control_word(token);
    let asks_place = tokens.iter().any(|token| control(token) == "where");
    let asks_definition = tokens.iter().any(|token| {
        matches!(
            control(token).as_str(),
            "defined" | "definition" | "implementation" | "implement" | "implements"
        )
    });
    let explicit_definition = tokens
        .windows(2)
        .any(|pair| control(&pair[0]) == "definition" && control(&pair[1]) == "of");
    (explicit_definition || (asks_place && asks_definition))
        && definition_identity(tokens).0.is_some()
}

fn definition_identity(tokens: &[String]) -> (Option<String>, Option<String>, Option<String>) {
    let noise = |token: &str| {
        let word = control_word(token);
        matches!(
            word.as_str(),
            "where"
                | "location"
                | "defined"
                | "definition"
                | "implementation"
                | "implement"
                | "implements"
                | "fn"
                | "func"
                | "function"
                | "def"
                | "struct"
                | "class"
                | "method"
                | "in"
                | "at"
                | "file"
                | "of"
                | "the"
        ) || token.contains('.')
            || token.contains('/')
    };
    let path = tokens
        .windows(2)
        .find(|pair| pair[0].eq_ignore_ascii_case("in") && path_token(&pair[1]))
        .map(|pair| pair[1].clone())
        .or_else(|| tokens.iter().find(|token| path_token(token)).cloned());
    let mut names = tokens
        .iter()
        .filter(|token| !noise(token) && !token.contains('/') && !token.contains('.'))
        .cloned()
        .collect::<Vec<_>>();
    let owner = names
        .iter()
        .position(|token| token.contains("::"))
        .map(|index| {
            let qualified = names.remove(index);
            let (owner, member) = qualified.rsplit_once("::").unwrap_or((&qualified, ""));
            if !member.is_empty() {
                names.insert(index.min(names.len()), member.to_owned());
            }
            owner.to_owned()
        });
    let symbol = names
        .iter()
        .rev()
        .find(|token| {
            token.contains('_') || token.chars().any(|character| character.is_uppercase())
        })
        .or(names.last())
        .cloned();
    (symbol, owner, path)
}

fn raw_query_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in value.chars() {
        if character.is_alphanumeric()
            || character == '_'
            || character == ':'
            || matches!(character, '/' | '.' | '-')
        {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn control_word(token: &str) -> String {
    token
        .trim_end_matches(['.', '?', '!', ',', ';', ':'])
        .to_lowercase()
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
    // Underscored symbols stay exact. Ordinary words share a prefix, not an arbitrary stem.
    if left.contains('_') || right.contains('_') {
        return false;
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
    if group
        .terms
        .iter()
        .any(|term| token_matches(term, "unknown"))
        && group.terms.iter().any(|term| {
            ["field", "fields", "handling", "handle"]
                .iter()
                .any(|word| token_matches(term, word))
        })
    {
        add(&mut features, "unknown-field-handling");
    }
    features
}

struct CodeView {
    code: String,
}

impl CodeView {
    // Byte-preserving lexical projection; semantic proof uses the syntax tree.
    // Mask bytes, not lines, so every window still addresses the original source.
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut code = bytes.to_vec();
        let mut index = 0;
        while index < bytes.len() {
            let start = index;
            if bytes[index..].starts_with(b"//") {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            } else if bytes[index..].starts_with(b"/*") {
                index += 2;
                let mut depth = 1;
                while index < bytes.len() && depth > 0 {
                    if bytes[index..].starts_with(b"/*") {
                        depth += 1;
                        index += 2;
                    } else if bytes[index..].starts_with(b"*/") {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            } else if bytes[index] == b'\'' && character_end(source, index).is_some() {
                index = character_end(source, index).unwrap_or(index + 1);
            } else {
                let mut quote = index;
                if bytes[index] == b'r' {
                    quote += 1;
                    while quote < bytes.len() && bytes[quote] == b'#' {
                        quote += 1;
                    }
                }
                if bytes.get(quote) != Some(&b'"') {
                    index += 1;
                    continue;
                }
                let raw = bytes[index] == b'r';
                let hashes = quote.saturating_sub(index + 1);
                index = quote + 1;
                while index < bytes.len() {
                    if !raw && bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == b'"'
                        && (!raw
                            || bytes
                                .get(index + 1..index + 1 + hashes)
                                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#')))
                    {
                        index += 1 + if raw { hashes } else { 0 };
                        break;
                    } else {
                        index += 1;
                    }
                }
            }
            for byte in &mut code[start..index] {
                if *byte != b'\n' && *byte != b'\r' {
                    *byte = b' ';
                }
            }
        }
        Self {
            // Retained UTF-8 is unchanged; each removed byte is ASCII whitespace.
            code: String::from_utf8_lossy(&code).into_owned(),
        }
    }
}

// A character has one scalar or one escape followed by a closing apostrophe.
// Lifetimes have no closing apostrophe and must remain executable syntax.
fn character_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut end = start + 1;
    if bytes.get(end) == Some(&b'\\') {
        end += 1;
        match bytes.get(end)? {
            b'u' if bytes.get(end + 1) == Some(&b'{') => {
                end += 2;
                let limit = (end + 7).min(bytes.len());
                while end < limit && bytes[end] != b'}' {
                    end += 1;
                }
                if bytes.get(end) != Some(&b'}') {
                    return None;
                }
                end += 1;
            }
            b'x' => end += 3,
            _ => end += 1,
        }
    } else {
        end += source.get(end..)?.chars().next()?.len_utf8();
    }
    (bytes.get(end) == Some(&b'\'')).then_some(end + 1)
}

fn window_features(code: &str, unknown_field_proved: bool) -> Vec<&'static str> {
    let lower = code.to_lowercase();
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

    if unknown_field_proved {
        add(&mut features, "unknown-field-handling");
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
    root: &Path,
    relative: &Path,
    path: &Path,
    lines: &[&str],
    order: usize,
    test_candidate: bool,
) -> Option<ScoredEvidence> {
    let all_terms = all_groups
        .iter()
        .flat_map(|group| group.terms.iter().cloned())
        .fold(Vec::new(), |mut terms, term| {
            if !terms.contains(&term) {
                terms.push(term);
            }
            terms
        });
    let path_text = relative.to_string_lossy();
    let requested = requested_features(group);
    let behavioral = behavior_query(group);
    let context_terms = all_groups
        .iter()
        .flat_map(|candidate| candidate.terms.iter().cloned())
        .filter(|term| !group.terms.contains(term))
        .collect::<Vec<_>>();
    let source = lines.join("\n");
    let view = CodeView::new(&source);
    // split, unlike lines(), preserves the cardinality of the original joined lines.
    let code_lines: Vec<&str> = view.code.split('\n').collect();
    let scopes = relevance_scope::Proofs::new(&source, &view.code);
    if requested.contains(&"unknown-field-handling") && scopes.is_empty() {
        return None;
    }
    let mut best: Option<ScoredEvidence> = None;
    for start in 0..lines.len() {
        for length in 1..=MAX_EVIDENCE_LINES.min(lines.len() - start) {
            let end = start + length;
            let text = lines[start..end].join("\n");
            let code = code_lines[start..end].join("\n");
            let source_text = if behavioral { &code } else { &text };
            let relevance_text = format!("{path_text}\n{source_text}");
            let group_matches = if group.exact_symbols.is_empty() {
                matching_terms(&group.terms, source_text)
            } else {
                group
                    .terms
                    .iter()
                    .filter(|term| {
                        let expected = compact_alphanumeric(term);
                        raw_identifiers(source_text)
                            .iter()
                            .any(|candidate| compact_alphanumeric(candidate) == expected)
                    })
                    .cloned()
                    .collect()
            };
            let any_matches = matching_terms(&all_terms, source_text).len();
            let path_matches = matching_terms(&group.terms, &path_text);
            let path_context = matching_terms(&context_terms, &path_text);
            let context_matches = matching_terms(&all_terms, &relevance_text);
            let features = window_features(&code, scopes.covers(start + 1, end));
            if requested.contains(&"unknown-field-handling")
                && !features.contains(&"unknown-field-handling")
            {
                continue;
            }
            let overlap = requested
                .iter()
                .filter(|feature| features.contains(feature))
                .count();
            let markers = code_lines[start..end]
                .iter()
                .flat_map(|line| line_markers(line))
                .fold(Vec::new(), |mut markers, marker| {
                    if !markers.contains(&marker) {
                        markers.push(marker);
                    }
                    markers
                });
            let exact_symbol = exact_symbol_in_lines(group, &code_lines, start, end);
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
            // A requested compound symbol is covered only when every term is exact.
            let exact_group = !group.exact_symbols.is_empty() && direct == group.terms.len();
            let simple_lexical = !behavioral
                && (direct >= 2
                    || exact_group
                    || (group.any_of && direct >= 1 && any_matches >= 2));
            let exact = exact_symbol.is_some() && group.exact_symbols.len() <= 1;
            let defined = !group.definition
                || lines[start..end].iter().enumerate().any(|(offset, line)| {
                    defines_requested(line, group, &code_lines, start + offset, relative)
                });
            if !defined {
                continue;
            }
            if group.definition {
                if group
                    .path
                    .as_ref()
                    .is_some_and(|requested| !paths_match(root, &path_text, requested))
                {
                    continue;
                }
                let signature = lines[start..end].iter().enumerate().any(|(offset, line)| {
                    defines_requested(line, group, &code_lines, start + offset, relative)
                });
                if !signature {
                    continue;
                }
            }
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
                + if !test_candidate
                    && lines[start..end].iter().enumerate().any(|(offset, line)| {
                        declaration_names(line, code_lines[start + offset], relative)
                            .iter()
                            .any(|name| same_name(name, group))
                    })
                {
                    120
                } else {
                    0
                }
                - length as i32;
            let location = SourceLocation::new(path.to_path_buf(), start + 1, end);
            let symbol = lines[start..end]
                .iter()
                .enumerate()
                .find_map(|(offset, line)| {
                    defines_requested(line, group, &code_lines, start + offset, relative)
                        .then(|| group.symbol.clone())
                        .flatten()
                        .or_else(|| {
                            raw_identifiers(code_lines[start + offset])
                                .into_iter()
                                .find(|name| same_name(name, group))
                        })
                })
                .or(exact_symbol)
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

fn path_token(token: &str) -> bool {
    token.contains('/')
        || token.ends_with(".rs")
        || token.ends_with(".py")
        || token.ends_with(".js")
        || token.ends_with(".c")
        || token.ends_with(".h")
}

fn paths_match(root: &Path, actual: &str, requested: &str) -> bool {
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };
    match fs::canonicalize(&candidate) {
        Ok(resolved) => resolved.strip_prefix(root).is_ok_and(|relative| {
            !relative.as_os_str().is_empty()
                && !excluded_path(relative)
                && resolved
                    == fs::canonicalize(root.join(actual)).unwrap_or_else(|_| root.join(actual))
        }),
        Err(_) => {
            let actual = actual.replace('\\', "/");
            let requested = requested.replace('\\', "/");
            actual == requested || actual.ends_with(&format!("/{requested}"))
        }
    }
}

fn same_name(name: &str, group: &QueryGroup) -> bool {
    group
        .symbol
        .as_ref()
        .is_some_and(|symbol| compact_alphanumeric(name) == compact_alphanumeric(symbol))
        || group
            .exact_symbols
            .iter()
            .any(|expected| !expected.is_empty() && compact_alphanumeric(name) == *expected)
}

fn defines_requested(
    line: &str,
    group: &QueryGroup,
    code_lines: &[&str],
    index: usize,
    relative: &Path,
) -> bool {
    let code_line = code_lines.get(index).copied().unwrap_or("");
    if !group.definition || code_line.trim().is_empty() {
        return false;
    }
    let Some(symbol) = &group.symbol else {
        return false;
    };
    let names = declaration_names(line, code_line, relative);
    let declared = names
        .iter()
        .any(|name| compact_alphanumeric(name) == compact_alphanumeric(symbol));
    if !declared {
        return false;
    }
    group.owner.as_ref().is_none_or(|owner| {
        names
            .iter()
            .any(|name| compact_alphanumeric(name) == compact_alphanumeric(owner))
            || enclosing_owner(code_lines, index)
                .is_some_and(|found| compact_alphanumeric(&found) == compact_alphanumeric(owner))
    })
}

fn enclosing_owner(code_lines: &[&str], index: usize) -> Option<String> {
    code_lines[..=index]
        .iter()
        .rev()
        .find_map(|line| rust_impl_owner(line))
}

fn declaration_names(line: &str, code_line: &str, relative: &Path) -> Vec<String> {
    let rust = relative
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("rs");
    if !rust {
        return foreign_declaration_name(code_line).into_iter().collect();
    }
    if production_source(line) {
        rust_declaration_name(code_line).into_iter().collect()
    } else {
        Vec::new()
    }
}

fn production_source(line: &str) -> bool {
    let trimmed = line.trim_start();
    !(trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
        || trimmed.starts_with("```"))
}

fn rust_declaration_name(code_line: &str) -> Option<String> {
    let trimmed = code_line.trim_start();
    let rest = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub(super) "))
        .or_else(|| trimmed.strip_prefix("pub "))
        .or_else(|| trimmed.strip_prefix("async "))
        .or_else(|| trimmed.strip_prefix("unsafe "))
        .or_else(|| trimmed.strip_prefix("const "))
        .unwrap_or(trimmed);
    let rest = rest
        .strip_prefix("async ")
        .or_else(|| rest.strip_prefix("unsafe "))
        .or_else(|| rest.strip_prefix("const "))
        .unwrap_or(rest);
    for keyword in ["fn ", "struct ", "enum ", "trait ", "type ", "mod "] {
        if let Some(name) = rest.strip_prefix(keyword) {
            return identifier_head(name);
        }
    }
    None
}

fn rust_impl_owner(code_line: &str) -> Option<String> {
    let trimmed = code_line.trim_start();
    let rest = trimmed.strip_prefix("impl ").or_else(|| {
        trimmed
            .strip_prefix("pub ")
            .and_then(|value| value.strip_prefix("impl "))
    })?;
    let owner = rest.split(['<', ' ', '{']).next()?.trim();
    (!owner.is_empty() && owner != "for").then(|| owner.to_owned())
}

fn foreign_declaration_name(code_line: &str) -> Option<String> {
    let trimmed = code_line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with('#') {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("def ") {
        return identifier_head(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("async def ") {
        return identifier_head(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("function ") {
        return identifier_head(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("async function ") {
        return identifier_head(rest);
    }
    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?;
    let second = parts.next()?;
    if matches!(
        first,
        "int" | "void" | "char" | "long" | "short" | "float" | "double" | "size_t"
    ) && second.contains('(')
    {
        return identifier_head(second);
    }
    None
}

fn identifier_head(value: &str) -> Option<String> {
    let name: String = value
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect();
    (!name.is_empty()).then_some(name)
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
    fn source_evidence_respects_flat_boolean_connectors() {
        let fixture = Fixture::new();
        let output = format!(
            "File: {}, Lines: 1-1\n",
            fixture.root.join("src/lib.rs").display()
        );
        let or_report = verify_probe_evidence(&output, &fixture.root, "compression or cache", 8)
            .expect("either disjunct is sufficient");
        assert!(or_report.is_complete());
        assert!(or_report.missing_targets().is_empty());

        fs::write(fixture.root.join("src/lib.rs"), "compression\n")
            .expect("insufficient one-term source");
        assert_eq!(
            verify_probe_evidence(&output, &fixture.root, "compression or cache", 8),
            Err(EvidenceError::NoSourceLocations),
            "one lexical span must not bypass the existing two-term evidence floor"
        );

        fs::write(fixture.root.join("src/lib.rs"), "compression publication\n")
            .expect("single OR branch source");
        let branch_or_report = verify_probe_evidence(
            &output,
            &fixture.root,
            "compression publication or cache key",
            8,
        )
        .expect("one complete OR branch is sufficient");
        assert!(branch_or_report.is_complete());
        assert!(branch_or_report.missing_targets().is_empty());

        let and_report = verify_probe_evidence(
            &output,
            &fixture.root,
            "compression publication and cache key",
            8,
        )
        .expect("partial AND evidence");
        assert!(!and_report.is_complete());
        assert_eq!(and_report.missing_targets(), &["cache key"]);

        for unsupported in [
            "(compression publication or cache key)",
            "compression publication or cache key and assembly",
        ] {
            assert_eq!(
                verify_probe_evidence(&output, &fixture.root, unsupported, 8),
                Err(EvidenceError::NoSourceLocations),
                "unsupported boolean syntax must fail closed: {unsupported}"
            );
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
        fs::write(
            &parser,
            "fn decode(line: &str) {\n    let decoded = parse_jsonl_value(line).map_err(|error| {\n        Error::new(JsonlDecodeError { source: error })\n    })?;\n    use_value(decoded);\n}\n",
        )
        .expect("parser source");
        fs::write(
            &permissive,
            "fn visit(map: &mut Map) {\n    let mut seen = BTreeSet::new();\n    let mut extensions = Vec::new();\n    while let Some(key) = map.next_key::<String>()? {\n        if !seen.insert(key.clone()) {\n            return Err(A::Error::custom(format!(\"duplicate field `{key}`\")));\n        }\n        if RECORD_FIELDS.contains(&key.as_str()) {\n            map.next_value::<IgnoredAny>()?;\n        } else {\n            extensions.push((key, map.next_value()?));\n        }\n    }\n}\n",
        )
        .expect("unknown-field source");
        fs::write(
            &test_file,
            "let _fixture = NamedTempFile::new().expect(\"test fixture\");\n// where is JSONL parser error conversion and unknown-field handling?\n",
        )
        .expect("test source");
        let output = format!(
            "File: {}, Lines: 1-99\nFile: {}, Lines: 1-99\nFile: {}, Lines: 1-99\n",
            parser.display(),
            permissive.display(),
            test_file.display()
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
    }

    #[test]
    fn unknown_field_evidence_requires_semantic_code() {
        let fixture = Fixture::new();
        let verify = |name: &str, source: &str| {
            let path = fixture.root.join("src").join(name);
            fs::write(&path, source).expect("fixture source");
            let output = format!("File: {}, Lines: 1-99\n", path.display());
            verify_probe_evidence(&output, &fixture.root, "unknown field handling", 8)
        };

        let serde = verify(
            "serde.rs",
            r#"use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config { enabled: bool }
fn decode(input: &str) -> Result<Config, serde_json::Error> { serde_json::from_str(input) }
"#,
        )
        .is_ok_and(|report| {
            report.is_complete()
                && report.evidence().iter().any(|evidence| {
                    evidence.snippet().contains("deny_unknown_fields")
                        && evidence.snippet().contains("from_str")
                })
        });
        let alternate_extras = verify(
            "alternate.rs",
            r#"fn retain<T>(known_fields: &std::collections::HashSet<String>, key: String, value: T) -> Vec<(String, T)> {
    let mut other_values = Vec::new();
    if known_fields.contains(&key) { drop(value); } else { other_values.push((key, value)); }
    other_values
}
"#,
        )
        .is_ok_and(|report| {
            report.is_complete()
                && report
                    .evidence()
                    .iter()
                    .any(|evidence| evidence.snippet().contains("other_values.push"))
        });
        let rejecting_fallback = verify(
            "fallback.rs",
            r#"#[derive(Debug)]
enum FieldError { UnknownField(String) }
fn parse_field(key: &str) -> Result<(), FieldError> {
    match key {
        "id" => Ok(()),
        other => Err(FieldError::UnknownField(other.to_owned())),
    }
}
"#,
        )
        .is_ok_and(|report| {
            report.is_complete()
                && report.evidence().iter().any(|evidence| {
                    evidence.snippet().contains("match key")
                        && evidence.snippet().contains("UnknownField")
                })
        });
        let duplicate_key = matches!(
            verify(
                "duplicate.rs",
                r#"fn reject_duplicate_key(key: &str, seen: &mut Set<String>) -> Result<(), KeyError> {
    if !seen.insert(key.to_owned()) {
        return Err(KeyError::DuplicateKey(key.to_owned()));
    }
    Ok(())
}
"#,
            ),
            Err(EvidenceError::NoSourceLocations)
        );
        let comment_only = matches!(
            verify("comment.rs", "// unknown field handling?\n"),
            Err(EvidenceError::NoSourceLocations)
        );
        let comment_with_unrelated_code = matches!(
            verify(
                "comment_with_code.rs",
                "// unknown field handling?\nfn unrelated() { let _ = String::new(); }\n",
            ),
            Err(EvidenceError::NoSourceLocations)
        );

        assert!(
            serde
                && alternate_extras
                && rejecting_fallback
                && duplicate_key
                && comment_only
                && comment_with_unrelated_code,
            "serde={serde}, alternate extras={alternate_extras}, rejecting fallback={rejecting_fallback}, duplicate-key decoy={duplicate_key}, comment-only decoy={comment_only}, comment with unrelated code={comment_with_unrelated_code}"
        );
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
    fn evidence_rejects_display_method_as_display_relative() {
        let fixture = Fixture::new();
        let path = fixture.root.join("crates/verbatim-core/src/ingest.rs");
        fs::create_dir_all(path.parent().unwrap()).expect("parents");
        fs::write(
            &path,
            "                .with_context(|| format!(\"remove stale image artifact: {}\", path.display()))?;\n        }\n    }\n    if artifacts.is_empty() || fs::read_dir(&source_dir)?.next().is_none() {\n",
        )
        .expect("decoy");
        let output = format!("File: {}, Lines: 1-4\n", path.display());
        let report =
            verify_probe_evidence(&output, &fixture.root, "SourceLocation display_relative", 4);
        assert_eq!(report, Err(EvidenceError::NoSourceLocations));
    }

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
