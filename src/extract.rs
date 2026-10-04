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

pub(super) struct Options<'a> {
    path: &'a str,
    line: usize,
    pub(super) timeout: u64,
    max_bytes: usize,
}

/// Validate all extract options before constructing any execution deadline.
/// Errors deliberately never echo user input; None means help only.
pub(super) fn parse(arguments: &[String]) -> Result<Option<Options<'_>>, crate::CliError> {
    if arguments == ["--help"] || arguments == ["-h"] {
        return Ok(None);
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
    Ok(Some(Options {
        path,
        line,
        timeout,
        max_bytes,
    }))
}

/// Execute the validated no-model extract CLI with the existing safety bounds.
pub(super) fn run(arguments: &[String]) -> Result<String, crate::CliError> {
    let Some(Options {
        path,
        line,
        timeout,
        max_bytes,
    }) = parse(arguments)?
    else {
        return Ok("Usage: pbi-rs extract <path>:<line> [--timeout <SECONDS>] [--max-bytes <N>]\nRust items are complete; other locations use approximate four-line windows. Oversized blocks are explicitly truncated.\n".to_owned());
    };
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

pub(super) fn run_symbols(arguments: &[String]) -> Result<String, crate::CliError> {
    if arguments == ["--help"] || arguments == ["-h"] {
        return Ok("Usage: pbi-rs symbols <path>\nLists Rust functions/structs/impl methods, Python defs/classes, or Go funcs/types.\n".to_owned());
    }
    let [path] = arguments else {
        return Err(crate::CliError::usage("symbols requires one source path"));
    };
    let path = Path::new(path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| crate::CliError::usage("symbols supports .rs, .py, and .go files"))?;
    if !matches!(extension, "rs" | "py" | "go") {
        return Err(crate::CliError::usage(
            "symbols supports .rs, .py, and .go files",
        ));
    }
    let root = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| crate::CliError::failed("symbols source unavailable"))?;
    let deadline = Instant::now()
        .checked_add(std::time::Duration::from_secs(
            crate::SEARCH_OUTER_DEADLINE_SECONDS,
        ))
        .ok_or_else(|| crate::CliError::usage("symbols deadline is too large"))?;
    let limits = SearchLimits {
        deadline,
        max_results: 1,
        language: None,
        ignores: Vec::new(),
    };
    let (source, _) = read_admitted_source(&root, path, &limits).map_err(|failure| {
        crate::CliError::failed(match failure {
            SearchFailure::Deadline => "symbols deadline exceeded",
            SearchFailure::Limit | SearchFailure::TargetLimit => "symbols safety limit exceeded",
            SearchFailure::Unavailable => "symbols source unavailable or invalid",
        })
    })?;
    let (symbols, truncated) = match extension {
        "rs" => rust_symbols(&source, &limits),
        "py" | "go" => line_symbols(&source, extension, &limits),
        _ => unreachable!("extension was validated above"),
    }
    .map_err(|failure| {
        crate::CliError::failed(match failure {
            SearchFailure::Deadline => "symbols deadline exceeded",
            SearchFailure::Limit | SearchFailure::TargetLimit => "symbols safety limit exceeded",
            SearchFailure::Unavailable => "symbols source unavailable or invalid",
        })
    })?;
    Ok(render_symbols(&symbols, truncated))
}

const MAX_SYMBOLS: usize = 256;

struct Symbol {
    line: usize,
    kind: &'static str,
    name: String,
}

