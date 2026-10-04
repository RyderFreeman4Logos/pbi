//! Bounded in-process repository search.
//!
//! No Probe binary, probe-chat, or search subprocess. Hits are ordinary
//! `File:` ranges so the existing verifier remains the citation boundary.
//!
//! Verified locations use one lexical pass; raw results rank with BM25 over
//! the same bounded walk, with exact Rust declarations before lexical mentions.
//! Neither path creates an index or a model request.

use crate::extract::{check_source_namespace, open_source, policy_admitted};
use crate::strict_query::StrictQuery;
use ignore::{gitignore::GitignoreBuilder, WalkBuilder};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use syn::visit::{self, Visit};

const EXCLUDED: [&str; 5] = [".git", "target", "drafts", "node_modules", "__pycache__"];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WALK_FILES: usize = 20_000;
const MAX_ROOT_TARGETS: usize = 16;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_PLAN_FILES: usize = 1;
const MAX_PARSED_FUNCTIONS: usize = 512;
const MAX_PLAN_SYMBOLS: usize = 8;
const MAX_RAW_BLOCKS: usize = 4096;
const MAX_RAW_BLOCKS_PER_FILE: usize = 128;

pub struct SearchLimits {
    pub deadline: Instant,
    pub max_results: usize,
    pub language: Option<String>,
    pub ignores: Vec<String>,
}

#[derive(Debug)]
pub enum SearchFailure {
    Deadline,
    Limit,
    TargetLimit,
    Unavailable,
}

/// One native BM25 result. Paths are repository relative and snippets come
/// from a checked, bounded source file; these are search hits, not citations.
pub struct RawHit {
    pub file: String,
    pub line: Option<usize>,
    pub end_line: Option<usize>,
    pub snippet: String,
    pub score: f64,
    pub occurrences: usize,
    declaration: bool,
}

/// Raw search shares the walk and file limits with verified search.
pub struct RawSearchOptions<'a> {
    pub exact: bool,
    pub exclude_filenames: bool,
    pub merge_threshold: usize,
    pub strict: Option<&'a StrictQuery>,
}

struct RawCandidate {
    file: String,
    line: Option<usize>,
    end_line: Option<usize>,
    snippet: String,
    term_counts: Vec<usize>,
    length: usize,
    occurrences: usize,
    declaration: bool,
}

struct Hit {
    path: PathBuf,
    line: usize,
    end: usize,
    score: i32,
}

pub fn search_repository(
    root: &Path,
    query: &str,
    limits: &SearchLimits,
) -> Result<String, SearchFailure> {
    let terms = query_terms(query);
    if terms.is_empty() || limits.max_results == 0 {
        return Ok(String::new());
    }
    let root_file = open_root(root)?;
    let root_meta = root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?;
    let files = walk_owned(root, &root_file, root_meta.dev(), limits)?;
    let mut hits = Vec::new();
    for path in files {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        if !language_matches(&path, limits.language.as_deref()) {
            continue;
        }
        let Some(bytes) = read_source(root, &root_file, &path, root_meta.dev(), limits)? else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some(mut hit) = score_file(&path, text, &terms) {
            let count = text.lines().count();
            hit.end = if count <= 4 {
                count.max(hit.line)
            } else {
                hit.line
            };
            hits.push(hit);
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
    });
    hits.truncate(limits.max_results);
    let mut output = String::new();
    for hit in hits {
        let line = format!(
            "File: {}, Lines: {}-{}\n",
            hit.path.display(),
            hit.line,
            hit.end
        );
        if output.len() + line.len() > MAX_OUTPUT_BYTES {
            return Err(SearchFailure::Limit);
        }
        output.push_str(&line);
    }
    Ok(output)
}

