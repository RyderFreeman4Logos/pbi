use pbi_rs::semantic::{
    investigate, local_binding_from_environment, SemanticAnswer, SemanticError, SemanticRouteError,
    DEFAULT_LOCAL_BASE_URL, DEFAULT_LOCAL_MODEL, MODEL_CREDENTIAL_HANDLES,
};
use pbi_rs::{verify_probe_evidence, EvidenceError, SourceEvidence};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(test)]
use workflow_adk::model_profiles::ModelBinding;

const VERSION: &str = "0.1.0";
const DEFAULT_TIMEOUT: &str = "540";
const DEFAULT_MAX_RESULTS: usize = 8;
const PROBE_OUTER_DEADLINE_SECONDS: u64 = 8;
const MESSAGE_OUTER_DEADLINE_SECONDS: u64 = 30;
const PROBE_CLEANUP_GRACE_MILLIS: u64 = 100;
const MAX_SCOPED_PROBE_TARGETS: usize = 16;
const MAX_PROBE_OUTPUT_BYTES: usize = 32 * 1024;

fn usage() {
    println!(
        "pbi-rs {VERSION} — Probe-backed source evidence\n\
         Usage: pbi-rs <question...> [--json]\n\
                pbi-rs search [--bm25] <query>\n\
                pbi-rs --message <question> [--json]\n\
                pbi-rs --debug-config\n\
         Positional questions use source-verified synthesis when explicitly opted in; search remains BM25-only; --bm25 relays raw Probe output."
    );
}

fn main() {
    let code = match run(
        env::args().skip(1).collect(),
        #[cfg(test)]
        None,
        #[cfg(test)]
        &mut Vec::new(),
    ) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}: {}", error.prefix, error.message);
            error.code
        }
    };
    std::process::exit(code);
}

struct CliError {
    code: i32,
    prefix: &'static str,
    message: String,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            prefix: "pbi-rs",
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            prefix: "pbi-rs",
            message: message.into(),
        }
    }

    fn compatibility_failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            prefix: "pbi",
            message: message.into(),
        }
    }
}

fn run(
    arguments: Vec<String>,
    #[cfg(test)] _injected_binding: Option<&ModelBinding>,
    #[cfg(test)] _semantic_output: &mut Vec<u8>,
) -> Result<i32, CliError> {
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
        println!("model_path=adk_workflow_kit_single_binding_opt_in");
        println!("model_opt_in_env=PBI_RS_ADK_ENABLE");
        println!("model_route_policy=approved_local_only");
        println!("model_default_base_url={DEFAULT_LOCAL_BASE_URL}");
        println!("model_default_name={DEFAULT_LOCAL_MODEL}");
        println!(
            "model_credential_handles={}",
            MODEL_CREDENTIAL_HANDLES.join(",")
        );
        println!("model_binding=single_immutable_snapshot");
        println!("model_route_chain=unavailable_in_pinned_adk_revision");
        println!("api_key=[REDACTED]");
        return Ok(0);
    }

    let json_output = arguments[0] != "search" && arguments.iter().any(|arg| arg == "--json");
    let (raw, semantic, query, timeout, max_results) = if arguments[0] == "search" {
        let (raw, query, timeout, max_results) = parse_search(&arguments[1..])?;
        (raw, false, query, timeout, max_results)
    } else if arguments[0] == "--message" {
        let query = arguments[1..]
            .iter()
            .filter(|arg| arg.as_str() != "--json")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            true,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    } else {
        let query = arguments
            .iter()
            .filter(|arg| arg.as_str() != "--json")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            true,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    };

    let root = env::current_dir()
        .map_err(|_| CliError::failed("cannot determine repository root"))
        .and_then(|root| {
            fs::canonicalize(root)
                .map_err(|_| CliError::failed("cannot canonicalize repository root"))
        })?;
    let deadline = Instant::now()
        + Duration::from_secs(if semantic {
            MESSAGE_OUTER_DEADLINE_SECONDS
        } else {
            PROBE_OUTER_DEADLINE_SECONDS
        });
    let output = invoke_probe(&root, &query, &timeout, max_results, raw, deadline)?;
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
    let report = verify_probe_evidence(&probe_stdout, &root, &query, max_results)
        .map_err(evidence_cli_error)?;
    if semantic {
        #[cfg(test)]
        let owned_binding = if _injected_binding.is_some() {
            None
        } else {
            local_binding_from_environment().map_err(route_cli_error)?
        };
        #[cfg(not(test))]
        let owned_binding = local_binding_from_environment().map_err(route_cli_error)?;
        let binding = owned_binding.as_ref();
        #[cfg(test)]
        let binding = _injected_binding.or(binding);
        if let Some(binding) = binding {
            let cancellation = AtomicBool::new(false);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| CliError::failed("semantic runtime could not be created"))?;
            let answer = runtime
                .block_on(investigate(
                    &query,
                    &root,
                    &report,
                    binding,
                    deadline,
                    &cancellation,
                ))
                .map_err(semantic_cli_error)?;
            #[cfg(test)]
            print_semantic(
                answer,
                &root,
                _semantic_output,
                arguments[0] != "--message",
                json_output,
            )?;
            #[cfg(not(test))]
            print_semantic(
                answer,
                &root,
                &mut io::stdout(),
                arguments[0] != "--message",
                json_output,
            )?;
            return Ok(0);
        }
    }
    print_evidence(report.evidence(), report.missing_targets(), &root)?;
    Ok(if report.is_complete() { 0 } else { 1 })
}

