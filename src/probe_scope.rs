use super::{CliError, SearchOptions};
use pbi_rs::EvidenceError;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const PROBE_CLEANUP_GRACE_MILLIS: u64 = 100;
pub(crate) const MAX_SCOPED_PROBE_TARGETS: usize = 16;
pub(crate) const MAX_PROBE_OUTPUT_BYTES: usize = 32 * 1024;

pub(crate) const PROBE_SCOPE_EXCLUDED_NAMES: [&str; 5] =
    [".git", "target", "drafts", "node_modules", "__pycache__"];

fn scope_skipped_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_none_or(|name| name.starts_with('.') || PROBE_SCOPE_EXCLUDED_NAMES.contains(&name))
}

fn confined_scope_path(root: &Path, path: &Path) -> Result<bool, CliError> {
    let failed = || CliError::failed("cannot inspect repository files for Probe");
    // Never open the path: a fifo or socket would block past the shared deadline.
    let root_metadata = fs::symlink_metadata(root).map_err(|_| failed())?;
    let path_metadata = fs::symlink_metadata(path).map_err(|_| failed())?;
    if path_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let regular = path_metadata.is_file() || path_metadata.is_dir();
    Ok(regular
        && root_metadata.dev() == path_metadata.dev()
        && path.starts_with(root)
        && fs::read_link(path).is_err())
}

struct ScopePlan {
    paths: Vec<PathBuf>,
    truncated: bool,
}

// Root entries share one 16-target ledger: files, docs, config, and directories.
// A directory is one target. Deeper files are reached by that directory, not by
// raising the cap. Truncation is incomplete coverage, not a full listing.
fn plan_scope(root: &Path) -> Result<ScopePlan, CliError> {
    let failed = || CliError::failed("cannot inspect repository files for Probe");
    let mut paths = Vec::new();
    let mut bounded = 0usize;
    let mut truncated = false;
    let mut children = fs::read_dir(root)
        .map_err(|_| failed())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| failed())?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let file_type = entry.file_type().map_err(|_| failed())?;
        if file_type.is_symlink() || scope_skipped_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        if !confined_scope_path(root, &path)? {
            continue;
        }
        if file_type.is_dir() || file_type.is_file() {
            if bounded == MAX_SCOPED_PROBE_TARGETS {
                truncated = true;
            } else {
                bounded += 1;
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(ScopePlan { paths, truncated })
}

fn probe_scope_paths(root: &Path) -> Result<ScopePlan, CliError> {
    plan_scope(root)
}

fn probe_has_file_records(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|line| line.trim_start().starts_with("File: "))
}