/// Rank bounded repository files with BM25, retaining separate source blocks.
/// The caller applies its page limit after block merging and optional dedup.
pub fn search_raw_repository(
    root: &Path,
    query: &str,
    limits: &SearchLimits,
    options: &RawSearchOptions<'_>,
) -> Result<(Vec<RawHit>, u64), SearchFailure> {
    let terms = options
        .strict
        .map(|strict| strict.positive_terms().to_vec())
        .unwrap_or_else(|| raw_terms(query));
    if terms.is_empty() || terms.len() > 32 {
        return Err(SearchFailure::Limit);
    }
    let root_file = open_root(root)?;
    let root_meta = root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?;
    let mut files = walk_owned(root, &root_file, root_meta.dev(), limits)?;
    files.sort();
    let mut freshness = DefaultHasher::new();
    let mut documents = 0usize;
    let mut total_length = 0usize;
    let mut document_frequency = vec![0usize; terms.len()];
    let mut candidates = Vec::new();
    let exact_phrase = query.trim().to_lowercase();
    for path in files {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        if !language_matches(&path, limits.language.as_deref()) {
            continue;
        }
        let Some(bytes) = read_source(root, &root_file, &path, root_meta.dev(), limits)? else {
            continue;
        };
        path.hash(&mut freshness);
        bytes.hash(&mut freshness);
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let filename = path.file_name().and_then(|name| name.to_str());
        let strict_match = options.strict.is_none_or(|strict| {
            strict.matches(
                source,
                (!options.exclude_filenames).then_some(filename).flatten(),
            )
        });
        let Some(relative) = path.strip_prefix(root).ok().and_then(Path::to_str) else {
            continue;
        };
        documents += 1;
        let mut counts = vec![0usize; terms.len()];
        let mut length = 0usize;
        let source_lines = source.lines().collect::<Vec<_>>();
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for (index, line) in source_lines.iter().enumerate() {
            if Instant::now() >= limits.deadline {
                return Err(SearchFailure::Deadline);
            }
            let normalized = line.to_lowercase();
            let mut line_matches = 0usize;
            for word in normalized
                .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .filter(|word| !word.is_empty())
            {
                length += 1;
                for (term_index, term) in terms.iter().enumerate() {
                    if word == term {
                        counts[term_index] += 1;
                        line_matches += 1;
                    }
                }
            }
            if (options.exact && normalized.contains(&exact_phrase))
                || (!options.exact && line_matches > 0)
            {
                let line_number = index + 1;
                if let Some(last) = blocks.last_mut() {
                    if line_number.saturating_sub(last.1 + 1) <= options.merge_threshold {
                        last.1 = line_number;
                        continue;
                    }
                }
                if blocks.len() >= MAX_RAW_BLOCKS_PER_FILE {
                    return Err(SearchFailure::Limit);
                }
                blocks.push((line_number, line_number));
            }
        }
        if !options.exclude_filenames {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                for word in name
                    .to_lowercase()
                    .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                {
                    length += usize::from(!word.is_empty());
                    for (term_index, term) in terms.iter().enumerate() {
                        if word == term {
                            counts[term_index] += 1;
                        }
                    }
                }
            }
        }
        total_length += length;
        for (df, count) in document_frequency.iter_mut().zip(&counts) {
            *df += usize::from(*count > 0);
        }
        if counts.iter().all(|count| *count == 0)
            || (options.exact && !source.to_lowercase().contains(&exact_phrase))
            || !strict_match
        {
            continue;
        }
        if blocks.is_empty() {
            blocks.push((0, 0));
        }
        let declarations = if path.extension().is_some_and(|extension| extension == "rs") {
            pbi_rs::matching_rust_declaration_lines(source, &terms)
        } else {
            Vec::new()
        };
        for (start, end) in blocks {
            if candidates.len() >= MAX_RAW_BLOCKS {
                return Err(SearchFailure::Limit);
            }
            let snippet = if start == 0 {
                String::new()
            } else {
                source_lines[start - 1..end]
                    .join("\n")
                    .chars()
                    .take(512)
                    .collect()
            };
            candidates.push(RawCandidate {
                file: relative.to_owned(),
                line: (start != 0).then_some(start),
                end_line: (start != 0).then_some(end),
                snippet,
                occurrences: counts.iter().sum(),
                term_counts: counts.clone(),
                length,
                declaration: declarations.iter().any(|line| (start..=end).contains(line)),
            });
        }
    }
    if documents == 0 {
        return Ok((Vec::new(), freshness.finish()));
    }
    let average_length = (total_length as f64 / documents as f64).max(1.0);
    let mut hits = candidates
        .into_iter()
        .map(|candidate| {
            let score = candidate
                .term_counts
                .iter()
                .zip(&document_frequency)
                .map(|(count, frequency)| {
                    let tf = *count as f64;
                    let idf = ((documents as f64 - *frequency as f64 + 0.5)
                        / (*frequency as f64 + 0.5)
                        + 1.0)
                        .ln();
                    let normalization =
                        1.2 * (0.25 + 0.75 * candidate.length as f64 / average_length);
                    idf * tf * 2.2 / (tf + normalization)
                })
                .sum();
            RawHit {
                file: candidate.file,
                line: candidate.line,
                end_line: candidate.end_line,
                snippet: candidate.snippet,
                score,
                occurrences: candidate.occurrences,
                declaration: candidate.declaration,
            }
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .declaration
            .cmp(&left.declaration)
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
    });
    Ok((hits, freshness.finish()))
}