fn print_evidence(
    evidence: &[SourceEvidence],
    missing_targets: &[String],
    root: &Path,
) -> Result<(), CliError> {
    println!(
        "Coverage: {}",
        if missing_targets.is_empty() {
            "complete"
        } else {
            "incomplete"
        }
    );
    println!("Verified source evidence:");
    print_evidence_items(evidence, root, &mut io::stdout())?;
    if !missing_targets.is_empty() {
        println!("Missing targets:");
        for target in missing_targets {
            println!("- {target}");
        }
    }
    Ok(())
}

fn print_evidence_items(
    evidence: &[SourceEvidence],
    root: &Path,
    writer: &mut impl Write,
) -> Result<(), CliError> {
    for item in evidence {
        let location = item
            .location()
            .display_relative(root)
            .map_err(evidence_cli_error)?;
        let symbol = item.symbol().map_or_else(
            || "symbol=none".to_owned(),
            |symbol| format!("symbol={symbol}"),
        );
        writeln!(
            writer,
            "- {location} | target={} | {symbol} | {}",
            item.target(),
            item.relevance()
        )
        .map_err(|_| CliError::failed("cannot write source evidence"))?;
        for (offset, line) in item.snippet().lines().enumerate() {
            writeln!(
                writer,
                "  {}: {}",
                item.location().start_line() + offset,
                line
            )
            .map_err(|_| CliError::failed("cannot write source evidence"))?;
        }
    }
    Ok(())
}

fn print_semantic(
    answer: SemanticAnswer,
    root: &Path,
    writer: &mut impl Write,
    compact: bool,
    json_output: bool,
) -> Result<(), CliError> {
    if json_output {
        // Probe Chat's sessionId is replaced by this ADK invocation identity;
        // token usage is unavailable from the validated answer contract.
        let output = json!({"response": answer.answer(), "sessionId": answer.invocation_identity(), "tokenUsage": null});
        let mut bytes = serde_json::to_vec(&output)
            .map_err(|_| CliError::failed("cannot serialize semantic answer"))?;
        bytes.push(b'\n');
        return writer
            .write_all(&bytes)
            .map_err(|_| CliError::failed("cannot write semantic answer"));
    }
    if compact {
        return writeln!(writer, "{}", answer.answer())
            .map_err(|_| CliError::failed("cannot write semantic answer"));
    }
    let mut output = Vec::new();
    writeln!(output, "Stage: semantic_adk_model\nInvocation attestation: {}\nAnswer: {}\nUncertainty: {}\nVerified source evidence:", answer.invocation_identity(), answer.answer(), answer.uncertainty())
        .map_err(|_| CliError::failed("cannot write semantic answer"))?;
    print_evidence_items(answer.citations(), root, &mut output)?;
    writer
        .write_all(&output)
        .map_err(|_| CliError::failed("cannot write semantic answer"))
}

