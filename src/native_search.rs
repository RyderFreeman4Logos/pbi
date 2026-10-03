//! Bounded in-process repository search.
//!
//! No Probe binary, probe-chat, or search subprocess. Hits are ordinary
//! `File:` ranges so the existing verifier remains the citation boundary.
//!
//! ponytail: one lexical pass, term frequency over document frequency.
//! Replace with a real inverted index if a repository walk exceeds the deadline.

use ignore::{gitignore::GitignoreBuilder, WalkBuilder};
use std::fs;
use std::os::unix::fs::MetadataExt;
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
    let root_meta = fs::symlink_metadata(root).map_err(|_| SearchFailure::Unavailable)?;
    let files = walk(root, root_meta.dev(), limits)?;
    let mut hits = Vec::new();
    for path in files {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        if !language_matches(&path, limits.language.as_deref()) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| SearchFailure::Unavailable)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.dev() != root_meta.dev()
            || metadata.len() > MAX_FILE_BYTES
        {
            continue;
        }
        let bytes = fs::read(&path).map_err(|_| SearchFailure::Unavailable)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            continue;
        }
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
    let root_meta = fs::symlink_metadata(root).map_err(|_| SearchFailure::Unavailable)?;
    let files = walk(root, root_meta.dev(), limits)?;
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
        let metadata = fs::symlink_metadata(&path).map_err(|_| SearchFailure::Unavailable)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.dev() != root_meta.dev()
            || metadata.len() > MAX_FILE_BYTES
        {
            continue;
        }
        let bytes = fs::read(&path).map_err(|_| SearchFailure::Unavailable)?;
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

fn walk(root: &Path, device: u64, limits: &SearchLimits) -> Result<Vec<PathBuf>, SearchFailure> {
    validate_gitignore(&root.join(".gitignore"), device)?;
    let mut root_targets = 0usize;
    for entry in fs::read_dir(root).map_err(|_| SearchFailure::Unavailable)? {
        let entry = entry.map_err(|_| SearchFailure::Unavailable)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || EXCLUDED.contains(&name) {
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| SearchFailure::Unavailable)?;
        if metadata.file_type().is_symlink() || metadata.dev() != device {
            continue;
        }
        if !metadata.is_dir() && !metadata.is_file() {
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
    let mut walker = WalkBuilder::new(root);
    walker
        .follow_links(false)
        .same_file_system(true)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
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
            if is_dir && validate_gitignore(&entry.path().join(".gitignore"), device).is_err() {
                unsafe_ignore_filter.store(true, Ordering::Relaxed);
                return false;
            }
            true
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
    Ok(files)
}

fn validate_gitignore(path: &Path, device: u64) -> Result<(), SearchFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(SearchFailure::Unavailable),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.dev() != device
        || metadata.len() > MAX_FILE_BYTES
    {
        return Err(SearchFailure::Unavailable);
    }
    Ok(())
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