fn raw_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for term in query
        .to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|term| !term.is_empty())
    {
        if !terms.iter().any(|known| known == term) {
            terms.push(term.to_owned());
        }
    }
    terms
}

fn open_root(root: &Path) -> Result<File, SearchFailure> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(root)
        .map_err(|_| SearchFailure::Unavailable)
}

fn read_source(
    root: &Path,
    root_file: &File,
    path: &Path,
    device: u64,
    limits: &SearchLimits,
) -> Result<Option<Vec<u8>>, SearchFailure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SearchFailure::Unavailable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.dev() != device
        || metadata.len() > MAX_FILE_BYTES
    {
        return Ok(None);
    }
    #[cfg(test)]
    issue_327_tests::after_metadata();
    let relative = path
        .strip_prefix(root)
        .map_err(|_| SearchFailure::Unavailable)?;
    let (file, directories) = open_source(
        root_file
            .try_clone()
            .map_err(|_| SearchFailure::Unavailable)?,
        relative,
        device,
        limits,
        false,
    )?;
    check_source_namespace(root, relative, &directories, &file)?;
    if !policy_admitted(root, relative, &directories, device, limits, false)? {
        return Err(SearchFailure::Unavailable);
    }
    let bytes = read_source_file(
        file.try_clone().map_err(|_| SearchFailure::Unavailable)?,
        device,
    )?;
    check_source_namespace(root, relative, &directories, &file)?;
    if !policy_admitted(root, relative, &directories, device, limits, false)? {
        return Err(SearchFailure::Unavailable);
    }
    if Instant::now() >= limits.deadline {
        return Err(SearchFailure::Deadline);
    }
    Ok(bytes)
}

/// Recheck and bound a descriptor opened by a no-follow source reader.
pub(super) fn read_source_file(file: File, device: u64) -> Result<Option<Vec<u8>>, SearchFailure> {
    let opened = file.metadata().map_err(|_| SearchFailure::Unavailable)?;
    if !opened.is_file() || opened.dev() != device || opened.len() > MAX_FILE_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SearchFailure::Unavailable)?;
    Ok((bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes))
}

