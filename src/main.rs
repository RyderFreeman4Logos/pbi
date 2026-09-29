use pbi_rs::{verify_probe_locations, EvidenceError};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const VERSION: &str = "0.1.0";
const DEFAULT_TIMEOUT: &str = "540";
const DEFAULT_MAX_RESULTS: usize = 8;
const PROBE_OUTER_DEADLINE_SECONDS: u64 = 8;
const PROBE_CLEANUP_GRACE_MILLIS: u64 = 100;
const MAX_SCOPED_PROBE_TARGETS: usize = 16;
const MAX_PROBE_OUTPUT_BYTES: usize = 32 * 1024;

fn usage() {
    println!(
        "pbi-rs {VERSION} — Probe-backed source evidence\n\
         Usage: pbi-rs <question...>\n\
                pbi-rs search [--bm25] <query>\n\
                pbi-rs --message <question>\n\
                pbi-rs --debug-config\n\
         Default/search output is compact source-verified BM25 evidence; --bm25 relays raw Probe output."
    );
}

fn main() {
    let code = match run(env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("pbi-rs: {}", error.message);
            error.code
        }
    };
    std::process::exit(code);
}

struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

fn run(arguments: Vec<String>) -> Result<i32, CliError> {
    if arguments.is_empty() {
        usage();
        return Ok(2);
    }
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        usage();
        return Ok(0);
    }
    if arguments[0] == "--version" {
        println!("pbi-rs {VERSION}");
        return Ok(0);
    }
    if arguments[0] == "--debug-config" {
        println!(
            "probe_binary={}",
            env::var("PBI_RS_PROBE").unwrap_or_else(|_| "probe".to_owned())
        );
        println!("search_default=compact_verified_bm25_no_chat");
        println!("search_bm25_opt_in=--bm25_raw_no_llm_probe");
        println!("search_outer_deadline_seconds={PROBE_OUTER_DEADLINE_SECONDS}");
        println!("search_scoped_target_limit={MAX_SCOPED_PROBE_TARGETS}");
        println!("model_path=not_configured_adk_workflow_kit_seam_pending");
        println!("api_key=[REDACTED]");
        return Ok(0);
    }

    let (raw, query, timeout, max_results) = if arguments[0] == "search" {
        parse_search(&arguments[1..])?
    } else if arguments[0] == "--message" {
        let query = arguments[1..].join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    } else {
        let query = arguments.join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    };

    let root =
        env::current_dir().map_err(|_| CliError::failed("cannot determine repository root"))?;
    let output = invoke_probe(&root, &query, &timeout, max_results, raw)?;
    if raw {
        io::stdout()
            .write_all(&output.stdout)
            .map_err(|_| CliError::failed("cannot write Probe output"))?;
        io::stderr()
            .write_all(&output.stderr)
            .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
        return Ok(exit_status(&output));
    }
    if !output.status.success() {
        io::stderr()
            .write_all(&output.stderr)
            .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
        return Ok(exit_status(&output));
    }
    let probe_stdout = String::from_utf8_lossy(&output.stdout);
    let locations = verify_probe_locations(&probe_stdout, &root, &query, max_results)
        .map_err(evidence_cli_error)?;
    for location in locations {
        println!(
            "{}",
            location
                .display_relative(&root)
                .map_err(evidence_cli_error)?
        );
    }
    Ok(0)
}

fn parse_search(arguments: &[String]) -> Result<(bool, String, String, usize), CliError> {
    let mut raw = false;
    let mut timeout = DEFAULT_TIMEOUT.to_owned();
    let mut max_results = DEFAULT_MAX_RESULTS;
    let mut query_parts = Vec::new();
    let mut after_separator = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if after_separator {
            query_parts.push(argument.clone());
            index += 1;
            continue;
        }
        match argument.as_str() {
            "--" => {
                after_separator = true;
                index += 1;
            }
            "--bm25" => {
                raw = true;
                index += 1;
            }
            "--timeout" => {
                timeout = next_value(arguments, &mut index, "--timeout")?;
                validate_decimal(&timeout, "--timeout")?;
            }
            value if value.starts_with("--timeout=") => {
                timeout = value[10..].to_owned();
                validate_decimal(&timeout, "--timeout")?;
                index += 1;
            }
            "--max-results" => {
                let value = next_value(arguments, &mut index, "--max-results")?;
                max_results = value
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
            }
            value if value.starts_with("--max-results=") => {
                max_results = value[14..]
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!(
                    "unsupported search option: {value}"
                )));
            }
            value => {
                query_parts.push(value.to_owned());
                index += 1;
            }
        }
    }
    let query = query_parts.join(" ");
    if query.trim().is_empty() {
        return Err(CliError::usage("search query is required"));
    }
    Ok((raw, query, timeout, max_results))
}

fn next_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, CliError> {
    *index += 1;
    let value = arguments
        .get(*index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| CliError::usage(format!("{option} requires a value")))?;
    *index += 1;
    Ok(value)
}

fn validate_decimal(value: &str, option: &str) -> Result<(), CliError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CliError::usage(format!(
            "{option} must be a non-negative integer"
        )));
    }
    Ok(())
}

const PROBE_SCOPE_EXCLUDED_NAMES: [&str; 5] =
    [".git", "target", "drafts", "node_modules", "__pycache__"];

