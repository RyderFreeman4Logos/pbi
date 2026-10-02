//! Bounded in-process repository search.
//!
//! No Probe binary, probe-chat, or search subprocess. Hits are ordinary
//! `File:` ranges so the existing verifier remains the citation boundary.
//!
//! ponytail: one lexical pass, term frequency over document frequency.
//! Replace with a real inverted index if a repository walk exceeds the deadline.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const EXCLUDED: [&str; 5] = [".git", "target", "drafts", "node_modules", "__pycache__"];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WALK_FILES: usize = 20_000;
const MAX_ROOT_TARGETS: usize = 16;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub struct SearchLimits {
    pub deadline: Instant,
    pub max_results: usize,
    pub language: Option<String>,
    pub ignores: Vec<String>,
}

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
    let mut files = Vec::new();
    walk(
        root,
        root,
        root_meta.dev(),
        limits,
        &gitignore_patterns(root, root_meta.dev())?,
        true,
        &mut files,
    )?;
    let mut hits = Vec::new();
    for path in files {
        if Instant::now() >= limits.deadline {
            return Err(SearchFailure::Deadline);
        }
        if ignored(root, &path, &limits.ignores)
            || !language_matches(&path, limits.language.as_deref())
        {
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

fn walk(
    root: &Path,
    dir: &Path,
    device: u64,
    limits: &SearchLimits,
    gitignore: &[String],
    count_root: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), SearchFailure> {
    if Instant::now() >= limits.deadline {
        return Err(SearchFailure::Deadline);
    }
    let mut children = fs::read_dir(dir)
        .map_err(|_| SearchFailure::Unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SearchFailure::Unavailable)?;
    children.sort_by_key(|entry| entry.file_name());
    let mut root_targets = 0usize;
    for entry in children {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || EXCLUDED.contains(&name) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| SearchFailure::Unavailable)?;
        if metadata.file_type().is_symlink() || metadata.dev() != device || !path.starts_with(root)
        {
            continue;
        }
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        if count_root {
            if root_targets == MAX_ROOT_TARGETS {
                return Err(SearchFailure::TargetLimit);
            }
            root_targets += 1;
        }
        if ignored(root, &path, gitignore) || ignored(root, &path, &limits.ignores) {
            continue;
        }
        if metadata.is_dir() {
            if !ignored_directory(root, &path, &limits.ignores) {
                walk(root, &path, device, limits, gitignore, false, files)?;
            }
        } else if metadata.len() <= MAX_FILE_BYTES {
            if files.len() == MAX_WALK_FILES {
                return Err(SearchFailure::Limit);
            }
            files.push(path);
        }
    }
    Ok(())
}

fn gitignore_patterns(root: &Path, device: u64) -> Result<Vec<String>, SearchFailure> {
    let path = root.join(".gitignore");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(SearchFailure::Unavailable),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.dev() != device
        || metadata.len() > MAX_FILE_BYTES
    {
        return Err(SearchFailure::Unavailable);
    }
    Ok(fs::read_to_string(path)
        .map_err(|_| SearchFailure::Unavailable)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
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

fn ignored(root: &Path, path: &Path, ignores: &[String]) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    let mut ignored = false;
    for pattern in ignores {
        let negated = pattern.starts_with('!');
        let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        if glob_match(pattern, &relative) {
            ignored = !negated;
        }
    }
    ignored
}

fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let doubled = pattern.contains("**");
    let pattern = pattern.trim_end_matches('/');
    if doubled {
        let (prefix, suffix) = pattern.split_once("**").unwrap_or((pattern, ""));
        let prefix = prefix.trim_end_matches('/');
        let suffix = suffix.trim_start_matches('/');
        let rest = if prefix.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/').or(Some("")))
                .filter(|_| path == prefix || path.starts_with(&format!("{prefix}/")))
        };
        return rest.is_some_and(|rest| suffix.is_empty() || glob_match(suffix, rest));
    }
    let mut path_parts = path.split('/');
    for part in pattern.split('/') {
        let Some(candidate) = path_parts.next() else {
            return false;
        };
        if part == "*" {
            continue;
        }
        if part.starts_with('*') && !part[1..].contains('*') && candidate.ends_with(&part[1..]) {
            continue;
        }
        if part != candidate {
            return false;
        }
    }
    path_parts.next().is_none()
}

fn ignored_directory(root: &Path, path: &Path, ignores: &[String]) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    ignores.iter().any(|pattern| {
        let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let Some((prefix, _)) = pattern.split_once("/**") else {
            return false;
        };
        let prefix = prefix.trim_end_matches('/');
        !prefix.is_empty() && (relative == prefix || relative.starts_with(&format!("{prefix}/")))
    })
}
