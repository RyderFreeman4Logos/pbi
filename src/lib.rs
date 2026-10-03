mod declaration_identity;
#[cfg(test)]
#[path = "definition_intent_tests.rs"]
mod definition_intent_tests;
mod relevance_scope;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub mod semantic;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EVIDENCE_LINES: usize = 4;
const MAX_FOLLOWING_LINES: usize = 8;
const MAX_CALLER_CONTEXT_LINES: usize = 40;

#[cfg(test)]
std::thread_local! {
    static WINDOW_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FEATURE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A verified source path and the exact cited line returned to a caller.
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
    cited: Vec<usize>,
    followed_from: Vec<Option<usize>>,
    call_edges: Vec<(usize, usize, bool)>,
}

impl EvidenceReport {
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn evidence(&self) -> &[SourceEvidence] {
        &self.evidence
    }

    pub fn cited_line(&self, index: usize) -> Option<usize> {
        self.cited.get(index).copied()
    }

    pub(crate) fn followed_from(&self, index: usize) -> Option<usize> {
        self.followed_from.get(index).copied().flatten()
    }

    /// Evidence on a parsed call path through the selected stopping branch.
    /// Ancestors explain who uses its result; descendants explain its inputs.
    pub(crate) fn causal_indices(&self, stop: usize) -> Vec<usize> {
        let mut selected = vec![false; self.evidence.len()];
        if stop >= selected.len() {
            return Vec::new();
        }
        let mut ancestor = Some(stop);
        let mut ancestors = Vec::new();
        while let Some(index) = ancestor {
            if selected[index] {
                break;
            }
            selected[index] = true;
            ancestors.push(index);
            ancestor = self.followed_from(index);
        }
        let mut pending = vec![stop];
        while let Some(parent) = pending.pop() {
            for &(_, child, _) in self
                .call_edges
                .iter()
                .filter(|(owner, _, _)| *owner == parent)
            {
                if !selected[child] {
                    selected[child] = true;
                    pending.push(child);
                }
            }
        }
        // A caller can apply another value-producing gate before the selected
        // stop. Its definition is evidence for comparisons between budgets.
        for parent in ancestors.into_iter().skip(1) {
            let mut sibling_pending = self
                .call_edges
                .iter()
                .filter(|(owner, _, value_used)| *owner == parent && *value_used)
                .map(|(_, child, _)| *child)
                .collect::<Vec<_>>();
            while let Some(index) = sibling_pending.pop() {
                if selected[index] {
                    continue;
                }
                selected[index] = true;
                sibling_pending.extend(
                    self.call_edges
                        .iter()
                        .filter(|(owner, _, value_used)| *owner == index && *value_used)
                        .map(|(_, child, _)| *child),
                );
            }
        }
        selected
            .into_iter()
            .enumerate()
            .filter_map(|(index, keep)| keep.then_some(index))
            .collect()
    }

    pub fn missing_targets(&self) -> &[String] {
        &self.missing_targets
    }

