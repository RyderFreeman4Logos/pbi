//! No-model source extraction using the existing Rust parser and source limits.
use crate::native_search::{read_source_file, walk, SearchFailure, SearchLimits};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};
use std::time::Instant;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

/// Parse the no-model extract CLI; errors deliberately never echo user input.
pub(super) fn run(arguments: &[String]) -> Result<String, crate::CliError> {
    if arguments == ["--help"] || arguments == ["-h"] {
        return Ok("Usage: pbi-rs extract <path>:<line> [--timeout <SECONDS>] [--max-bytes <N>]\nRust items are complete; other locations use approximate four-line windows. Oversized blocks are explicitly truncated.\n".to_owned());
    }
    let (position, options) = arguments
        .split_first()
        .ok_or_else(|| crate::CliError::usage("extract requires path:line"))?;
    let (path, line) = position
        .rsplit_once(':')
        .ok_or_else(|| crate::CliError::usage("extract requires path:line"))?;
    let line = line
        .parse::<usize>()
        .ok()
        .filter(|line| *line > 0)
        .ok_or_else(|| crate::CliError::usage("invalid extract line"))?;
    let mut timeout = crate::SEARCH_OUTER_DEADLINE_SECONDS;
    let mut max_bytes = crate::MAX_RAW_OUTPUT_BYTES;
    let mut seen_timeout = false;
    let mut seen_bytes = false;
    if options.len() % 2 != 0 {
        return Err(crate::CliError::usage("invalid extract options"));
    }
    for pair in options.as_chunks::<2>().0 {
        match pair[0].as_str() {
            "--timeout" if !seen_timeout => {
                timeout = pair[1]
                    .parse()
                    .map_err(|_| crate::CliError::usage("invalid extract timeout"))?;
                seen_timeout = true;
            }
            "--max-bytes" if !seen_bytes => {
                max_bytes = pair[1]
                    .parse::<usize>()
                    .ok()
                    .filter(|bytes| *bytes > 0)
                    .ok_or_else(|| crate::CliError::usage("invalid extract byte cap"))?
                    .min(crate::MAX_RAW_OUTPUT_BYTES);
                seen_bytes = true;
            }
            _ => return Err(crate::CliError::usage("invalid extract options")),
        }
    }
    let deadline = Instant::now()
        .checked_add(std::time::Duration::from_secs(timeout))
        .ok_or_else(|| crate::CliError::usage("extract timeout is too large"))?;
    let root = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| crate::CliError::failed("extract source unavailable"))?;
    let limits = SearchLimits {
        deadline,
        max_results: 1,
        language: None,
        ignores: Vec::new(),
    };
    extract(&root, Path::new(path), line, &limits, max_bytes).map_err(|failure| {
        crate::CliError::failed(match failure {
            SearchFailure::Deadline => "extract deadline exceeded",
            SearchFailure::Limit | SearchFailure::TargetLimit => "extract safety limit exceeded",
            SearchFailure::Unavailable => "extract source unavailable or invalid position",
        })
    })
}