fn strip_ignored_file_records(root: &Path, stdout: &[u8], ignores: &[String]) -> Vec<u8> {
    if ignores.is_empty() || !probe_has_file_records(stdout) {
        return stdout.to_vec();
    }
    let text = String::from_utf8_lossy(stdout);
    let mut kept = String::new();
    let mut skip = false;
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("File: ") {
            let file = rest.split(", Lines:").next().unwrap_or(rest).trim();
            let path = Path::new(file);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            skip = ignored_scope_path(root, &path, ignores);
        }
        if skip {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept.into_bytes()
}

pub(crate) fn probe_base_command(root: &Path) -> Command {
    let probe = env::var_os("PBI_RS_PROBE").unwrap_or_else(|| "probe".into());
    let mut command = Command::new(probe);
    // Probe search_runner.rs:306-314 otherwise opens an implicit durable cache.
    command.env_remove("PROBE_SESSION_ID");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.current_dir(root);
    command
}

pub(crate) fn probe_command(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    raw: bool,
) -> Command {
    let mut command = probe_base_command(root);
    command.args([
        "search",
        "--timeout",
        &options.timeout,
        "--max-results",
        &options.max_results.to_string(),
        "--reranker",
        "bm25",
    ]);
    for (option, value) in [
        ("--max-bytes", &options.max_bytes),
        ("--max-tokens", &options.max_tokens),
        ("--merge-threshold", &options.merge_threshold),
    ] {
        if let Some(value) = value {
            command.args([option, value]);
        }
    }
    if let Some(language) = &options.language {
        command.args(["--language", language]);
    }
    for ignore in &options.ignores {
        command.arg(format!("--ignore={ignore}"));
    }
    // Probe's last matching override wins, including user negation patterns.
    for excluded in PROBE_SCOPE_EXCLUDED_NAMES {
        command.args(["--ignore", excluded]);
    }
    if !raw {
        command.args(["--format", "plain", "--dry-run"]);
    } else if let Some(format) = &options.format {
        command.args(["--format", format]);
    }
    command.args(["--", query]);
    command
}

#[cfg(unix)]
fn signal_probe_group_id(pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args([signal, "--", group.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn signal_probe_group(child: &Child, signal: &str) {
    signal_probe_group_id(child.id(), signal);
}

#[cfg(not(unix))]
fn signal_probe_group(_child: &Child, _signal: &str) {}

#[cfg(not(unix))]
fn signal_probe_group_id(_pid: u32, _signal: &str) {}

fn wait_probe_child(child: &mut Child, deadline: Instant) -> Result<(ExitStatus, bool), CliError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                signal_probe_group(child, "-TERM");
                return Ok((status, false));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                signal_probe_group(child, "-TERM");
                let cleanup_deadline =
                    Instant::now() + Duration::from_millis(PROBE_CLEANUP_GRACE_MILLIS);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => return Ok((status, true)),
                        Ok(None) if Instant::now() < cleanup_deadline => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                signal_probe_group(child, "-KILL");
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|_| CliError::failed("cannot reap Probe after timeout"))?;
                return Ok((status, true));
            }
            Err(_) => {
                signal_probe_group(child, "-TERM");
                signal_probe_group(child, "-KILL");
                let _ = child.kill();
                let _ = child.wait();
                return Err(CliError::failed("cannot wait for Probe"));
            }
        }
    }
}

fn read_probe_pipe(pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.take((MAX_PROBE_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut output)?;
    Ok(output)
}

fn receive_probe_output(
    receiver: &Receiver<(bool, io::Result<Vec<u8>>)>,
    stdout: &mut Option<io::Result<Vec<u8>>>,
    stderr: &mut Option<io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Result<bool, CliError> {
    while stdout.is_none() || stderr.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        match receiver.recv_timeout(remaining) {
            Ok((is_stdout, result)) if is_stdout => *stdout = Some(result),
            Ok((_, result)) => *stderr = Some(result),
            Err(RecvTimeoutError::Timeout) => return Ok(false),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CliError::failed("Probe output reader failed"));
            }
        }
    }
    Ok(true)
}

fn collect_probe_output(
    receiver: Receiver<(bool, io::Result<Vec<u8>>)>,
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
    group_id: u32,
) -> Result<(Vec<u8>, Vec<u8>), CliError> {
    signal_probe_group_id(group_id, "-TERM");
    let mut stdout = None;
    let mut stderr = None;
    let first_deadline = Instant::now() + Duration::from_millis(PROBE_CLEANUP_GRACE_MILLIS);
    let drained = match receive_probe_output(&receiver, &mut stdout, &mut stderr, first_deadline) {
        Ok(true) => true,
        Ok(false) => {
            signal_probe_group_id(group_id, "-KILL");
            let final_deadline = Instant::now() + Duration::from_millis(PROBE_CLEANUP_GRACE_MILLIS);
            receive_probe_output(&receiver, &mut stdout, &mut stderr, final_deadline)?
        }
        Err(error) => {
            signal_probe_group_id(group_id, "-KILL");
            return Err(error);
        }
    };
    signal_probe_group_id(group_id, "-KILL");
    if !drained {
        return Err(CliError::failed("cannot drain Probe output"));
    }
    stdout_reader
        .join()
        .map_err(|_| CliError::failed("Probe stdout reader failed"))?;
    stderr_reader
        .join()
        .map_err(|_| CliError::failed("Probe stderr reader failed"))?;
    let stdout = stdout.ok_or_else(|| CliError::failed("Probe stdout reader failed"))?;
    let stderr = stderr.ok_or_else(|| CliError::failed("Probe stderr reader failed"))?;
    let stdout = stdout.map_err(|_| CliError::failed("cannot read Probe output"))?;
    let stderr = stderr.map_err(|_| CliError::failed("cannot read Probe diagnostics"))?;
    Ok((stdout, stderr))
}