    /// Extend selected spans and their called Rust definitions within the same
    /// verified files. Every added range is reread and bounded before synthesis.
    pub fn with_following_lines(
        mut self,
        root: &Path,
        max_total: usize,
    ) -> Result<Self, EvidenceError> {
        let root = fs::canonicalize(root).map_err(|_| EvidenceError::SourceUnavailable)?;
        let count = max_total.saturating_sub(self.evidence.len());
        for index in 0..count.min(self.evidence.len()) {
            let item = self.evidence[index].clone();
            let path = resolve_candidate_path(item.location().path(), &root)
                .ok_or(EvidenceError::SourceOutsideRoot)?;
            if source_is_too_large(&path) {
                return Err(EvidenceError::SourceUnavailable);
            }
            let source = fs::read_to_string(&path).map_err(|_| EvidenceError::SourceUnavailable)?;
            let lines = source.lines().collect::<Vec<_>>();
            let start = item.location().end_line().saturating_add(1);
            if start > lines.len() {
                continue;
            }
            let declarations =
                if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                    declaration_identity::declarations(&source)
                } else {
                    Vec::new()
                };
            let next_declaration = declarations
                .iter()
                .map(|declaration| declaration.line)
                .find(|line| *line >= start);
            let end = next_declaration
                .map(|line| line.saturating_sub(1))
                .unwrap_or(start.saturating_add(MAX_FOLLOWING_LINES - 1))
                .max(item.location().end_line())
                .min(lines.len());
            let mut first = item
                .location()
                .start_line()
                .saturating_sub(MAX_CALLER_CONTEXT_LINES)
                .max(
                    declarations
                        .iter()
                        .map(|declaration| declaration.line)
                        .take_while(|line| *line <= item.location().start_line())
                        .last()
                        .unwrap_or(1),
                );
            let mut snippet = lines[first - 1..end].join("\n");
            if snippet.len() > 4096 {
                first = item.location().start_line();
                snippet = lines[first - 1..end].join("\n");
            }
            if snippet.len() > 4096 {
                continue;
            }
            self.evidence[index] = SourceEvidence {
                location: SourceLocation::new(path, first, end),
                target: item.target().to_owned(),
                snippet,
                symbol: item.symbol().map(str::to_owned),
                relevance: item.relevance().to_owned(),
            };
        }
        let root_device = fs::metadata(&root)
            .map_err(|_| EvidenceError::SourceUnavailable)?
            .dev();
        let mut sources = Vec::new();
        for item in &self.evidence {
            let path = item.location().path();
            if path.extension().and_then(|value| value.to_str()) != Some("rs")
                || sources.iter().any(|(seen, _, _, _)| seen == path)
            {
                continue;
            }
            let path =
                resolve_candidate_path(path, &root).ok_or(EvidenceError::SourceOutsideRoot)?;
            if source_is_too_large(&path)
                || fs::metadata(&path)
                    .map_err(|_| EvidenceError::SourceUnavailable)?
                    .dev()
                    != root_device
            {
                return Err(EvidenceError::SourceUnavailable);
            }
            let source = fs::read_to_string(&path).map_err(|_| EvidenceError::SourceUnavailable)?;
            let declarations = declaration_identity::declarations(&source);
            let calls = declaration_identity::calls(&source);
            sources.push((path, source, declarations, calls));
        }
        let mut module_files = Vec::new();
        for (path, source, _, calls) in &sources {
            let Some(parent_dir) = path.parent() else {
                continue;
            };
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            for call in calls {
                let Some(module) = call.marker.split("::").next() else {
                    continue;
                };
                if module.is_empty() || module.contains('.') || module == call.marker {
                    continue;
                }
                let candidate = parent_dir.join(stem).join(format!("{module}.rs"));
                if sources.iter().any(|(seen, _, _, _)| seen == &candidate)
                    || module_files.iter().any(|seen| seen == &candidate)
                {
                    continue;
                }
                let Some(resolved) = resolve_candidate_path(&candidate, &root) else {
                    continue;
                };
                if source_is_too_large(&resolved)
                    || fs::metadata(&resolved)
                        .map_err(|_| EvidenceError::SourceUnavailable)?
                        .dev()
                        != root_device
                {
                    return Err(EvidenceError::SourceUnavailable);
                }
                module_files.push(resolved);
            }
            let _ = source;
        }
        for path in module_files {
            let source = fs::read_to_string(&path).map_err(|_| EvidenceError::SourceUnavailable)?;
            let declarations = declaration_identity::declarations(&source);
            let calls = declaration_identity::calls(&source);
            sources.push((path, source, declarations, calls));
        }
        let source_fields = sources
            .iter()
            .map(|(_, source, _, _)| declaration_identity::field_types(source))
            .collect::<Vec<_>>();
        let all_fields = source_fields
            .iter()
            .flat_map(|fields| fields.iter().cloned())
            .collect::<Vec<_>>();
        let mut pending = (0..self.evidence.len())
            .rev()
            .map(|index| (index, 0))
            .collect::<Vec<_>>();
        while let Some((index, next_call)) = pending.pop() {
            if self.evidence.len() >= max_total {
                break;
            }
            let item = self.evidence[index].clone();
            let source_index = sources
                .iter()
                .position(|(path, _, _, _)| path == item.location().path());
            let own_fields = source_index
                .and_then(|index| source_fields.get(index))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut calls = source_index
                .and_then(|index| sources.get(index))
                .map(|(_, _, _, calls)| {
                    calls
                        .iter()
                        .filter(|call| {
                            call.line >= item.location().start_line()
                                && call.line <= item.location().end_line()
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            calls.sort_by_key(|call| {
                let specificity = if call.marker.starts_with('.') {
                    usize::from(declaration_identity::receiver_owner(call, own_fields).is_none())
                } else if call.marker.contains("::") {
                    1
                } else {
                    2
                };
                (!call.value_used, specificity)
            });
            for (call_index, call) in calls.into_iter().enumerate().skip(next_call) {
                let (kind, qualifier, name) =
                    if let Some((owner, name)) = call.marker.rsplit_once("::") {
                        ("qualified", owner, name)
                    } else if let Some((_, name)) = call.marker.rsplit_once('.') {
                        ("method", "", name)
                    } else {
                        ("bare", "", call.marker.as_str())
                    };
                let receiver = if kind == "method" {
                    let fields = if own_fields.is_empty() {
                        &all_fields
                    } else {
                        own_fields
                    };
                    let Some(owner) = declaration_identity::receiver_owner(&call, fields) else {
                        continue;
                    };
                    Some(owner)
                } else {
                    None
                };
                let mut matches = Vec::new();
                for (source_index, (path, _, declarations, _)) in sources.iter().enumerate() {
                    let same_file = path == item.location().path();
                    let file_stem = path.file_stem().and_then(|stem| stem.to_str());
                    let child_module = item.location().path().parent().is_some_and(|parent| {
                        path.parent()
                            == Some(
                                &parent
                                    .join(item.location().path().file_stem().unwrap_or_default()),
                            )
                            && file_stem == Some(qualifier)
                    });
                    if (kind != "qualified" && !same_file)
                        || (kind == "qualified"
                            && file_stem != Some(qualifier)
                            && !same_file
                            && !child_module)
                    {
                        continue;
                    }
                    for (declaration_index, declaration) in declarations.iter().enumerate() {
                        if declaration.name == name
                            && declaration.owner.as_ref().is_some_and(|owner| match kind {
                                "method" => {
                                    !owner.is_root()
                                        && receiver.as_ref().is_some_and(|receiver| {
                                            owner.matches_receiver(receiver, file_stem)
                                        })
                                }
                                "qualified" => {
                                    (file_stem == Some(qualifier) && owner.is_root())
                                        || owner.equals(qualifier)
                                }
                                _ => owner.is_root(),
                            })
                        {
                            matches.push((source_index, declaration_index));
                        }
                    }
                }
                let [(source_index, declaration_index)] = matches.as_slice() else {
                    continue;
                };
                let (path, source, declarations, _) = &sources[*source_index];
                let lines = source.lines().collect::<Vec<_>>();
                let start = declarations[*declaration_index].line;
                let end = declarations
                    .iter()
                    .find(|next| next.line > start)
                    .map_or(lines.len(), |next| next.line.saturating_sub(1));
                if start == 0 || end < start || end > lines.len() {
                    continue;
                }
                let snippet = lines[start - 1..end].join("\n");
                if snippet.len() > 4096 {
                    continue;
                }
                if let Some(child) = self.evidence.iter().position(|existing| {
                    existing.location().path() == path
                        && existing.location().start_line() <= start
                        && existing.location().end_line() >= end
                }) {
                    if child != index
                        && !self
                            .call_edges
                            .iter()
                            .any(|(owner, target, _)| *owner == index && *target == child)
                    {
                        self.call_edges.push((index, child, call.value_used));
                    }
                    continue;
                }
                let child = self.evidence.len();
                self.evidence.push(SourceEvidence {
                    location: SourceLocation::new(path.clone(), start, end),
                    target: item.target().to_owned(),
                    snippet,
                    symbol: Some(name.to_owned()),
                    relevance: String::from("called Rust definition candidate"),
                });
                self.followed_from.push(Some(index));
                self.call_edges.push((index, child, call.value_used));
                pending.push((index, call_index + 1));
                pending.push((self.evidence.len() - 1, 0));
                break;
            }
        }
        Ok(self)
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
    cited: usize,
    covers_terms: bool,
    declaration_owner: Option<declaration_identity::OwnerPath>,
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
        return Err(EvidenceError::NoSourceLocations);
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
    let mut scanned_paths = HashSet::new();
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
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        // Source windows cover the entire file; repeated range hints yield the same candidates.
        if !scanned_paths.insert(path.clone()) {
            continue;
        }
        let lines: Vec<&str> = source.lines().collect();
        if lines.is_empty() {
            continue;
        }
        for (group_index, group) in groups.iter().enumerate() {
            let raw_order = raw
                .order
                .saturating_add(raw.start_line)
                .saturating_add(raw.end_line);
            choices[group_index].extend(best_windows(
                group,
                &groups,
                relative,
                &path,
                &lines,
                raw_order,
                max_results,
            ));
        }
    }

    let mut evidence = Vec::new();
    let mut cited = Vec::new();
    let mut owners: Vec<Vec<usize>> = vec![Vec::new(); groups.len()];
    for group_choices in choices.iter_mut() {
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
        group_choices.dedup_by(|left, right| {
            left.evidence.location.path == right.evidence.location.path
                && left.evidence.location.start_line == right.evidence.location.start_line
                && left.evidence.location.end_line == right.evidence.location.end_line
        });
    }
    let passes = if any_of { 1 } else { 2 };
    for pass in 0..passes {
        for (group_index, group_choices) in choices.iter().enumerate() {
            for choice in group_choices {
                let same_window = evidence.iter().position(|candidate: &SourceEvidence| {
                    candidate.location.path == choice.evidence.location.path
                        && candidate.location.start_line == choice.evidence.location.start_line
                        && candidate.location.end_line == choice.evidence.location.end_line
                });
                if let Some(existing) = same_window {
                    if !owners[group_index].contains(&existing) {
                        owners[group_index].push(existing);
                    }
                    continue;
                }
                let same_file = evidence.iter().position(|candidate: &SourceEvidence| {
                    candidate.location.path == choice.evidence.location.path
                });
                if let Some(existing) = same_file {
                    let covers_prior =
                        owners
                            .iter()
                            .enumerate()
                            .all(|(owner_index, group_owners)| {
                                !group_owners.contains(&existing)
                                    || (choice.evidence.target == groups[owner_index].label
                                        && evidence[existing].snippet.lines().all(|line| {
                                            choice.evidence.snippet.lines().any(|kept| kept == line)
                                        }))
                            });
                    let range_covers = choice.evidence.location.start_line
                        <= evidence[existing].location.start_line
                        && choice.evidence.location.end_line
                            >= evidence[existing].location.end_line;
                    if covers_prior
                        && range_covers
                        && choice.evidence.snippet.lines().count()
                            > evidence[existing].snippet.lines().count()
                    {
                        evidence[existing] = choice.evidence.clone();
                        cited[existing] = choice.cited;
                        if !owners[group_index].contains(&existing) {
                            owners[group_index].push(existing);
                        }
                        continue;
                    }
                    if !(covers_prior && range_covers) {
                        let representative_only = pass == 0 && !any_of;
                        let distinct_group =
                            !owners
                                .iter()
                                .enumerate()
                                .any(|(owner_index, group_owners)| {
                                    !group_owners.is_empty()
                                        && groups[owner_index].label == choice.evidence.target
                                });
                        let disjoint_window = evidence
                            .iter()
                            .filter(|candidate| {
                                candidate.location.path == choice.evidence.location.path
                            })
                            .all(|candidate| {
                                choice.evidence.location.start_line > candidate.location.end_line
                                    || choice.evidence.location.end_line
                                        < candidate.location.start_line
                            });
                        let same_call = choice.evidence.symbol.as_ref().is_some_and(|symbol| {
                            evidence.iter().any(|candidate| {
                                candidate.location.path == choice.evidence.location.path
                                    && candidate.symbol.as_deref() == Some(symbol.as_str())
                            })
                        });
                        if !disjoint_window
                            || (!distinct_group && !same_call && !choice.covers_terms)
                            || (representative_only
                                && !same_call
                                && !owners[group_index].is_empty())
                        {
                            continue;
                        }
                        if evidence.len() >= max_results {
                            continue;
                        }
                        owners[group_index].push(evidence.len());
                        cited.push(choice.cited);
                        evidence.push(choice.evidence.clone());
                    }
                    continue;
                }
                let representative_only = pass == 0 && !any_of;
                if representative_only && !owners[group_index].is_empty() {
                    continue;
                }
                if evidence.len() >= max_results {
                    continue;
                }
                owners[group_index].push(evidence.len());
                cited.push(choice.cited);
                evidence.push(choice.evidence.clone());
            }
        }
    }

    let covered = owners
        .iter()
        .map(|group_owners| !group_owners.is_empty())
        .collect::<Vec<_>>();
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
    let evidence_count = evidence.len();
    Ok(EvidenceReport {
        complete: missing_targets.is_empty(),
        evidence,
        missing_targets,
        cited,
        followed_from: vec![None; evidence_count],
        call_edges: Vec::new(),
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
            let definition = definition_request(&tokens);
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
                    if definition && term.contains("::") {
                        for piece in term.split("::").filter(|piece| piece.len() >= 3) {
                            if !terms.contains(&piece.to_owned()) {
                                terms.push(piece.to_owned());
                            }
                        }
                    } else {
                        terms.push(term);
                    }
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
                    if piece.contains('_') && !definition {
                        exact_symbols.push(compact_alphanumeric(piece));
                    }
                }
            }
            let (symbol, owner, path) = if definition {
                definition_identity(&tokens)
            } else {
                (None, None, None)
            };
            (!terms.is_empty()).then(|| QueryGroup {
                label: tokens
                    .iter()
                    .filter(|token| {
                        definition_request_shape(&tokens) && control_word(token) == "where"
                            || !query_stop_word(&token.to_lowercase())
                    })
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

fn definition_request_shape(tokens: &[String]) -> bool {
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
    explicit_definition || (asks_place && asks_definition)
}

fn definition_request(tokens: &[String]) -> bool {
    definition_request_shape(tokens) && definition_identity(tokens).0.is_some()
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
        })
        .or_else(|| {
            if !definition_request_shape(tokens) || names.len() < 2 {
                return None;
            }
            let member = names.last()?;
            let candidate = names.get(names.len() - 2)?;
            let owner_shaped = candidate
                .chars()
                .next()
                .is_some_and(|character| character.is_uppercase() || character == '_');
            let member_shaped = member
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_');
            (owner_shaped && member_shaped && !noise(member)).then(|| candidate.clone())
        });
    let symbol = if owner.is_some() && !names.iter().any(|token| token.contains("::")) {
        names.last().cloned()
    } else {
        names
            .iter()
            .rev()
            .find(|token| {
                token.contains('_') || token.chars().any(|character| character.is_uppercase())
            })
            .or(names.last())
            .cloned()
    };
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
            || (character == '#' && current.rsplit("::").next() == Some("r"))
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
    #[cfg(test)]
    FEATURE_SCANS.with(|scans| scans.set(scans.get() + 1));
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

fn best_windows(
    group: &QueryGroup,
    all_groups: &[QueryGroup],
    relative: &Path,
    path: &Path,
    lines: &[&str],
    order: usize,
    max_results: usize,
) -> Vec<ScoredEvidence> {
    #[cfg(test)]
    WINDOW_SCANS.with(|scans| scans.set(scans.get() + 1));
    let test_candidate = test_path(relative);
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
    let declarations = if relative
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("rs")
    {
        declaration_identity::declarations(&source)
    } else {
        Vec::new()
    };
    if requested.contains(&"unknown-field-handling") && scopes.is_empty() {
        return Vec::new();
    }
    let mut kept = Vec::new();
    for start in 0..lines.len() {
        for length in 1..=MAX_EVIDENCE_LINES.min(lines.len() - start) {
            let end = start + length;
            let text = lines[start..end].join("\n");
            let code = code_lines[start..end].join("\n");
            let lower_code = code.to_ascii_lowercase();
            let source_text = if behavioral { &code } else { &text };
            let relevance_text = format!("{path_text}\n{source_text}");
            let group_matches = if group.exact_symbols.is_empty() {
                matching_terms(&group.terms, source_text)
            } else {
                group
                    .terms
                    .iter()
                    .filter(|term| {
                        if term.contains("::") && lower_code.contains(term.as_str()) {
                            return true;
                        }
                        let expected = compact_alphanumeric(term);
                        raw_identifiers(source_text)
                            .iter()
                            .any(|candidate| compact_alphanumeric(candidate) == expected)
                    })
                    .cloned()
                    .collect()
            };
            // Without requested structural features, a non-definition window
            // with no query term or exact symbol cannot pass the final relevance guard.
            let early_exact =
                if group_matches.is_empty() && requested.is_empty() && !group.definition {
                    exact_symbol_in_lines(group, &code_lines, start, end)
                } else {
                    None
                };
            if group_matches.is_empty()
                && requested.is_empty()
                && !group.definition
                && early_exact.is_none()
            {
                continue;
            }
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
            let exact_symbol =
                early_exact.or_else(|| exact_symbol_in_lines(group, &code_lines, start, end));
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
                    defines_requested(
                        line,
                        group,
                        &code_lines,
                        start + offset,
                        relative,
                        &declarations,
                    )
                });
            if (group.definition || definition_request_shape(&raw_query_tokens(&group.label)))
                && !defined
            {
                continue;
            }
            if group.definition && group.symbol.is_some() {
                let symbol = group.symbol.as_deref().unwrap_or("");
                let starts = declarations.iter().any(|declaration| {
                    (start + 1..=start + 2).contains(&declaration.line)
                        && rust_identity_matches(&declaration.name, symbol)
                });
                let later = declarations.iter().any(|declaration| {
                    (start + 3..=end).contains(&declaration.line)
                        && rust_identity_matches(&declaration.name, symbol)
                });
                if !starts && later {
                    continue;
                }
            }
            if !defined {
                continue;
            }
            if group.definition {
                if group
                    .path
                    .as_ref()
                    .is_some_and(|requested| !paths_match(path, relative, requested))
                {
                    continue;
                }
                let signature = lines[start..end].iter().enumerate().any(|(offset, line)| {
                    defines_requested(
                        line,
                        group,
                        &code_lines,
                        start + offset,
                        relative,
                        &declarations,
                    )
                });
                if !signature {
                    continue;
                }
            }
            let starts_at_declaration = group.definition
                && (declarations.is_empty()
                    || group.symbol.as_ref().is_some_and(|symbol| {
                        declarations.iter().any(|declaration| {
                            (start..=start + 1).contains(&declaration.line)
                                && rust_identity_matches(&declaration.name, symbol)
                                && group.owner.as_ref().is_none_or(|owner| {
                                    declaration
                                        .owner
                                        .as_ref()
                                        .is_some_and(|found| found.matches(owner))
                                })
                        })
                    }));
            let call_of_kept = code_lines[start..end].iter().any(|line| {
                call_markers(line).iter().any(|marker| {
                    group.exact_symbols.iter().any(|expected| {
                        !expected.is_empty() && compact_alphanumeric(marker) == *expected
                    })
                })
            });
            if !starts_at_declaration
                && !exact
                && !call_of_kept
                && ((behavioral && (!actionable || (direct == 0 && overlap == 0)))
                    || (!behavioral && !simple_lexical))
            {
                continue;
            }
            if !starts_at_declaration && direct == 0 && overlap == 0 && !exact && !call_of_kept {
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
                        declaration_names(
                            line,
                            code_lines[start + offset],
                            relative,
                            start + offset + 1,
                            &declarations,
                        )
                        .iter()
                        .any(|name| same_name(name, group))
                    })
                {
                    120
                } else {
                    0
                }
                - length as i32;
            let cited = lines[start..end]
                .iter()
                .enumerate()
                .find(|(offset, line)| {
                    defines_requested(
                        line,
                        group,
                        &code_lines,
                        start + offset,
                        relative,
                        &declarations,
                    ) || declaration_names(
                        line,
                        code_lines[start + offset],
                        relative,
                        start + offset + 1,
                        &declarations,
                    )
                    .iter()
                    .any(|name| same_name(name, group))
                })
                .or_else(|| {
                    lines[start..end].iter().enumerate().find(|(offset, _)| {
                        let code = code_lines[start + offset].trim_start();
                        !code.starts_with("//")
                            && !code.starts_with('#')
                            && group.terms.iter().any(|term| {
                                code_lines[start + offset]
                                    .to_ascii_lowercase()
                                    .contains(term)
                            })
                    })
                })
                .map(|(offset, _)| start + offset + 1)
                .unwrap_or(start + 1);
            let location = SourceLocation::new(path.to_path_buf(), start + 1, end);
            let symbol = lines[start..end]
                .iter()
                .enumerate()
                .find_map(|(offset, line)| {
                    defines_requested(
                        line,
                        group,
                        &code_lines,
                        start + offset,
                        relative,
                        &declarations,
                    )
                    .then(|| {
                        declaration_names(
                            line,
                            code_lines[start + offset],
                            relative,
                            start + offset + 1,
                            &declarations,
                        )
                        .into_iter()
                        .find(|name| name == group.symbol.as_deref().unwrap_or(name))
                    })
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
                declaration_owner: declarations
                    .iter()
                    .find(|item| {
                        item.line == cited && symbol.as_deref() == Some(item.name.as_str())
                    })
                    .and_then(|item| item.owner.clone()),
                evidence: SourceEvidence {
                    location,
                    target: group.label.clone(),
                    snippet: text,
                    symbol,
                    relevance: relevance_parts.join(" "),
                },
                score,
                order,
                cited,
                covers_terms: group.terms.len() > 1 && direct == group.terms.len(),
            };
            let disjoint = |current: &ScoredEvidence| {
                candidate.evidence.location.start_line > current.evidence.location.end_line
                    || candidate.evidence.location.end_line < current.evidence.location.start_line
            };
            if kept
                .iter()
                .any(|current| !disjoint(current) && current.score >= candidate.score)
            {
                continue;
            }
            let same_declaration = |current: &ScoredEvidence| {
                candidate.declaration_owner.is_some()
                    && candidate.declaration_owner == current.declaration_owner
                    && candidate.evidence.symbol == current.evidence.symbol
            };
            if kept
                .iter()
                .any(|current| same_declaration(current) && current.score >= candidate.score)
            {
                continue;
            }
            kept.retain(disjoint);
            kept.retain(|current| !same_declaration(current));
            kept.push(candidate);
            kept.sort_by(|left, right| {
                right
                    .score
                    .cmp(&left.score)
                    .then_with(|| left.order.cmp(&right.order))
            });
            kept.truncate(max_results);
        }
    }
    kept
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

fn paths_match(actual_path: &Path, relative: &Path, requested: &str) -> bool {
    let Some(root) = actual_path.ancestors().nth(relative.components().count()) else {
        return false;
    };
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };
    match fs::canonicalize(&candidate) {
        Ok(resolved) => resolved.strip_prefix(root).is_ok_and(|resolved_relative| {
            !resolved_relative.as_os_str().is_empty()
                && !excluded_path(resolved_relative)
                && resolved
                    == fs::canonicalize(actual_path).unwrap_or_else(|_| actual_path.to_path_buf())
        }),
        Err(_) => {
            let actual = relative.to_string_lossy().replace('\\', "/");
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

fn rust_identity_matches(left: &str, right: &str) -> bool {
    fn bare(value: &str) -> &str {
        value.strip_prefix("r#").unwrap_or(value)
    }
    bare(left) == bare(right)
}

fn defines_requested(
    line: &str,
    group: &QueryGroup,
    code_lines: &[&str],
    index: usize,
    relative: &Path,
    declarations: &[declaration_identity::Declaration],
) -> bool {
    let code_line = code_lines.get(index).copied().unwrap_or("");
    if !group.definition || code_line.trim().is_empty() {
        return false;
    }
    let Some(symbol) = &group.symbol else {
        return false;
    };
    let names = declaration_names(line, code_line, relative, index + 1, declarations);
    let declared = names.iter().any(|name| rust_identity_matches(name, symbol));
    if !declared {
        return false;
    }
    group.owner.as_ref().is_none_or(|owner| {
        declarations.iter().any(|declaration| {
            declaration.line == index + 1
                && rust_identity_matches(&declaration.name, symbol)
                && declaration
                    .owner
                    .as_ref()
                    .is_some_and(|found| found.matches(owner))
        })
    })
}

fn declaration_names(
    line: &str,
    code_line: &str,
    relative: &Path,
    line_number: usize,
    declarations: &[declaration_identity::Declaration],
) -> Vec<String> {
    let rust = relative
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("rs");
    if rust {
        let structural: Vec<String> = declarations
            .iter()
            .filter(|declaration| declaration.line == line_number)
            .map(|declaration| declaration.name.clone())
            .collect();
        if !structural.is_empty() {
            return structural;
        }
        if !production_source(line) || code_line.trim().is_empty() {
            return Vec::new();
        }
        return rust_declaration_name(code_line).into_iter().collect();
    }
    foreign_declaration_name(code_line).into_iter().collect()
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
    fn repeated_locations_scan_each_verified_file_once() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/lib.rs");
        let locations = (1..=8)
            .map(|line| format!("File: {}, Lines: {line}-{line}\n", path.display()))
            .collect::<String>();
        WINDOW_SCANS.with(|scans| scans.set(0));
        let report = verify_probe_evidence(
            &locations,
            &fixture.root,
            "compression publication cache assembly",
            8,
        )
        .expect("verified evidence");
        assert!(!report.evidence().is_empty());
        WINDOW_SCANS.with(|scans| assert_eq!(scans.get(), 1));
    }

    #[test]
    fn unrelated_windows_skip_feature_scoring_when_no_feature_is_requested() {
        let source = format!(
            "{}\nfn persistent_outage_hold() {{}}\n",
            "let unrelated = 42;\n".repeat(1000)
        );
        let lines = source.lines().collect::<Vec<_>>();
        let groups = query_groups("Why does persistent_outage_hold stop with attempts left?")
            .expect("groups");
        assert!(requested_features(&groups[0]).is_empty());
        assert!(!groups[0].definition, "definition");
        assert_eq!(groups[0].exact_symbols.len(), 1);
        FEATURE_SCANS.with(|scans| scans.set(0));
        let evidence = best_windows(
            &groups[0],
            &groups,
            Path::new("src/proxy.rs"),
            Path::new("src/proxy.rs"),
            &lines,
            0,
            8,
        );
        assert!(!evidence.is_empty());
        FEATURE_SCANS.with(|scans| assert!(scans.get() < 100));
    }

    #[test]
    fn qualified_call_is_verified_without_an_unqualified_call() {
        let fixture = Fixture::new();
        let caller = fixture.root.join("src/proxy.rs");
        fs::write(&caller, "fn attempt() { outage_hold::drain_attempts(); }\n").expect("source");
        let report = verify_probe_evidence(
            &probe_file(&caller),
            &fixture.root,
            "outage_hold::drain_attempts",
            8,
        )
        .expect("verified qualified call");
        assert!(report
            .evidence()
            .iter()
            .any(|item| { item.snippet().contains("outage_hold::drain_attempts()") }));
    }

    #[test]
    fn following_calls_parse_field_types_once_per_source() {
        let fixture = Fixture::new();
        let caller = fixture.root.join("src/proxy.rs");
        fs::write(
            &caller,
            "fn attempt() { outage_hold::drain_attempts(); holder::second(); holder::third(); }\n",
        )
        .expect("source");
        let report = verify_probe_evidence(
            &probe_file(&caller),
            &fixture.root,
            "outage_hold::drain_attempts",
            4,
        )
        .expect("verified call");
        declaration_identity::FIELD_TYPE_SCANS.with(|scans| scans.set(0));
        report
            .with_following_lines(&fixture.root, 8)
            .expect("bounded following");
        declaration_identity::FIELD_TYPE_SCANS.with(|scans| assert_eq!(scans.get(), 1));
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
        assert_eq!(report.evidence().len(), 2, "{:?}", report.evidence());
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

    fn probe_file(path: &Path) -> String {
        format!("File: {}, Lines: 1-99\n", path.display())
    }

    #[test]
    fn same_file_and_windows_keep_each_required_snippet() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/lib.rs");
        let mut source = String::from("fn alpha_function() {}\n");
        source.push_str(&"\n".repeat(12));
        source.push_str("fn beta_function() {}\n");
        fs::write(&path, source).expect("distant declarations");
        let output = probe_file(&path);
        for (query, limit, complete, snippets) in [
            (
                "alpha_function AND beta_function",
                1_usize,
                false,
                vec!["alpha_function"],
            ),
            (
                "alpha_function AND beta_function",
                8,
                true,
                vec!["alpha_function", "beta_function"],
            ),
            (
                "beta_function AND alpha_function",
                1,
                false,
                vec!["beta_function"],
            ),
            (
                "beta_function AND alpha_function",
                8,
                true,
                vec!["beta_function", "alpha_function"],
            ),
        ] {
            let report = verify_probe_evidence(&output, &fixture.root, query, limit)
                .expect("same-file windows");
            assert_eq!(report.is_complete(), complete, "{query} limit={limit}");
            assert_eq!(
                report.evidence().len(),
                snippets.len(),
                "{query} limit={limit}"
            );
            for (item, snippet) in report.evidence().iter().zip(snippets) {
                assert!(
                    item.snippet().contains(snippet) && item.target().contains(snippet),
                    "{query} limit={limit} kept {} for {snippet}",
                    item.snippet()
                );
            }
        }
    }

    #[test]
    fn longer_same_file_window_keeps_the_prior_proof() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/lib.rs");
        let mut source = String::from("fn alpha_function() {}\n");
        source.push_str(&"\n".repeat(3));
        source.push_str("fn beta_function() {}\nfn beta_function() {}\n");
        fs::write(&path, source).expect("two proofs");
        let output = probe_file(&path);
        let report = verify_probe_evidence(
            &output,
            &fixture.root,
            "alpha_function AND beta_function",
            8,
        )
        .expect("retained proofs");
        let alpha = report
            .evidence()
            .iter()
            .find(|item| item.target().contains("alpha_function"));
        let beta = report
            .evidence()
            .iter()
            .find(|item| item.target().contains("beta_function"));
        assert!(
            alpha.is_some_and(|item| item.snippet().contains("fn alpha_function()")),
            "alpha={:?}",
            alpha.map(|item| item.snippet())
        );
        assert!(
            beta.is_some_and(|item| item.snippet().contains("fn beta_function()")),
            "beta={:?}",
            beta.map(|item| item.snippet())
        );
    }

    #[test]
    fn necessary_groups_take_a_representative_before_alternatives() {
        let fixture = Fixture::new();
        let mut output = String::new();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let path = fixture.root.join("src").join(name);
            fs::write(&path, "fn alpha_function() {}\nfn beta_function() {}\n").expect(name);
            output.push_str(&probe_file(&path));
        }
        for (query, expected) in [
            (
                "alpha_function AND beta_function",
                ["alpha_function", "beta_function"],
            ),
            (
                "beta_function AND alpha_function",
                ["beta_function", "alpha_function"],
            ),
        ] {
            let report =
                verify_probe_evidence(&output, &fixture.root, query, 2).expect("two groups");
            assert!(report.is_complete(), "{query}");
            assert_eq!(report.evidence().len(), 2, "{query}");
            for (item, target) in report.evidence().iter().zip(expected) {
                assert!(
                    item.target().contains(target) && item.snippet().contains(target),
                    "{query} kept {} / {}",
                    item.target(),
                    item.snippet()
                );
            }
        }
    }

    #[test]
    fn one_group_two_files_keeps_legacy_file_compact() {
        let fixture = Fixture::new();
        let mut output = String::new();
        for name in ["a.rs", "b.rs"] {
            let path = fixture.root.join("src").join(name);
            fs::write(&path, "fn alpha_function() {}\n").expect(name);
            output.push_str(&probe_file(&path));
        }
        let report =
            verify_probe_evidence(&output, &fixture.root, "alpha_function", 8).expect("one group");
        assert!(report.is_complete());
        assert_eq!(report.evidence().len(), 2);
        assert!(report
            .evidence()
            .iter()
            .all(|item| item.snippet().contains("alpha_function")));
    }

    #[test]
    fn semantic_following_lines_include_the_guard_after_a_signature() {
        let fixture = Fixture::new();
        let source = fixture.root.join("src/lib.rs");
        fs::write(
            &source,
            "fn display_relative() {\n    let relative = source\n        .strip_prefix(root)\n        .unwrap();\n    // Validate the relative path.\n    // Keep citations inside this function.\n    // The guard follows this setup.\n    if relative.as_os_str().is_empty() {\n        return Err(SourceOutsideRoot);\n    }\n}\n",
        )
        .expect("source");
        let report = verify_probe_evidence(
            &format!("File: {}, Lines: 1-1\n", source.display()),
            &fixture.root,
            "where is display_relative?",
            8,
        )
        .expect("verified signature");
        let report = report
            .with_following_lines(&fixture.root, 8)
            .expect("bounded adjacent source");
        assert!(report.evidence().iter().any(|evidence| {
            evidence.location().start_line() == 1 && evidence.location().end_line() >= 9
        }));
        assert!(report.evidence().iter().any(|evidence| {
            evidence.location().start_line() <= 8
                && evidence.location().end_line() >= 9
                && evidence.snippet().contains("SourceOutsideRoot")
        }));
    }

    #[test]
    fn why_question_keeps_the_called_stop_not_only_the_predicate() {
        let fixture = Fixture::new();
        let hold = fixture.root.join("src/hold.rs");
        let caller = fixture.root.join("src/caller.rs");
        let noise = fixture.root.join("src/other.rs");
        fs::write(
            &hold,
            "fn persistent_outage_hold_left(started: bool) -> u64 {\n    \
             if started { 0 } else { 9 }\n}\n\n\
             /// Stop with attempts left when the hold budget is gone.\n\
             async fn persistent_outage_hold_wait(started: bool) -> bool {\n    \
             let left = persistent_outage_hold_left(started);\n    \
             // The hold shares one wall-clock cap with selection.\n    \
             // A closed shutdown ends the wait immediately.\n    \
             // The cadence stays inside the remaining budget.\n    \
             // Cancellation is checked before the sleep returns.\n    \
             // A dropped downstream signal also ends the hold.\n    \
             // A committed downstream signal also ends the hold.\n    \
             // The ordinary retry ladder is not used after this.\n    \
             // Probe count increases only when the wait completes.\n    \
             let expired = left == 0;\n    \
             if expired {\n        return false;\n    }\n    \
             left > 0\n}\n\n\
             fn persistent_outage_hold_active(started: bool) -> bool {\n    \
             // A started hold stays on after its budget ends.\n    \
             started || persistent_outage_hold_left(started) > 0\n}\n\n\
             pub async fn persistent_outage_hold_continue(started: bool) -> bool {\n    \
             persistent_outage_hold_wait(started).await\n}\n",
        )
        .expect("hold");
        fs::write(
            &caller,
            "fn step(started: bool) {\n    \
             if persistent_outage_hold_active(started) {\n        \
             let again = persistent_outage_hold_continue(started);\n        \
             let _ = again;\n    }\n}\n",
        )
        .expect("caller");
        fs::write(
            &noise,
            "/// Why persistent_outage_hold stop with attempts left.\n\
             fn note() {\n    assert!(true);\n}\n",
        )
        .expect("noise");
        let mut located = String::new();
        for path in [&hold, &caller, &noise] {
            located.push_str(&probe_file(path));
        }
        let query = "Why does persistent_outage_hold stop with attempts left?";
        let report = verify_probe_evidence(&located, &fixture.root, query, 8)
            .expect("ranked windows")
            .with_following_lines(&fixture.root, 8)
            .expect("existing why extension");
        let snippets = report
            .evidence()
            .iter()
            .map(|item| item.snippet())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            snippets.contains("return false"),
            "stop decision missing from {:?}",
            report
                .evidence()
                .iter()
                .map(|item| (item.location().start_line(), item.location().end_line()))
                .collect::<Vec<_>>()
        );
        assert!(
            !snippets.contains("persistent_outage_hold_active"),
            "next function admitted: {snippets}"
        );
    }

    #[test]
    fn planned_predicate_keeps_two_disjoint_call_sites() {
        let fixture = Fixture::new();
        let hold = fixture.root.join("src/hold.rs");
        let caller = fixture.root.join("src/caller.rs");
        let noise = fixture.root.join("src/other.rs");
        fs::write(
            &hold,
            "fn persistent_outage_hold_left(started: bool) -> u64 {\n    \
             if started { 0 } else { 9 }\n}\n\n\
             /// Stop with attempts left when the hold budget is gone.\n\
             async fn persistent_outage_hold_wait(started: bool) -> bool {\n    \
             let left = persistent_outage_hold_left(started);\n    \
             // The hold shares one wall-clock cap with selection.\n    \
             // A closed shutdown ends the wait immediately.\n    \
             // The cadence stays inside the remaining budget.\n    \
             // Cancellation is checked before the sleep returns.\n    \
             // A dropped downstream signal also ends the hold.\n    \
             // A committed downstream signal also ends the hold.\n    \
             // The ordinary retry ladder is not used after this.\n    \
             // Probe count increases only when the wait completes.\n    \
             let expired = left == 0;\n    \
             if expired {\n        return false;\n    }\n    \
             left > 0\n}\n\n\
             fn persistent_outage_hold_active(started: bool) -> bool {\n    \
             // A started hold stays on after its budget ends.\n    \
             started || persistent_outage_hold_left(started) > 0\n}\n\n\
             pub async fn persistent_outage_hold_continue(started: bool) -> bool {\n    \
             persistent_outage_hold_wait(started).await\n}\n\n\
             fn is_outage(started: bool) -> bool {\n    started\n}\n",
        )
        .expect("hold");
        fs::write(
            &caller,
            "fn step_start(started: bool) {\n    \
             if outage_hold::is_outage(started) {\n        \
             let again = outage_hold::wait(started);\n        \
             let _ = again;\n    }\n}\n\n\
             fn distractor(started: bool) {\n    \
             let note = persistent_outage_hold_left(started);\n    \
             let _ = note;\n}\n\n\
             fn step_status(started: bool) {\n    \
             if outage_hold::is_outage(started) {\n        \
             let again = outage_hold::wait(started);\n        \
             let _ = again;\n    }\n}\n",
        )
        .expect("caller");
        fs::write(
            &noise,
            "/// Why persistent_outage_hold stop with attempts left.\n\
             fn note() {\n    assert!(true);\n}\n",
        )
        .expect("noise");
        let mut located = String::new();
        for path in [&hold, &caller, &noise] {
            located.push_str(&probe_file(path));
        }
        let query = "is_outage";
        let report = verify_probe_evidence(&located, &fixture.root, query, 8)
            .expect("ranked windows")
            .with_following_lines(&fixture.root, 8)
            .expect("existing why extension");
        let stops = report
            .evidence()
            .iter()
            .filter(|item| {
                item.location().path().ends_with("src/caller.rs")
                    && item.snippet().contains("outage_hold::wait")
            })
            .count();
        assert!(
            stops >= 2,
            "second same-file stop missing from {:?}",
            report
                .evidence()
                .iter()
                .map(|item| (
                    item.location().path().display().to_string(),
                    item.location().start_line(),
                    item.location().end_line(),
                ))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn disjoint_read_survives_a_later_unrelated_push() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/flow.rs");
        fs::write(
            &path,
            "fn decision() -> bool {\n    let remaining = budget.remaining();\n    remaining > 0\n}\n\nfn record() {\n    let remaining = 7;\n    notes.push(decision(remaining));\n}\n",
        )
        .expect("source");
        let report =
            verify_probe_evidence(&probe_file(&path), &fixture.root, "decision remaining", 8)
                .expect("verified evidence");
        assert!(
            report.evidence().iter().any(|item| item
                .snippet()
                .contains("let remaining = budget.remaining()")),
            "read lost: {:?}",
            report.evidence()
        );
    }

    #[test]
    fn disjoint_bare_calls_remain_bounded() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/flow.rs");
        fs::write(
            &path,
            "fn first() {\n    let retry = wait();\n}\n\nfn second() {\n    let retry = wait();\n}\n\nfn third() {\n    let retry = wait();\n}\n",
        )
        .expect("source");
        let output = probe_file(&path);
        let report = verify_probe_evidence(&output, &fixture.root, "wait retry", 8)
            .expect("verified evidence");
        assert!(
            report
                .evidence()
                .iter()
                .any(|item| item.location().start_line() <= 2)
                && report
                    .evidence()
                    .iter()
                    .any(|item| item.location().start_line() >= 5),
            "bare calls collapsed: {:?}",
            report.evidence()
        );
        assert_eq!(
            verify_probe_evidence(&output, &fixture.root, "wait retry", 2)
                .expect("capped evidence")
                .evidence()
                .len(),
            2
        );
    }

    #[test]
    fn path_fragment_is_not_a_compound_call() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/other.rs");
        fs::write(&path, "fn unrelated() {\n    other::hold();\n}\n").expect("source");
        assert_eq!(
            verify_probe_evidence(&probe_file(&path), &fixture.root, "retry_hold", 8),
            Err(EvidenceError::NoSourceLocations)
        );
    }

    #[test]
    fn same_named_methods_of_distinct_owners_remain_distinct() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/owners.rs");
        fs::write(
            &path,
            "struct First;\nstruct Second;\nimpl First {\n    fn wait_step() {}\n}\nimpl Second {\n    fn wait_step() {}\n}\n",
        )
        .expect("source");
        let report = verify_probe_evidence(&probe_file(&path), &fixture.root, "wait_step", 8)
            .expect("verified methods");
        assert!(
            report
                .evidence()
                .iter()
                .any(|item| item.location().start_line() <= 4)
                && report
                    .evidence()
                    .iter()
                    .any(|item| item.location().start_line() >= 7),
            "method owners collapsed: {:?}",
            report.evidence()
        );
    }