/// List bounded, parsed Rust function names from source files whose names match
/// a question's code-like term. The model can choose a name; it cannot choose
/// an unchecked file read or supply evidence directly.
pub fn candidate_symbols(
    root: &Path,
    question: &str,
    limits: &SearchLimits,
) -> Result<Vec<(String, String)>, SearchFailure> {
    let anchor = question
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| word.contains('_'))
        .max_by_key(|word| word.len())
        .unwrap_or("");
    let parts = anchor
        .split('_')
        .filter(|part| part.len() >= 3)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let root_file = open_root(root)?;
    let root_meta = root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?;
    let files = walk_owned(root, &root_file, root_meta.dev(), limits)?;
    let mut ranked = files
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_string_lossy().to_lowercase();
            let score = parts
                .iter()
                .filter(|part| name.contains(part.as_str()))
                .count();
            (score > 0).then_some((score, path))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let mut symbols = Vec::new();
    for (_, path) in ranked.into_iter().take(MAX_PLAN_FILES) {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        let Some(bytes) = read_source(root, &root_file, &path, root_meta.dev(), limits)? else {
            continue;
        };
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(parsed) = syn::parse_file(source) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SearchFailure::Unavailable)?
            .to_string_lossy()
            .into_owned();
        for item in parsed.items {
            match item {
                syn::Item::Fn(function) => {
                    symbols.push((
                        relative.clone(),
                        function.sig.ident.to_string(),
                        has_false_return(&function.block),
                    ));
                }
                syn::Item::Impl(block) => {
                    for item in block.items {
                        if let syn::ImplItem::Fn(function) = item {
                            symbols.push((
                                relative.clone(),
                                function.sig.ident.to_string(),
                                has_false_return(&function.block),
                            ));
                        }
                    }
                }
                _ => {}
            }
            if symbols.len() >= MAX_PARSED_FUNCTIONS {
                break;
            }
        }
        if symbols.len() >= MAX_PARSED_FUNCTIONS {
            break;
        }
    }
    let question_lower = question.to_lowercase();
    let stop_question = question_lower
        .split(|character: char| !character.is_alphabetic())
        .any(|word| matches!(word, "stop" | "stops" | "stopped"));
    // The answer contract needs an executable false-return branch. When one
    // is present, do not ask the planner to choose a classifier or drain step.
    if stop_question && symbols.iter().any(|(_, _, returns_false)| *returns_false) {
        symbols.retain(|(_, _, returns_false)| *returns_false);
    }
    let score = |name: &str, returns_false: bool| {
        usize::from(stop_question && returns_false) * 2
            + usize::from(name.starts_with("is_") || name.starts_with("should_"))
            + name
                .split('_')
                .filter(|part| part.len() >= 3 && question_lower.contains(part))
                .count()
    };
    symbols.sort_by(|left, right| {
        score(&right.1, right.2)
            .cmp(&score(&left.1, left.2))
            .then_with(|| left.1.cmp(&right.1))
    });
    symbols.truncate(MAX_PLAN_SYMBOLS);
    Ok(symbols
        .into_iter()
        .map(|(path, name, _)| (path, name))
        .collect())
}

fn has_false_return(block: &syn::Block) -> bool {
    struct FalseReturn(bool);

    impl<'ast> Visit<'ast> for FalseReturn {
        fn visit_expr_return(&mut self, node: &'ast syn::ExprReturn) {
            if matches!(node.expr.as_deref(), Some(syn::Expr::Lit(literal))
                if matches!(&literal.lit, syn::Lit::Bool(value) if !value.value))
            {
                self.0 = true;
            }
            visit::visit_expr_return(self, node);
        }
    }

    let mut visitor = FalseReturn(false);
    visitor.visit_block(block);
    visitor.0
}

/// Pathname traversal supplies candidates and bounds, never source/policy authority.
pub(super) fn walk(
    root: &Path,
    device: u64,
    limits: &SearchLimits,
) -> Result<Vec<PathBuf>, SearchFailure> {
    let root_file = open_root(root)?;
    walk_owned(root, &root_file, device, limits)
}