/// Return the smallest enclosing Rust item, or a four-line approximate window.
/// Admission uses the same ignore/device/file bounds as search. Each path
/// component is opened relative to a retained descriptor, without following links.
pub(super) fn extract(
    root: &Path,
    path: &Path,
    line: usize,
    limits: &SearchLimits,
    max_bytes: usize,
) -> Result<String, SearchFailure> {
    check_deadline(limits)?;
    if line == 0
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(SearchFailure::Unavailable);
    }
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| SearchFailure::Unavailable)?
    } else {
        path
    };
    if relative.components().any(|part| matches!(part, Component::Normal(name) if name.to_str().is_none_or(|name| name.starts_with('.')))) {
        return Err(SearchFailure::Unavailable);
    }
    if relative.as_os_str().is_empty()
        || relative
            .to_str()
            .is_none_or(|name| name.chars().any(char::is_control))
    {
        return Err(SearchFailure::Unavailable);
    }
    let root_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|_| SearchFailure::Unavailable)?;
    let device = root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?
        .dev();
    let candidate = root.join(relative);
    if !walk(root, device, limits)?.contains(&candidate) {
        return Err(SearchFailure::Unavailable);
    }
    let file = open_source(root_file, relative, device, limits)?;
    let bytes = read_source_file(file, device)?.ok_or(SearchFailure::Unavailable)?;
    let source = std::str::from_utf8(&bytes).map_err(|_| SearchFailure::Unavailable)?;
    if source.contains('\0') {
        return Err(SearchFailure::Unavailable);
    }
    check_deadline(limits)?;
    let offsets = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(at, _)| at + 1))
        .collect::<Vec<_>>();
    let count = source.lines().count();
    if line > count {
        return Err(SearchFailure::Unavailable);
    }
    let mut block = RustBlock { line, best: None };
    if path.extension().is_some_and(|extension| extension == "rs") {
        if let Ok(parsed) = syn::parse_file(source) {
            block.visit_file(&parsed);
        }
    }
    check_deadline(limits)?;
    let (start_line, end_line, mut start, end, complete) = if let Some(span) = block.best {
        let range = span.byte_range();
        (
            span.start().line,
            span.end().line,
            range.start,
            range.end,
            true,
        )
    } else {
        let first = line.saturating_sub(1).max(1);
        let last = (first + 3).min(count);
        (
            first,
            last,
            offsets[first - 1],
            offsets.get(last).copied().unwrap_or(source.len()),
            false,
        )
    };
    let line_start = offsets[start_line - 1];
    if source[line_start..start].chars().all(char::is_whitespace) {
        start = line_start;
    }
    let body = source
        .get(start..end)
        .ok_or(SearchFailure::Unavailable)?
        .trim_end_matches(['\r', '\n']);
    let header = |status| {
        format!(
            "File: {}, Lines: {start_line}-{end_line}\nBlock: {status}\n\n",
            relative.display()
        )
    };
    let mut output = header(if complete { "complete" } else { "approximate" });
    if output.len() + body.len() < max_bytes {
        output.push_str(body);
        output.push('\n');
    } else {
        output = header("truncated");
        const MARKER: &str = "\n[truncated]\n";
        let available = max_bytes
            .checked_sub(output.len() + MARKER.len())
            .ok_or(SearchFailure::Limit)?;
        let mut end = available.min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&body[..end]);
        output.push_str(MARKER);
    }
    check_deadline(limits)?;
    Ok(output)
}

fn check_deadline(limits: &SearchLimits) -> Result<(), SearchFailure> {
    if Instant::now() >= limits.deadline {
        Err(SearchFailure::Deadline)
    } else {
        Ok(())
    }
}

fn open_source(
    mut parent: File,
    relative: &Path,
    device: u64,
    limits: &SearchLimits,
) -> Result<File, SearchFailure> {
    let mut parts = relative
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .peekable();
    while let Some(Component::Normal(name)) = parts.next() {
        check_deadline(limits)?;
        let name = CString::new(name.as_encoded_bytes()).map_err(|_| SearchFailure::Unavailable)?;
        let directory = parts.peek().is_some();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is a retained live descriptor; name is a NUL-terminated
        // single component. openat returns a new owned descriptor or -1.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(SearchFailure::Unavailable);
        }
        // SAFETY: a successful openat returned a fresh descriptor owned here.
        let opened = unsafe { File::from_raw_fd(fd) };
        let metadata = opened.metadata().map_err(|_| SearchFailure::Unavailable)?;
        if metadata.dev() != device
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(SearchFailure::Unavailable);
        }
        parent = opened;
    }
    Ok(parent)
}

struct RustBlock {
    line: usize,
    best: Option<proc_macro2::Span>,
}
impl RustBlock {
    fn consider(&mut self, span: proc_macro2::Span) {
        if span.start().line <= self.line
            && self.line <= span.end().line
            && self
                .best
                .is_none_or(|best| span.byte_range().len() < best.byte_range().len())
        {
            self.best = Some(span);
        }
    }
}
impl<'ast> Visit<'ast> for RustBlock {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        self.consider(item.span());
        visit::visit_item(self, item);
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        self.consider(item.span());
        visit::visit_impl_item(self, item);
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        self.consider(item.span());
        visit::visit_trait_item(self, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_descriptor_refuses_an_actual_other_device() {
        let root = std::env::current_dir().expect("root");
        let device = std::fs::metadata(root).expect("root metadata").dev();
        let proc_root = File::open("/proc").expect("Linux procfs");
        assert_ne!(proc_root.metadata().expect("proc metadata").dev(), device);
        let limits = SearchLimits {
            deadline: Instant::now() + std::time::Duration::from_secs(8),
            max_results: 1,
            language: None,
            ignores: Vec::new(),
        };
        assert!(matches!(
            open_source(proc_root, Path::new("version"), device, &limits),
            Err(SearchFailure::Unavailable)
        ));
    }
}