    #[test]
    fn why_evidence_follows_wait_to_the_hold_budget_decision() {
        let fixture = Fixture::new();
        let hold = fixture.root.join("src/hold.rs");
        let caller = fixture.root.join("src/proxy.rs");
        fs::write(
            &hold,
            "struct Hold;\nimpl Hold {\n    fn remaining(&self, config: &Config) -> Duration {\n        Duration::from_millis(config.outage_hold_ms).saturating_sub(self.started.elapsed())\n    }\n\n    async fn wait(&self, config: &Config) -> bool {\n        let remaining = self.remaining(config);\n        if remaining.is_zero() {\n            return false;\n        }\n        sleep(remaining).await;\n        !self.remaining(config).is_zero()\n    }\n}\n\nfn is_outage(runtime: &Runtime) -> bool {\n    runtime.hold.remaining(&runtime.config) > Duration::ZERO\n}\n\nasync fn wait(runtime: &Runtime) -> bool {\n    runtime.hold\n        .wait(\n            &runtime.config,\n        )\n        .await\n}\n",
        )
        .expect("hold source");
        fs::write(
            &caller,
            "struct Config;\nstruct Runtime { hold: hold::Hold, config: Config }\nasync fn attempt(runtime: &Runtime) -> bool {\n    let mut can_retry = true;\n    if hold::is_outage(runtime) {\n        log_failure();\n        can_retry = hold::wait(runtime).await;\n        next_step();\n        record();\n    }\n    can_retry\n}\nfn log_failure() {}\nfn next_step() {}\nfn record() {}\nasync fn later(runtime: &Runtime) -> bool {\n    if hold::is_outage(runtime) {\n        return hold::wait(runtime).await;\n    }\n    false\n}\n",
        )
        .expect("caller source");
        let located = format!("{}{}", probe_file(&hold), probe_file(&caller));
        let report = verify_probe_evidence(&located, &fixture.root, "is_outage", 8)
            .expect("verified seed")
            .with_following_lines(&fixture.root, 8)
            .expect("bounded causal evidence");
        let text = report
            .evidence()
            .iter()
            .map(SourceEvidence::snippet)
            .collect::<Vec<_>>()
            .join("\n");
        for required in [
            "can_retry = hold::wait(runtime).await",
            "runtime.hold\n        .wait(",
            "if remaining.is_zero()",
            "return false",
            "config.outage_hold_ms).saturating_sub",
        ] {
            assert!(text.contains(required), "missing {required}: {text}");
        }
        assert!(report.evidence().len() <= 8);
        let stop = report
            .evidence()
            .iter()
            .position(|item| item.snippet().contains("return false"))
            .expect("stop branch");
        let selected = report.causal_indices(stop);
        let selected_text = selected
            .iter()
            .map(|index| report.evidence()[*index].snippet())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            selected_text.contains("can_retry = hold::wait(runtime).await"),
            "assignment use left the stop path: {selected_text}"
        );
        assert!(
            !selected_text.contains("fn later"),
            "an independent caller stayed on the stop path: {selected_text}"
        );
    }

    #[test]
    fn child_module_method_call_keeps_the_assignment_on_the_stop_path() {
        let fixture = Fixture::new();
        let caller = fixture.root.join("src/proxy.rs");
        let module = fixture.root.join("src/proxy");
        fs::create_dir(&module).expect("module dir");
        fs::write(
            &caller,
            "mod outage_hold;\nstruct Runtime { hold: outage_hold::Hold }\nasync fn attempt(runtime: &Runtime) -> bool {\n    let mut can_retry = true;\n    if outage_hold::is_outage(runtime) {\n        can_retry = outage_hold::wait(runtime).await;\n    }\n    can_retry\n}\n",
        )
        .expect("caller");
        fs::write(
            module.join("outage_hold.rs"),
            "struct Hold;\nimpl Hold {\n    fn remaining(&self) -> u64 { 0 }\n    async fn wait(&self) -> bool {\n        if self.remaining() == 0 { return false; }\n        true\n    }\n}\npub(super) async fn wait(runtime: &super::Runtime) -> bool {\n    runtime.hold.wait().await\n}\npub(super) fn is_outage(runtime: &super::Runtime) -> bool { true }\n",
        )
        .expect("module");
        let report = verify_probe_evidence(&probe_file(&caller), &fixture.root, "outage_hold", 8)
            .expect("caller seed")
            .with_following_lines(&fixture.root, 8)
            .expect("module follow");
        let text = report
            .evidence()
            .iter()
            .map(SourceEvidence::snippet)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("return false"),
            "child module stop was not followed: {text}"
        );
        let stop = report
            .evidence()
            .iter()
            .position(|item| item.snippet().contains("return false"))
            .expect("stop");
        let selected = report
            .causal_indices(stop)
            .iter()
            .map(|index| report.evidence()[*index].snippet())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            selected.contains("can_retry = outage_hold::wait(runtime).await"),
            "assignment use left the module stop path: {selected}"
        );
    }

    #[test]
    fn typed_field_call_precedes_unrelated_and_bare_calls() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/flow.rs");
        fs::write(
            &path,
            "struct Runtime { deadline: Deadline, hold: Holder }\n\
             struct Holder;\nstruct Decoy;\nstruct Deadline;\n\
             fn caller(runtime: &Runtime) -> bool {\n\
                 runtime.deadline.pause();\n\
                 log();\n\
                 let retry = runtime.hold.wait();\n\
                 wait();\n\
                 retry\n}\n\
             impl Deadline { fn pause(&self) {} }\n\
             impl Holder {\n\
                 fn wait(&self) -> bool {\n\
                     let left = budget();\n\
                     left > 0\n    }\n}\n\
             impl Decoy { fn wait(&self) -> bool { false } }\n\
             fn wait() -> bool { false }\n\
             fn log() {}\n\
             fn budget() -> u64 { 1 }\n",
        )
        .expect("source");
        let report = EvidenceReport {
            complete: true,
            evidence: vec![SourceEvidence {
                location: SourceLocation::new(path, 5, 11),
                target: "caller".to_owned(),
                snippet: String::new(),
                symbol: Some("caller".to_owned()),
                relevance: String::new(),
            }],
            missing_targets: Vec::new(),
            cited: vec![5],
            followed_from: vec![None],
            call_edges: Vec::new(),
        }
        .with_following_lines(&fixture.root, 3)
        .expect("bounded calls");
        let text = report
            .evidence()
            .iter()
            .map(SourceEvidence::snippet)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("let left = budget()"), "{text}");
        assert!(text.contains("fn budget()"), "{text}");
        assert!(!text.contains("fn pause(&self)"), "{text}");
        assert!(!text.contains("fn log()"), "{text}");
        assert!(!text.contains("fn wait() -> bool"), "{text}");
        assert!(!text.contains("impl Decoy"), "{text}");
        assert_eq!(report.evidence().len(), 3);
    }

    #[test]
    fn unproven_receiver_does_not_admit_a_same_named_impl() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/flow.rs");
        fs::write(
            &path,
            "use external::External;\n\
             fn caller(value: &dyn External) { value.wait(); }\n\
             struct Decoy;\n\
             impl Decoy { fn wait(&self) {} }\n",
        )
        .expect("source");
        let report = EvidenceReport {
            complete: true,
            evidence: vec![SourceEvidence {
                location: SourceLocation::new(path, 2, 2),
                target: "caller".to_owned(),
                snippet: String::new(),
                symbol: Some("caller".to_owned()),
                relevance: String::new(),
            }],
            missing_targets: Vec::new(),
            cited: vec![2],
            followed_from: vec![None],
            call_edges: Vec::new(),
        }
        .with_following_lines(&fixture.root, 2)
        .expect("bounded calls");
        assert_eq!(report.evidence().len(), 1, "{:?}", report.evidence());
    }

    #[test]
    fn causal_path_excludes_an_unrelated_predicate_sharing_a_callee() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/flow.rs");
        let source = "fn predicate() -> bool { remaining() > 0 }\n\n\n\n\n\n\n\n\
                      fn caller() -> bool { wait() }\n\n\n\n\n\n\n\n\
                      fn wait() -> bool { if remaining() == 0 { return false; } true }\n\n\n\n\n\n\n\n\
                      fn remaining() -> u64 { 0 }\n";
        fs::write(&path, source).expect("source");
        let evidence = [1, 9]
            .into_iter()
            .map(|line| SourceEvidence {
                location: SourceLocation::new(path.clone(), line, line),
                target: "hold".to_owned(),
                snippet: String::new(),
                symbol: None,
                relevance: String::new(),
            })
            .collect();
        let report = EvidenceReport {
            complete: true,
            evidence,
            missing_targets: Vec::new(),
            cited: vec![1, 9],
            followed_from: vec![None, None],
            call_edges: Vec::new(),
        }
        .with_following_lines(&fixture.root, 5)
        .expect("bounded call graph");
        let stop = report
            .evidence()
            .iter()
            .position(|item| item.snippet().contains("return false"))
            .expect("stop branch");
        let selected = report.causal_indices(stop);
        assert_eq!(selected.len(), 3, "{selected:?}");
        assert!(selected.contains(&1), "caller remains on the path");
        assert!(!selected.contains(&0), "predicate is a sibling path");
        assert!(selected
            .iter()
            .any(|index| report.evidence()[*index].snippet().contains("fn remaining")));
    }

    #[test]
    fn why_stop_admits_the_callers_other_budget_without_unrelated_calls() {
        let fixture = Fixture::new();
        let path = fixture.root.join("src/budgets.rs");
        let source = "fn attempt(used: u32) -> bool {\n    let mut retry = attempt_budget(used);\n    if is_outage() {\n        retry = wait();\n    }\n    if retry { return true; }\n    audit();\n    false\n}\nfn attempt_budget(used: u32) -> bool { used < 3 }\nfn is_outage() -> bool { true }\nfn wait() -> bool { if remaining() == 0 { return false; } true }\nfn remaining() -> u64 { 0 }\nfn audit() {}\nfn noise_budget() -> u64 { 99 }\n";
        fs::write(&path, source).expect("source");
        let report = EvidenceReport {
            complete: true,
            evidence: vec![SourceEvidence {
                location: SourceLocation::new(path, 3, 4),
                target: "wait".to_owned(),
                snippet: String::new(),
                symbol: Some("attempt".to_owned()),
                relevance: String::new(),
            }],
            missing_targets: Vec::new(),
            cited: vec![3],
            followed_from: vec![None],
            call_edges: Vec::new(),
        }
        .with_following_lines(&fixture.root, 6)
        .expect("bounded evidence");
        let stop = report
            .evidence()
            .iter()
            .position(|item| item.snippet().contains("return false"))
            .expect("stop branch");
        let selected = report.causal_indices(stop);
        let selected_text = selected
            .iter()
            .map(|index| report.evidence()[*index].snippet())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(selected_text.contains("used < 3"), "{selected_text}");
        assert!(
            selected_text.contains("remaining() -> u64"),
            "{selected_text}"
        );
        assert!(!selected_text.contains("noise_budget"), "{selected_text}");
        assert!(report.evidence().len() <= 6);
    }
}