pub(crate) fn run_probe_command(
    mut command: Command,
    deadline: Instant,
) -> Result<Output, CliError> {
    if Instant::now() >= deadline {
        return Err(CliError {
            code: 124,
            prefix: "pbi-rs",
            message: "Probe query exceeded bounded deadline".to_owned(),
        });
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| CliError {
            code: 127,
            prefix: "pbi-rs",
            message: "probe is unavailable on PATH".to_owned(),
        })?;
    let group_id = child.id();
    let Some(stdout_pipe) = child.stdout.take() else {
        signal_probe_group_id(group_id, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::failed("Probe stdout pipe was unavailable"));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        signal_probe_group_id(group_id, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::failed("Probe stderr pipe was unavailable"));
    };
    let (sender, receiver) = mpsc::channel();
    let stdout_reader = thread::spawn({
        let sender = sender.clone();
        move || {
            let _ = sender.send((true, read_probe_pipe(stdout_pipe)));
        }
    });
    let stderr_reader = thread::spawn({
        let sender = sender.clone();
        move || {
            let _ = sender.send((false, read_probe_pipe(stderr_pipe)));
        }
    });
    drop(sender);
    let wait_result = wait_probe_child(&mut child, deadline);
    let output_result = collect_probe_output(receiver, stdout_reader, stderr_reader, group_id);
    let (status, timed_out) = match wait_result {
        Ok(result) => result,
        Err(error) => {
            let _ = output_result;
            return Err(error);
        }
    };
    let (stdout, stderr) = output_result?;
    if stdout.len().saturating_add(stderr.len()) > MAX_PROBE_OUTPUT_BYTES {
        return Err(CliError::failed("Probe output exceeded the bounded limit"));
    }
    if timed_out {
        return Err(CliError {
            code: 124,
            prefix: "pbi-rs",
            message: "Probe query exceeded bounded deadline".to_owned(),
        });
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn probe_output_is_relevant(output: &Output, root: &Path, query: &str, max_results: usize) -> bool {
    output.status.success()
        && pbi_rs::verify_probe_evidence(
            &String::from_utf8_lossy(&output.stdout),
            root,
            query,
            max_results,
        )
        .is_ok_and(|report| !report.evidence().is_empty())
}

fn ignored_scope_path(root: &Path, path: &Path, ignores: &[String]) -> bool {
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

fn ignored_directory_target(relative: &str, ignores: &[String]) -> bool {
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

struct ScopeLedger {
    targets: usize,
    bytes: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<ExitStatus>,
}

fn invoke_probe_scope(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    paths: &[PathBuf],
    deadline: Instant,
) -> Result<Output, CliError> {
    let mut ledger = ScopeLedger {
        targets: 1,
        bytes: 0,
        stdout: Vec::new(),
        stderr: Vec::new(),
        status: None,
    };
    for path in paths {
        if Instant::now() >= deadline {
            return Err(CliError {
                code: 124,
                prefix: "pbi-rs",
                message: "Probe query exceeded bounded deadline".to_owned(),
            });
        }
        if ignored_scope_path(root, path, &options.ignores) {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if path.is_dir() && ignored_directory_target(&relative, &options.ignores) {
                continue;
            }
        }
        if ledger.targets == MAX_SCOPED_PROBE_TARGETS {
            return Err(CliError::failed(
                "Probe scope exceeded the bounded target limit",
            ));
        }
        let mut command = probe_command(root, query, options, false);
        command.arg(path);
        let output = run_probe_command(command, deadline)?;
        let used = output.stdout.len().saturating_add(output.stderr.len());
        if ledger.bytes.saturating_add(used) > MAX_PROBE_OUTPUT_BYTES {
            return Err(CliError::failed("Probe output exceeded the bounded limit"));
        }
        ledger.targets += 1;
        ledger.bytes += used;
        ledger.status = Some(output.status);
        if !output.status.success() {
            return Ok(output);
        }
        ledger.stdout.extend_from_slice(&output.stdout);
        if !ledger.stdout.ends_with(b"\n") && !ledger.stdout.is_empty() {
            ledger.stdout.push(b'\n');
        }
        ledger.stdout = strip_ignored_file_records(root, &ledger.stdout, &options.ignores);
        ledger.stderr.extend_from_slice(&output.stderr);
        let retained = Output {
            status: output.status,
            stdout: ledger.stdout.clone(),
            stderr: ledger.stderr.clone(),
        };
        if probe_output_is_relevant(&retained, root, query, options.max_results)
            && pbi_rs::verify_probe_evidence(
                &String::from_utf8_lossy(&retained.stdout),
                root,
                query,
                options.max_results,
            )
            .is_ok_and(|report| report.is_complete())
        {
            return Ok(retained);
        }
    }
    let Some(status) = ledger.status else {
        return Err(CliError::compatibility_failed("no source locations found"));
    };
    Ok(Output {
        status,
        stdout: ledger.stdout,
        stderr: ledger.stderr,
    })
}

pub(crate) fn invoke_probe(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    raw: bool,
    deadline: Instant,
) -> Result<Output, CliError> {
    let output = run_probe_command(probe_command(root, query, options, raw), deadline)?;
    let had_records = probe_has_file_records(&output.stdout);
    let stdout = strip_ignored_file_records(root, &output.stdout, &options.ignores);
    let output = Output {
        status: output.status,
        stdout,
        stderr: output.stderr,
    };
    // Probe already applied the ignore on the root call. "No results" is final:
    // searching a child directory can cite an ignored path from another file.
    if !options.ignores.is_empty()
        && (probe_has_file_records(&output.stdout)
            || output
                .stdout
                .windows(11)
                .any(|window| window == b"No results"))
    {
        return Ok(output);
    }
    if had_records && !probe_has_file_records(&output.stdout) {
        return Ok(output);
    }
    // Zero output budgets stay root-only. Merge distance zero is not an output budget.
    let zero_output = [&options.max_bytes, &options.max_tokens]
        .iter()
        .any(|value| {
            value
                .as_deref()
                .is_some_and(|text| text.parse::<usize>() == Ok(0))
        });
    if raw || zero_output || !output.status.success() || probe_has_file_records(&output.stdout) {
        return Ok(output);
    }

    let plan = probe_scope_paths(root)?;
    if plan.paths.is_empty() {
        return Ok(output);
    }
    let scoped = invoke_probe_scope(root, query, options, &plan.paths, deadline)?;
    if plan.truncated {
        let covered = pbi_rs::verify_probe_evidence(
            &String::from_utf8_lossy(&scoped.stdout),
            root,
            query,
            options.max_results,
        )
        .is_ok_and(|report| report.is_complete());
        if !covered {
            return Err(CliError::failed(
                "Probe scope exceeded the bounded target limit",
            ));
        }
    }
    Ok(scoped)
}

pub(crate) fn exit_status(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

pub(crate) fn relay_probe_output(output: &Output) -> Result<i32, CliError> {
    io::stdout()
        .write_all(&output.stdout)
        .map_err(|_| CliError::failed("cannot write Probe output"))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
    Ok(exit_status(output))
}

pub(crate) fn evidence_cli_error(error: EvidenceError) -> CliError {
    if error == EvidenceError::NoSourceLocations {
        CliError::compatibility_failed(error.to_string())
    } else {
        CliError::failed(error.to_string())
    }
}