fn walk_owned(
    root: &Path,
    root_file: &File,
    device: u64,
    limits: &SearchLimits,
) -> Result<Vec<PathBuf>, SearchFailure> {
    check_source_namespace(root, Path::new(""), &[], root_file)?;
    if root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?
        .dev()
        != device
    {
        return Err(SearchFailure::Unavailable);
    }
    // Check both root policy classes, even when no visible entry exists.
    policy_admitted(
        root,
        Path::new(""),
        &[root_file
            .try_clone()
            .map_err(|_| SearchFailure::Unavailable)?],
        device,
        limits,
        true,
    )?;
    #[cfg(test)]
    issue_327_tests::after_policy_validation();
    let mut root_targets = 0usize;
    // Linux-only: enumerate the retained root, not a substituted named root.
    for entry in fs::read_dir(format!("/proc/self/fd/{}", root_file.as_raw_fd()))
        .map_err(|_| SearchFailure::Unavailable)?
    {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        let entry = entry.map_err(|_| SearchFailure::Unavailable)?;
        let name = entry.file_name();
        if name.as_bytes().starts_with(b".")
            || name.to_str().is_some_and(|name| EXCLUDED.contains(&name))
        {
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| SearchFailure::Unavailable)?;
        if metadata.file_type().is_symlink()
            || metadata.dev() != device
            || (!metadata.is_dir() && !metadata.is_file())
        {
            continue;
        }
        if root_targets == MAX_ROOT_TARGETS {
            return Err(SearchFailure::TargetLimit);
        }
        root_targets += 1;
    }
    let mut user_ignores = GitignoreBuilder::new(root);
    for pattern in &limits.ignores {
        user_ignores
            .add_line(None, pattern)
            .map_err(|_| SearchFailure::Unavailable)?;
    }
    let user_ignores = user_ignores
        .build()
        .map_err(|_| SearchFailure::Unavailable)?;
    let unsafe_ignore = Arc::new(AtomicBool::new(false));
    let unsafe_ignore_filter = Arc::clone(&unsafe_ignore);
    let policy_root = root.to_path_buf();
    let policy_owner = root_file
        .try_clone()
        .map_err(|_| SearchFailure::Unavailable)?;
    let policy_limits = SearchLimits {
        deadline: limits.deadline,
        max_results: limits.max_results,
        language: None,
        ignores: Vec::new(),
    };
    let mut walker = WalkBuilder::new(root);
    walker
        .follow_links(false)
        .same_file_system(true)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        // Never let the library independently reopen policy pathnames.
        .ignore(false)
        .git_ignore(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name().as_bytes().starts_with(b".") {
                return false;
            }
            let kind = entry.file_type();
            let is_dir = kind.is_some_and(|kind| kind.is_dir());
            if !is_dir && !kind.is_some_and(|kind| kind.is_file()) {
                return false;
            }
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| EXCLUDED.contains(&name))
                || user_ignores
                    .matched_path_or_any_parents(entry.path(), is_dir)
                    .is_ignore()
            {
                return false;
            }
            let admitted = (|| {
                let relative = entry
                    .path()
                    .strip_prefix(&policy_root)
                    .map_err(|_| SearchFailure::Unavailable)?;
                let (file, directories) = open_source(
                    policy_owner
                        .try_clone()
                        .map_err(|_| SearchFailure::Unavailable)?,
                    relative,
                    device,
                    &policy_limits,
                    is_dir,
                )?;
                check_source_namespace(&policy_root, relative, &directories, &file)?;
                let admitted = policy_admitted(
                    &policy_root,
                    relative,
                    &directories,
                    device,
                    &policy_limits,
                    is_dir,
                )?;
                // A directory's policies must also be safe before descending.
                if is_dir && admitted {
                    policy_admitted(
                        entry.path(),
                        Path::new(""),
                        &[file.try_clone().map_err(|_| SearchFailure::Unavailable)?],
                        device,
                        &policy_limits,
                        true,
                    )?;
                }
                check_source_namespace(&policy_root, relative, &directories, &file)?;
                Ok::<_, SearchFailure>(admitted)
            })();
            match admitted {
                Ok(admitted) => admitted,
                Err(_) => {
                    unsafe_ignore_filter.store(true, Ordering::Relaxed);
                    false
                }
            }
        });
    let mut files = Vec::new();
    let mut file_count = 0usize;
    for entry in walker.build() {
        if unsafe_ignore.load(Ordering::Relaxed) {
            return Err(SearchFailure::Unavailable);
        }
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        let entry = entry.map_err(|_| SearchFailure::Unavailable)?;
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(path).map_err(|_| SearchFailure::Unavailable)?;
        if metadata.file_type().is_symlink() || metadata.dev() != device || !metadata.is_file() {
            continue;
        }
        file_count += 1;
        if file_count > MAX_WALK_FILES {
            return Err(SearchFailure::Limit);
        }
        if metadata.len() <= MAX_FILE_BYTES {
            files.push(path.to_path_buf());
        }
    }
    if unsafe_ignore.load(Ordering::Relaxed) {
        return Err(SearchFailure::Unavailable);
    }
    check_source_namespace(root, Path::new(""), &[], root_file)?;
    Ok(files)
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.len() >= 3)
        .map(|term| term.to_lowercase())
        .filter(|term| {
            !matches!(
                term.as_str(),
                "and" | "the" | "for" | "from" | "how" | "why" | "where" | "what" | "does"
            )
        })
        .collect()
}