fn probe_scope_paths(root: &Path) -> Result<Vec<PathBuf>, CliError> {
    let entries = fs::read_dir(root)
        .map_err(|_| CliError::failed("cannot enumerate repository files for Probe"))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|_| CliError::failed("cannot enumerate repository files for Probe"))?;
        let file_type = entry
            .file_type()
            .map_err(|_| CliError::failed("cannot inspect repository files for Probe"))?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| PROBE_SCOPE_EXCLUDED_NAMES.contains(&name))
        {
            continue;
        }
        paths.push(entry.path());
        if paths.len() > MAX_SCOPED_PROBE_TARGETS {
            return Err(CliError::failed(
                "Probe scope exceeded the bounded target limit",
            ));
        }
    }
    paths.sort();
    Ok(paths)
}

fn probe_has_file_records(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|line| line.trim_start().starts_with("File: "))
}

fn probe_command(
    root: &Path,
    query: &str,
    timeout: &str,
    max_results: usize,
    raw: bool,
) -> Command {
    let probe = env::var_os("PBI_RS_PROBE").unwrap_or_else(|| "probe".into());
    let mut command = Command::new(probe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.current_dir(root).args([
        "search",
        "--timeout",
        timeout,
        "--max-results",
        &max_results.to_string(),
        "--ignore",
        "drafts",
        "--reranker",
        "bm25",
    ]);
    if !raw {
        command.args(["--format", "plain", "--dry-run"]);
    }
    command.args(["--", query]);
    command
}

#[cfg(unix)]
fn signal_probe_group(child: &Child, signal: &str) {
    let group = format!("-{}", child.id());
    let _ = Command::new("/bin/kill")
        .args([signal, "--", group.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn signal_probe_group(_child: &Child, _signal: &str) {}

fn wait_probe_child(child: &mut Child, deadline: Instant) -> Result<(ExitStatus, bool), CliError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, false)),
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

fn run_probe_command(mut command: Command, deadline: Instant) -> Result<Output, CliError> {
    if Instant::now() >= deadline {
        return Err(CliError {
            code: 124,
            message: "Probe query exceeded bounded deadline".to_owned(),
        });
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| CliError {
            code: 127,
            message: "probe is unavailable on PATH".to_owned(),
        })?;
    let Some(mut stdout_pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::failed("Probe stdout pipe was unavailable"));
    };
    let Some(mut stderr_pipe) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::failed("Probe stderr pipe was unavailable"));
    };
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout_pipe.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stderr_pipe.read_to_end(&mut output).map(|_| output)
    });
    let (status, timed_out) = wait_probe_child(&mut child, deadline)?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| CliError::failed("Probe stdout reader failed"))?
        .map_err(|_| CliError::failed("cannot read Probe output"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CliError::failed("Probe stderr reader failed"))?
        .map_err(|_| CliError::failed("cannot read Probe diagnostics"))?;
    if stdout.len().saturating_add(stderr.len()) > MAX_PROBE_OUTPUT_BYTES {
        return Err(CliError::failed("Probe output exceeded the bounded limit"));
    }
    if timed_out {
        return Err(CliError {
            code: 124,
            message: "Probe query exceeded bounded deadline".to_owned(),
        });
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn invoke_probe_scope(
    root: &Path,
    query: &str,
    timeout: &str,
    max_results: usize,
    paths: &[PathBuf],
    deadline: Instant,
) -> Result<Output, CliError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    for path in paths {
        let mut command = probe_command(root, query, timeout, max_results, false);
        command.arg(path);
        let output = run_probe_command(command, deadline)?;
        let success = output.status.success();
        if stdout
            .len()
            .saturating_add(stderr.len())
            .saturating_add(output.stdout.len())
            .saturating_add(output.stderr.len())
            > MAX_PROBE_OUTPUT_BYTES
        {
            return Err(CliError::failed("Probe output exceeded the bounded limit"));
        }
        stdout.extend_from_slice(&output.stdout);
        stderr.extend_from_slice(&output.stderr);
        status = Some(output.status);
        if !success {
            break;
        }
    }
    let Some(status) = status else {
        return Err(CliError::failed("Probe scope is empty"));
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn invoke_probe(
    root: &Path,
    query: &str,
    timeout: &str,
    max_results: usize,
    raw: bool,
) -> Result<Output, CliError> {
    let deadline = Instant::now() + Duration::from_secs(PROBE_OUTER_DEADLINE_SECONDS);
    let output = run_probe_command(
        probe_command(root, query, timeout, max_results, raw),
        deadline,
    )?;
    if raw || !output.status.success() || probe_has_file_records(&output.stdout) {
        return Ok(output);
    }

    let paths = probe_scope_paths(root)?;
    if paths.is_empty() {
        return Ok(output);
    }
    invoke_probe_scope(root, query, timeout, max_results, &paths, deadline)
}

fn exit_status(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

fn evidence_cli_error(error: EvidenceError) -> CliError {
    CliError::failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_process_is_killed_at_outer_deadline() {
        let mut command = Command::new("/usr/bin/sleep");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.arg("1");
        let result = run_probe_command(command, Instant::now() + Duration::from_millis(20));
        match result {
            Err(error) => assert_eq!(error.code, 124),
            Ok(_) => panic!("Probe exceeded its outer deadline"),
        }
    }
}