fn semantic_cli_error(error: SemanticError) -> CliError {
    CliError::failed(error.to_string())
}

fn route_cli_error(error: SemanticRouteError) -> CliError {
    CliError {
        code: 78,
        prefix: "pbi-rs",
        message: format!("semantic route denied: {error}"),
    }
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

fn run_probe_command(mut command: Command, deadline: Instant) -> Result<Output, CliError> {
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
    deadline: Instant,
) -> Result<Output, CliError> {
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
    if error == EvidenceError::NoSourceLocations {
        CliError::compatibility_failed(error.to_string())
    } else {
        CliError::failed(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::SystemTime;
    use workflow_adk::model_profiles::{CredentialBroker, FakeModelProfile, ModelProfileRegistry};

    #[test]
    fn positional_question_dispatches_through_adk_and_checks_citations() {
        let root = env::temp_dir().join(format!(
            "pbi-rs-answer-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            root.join("receipt.py"),
            "def exact_reuse_receipt():\n    return True\n",
        )
        .expect("source");
        let probe = root.join("probe");
        fs::write(
            &probe,
            "#!/bin/sh\nprintf 'File: %s/receipt.py, Lines: 1-2\\n' \"$PWD\"\n",
        )
        .expect("probe");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).expect("probe mode");
        let previous_dir = env::current_dir().expect("cwd");
        let previous_probe = env::var_os("PBI_RS_PROBE");
        env::set_current_dir(&root).expect("fixture cwd");
        env::set_var("PBI_RS_PROBE", &probe);
        let question = "where is exact_reuse_receipt?".to_owned();
        let answer = "The check is implemented by exact_reuse_receipt in receipt.py:1. Quoted: \"back\\slash\".";
        for (path, expected, arguments) in [
            ("receipt.py", true, vec![question.clone()]),
            (
                "receipt.py",
                true,
                vec![question.clone(), "--json".to_owned()],
            ),
            (
                "receipt.py",
                true,
                vec![
                    "--message".to_owned(),
                    question.clone(),
                    "--json".to_owned(),
                ],
            ),
            (
                "../outside.py",
                false,
                vec![question.clone(), "--json".to_owned()],
            ),
        ] {
            let response = json!({"answer":answer,"uncertainty":"Only the verified source was inspected.","citations":[{"path":path,"start_line":1,"end_line":1}]});
            let profile =
                FakeModelProfile::new("pbi-test", "1", "fake-model", [response.to_string()]);
            let binding = ModelProfileRegistry::new()
                .with_worker(profile)
                .expect("profile")
                .bind_worker(&CredentialBroker::new())
                .expect("binding");
            let mut output = Vec::new();
            let result = run(arguments.clone(), Some(&binding), &mut output);
            if expected {
                assert!(matches!(result, Ok(0)), "expected cited answer");
                if arguments.contains(&"--json".to_owned()) {
                    let parsed: serde_json::Value =
                        serde_json::from_slice(&output).expect("valid JSON with escaped answer");
                    assert_eq!(parsed["response"], answer);
                    assert!(parsed["sessionId"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty()));
                    assert!(parsed["tokenUsage"].is_null());
                } else {
                    assert_eq!(
                        String::from_utf8(output).expect("utf8"),
                        format!("{answer}\n")
                    );
                }
            } else {
                assert!(
                    matches!(result, Err(CliError { code: 1, .. })),
                    "forged citation must fail"
                );
                assert!(output.is_empty(), "forged answer must not leak");
            }
        }
        let invalid = json!({"answer":"", "uncertainty":"unknown", "citations":[{"path":"receipt.py","start_line":1,"end_line":1}]});
        let invalid_profile =
            FakeModelProfile::new("pbi-test", "1", "fake-model", [invalid.to_string()]);
        let invalid_binding = ModelProfileRegistry::new()
            .with_worker(invalid_profile)
            .expect("profile")
            .bind_worker(&CredentialBroker::new())
            .expect("binding");
        let mut invalid_output = Vec::new();
        assert!(matches!(
            run(
                vec![question.clone(), "--json".to_owned()],
                Some(&invalid_binding),
                &mut invalid_output
            ),
            Err(CliError { code: 1, .. })
        ));
        assert!(invalid_output.is_empty());
        let mut no_hit = Vec::new();
        let miss = run(
            vec!["unfindable_xyz".to_owned(), "--json".to_owned()],
            None,
            &mut no_hit,
        );
        assert!(matches!(
            miss,
            Err(CliError {
                code: 1,
                prefix: "pbi",
                ..
            })
        ));
        assert!(no_hit.is_empty());
        env::set_current_dir(previous_dir).expect("restore cwd");
        if let Some(value) = previous_probe {
            env::set_var("PBI_RS_PROBE", value);
        } else {
            env::remove_var("PBI_RS_PROBE");
        }
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[cfg(target_os = "linux")]
    fn process_identity(root: &Path, name: &str) -> (u32, u64) {
        let identity = fs::read_to_string(root.join(name)).expect("fixture process identity");
        let mut fields = identity.split_whitespace();
        (
            fields.next().expect("process id").parse().expect("pid"),
            fields
                .next()
                .expect("start time")
                .parse()
                .expect("start time"),
        )
    }

    #[cfg(target_os = "linux")]
    fn process_identity_is_running(pid: u32, start_time: u64) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return false;
        };
        let mut fields = fields.split_whitespace();
        let Some(state) = fields.next() else {
            return false;
        };
        let current_start = fields.nth(19).and_then(|value| value.parse::<u64>().ok());
        current_start == Some(start_time) && state != "Z"
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_process_cleans_descendants_holding_pipes_on_timeout_and_early_exit() {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("pbi-rs-process-tree-{suffix}"));
        fs::create_dir_all(&root).expect("fixture directory");

        let run_fixture = |name: &str, body: &str, deadline: Duration| {
            let script = format!(
                "trap '' TERM\nprintf '%s %s\\n' \"$$\" \"$(awk '{{print $22}}' /proc/$$/stat)\" > \"$PBI_RS_TEST_ROOT/root.identity\"\n{body}"
            );
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(script).env("PBI_RS_TEST_ROOT", &root);
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let started = Instant::now();
            let result = run_probe_command(command, Instant::now() + deadline);
            let elapsed = started.elapsed();
            let root_identity = process_identity(&root, "root.identity");
            let descendant_identity = process_identity(&root, "descendant.identity");
            assert!(
                !process_identity_is_running(root_identity.0, root_identity.1),
                "{name}: direct Probe child remained alive"
            );
            assert!(
                !process_identity_is_running(descendant_identity.0, descendant_identity.1),
                "{name}: Probe descendant remained alive"
            );
            (result, elapsed)
        };

        let (timeout_result, timeout_elapsed) = run_fixture(
            "timeout",
            "sleep 30 &\nchild=$!\nprintf '%s %s\\n' \"$child\" \"$(awk '{print $22}' /proc/$child/stat)\" > \"$PBI_RS_TEST_ROOT/descendant.identity\"\nwait \"$child\"",
            Duration::from_millis(100),
        );
        assert!(
            timeout_elapsed < Duration::from_millis(500),
            "timeout cleanup exceeded bound: {timeout_elapsed:?}"
        );
        assert!(matches!(timeout_result, Err(error) if error.code == 124));

        let (early_exit_result, early_exit_elapsed) = run_fixture(
            "early exit",
            "sleep 1 &\nchild=$!\nprintf '%s %s\\n' \"$child\" \"$(awk '{print $22}' /proc/$child/stat)\" > \"$PBI_RS_TEST_ROOT/descendant.identity\"\nexit 0",
            Duration::from_millis(100),
        );
        assert!(
            early_exit_elapsed < Duration::from_millis(500),
            "early-exit cleanup exceeded bound: {early_exit_elapsed:?}"
        );
        assert!(matches!(early_exit_result, Ok(output) if output.status.success()));

        let _ = fs::remove_dir_all(root);
    }
}