fn rust_symbols(source: &str, limits: &SearchLimits) -> Result<(Vec<Symbol>, bool), SearchFailure> {
    check_deadline(limits)?;
    let parsed = syn::parse_file(source).map_err(|_| SearchFailure::Unavailable)?;
    let mut symbols = Vec::new();
    let mut truncated = false;
    'items: for item in parsed.items {
        match item {
            syn::Item::Fn(function) => {
                if symbols.len() == MAX_SYMBOLS {
                    truncated = true;
                    break;
                }
                symbols.push(Symbol {
                    line: function.sig.fn_token.span.start().line,
                    kind: "fn",
                    name: function.sig.ident.to_string(),
                });
            }
            syn::Item::Struct(item) => {
                if symbols.len() == MAX_SYMBOLS {
                    truncated = true;
                    break;
                }
                symbols.push(Symbol {
                    line: item.struct_token.span.start().line,
                    kind: "struct",
                    name: item.ident.to_string(),
                });
            }
            syn::Item::Impl(block) => {
                for item in block.items {
                    if let syn::ImplItem::Fn(function) = item {
                        if symbols.len() == MAX_SYMBOLS {
                            truncated = true;
                            break 'items;
                        }
                        symbols.push(Symbol {
                            line: function.sig.fn_token.span.start().line,
                            kind: "fn",
                            name: function.sig.ident.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    check_deadline(limits)?;
    Ok((symbols, truncated))
}

fn line_symbols(
    source: &str,
    language: &str,
    limits: &SearchLimits,
) -> Result<(Vec<Symbol>, bool), SearchFailure> {
    let normalized;
    let source = if language == "py" {
        // Python accepts one source BOM and universal physical newlines.
        normalized = source
            .strip_prefix('\u{feff}')
            .unwrap_or(source)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        normalized.as_str()
    } else {
        source
    };
    let mut symbols = Vec::new();
    let mut truncated = false;
    for (index, line) in source.lines().enumerate() {
        check_deadline(limits)?;
        let declaration = if language == "py" {
            python_symbol(line)
        } else {
            go_symbol(line)
        };
        if let Some((kind, name)) = declaration {
            if symbols.len() == MAX_SYMBOLS {
                truncated = true;
                break;
            }
            symbols.push(Symbol {
                line: index + 1,
                kind,
                name: name.to_owned(),
            });
        }
    }
    check_deadline(limits)?;
    Ok((symbols, truncated))
}

fn python_symbol(line: &str) -> Option<(&'static str, &str)> {
    let mut line = line.trim_start();
    if line.starts_with('#') {
        return None;
    }
    if let Some(rest) = line.strip_prefix("async") {
        if rest.chars().next().is_some_and(char::is_whitespace) {
            line = rest.trim_start();
        }
    }
    let split = line.find(char::is_whitespace)?;
    let (keyword, rest) = line.split_at(split);
    let kind = match keyword {
        "def" => "def",
        "class" => "class",
        _ => return None,
    };
    python_identifier(rest.trim_start()).map(|name| (kind, name))
}

fn go_symbol(line: &str) -> Option<(&'static str, &str)> {
    let line = line.trim_start();
    if line.starts_with("//") {
        return None;
    }
    if let Some(rest) = keyword_rest(line, "func") {
        let rest = if rest.starts_with('(') {
            &rest[rest.find(')')? + 1..]
        } else {
            rest
        };
        return identifier(rest.trim_start()).map(|name| ("func", name));
    }
    keyword_rest(line, "type")
        .and_then(identifier)
        .map(|name| ("type", name))
}

fn keyword_rest<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    rest.chars().next().filter(|character| {
        character.is_whitespace() || (keyword == "func" && *character == '(')
    })?;
    Some(rest.trim_start())
}

fn python_identifier(text: &str) -> Option<&str> {
    let end = text
        .char_indices()
        .find(|(_, character)| {
            character.is_ascii() && *character != '_' && !character.is_ascii_alphanumeric()
        })
        .map_or(text.len(), |(index, _)| index);
    let identifier = text.get(..end)?.trim_end();
    syn::parse::Parser::parse_str(<syn::Ident as syn::ext::IdentExt>::parse_any, identifier)
        .ok()?;
    Some(identifier)
}

fn identifier(text: &str) -> Option<&str> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if first != '_' && !first.is_alphabetic() {
        return None;
    }
    let end = chars
        .find(|(_, character)| *character != '_' && !character.is_alphanumeric())
        .map_or(text.len(), |(index, _)| index);
    Some(&text[..end])
}

fn render_symbols(symbols: &[Symbol], mut truncated: bool) -> String {
    const MARKER: &str = "[truncated]\n";
    let mut output = String::new();
    for symbol in symbols {
        let line = format!("{}: {} {}\n", symbol.line, symbol.kind, symbol.name);
        if output.len() + line.len() + MARKER.len() > crate::MAX_RAW_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        output.push_str(&line);
    }
    if truncated {
        output.push_str(MARKER);
    }
    output
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
    if line == 0 {
        return Err(SearchFailure::Unavailable);
    }
    let (source, relative) = read_admitted_source(root, path, limits)?;
    let offsets = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(at, _)| at + 1))
        .collect::<Vec<_>>();
    let count = source.lines().count();
    if line > count {
        return Err(SearchFailure::Unavailable);
    }
    let mut block = RustBlock { line, best: None };
    let mut parser_offset = 0;
    if path.extension().is_some_and(|extension| extension == "rs") {
        if let Ok(parsed) = syn::parse_file(&source) {
            // syn strips the BOM and shebang, but retains the shebang's LF.
            parser_offset = usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
                + parsed.shebang.as_ref().map_or(0, String::len);
            block.visit_file(&parsed);
        }
    }
    check_deadline(limits)?;
    let (start_line, end_line, mut start, end, complete) = if let Some(span) = block.best {
        let range = span.byte_range();
        (
            span.start().line,
            span.end().line,
            range
                .start
                .checked_add(parser_offset)
                .ok_or(SearchFailure::Unavailable)?,
            range
                .end
                .checked_add(parser_offset)
                .ok_or(SearchFailure::Unavailable)?,
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
    let line_start = *offsets
        .get(start_line - 1)
        .ok_or(SearchFailure::Unavailable)?;
    if source
        .get(line_start..start)
        .ok_or(SearchFailure::Unavailable)?
        .chars()
        .all(char::is_whitespace)
    {
        start = line_start;
    }
    let body = source
        .get(start..end)
        .ok_or(SearchFailure::Unavailable)?
        .trim_end_matches(['\r', '\n']);
    // Preserve source layout while using the established terminal-control policy.
    let body = body
        .split_inclusive(['\n', '\t'])
        .map(|part| {
            let (text, whitespace) = if let Some(text) = part.strip_suffix("\r\n") {
                (text, "\r\n")
            } else if part.ends_with(['\n', '\t']) {
                part.split_at(part.len() - 1)
            } else {
                (part, "")
            };
            crate::escape_control(text) + whitespace
        })
        .collect::<String>();
    let header = |status| {
        format!(
            "File: {}, Lines: {start_line}-{end_line}\nBlock: {status}\n\n",
            relative.display()
        )
    };
    let mut output = header(if complete { "complete" } else { "approximate" });
    if output.len() + body.len() < max_bytes {
        output.push_str(&body);
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

fn read_admitted_source<'a>(
    root: &Path,
    path: &'a Path,
    limits: &SearchLimits,
) -> Result<(String, &'a Path), SearchFailure> {
    check_deadline(limits)?;
    if path
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
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(root)
        .map_err(|_| SearchFailure::Unavailable)?;
    let device = root_file
        .metadata()
        .map_err(|_| SearchFailure::Unavailable)?
        .dev();
    let bytes = admitted_source(root_file, root, relative, device, limits, walk)?;
    let source = std::str::from_utf8(&bytes).map_err(|_| SearchFailure::Unavailable)?;
    if source.contains('\0') {
        return Err(SearchFailure::Unavailable);
    }
    check_deadline(limits)?;
    Ok((source.to_owned(), relative))
}

fn admitted_source(
    root_file: File,
    root: &Path,
    relative: &Path,
    device: u64,
    limits: &SearchLimits,
    walk_source: impl FnOnce(
        &Path,
        u64,
        &SearchLimits,
    ) -> Result<Vec<std::path::PathBuf>, SearchFailure>,
) -> Result<Vec<u8>, SearchFailure> {
    let (file, directories) = open_source(root_file, relative, device, limits, false)?;
    check_source_namespace(root, relative, &directories, &file)?;
    let admitted = policy_admitted(root, relative, &directories, device, limits, false)?;
    if !walk_source(root, device, limits)?.contains(&root.join(relative)) || !admitted {
        return Err(SearchFailure::Unavailable);
    }
    let bytes = read_source_file(
        file.try_clone().map_err(|_| SearchFailure::Unavailable)?,
        device,
    )?
    .ok_or(SearchFailure::Unavailable)?;
    // Current named-owner equality rejects detached roots/ancestors/leafs;
    // it is NOT the ABA proof. Descriptor-owned policy admission on both sides
    // of the read independently denies ignored bytes, even after restoration.
    check_source_namespace(root, relative, &directories, &file)?;
    if !policy_admitted(root, relative, &directories, device, limits, false)? {
        return Err(SearchFailure::Unavailable);
    }
    Ok(bytes)
}

/// Reject held source owners no longer named by the checked project path.
/// Equality at these observations is not an atomic snapshot or ABA guarantee.
pub(super) fn check_source_namespace(
    root: &Path,
    relative: &Path,
    directories: &[File],
    file: &File,
) -> Result<(), SearchFailure> {
    let mut path = root.to_path_buf();
    let mut names = relative.components().filter_map(|part| {
        if let Component::Normal(name) = part {
            Some(name)
        } else {
            None
        }
    });
    for owned in directories.iter().chain(std::iter::once(file)) {
        let held = owned.metadata().map_err(|_| SearchFailure::Unavailable)?;
        let named = std::fs::symlink_metadata(&path).map_err(|_| SearchFailure::Unavailable)?;
        if (held.dev(), held.ino(), held.file_type())
            != (named.dev(), named.ino(), named.file_type())
        {
            return Err(SearchFailure::Unavailable);
        }
        if let Some(name) = names.next() {
            path.push(name);
        }
    }
    Ok(())
}

/// Evaluate bounded .ignore/.gitignore bytes through the retained source owners.
/// An empty relative path validates a directory's policies without matching a leaf.
pub(super) fn policy_admitted(
    root: &Path,
    relative: &Path,
    directories: &[File],
    device: u64,
    limits: &SearchLimits,
    final_directory: bool,
) -> Result<bool, SearchFailure> {
    let mut policies = Vec::new();
    let mut directory_path = root.to_path_buf();
    let names = relative
        .components()
        .filter_map(|part| {
            if let Component::Normal(name) = part {
                Some(name)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    // The pathname walk supplies only bounds and an additional denial. Authority
    // comes from policy bytes opened through the same retained owners as source.
    // A swap-and-restore cannot replace these matchers with another tree's rules.
    for (depth, directory) in directories.iter().enumerate() {
        let mut local = Vec::new();
        for policy in [".gitignore", ".ignore"] {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(&directory_path);
            match open_at(directory, std::ffi::OsStr::new(policy), false) {
                Ok(policy_file) => {
                    let bytes =
                        read_source_file(policy_file, device)?.ok_or(SearchFailure::Unavailable)?;
                    let text =
                        std::str::from_utf8(&bytes).map_err(|_| SearchFailure::Unavailable)?;
                    for line in text.lines() {
                        check_deadline(limits)?;
                        builder
                            .add_line(Some(directory_path.join(policy)), line)
                            .map_err(|_| SearchFailure::Unavailable)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SearchFailure::Unavailable),
            }
            local.push(builder.build().map_err(|_| SearchFailure::Unavailable)?);
        }
        policies.push(local);
        if let Some(name) = names.get(depth) {
            directory_path.push(name);
        }
    }
    let mut admitted = true;
    let mut candidate = root.to_path_buf();
    for (depth, name) in names.iter().enumerate() {
        candidate.push(name);
        let is_dir = depth + 1 < names.len() || final_directory;
        // .ignore outranks .gitignore; nearest ancestor wins within each class.
        let matched = [1, 0].into_iter().find_map(|kind| {
            policies[..=depth].iter().rev().find_map(|local| {
                let matched = local[kind].matched(&candidate, is_dir);
                (!matched.is_none()).then_some(matched)
            })
        });
        if matched.is_some_and(|matched| matched.is_ignore()) {
            admitted = false;
        }
    }
    Ok(admitted)
}

fn check_deadline(limits: &SearchLimits) -> Result<(), SearchFailure> {
    if Instant::now() >= limits.deadline {
        Err(SearchFailure::Deadline)
    } else {
        Ok(())
    }
}

fn open_at(parent: &File, name: &std::ffi::OsStr, directory: bool) -> std::io::Result<File> {
    let name = CString::new(name.as_encoded_bytes()).map_err(std::io::Error::other)?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent is live and name is a NUL-terminated single component.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a fresh descriptor owned here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Open a validated project-relative path component by component, without links
/// or blocking nonregular opens, retaining every directory that owns the leaf.
pub(super) fn open_source(
    mut parent: File,
    relative: &Path,
    device: u64,
    limits: &SearchLimits,
    final_directory: bool,
) -> Result<(File, Vec<File>), SearchFailure> {
    let mut directories = Vec::new();
    let mut parts = relative
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .peekable();
    while let Some(Component::Normal(name)) = parts.next() {
        check_deadline(limits)?;
        directories.push(parent.try_clone().map_err(|_| SearchFailure::Unavailable)?);
        let directory = parts.peek().is_some() || final_directory;
        let opened = open_at(&parent, name, directory).map_err(|_| SearchFailure::Unavailable)?;
        let metadata = opened.metadata().map_err(|_| SearchFailure::Unavailable)?;
        if metadata.dev() != device
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(SearchFailure::Unavailable);
        }
        parent = opened;
    }
    Ok((parent, directories))
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
    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        self.consider(item.span());
        visit::visit_foreign_item(self, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_namespace_replacement_and_policy_aba_fail_closed() {
        let fixture = std::env::current_dir()
            .expect("cwd")
            .join("target")
            .join(format!("extract-namespace-{}", std::process::id()));
        std::fs::create_dir_all(&fixture).expect("fixture");
        let root = fixture.join("root");
        let replacement = fixture.join("replacement");
        let held = fixture.join("held");
        let mut refused = Vec::new();
        for swap_ancestor in [false, true] {
            for restore in [false, true] {
                std::fs::create_dir_all(root.join("dir")).expect("root");
                std::fs::create_dir_all(replacement.join("dir")).expect("replacement");
                let relative = Path::new("dir/file.rs");
                std::fs::write(root.join(relative), "fn synthetic_private() {}\n").expect("source");
                let policy = if swap_ancestor {
                    root.join("dir/.gitignore")
                } else {
                    root.join(".gitignore")
                };
                std::fs::write(policy, "file.rs\n").expect("policy");
                std::fs::write(replacement.join(relative), "fn allowed() {}\n")
                    .expect("replacement source");
                let root_file = File::open(&root).expect("root fd");
                let device = root_file.metadata().expect("metadata").dev();
                let limits = SearchLimits {
                    deadline: Instant::now() + std::time::Duration::from_secs(8),
                    max_results: 1,
                    language: None,
                    ignores: Vec::new(),
                };
                let (original, other) = if swap_ancestor {
                    (root.join("dir"), replacement.join("dir"))
                } else {
                    (root.clone(), replacement.clone())
                };
                let result = admitted_source(
                    root_file,
                    &root,
                    relative,
                    device,
                    &limits,
                    |path, device, limits| {
                        std::fs::rename(&original, &held).expect("move original");
                        std::fs::rename(&other, &original).expect("install replacement");
                        let admitted = walk(path, device, limits);
                        assert!(
                            admitted
                                .as_ref()
                                .expect("walk")
                                .contains(&root.join(relative)),
                            "controlled replacement must be admitted"
                        );
                        if restore {
                            std::fs::rename(&original, &other).expect("remove replacement");
                            std::fs::rename(&held, &original).expect("restore original");
                        }
                        admitted
                    },
                );
                refused.push(matches!(result, Err(SearchFailure::Unavailable)));
                for path in [&root, &replacement, &held] {
                    if path.exists() {
                        std::fs::remove_dir_all(path).expect("cleanup");
                    }
                }
            }
        }
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("file.rs"), "fn synthetic_private() {}\n").expect("source");
        for policy_name in [".gitignore", ".ignore"] {
            let policy = root.join(policy_name);
            std::fs::write(&policy, "file.rs\n").expect("policy");
            let root_file = File::open(&root).expect("root fd");
            let device = root_file.metadata().expect("metadata").dev();
            let limits = SearchLimits {
                deadline: Instant::now() + std::time::Duration::from_secs(8),
                max_results: 1,
                language: None,
                ignores: Vec::new(),
            };
            let result = admitted_source(
                root_file,
                &root,
                Path::new("file.rs"),
                device,
                &limits,
                |path, device, limits| {
                    std::fs::write(&policy, "").expect("temporary allow");
                    let admitted = walk(path, device, limits);
                    assert!(admitted
                        .as_ref()
                        .expect("walk")
                        .contains(&root.join("file.rs")));
                    std::fs::write(&policy, "file.rs\n").expect("restore policy");
                    admitted
                },
            );
            refused.push(matches!(result, Err(SearchFailure::Unavailable)));
            std::fs::remove_file(policy).expect("policy cleanup");
        }
        std::fs::remove_dir_all(fixture).expect("cleanup");
        assert_eq!(
            refused,
            vec![true; 6],
            "root/ancestor replacement, restored namespaces, and policy ABA must fail closed"
        );
    }

    #[test]
    fn extract_retained_namespace_rejects_detached_allowed_owners() {
        let fixture = std::env::current_dir()
            .expect("cwd")
            .join("target")
            .join(format!("extract-detached-{}", std::process::id()));
        std::fs::create_dir_all(&fixture).expect("fixture");
        let root = fixture.join("root");
        let replacement = fixture.join("replacement");
        let held = fixture.join("held");
        let mut refused = Vec::new();
        for ancestor in [false, true] {
            std::fs::create_dir_all(root.join("dir")).expect("root");
            std::fs::create_dir_all(replacement.join("dir")).expect("replacement");
            let relative = Path::new("dir/file.rs");
            std::fs::write(root.join(relative), "fn original_allowed() {}\n").expect("source");
            std::fs::write(replacement.join(relative), "fn replacement_allowed() {}\n")
                .expect("replacement source");
            let root_file = File::open(&root).expect("root fd");
            let device = root_file.metadata().expect("metadata").dev();
            let limits = SearchLimits {
                deadline: Instant::now() + std::time::Duration::from_secs(8),
                max_results: 1,
                language: None,
                ignores: Vec::new(),
            };
            let (original, other) = if ancestor {
                (root.join("dir"), replacement.join("dir"))
            } else {
                (root.clone(), replacement.clone())
            };
            let result = admitted_source(
                root_file,
                &root,
                relative,
                device,
                &limits,
                |path, device, limits| {
                    std::fs::rename(&original, &held).expect("move original");
                    std::fs::rename(&other, &original).expect("replace");
                    let admitted = walk(path, device, limits);
                    assert!(admitted
                        .as_ref()
                        .expect("walk")
                        .contains(&root.join(relative)));
                    admitted
                },
            );
            refused.push(matches!(result, Err(SearchFailure::Unavailable)));
            for path in [&root, &replacement, &held] {
                if path.exists() {
                    std::fs::remove_dir_all(path).expect("cleanup");
                }
            }
        }
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("file.rs"), "fn original_allowed() {}\n").expect("source");
        for policy_name in [".gitignore", ".ignore"] {
            let root_file = File::open(&root).expect("root fd");
            let device = root_file.metadata().expect("metadata").dev();
            let limits = SearchLimits {
                deadline: Instant::now() + std::time::Duration::from_secs(8),
                max_results: 1,
                language: None,
                ignores: Vec::new(),
            };
            let policy = root.join(policy_name);
            let result = admitted_source(
                root_file,
                &root,
                Path::new("file.rs"),
                device,
                &limits,
                |path, device, limits| {
                    let admitted = walk(path, device, limits);
                    assert!(admitted
                        .as_ref()
                        .expect("walk")
                        .contains(&root.join("file.rs")));
                    std::fs::write(&policy, "file.rs\n").expect("new denial policy");
                    admitted
                },
            );
            refused.push(matches!(result, Err(SearchFailure::Unavailable)));
            std::fs::remove_file(policy).expect("policy cleanup");
        }
        std::fs::remove_dir_all(fixture).expect("cleanup");
        assert_eq!(
            refused,
            vec![true; 4],
            "allowed policy does not authorize detached owners or newly denied source"
        );
    }

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
            open_source(proc_root, Path::new("version"), device, &limits, false),
            Err(SearchFailure::Unavailable)
        ));
    }
}