fn score_file(path: &Path, text: &str, terms: &[String]) -> Option<Hit> {
    let mut best_line = None;
    let mut best_score = 0;
    let mut total = 0;
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        let mut score = 0;
        for term in terms {
            if lower.contains(term) {
                score += 3;
            }
        }
        if score > best_score {
            best_score = score;
            best_line = Some(index + 1);
        }
        total += score;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();
    for term in terms {
        if name.contains(term) {
            total += 2;
        }
    }
    best_line.filter(|_| total > 0).map(|line| Hit {
        path: path.to_path_buf(),
        line,
        end: line,
        score: total,
    })
}

fn language_matches(path: &Path, language: Option<&str>) -> bool {
    let Some(language) = language else {
        return true;
    };
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    match language {
        "rust" | "rs" => extension == "rs",
        "python" | "py" => extension == "py",
        other => extension.eq_ignore_ascii_case(other),
    }
}

#[cfg(test)]
#[path = "native_search_custody_tests.rs"]
mod issue_327_tests;

#[cfg(test)]
mod issue_326_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const HIDDEN_MARKER: &str = "synthetic_issue326_hidden_marker";

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Fixture, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let base = PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp")
            .join(format!("pbi-rs-issue326-{}-{nonce}", std::process::id()));
        let root = base.join(".repo");
        fs::create_dir_all(root.join(".private")).expect("synthetic hidden fixture");
        fs::write(
            root.join(".gitignore"),
            "*.rs\n!.*\n!.private/**\n!visible_candidate.rs\n",
        )
        .expect("synthetic whitelist");
        fs::write(root.join(".env"), HIDDEN_MARKER).expect("synthetic env fixture");
        for name in [
            std::ffi::OsStr::new(".类型.rs"),
            std::ffi::OsStr::from_bytes(b".\xff.rs"),
        ] {
            fs::write(root.join(name), HIDDEN_MARKER).expect("synthetic hidden byte names");
        }
        fs::write(
            root.join(".private/issue326_hidden_candidate.rs"),
            "fn issue326_hidden_candidate() {}\n",
        )
        .expect("synthetic hidden candidate");
        fs::write(
            root.join("visible_candidate.rs"),
            "fn issue326_hidden_candidate() {}\n",
        )
        .expect("ordinary candidate");
        (Fixture(base), root)
    }

    fn limits() -> SearchLimits {
        SearchLimits {
            deadline: Instant::now() + Duration::from_secs(10),
            max_results: 16,
            language: None,
            ignores: Vec::new(),
        }
    }

    #[test]
    fn verified_search_denies_whitelisted_hidden_components() {
        let (_fixture, root) = fixture();
        let files = walk(
            &root,
            fs::metadata(&root).expect("root metadata").dev(),
            &limits(),
        )
        .expect("shared admission");
        assert!(files.len() == 1 && files[0] == root.join("visible_candidate.rs"));
        let results = search_repository(&root, HIDDEN_MARKER, &limits()).expect("verified search");
        assert!(results.is_empty());
    }

    #[test]
    fn semantic_candidates_deny_whitelisted_hidden_components() {
        let (_fixture, root) = fixture();
        let candidates = candidate_symbols(&root, "where is issue326_hidden_candidate?", &limits())
            .expect("semantic candidate ingestion");
        assert!(candidates.len() == 1 && candidates[0].0 == "visible_candidate.rs");
    }
}
