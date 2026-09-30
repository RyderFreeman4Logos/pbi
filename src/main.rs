use pbi_rs::semantic::{
    admit_local_routes, investigate, local_route_publisher_from_cli_routes,
    local_route_publisher_from_environment, LocalModelRoute, SemanticAnswer, SemanticError,
    SemanticRouteError,
};
#[cfg(test)]
use pbi_rs::semantic::{AdmittedLocalModelRoute, DEFAULT_LOCAL_BASE_URL, DEFAULT_LOCAL_MODEL};
use pbi_rs::{verify_probe_evidence, EvidenceError, SourceEvidence};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(test)]
use workflow_adk::model_profiles::{FakeModelProfile, ModelProfileRegistry};
use workflow_adk::ModelRouteCancellation;
#[cfg(test)]
use workflow_adk::{
    ModelRole, ModelRouteAuthorization, ModelRouteCandidate, ModelRoutePublisher,
    ModelRouteSnapshot,
};

const VERSION: &str = "0.1.0";
const DEFAULT_TIMEOUT: &str = "540";
const DEFAULT_MAX_RESULTS: usize = 8;
const PROBE_OUTER_DEADLINE_SECONDS: u64 = 8;
const MESSAGE_OUTER_DEADLINE_SECONDS: u64 = 30;
const PROBE_CLEANUP_GRACE_MILLIS: u64 = 100;
const MAX_SCOPED_PROBE_TARGETS: usize = 16;
const MAX_PROBE_OUTPUT_BYTES: usize = 32 * 1024;

struct SearchOptions {
    timeout: String,
    max_results: usize,
    language: Option<String>,
    ignores: Vec<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT.to_owned(),
            max_results: DEFAULT_MAX_RESULTS,
            language: None,
            ignores: Vec::new(),
        }
    }
}

fn usage() {
    println!(
        "pbi-rs {VERSION} — Probe-backed source evidence\n\
         Usage: pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... <question...> [--json]\n\
                pbi-rs search [--bm25] [--timeout <SECONDS>] [--max-results <N>] [--language/-l <LANGUAGE>] [--ignore/-i <PATTERN>]... <query>\n\
                pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... --message <question> [--json]\n\
                pbi-rs --debug-config\n\
         Repeat --model-route in order for approved local candidates (maximum 8). Flags must precede the question. Names only; the credential broker resolves secrets. Search does not accept model routes. Positional questions use source-verified synthesis when explicitly opted in; search remains BM25-only and expands OWNER:MEMBER to OWNER MEMBER; --bm25 relays raw Probe output without that expansion. Search --help/-h relays native Probe help under the same bounded deadline. Legacy --reranker/-r operands are discarded; BM25 is always forced. Language is a single Probe language/alias; ignores are repeatable Probe patterns. Mandatory scope exclusions cannot be overridden. User-filtered searches remain rooted at CWD, without per-path fallback; -- preserves literal query operands."
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

#[cfg(test)]
type TestRoutePublisherFactory<'a> =
    dyn Fn(&[AdmittedLocalModelRoute]) -> Result<ModelRoutePublisher, SemanticRouteError> + 'a;

#[cfg(test)]
#[derive(Clone, Copy)]
enum TestRouteInjection<'a> {
    Publisher(&'a ModelRoutePublisher),
    Factory {
        build: &'a TestRoutePublisherFactory<'a>,
        deadline: Duration,
    },
}

fn parse_local_route_prefix(
    arguments: Vec<String>,
) -> Result<(Vec<String>, Vec<LocalModelRoute>), CliError> {
    let mut index = 0;
    let mut routes = Vec::new();
    while arguments
        .get(index)
        .is_some_and(|argument| argument == "--model-route")
    {
        if arguments.len().saturating_sub(index) < 4 {
            return Err(CliError::usage(
                "--model-route requires BASE_URL, MODEL, and CREDENTIAL_HANDLE_NAME",
            ));
        }
        routes.push(LocalModelRoute::new(
            arguments[index + 1].clone(),
            arguments[index + 2].clone(),
            arguments[index + 3].clone(),
        ));
        index += 4;
    }
    if arguments[index..]
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == "--model-route")
    {
        return Err(CliError::usage(
            "--model-route options must precede the command or question",
        ));
    }
    Ok((arguments[index..].to_vec(), routes))
}

fn debug_config_output() -> String {
    format!(
        "probe_binary={}\nsearch_default=compact_verified_bm25_no_chat\nsearch_bm25_opt_in=--bm25_raw_no_llm_probe\nsearch_outer_deadline_seconds={PROBE_OUTER_DEADLINE_SECONDS}\nsearch_scoped_target_limit={MAX_SCOPED_PROBE_TARGETS}\nmodel_path=adk_workflow_kit_authorized_route_snapshot\nmodel_opt_in_env=PBI_RS_ADK_ENABLE\nmodel_route_policy=approved_local_only\nmodel_route_snapshot=ordered_authorized_candidates_bounded_by_kit\nmodel_route_chain=repeatable_cli_routes_or_single_default\nmodel_route_credentials=handle_names_only_values_not_emitted\napi_key=[REDACTED]\n",
        env::var("PBI_RS_PROBE").unwrap_or_else(|_| "probe".to_owned())
    )
}

fn run(
    arguments: Vec<String>,
    #[cfg(test)] _test_route_injection: Option<TestRouteInjection<'_>>,
    #[cfg(test)] _semantic_output: &mut Vec<u8>,
) -> Result<i32, CliError> {
    let (arguments, route_specs) = parse_local_route_prefix(arguments)?;
    if arguments.is_empty() {
        return Err(CliError {
            code: 2,
            prefix: "pbi",
            message: "question is required; interactive mode is disabled".to_owned(),
        });
    }
    if arguments
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == "--help" || argument == "-h")
    {
        if arguments[0] == "search" {
            if !route_specs.is_empty() {
                return Err(CliError::usage(
                    "--model-route is only supported for semantic questions",
                ));
            }
            let root = env::current_dir()
                .map_err(|_| CliError::failed("cannot determine repository root"))?;
            let mut command = probe_base_command(&root);
            command.args(&arguments);
            let output = run_probe_command(
                command,
                Instant::now() + Duration::from_secs(PROBE_OUTER_DEADLINE_SECONDS),
            )?;
            return relay_probe_output(&output);
        }
        usage();
        return Ok(0);
    }
    if arguments[0] == "--version" || arguments[0] == "-V" {
        println!("pbi-rs {VERSION}");
        return Ok(0);
    }
    if arguments[0] == "--debug-config" {
        print!("{}", debug_config_output());
        return Ok(0);
    }

    if arguments[0] == "search" && !route_specs.is_empty() {
        return Err(CliError::usage(
            "--model-route is only supported for semantic questions",
        ));
    }
    let json_output = arguments[0] != "search" && arguments.iter().any(|arg| arg == "--json");
    let (raw, semantic, query, options) = if arguments[0] == "search" {
        let (raw, query, options) = parse_search(&arguments[1..])?;
        (raw, false, query, options)
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
        (false, true, query, SearchOptions::default())
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
        (false, true, query, SearchOptions::default())
    };

    let admitted_routes = if route_specs.is_empty() {
        None
    } else {
        Some(admit_local_routes(route_specs).map_err(route_cli_error)?)
    };
    let root = env::current_dir()
        .map_err(|_| CliError::failed("cannot determine repository root"))
        .and_then(|root| {
            fs::canonicalize(root)
                .map_err(|_| CliError::failed("cannot canonicalize repository root"))
        })?;
    #[cfg(test)]
    let deadline_duration = match _test_route_injection {
        Some(TestRouteInjection::Factory { deadline, .. }) => deadline,
        _ => Duration::from_secs(if semantic {
            MESSAGE_OUTER_DEADLINE_SECONDS
        } else {
            PROBE_OUTER_DEADLINE_SECONDS
        }),
    };
    #[cfg(not(test))]
    let deadline_duration = Duration::from_secs(if semantic {
        MESSAGE_OUTER_DEADLINE_SECONDS
    } else {
        PROBE_OUTER_DEADLINE_SECONDS
    });
    let deadline = Instant::now() + deadline_duration;
    let output = invoke_probe(&root, &query, &options, raw, deadline)?;
    if raw {
        return relay_probe_output(&output);
    }
    if !output.status.success() {
        io::stderr()
            .write_all(&output.stderr)
            .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
        return Ok(exit_status(&output));
    }
    let probe_stdout = String::from_utf8_lossy(&output.stdout);
    let report = verify_probe_evidence(&probe_stdout, &root, &query, options.max_results)
        .map_err(evidence_cli_error)?;
    if semantic {
        #[cfg(test)]
        let injected_publisher = match _test_route_injection {
            Some(TestRouteInjection::Publisher(publisher)) => Some(publisher),
            Some(TestRouteInjection::Factory { .. }) | None => None,
        };
        #[cfg(test)]
        let owned_publisher = match _test_route_injection {
            Some(TestRouteInjection::Factory { build, .. }) => {
                let routes = admitted_routes
                    .as_deref()
                    .ok_or_else(|| route_cli_error(SemanticRouteError::IncompleteConfig))?;
                Some(build(routes).map_err(route_cli_error)?)
            }
            Some(TestRouteInjection::Publisher(_)) => None,
            None => match admitted_routes.as_deref() {
                Some(routes) => {
                    local_route_publisher_from_cli_routes(routes).map_err(route_cli_error)?
                }
                None => local_route_publisher_from_environment().map_err(route_cli_error)?,
            },
        };
        #[cfg(not(test))]
        let owned_publisher = match admitted_routes.as_deref() {
            Some(routes) => {
                local_route_publisher_from_cli_routes(routes).map_err(route_cli_error)?
            }
            None => local_route_publisher_from_environment().map_err(route_cli_error)?,
        };
        #[cfg(test)]
        let publisher = injected_publisher.or(owned_publisher.as_ref());
        #[cfg(not(test))]
        let publisher = owned_publisher.as_ref();
        if let Some(publisher) = publisher {
            let cancellation = ModelRouteCancellation::new();
            let policy = publisher.policy(deadline);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| CliError::failed("semantic runtime could not be created"))?;
            let answer = runtime
                .block_on(investigate(
                    &query,
                    &root,
                    &report,
                    &policy,
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

fn parse_search(arguments: &[String]) -> Result<(bool, String, SearchOptions), CliError> {
    let mut raw = false;
    let mut options = SearchOptions::default();
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
            "--reranker" | "-r" => {
                // Legacy discards any next operand, even an invalid/empty option.
                // Probe remains forced to BM25; this is not reranker activation.
                index = (index + 2).min(arguments.len());
            }
            value if value.starts_with("--reranker=") => index += 1,
            "--timeout" => {
                options.timeout = next_value(arguments, &mut index, "--timeout")?;
                validate_decimal(&options.timeout, "--timeout")?;
            }
            value if value.starts_with("--timeout=") => {
                options.timeout = value[10..].to_owned();
                validate_decimal(&options.timeout, "--timeout")?;
                index += 1;
            }
            "--max-results" => {
                let value = next_value(arguments, &mut index, "--max-results")?;
                options.max_results = value
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if options.max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
            }
            value if value.starts_with("--max-results=") => {
                options.max_results = value[14..]
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if options.max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
                index += 1;
            }
            "--language" | "-l" | "--ignore" | "-i" => {
                let value = next_value(arguments, &mut index, argument)?;
                set_search_filter(&mut options, argument, value)?;
            }
            value if value.starts_with("--language=") || value.starts_with("--ignore=") => {
                if let Some((option, operand)) = value.split_once('=') {
                    set_search_filter(&mut options, option, operand.to_owned())?;
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
    let mut query = query_parts.join(" ");
    if query.trim().is_empty() {
        return Err(CliError::usage("search query is required"));
    }
    // Legacy verified search expands owner:member, while raw BM25 stays literal.
    if !raw {
        if let Some((owner, member)) = query.split_once(':') {
            let is_name = |name: &str| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
            };
            if is_name(owner) && is_name(member) {
                query = format!("{owner} {member}");
            }
        }
    }
    Ok((raw, query, options))
}

fn next_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, CliError> {
    // Search filters and numeric options share the same operand boundary.
    *index += 1;
    let value = arguments
        .get(*index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| CliError::usage(format!("{option} requires a value")))?;
    *index += 1;
    Ok(value)
}

fn set_search_filter(
    options: &mut SearchOptions,
    option: &str,
    value: String,
) -> Result<(), CliError> {
    if value.is_empty() {
        return Err(CliError::usage(format!("{option} requires a value")));
    }
    if option == "--ignore" || option == "-i" {
        options.ignores.push(value);
    } else {
        // Probe v0.6.0-rc339 src/cli.rs:159-177, not arbitrary forwarding.
        const LANGUAGES: &str = "rust rs javascript js jsx typescript ts tsx python py go c h cpp cc cxx hpp hxx java ruby rb php swift solidity sol crystal cr haskell hs lhs csharp cs yaml yml";
        if options.language.is_some()
            || !LANGUAGES.split_ascii_whitespace().any(|name| name == value)
        {
            return Err(CliError::usage(
                "--language requires one supported Probe language or alias",
            ));
        }
        options.language = Some(value);
    }
    Ok(())
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

fn probe_base_command(root: &Path) -> Command {
    let probe = env::var_os("PBI_RS_PROBE").unwrap_or_else(|| "probe".into());
    let mut command = Command::new(probe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.current_dir(root);
    command
}

fn probe_command(root: &Path, query: &str, options: &SearchOptions, raw: bool) -> Command {
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
    options: &SearchOptions,
    paths: &[PathBuf],
    deadline: Instant,
) -> Result<Output, CliError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    for path in paths {
        let mut command = probe_command(root, query, options, false);
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
    options: &SearchOptions,
    raw: bool,
    deadline: Instant,
) -> Result<Output, CliError> {
    let output = run_probe_command(probe_command(root, query, options, raw), deadline)?;
    // ponytail: filtered root-only search; explicit file fallback bypasses Probe ignores.
    // Preserve Probe's CWD-relative patterns rather than reimplement glob matching.
    if raw
        || options.language.is_some()
        || !options.ignores.is_empty()
        || !output.status.success()
        || probe_has_file_records(&output.stdout)
    {
        return Ok(output);
    }

    let paths = probe_scope_paths(root)?;
    if paths.is_empty() {
        return Ok(output);
    }
    invoke_probe_scope(root, query, options, &paths, deadline)
}

fn exit_status(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

fn relay_probe_output(output: &Output) -> Result<i32, CliError> {
    io::stdout()
        .write_all(&output.stdout)
        .map_err(|_| CliError::failed("cannot write Probe output"))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
    Ok(exit_status(output))
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
    use adk_rust::{
        AdkError, Content, ErrorCategory, ErrorComponent, Llm, LlmRequest, LlmResponse,
    };
    use serde_json::json;
    use std::fs;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::SystemTime;

    fn test_publisher(response: serde_json::Value) -> ModelRoutePublisher {
        let profile = FakeModelProfile::new("pbi-test", "1", "fake-model", [response.to_string()]);
        let registry = ModelProfileRegistry::new()
            .with_worker(profile)
            .expect("profile");
        let candidate = ModelRouteCandidate::new(ModelRole::Worker, "pbi-test", "1");
        let snapshot = ModelRouteSnapshot::new(
            registry,
            vec![candidate.clone()],
            ModelRouteAuthorization::new(vec![candidate]),
        )
        .expect("authorized test snapshot");
        ModelRoutePublisher::new(snapshot)
    }

    enum TestModelBehavior {
        RateLimited,
        Internal,
        Pending,
        Respond(String),
    }

    struct TestRouteLlm {
        calls: Arc<AtomicUsize>,
        behavior: TestModelBehavior,
    }

    #[adk_rust::async_trait]
    impl Llm for TestRouteLlm {
        fn name(&self) -> &str {
            "pbi-rs-cli-route-test"
        }

        async fn generate_content(
            &self,
            _request: LlmRequest,
            _stream: bool,
        ) -> adk_rust::Result<adk_rust::LlmResponseStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let response = match &self.behavior {
                TestModelBehavior::RateLimited => {
                    return Err(AdkError::new(
                        ErrorComponent::Model,
                        ErrorCategory::RateLimited,
                        "pbi-rs-test",
                        "pbi-rs-test",
                    ));
                }
                TestModelBehavior::Internal => {
                    return Err(AdkError::new(
                        ErrorComponent::Model,
                        ErrorCategory::Internal,
                        "pbi-rs-test",
                        "pbi-rs-test",
                    ));
                }
                TestModelBehavior::Pending => return std::future::pending().await,
                TestModelBehavior::Respond(response) => response.clone(),
            };
            Ok(Box::pin(adk_rust::futures::stream::iter([Ok(
                LlmResponse::new(Content::new("assistant").with_text(response)),
            )])))
        }
    }

    fn cli_route_publisher(
        routes: &[AdmittedLocalModelRoute],
        response: &str,
        authorize_first: bool,
        first_behavior: TestModelBehavior,
        first_calls: Arc<AtomicUsize>,
        second_calls: Arc<AtomicUsize>,
    ) -> Result<ModelRoutePublisher, SemanticRouteError> {
        if routes.len() != 2 {
            return Err(SemanticRouteError::IncompleteConfig);
        }
        let mut registry = ModelProfileRegistry::new()
            .with_worker(FakeModelProfile::new(
                routes[0].profile_name(),
                "1",
                routes[0].model(),
                [response],
            ))
            .map_err(|_| SemanticRouteError::Profile)?;
        registry
            .register(FakeModelProfile::new(
                routes[1].profile_name(),
                "1",
                routes[1].model(),
                [response],
            ))
            .map_err(|_| SemanticRouteError::Profile)?;
        let candidates = routes
            .iter()
            .map(AdmittedLocalModelRoute::candidate)
            .collect::<Vec<_>>();
        let authorized = if authorize_first {
            candidates.clone()
        } else {
            vec![candidates[1].clone()]
        };
        let snapshot = ModelRouteSnapshot::new(
            registry,
            candidates.clone(),
            ModelRouteAuthorization::new(authorized),
        )
        .map_err(|_| SemanticRouteError::Profile)?
        .with_test_llm(
            candidates[0].clone(),
            Arc::new(TestRouteLlm {
                calls: first_calls,
                behavior: first_behavior,
            }),
        )
        .map_err(|_| SemanticRouteError::Profile)?
        .with_test_llm(
            candidates[1].clone(),
            Arc::new(TestRouteLlm {
                calls: second_calls,
                behavior: TestModelBehavior::Respond(response.to_owned()),
            }),
        )
        .map_err(|_| SemanticRouteError::Profile)?;
        Ok(ModelRoutePublisher::new(snapshot))
    }

    fn cli_route_arguments(question: &str) -> Vec<String> {
        [
            "--model-route",
            DEFAULT_LOCAL_BASE_URL,
            DEFAULT_LOCAL_MODEL,
            "CLIPROXY_API_KEY",
            "--model-route",
            "http://gb10:18009/v1",
            "abliterated-qwen-latest-27b-low",
            "OPENAI_API_KEY",
            "--message",
            question,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn assert_semantic_message_output(output: &[u8], answer: &str) {
        let output = String::from_utf8(output.to_vec()).expect("semantic message output");
        assert!(output.starts_with("Stage: semantic_adk_model\nInvocation attestation: sha256:"));
        assert!(output.contains(&format!("Answer: {answer}\n")));
        assert!(output.contains("Uncertainty: Only the verified source span was inspected.\n"));
        assert!(output
            .contains("- receipt.py:1 | target=exact_reuse_receipt | symbol=exact_reuse_receipt"));
        assert!(output.ends_with("  1: def exact_reuse_receipt():\n"));
    }

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
            r##"#!/bin/sh
printf '%s\n' "$@" > "$PWD/probe.args"
printf 'File: %s/receipt.py, Lines: 1-2\n' "$PWD"
"##,
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
        let search_route = test_publisher(json!({
            "answer":"search must not synthesize",
            "uncertainty":"This is a no-model control.",
            "citations":[{"path":"receipt.py","start_line":1,"end_line":1}]
        }));
        let mut search_output = Vec::new();
        assert!(matches!(
            run(
                vec!["search".to_owned(), question.clone()],
                Some(TestRouteInjection::Publisher(&search_route)),
                &mut search_output
            ),
            Ok(0)
        ));
        assert!(search_output.is_empty(), "search must not invoke synthesis");

        let route_question = vec![
            "--model-route".to_owned(),
            DEFAULT_LOCAL_BASE_URL.to_owned(),
            DEFAULT_LOCAL_MODEL.to_owned(),
            "CLIPROXY_API_KEY".to_owned(),
            "--message".to_owned(),
            question.clone(),
        ];
        let route_publisher = test_publisher(json!({
            "answer": answer,
            "uncertainty": "Only the verified source span was inspected.",
            "citations": [{"path": "receipt.py", "start_line": 1, "end_line": 1}]
        }));
        let mut route_output = Vec::new();
        assert!(matches!(
            run(
                route_question,
                Some(TestRouteInjection::Publisher(&route_publisher)),
                &mut route_output
            ),
            Ok(0)
        ));
        let probe_arguments = fs::read_to_string(root.join("probe.args")).expect("probe args");
        assert!(!probe_arguments.contains(DEFAULT_LOCAL_BASE_URL));
        assert!(!probe_arguments.contains(DEFAULT_LOCAL_MODEL));
        assert!(!probe_arguments.contains("CLIPROXY_API_KEY"));
        assert!(probe_arguments.ends_with(&format!("--\n{question}\n")));
        assert_semantic_message_output(&route_output, answer);

        let semantic_response = json!({
            "answer": answer,
            "uncertainty": "Only the verified source span was inspected.",
            "citations": [{"path": "receipt.py", "start_line": 1, "end_line": 1}]
        })
        .to_string();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let fallback_factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                &semantic_response,
                true,
                TestModelBehavior::RateLimited,
                first_calls.clone(),
                second_calls.clone(),
            )
        };
        let mut fallback_output = Vec::new();
        assert_eq!(
            run(
                cli_route_arguments(&question),
                Some(TestRouteInjection::Factory {
                    build: &fallback_factory,
                    deadline: Duration::from_secs(30),
                }),
                &mut fallback_output,
            )
            .unwrap_or_else(|_| panic!("authorized local route fallback failed")),
            0
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
        assert_semantic_message_output(&fallback_output, answer);
        let route_arguments = fs::read_to_string(root.join("probe.args")).expect("probe args");
        for secretish in [
            DEFAULT_LOCAL_BASE_URL,
            DEFAULT_LOCAL_MODEL,
            "CLIPROXY_API_KEY",
            "http://gb10:18009/v1",
            "abliterated-qwen-latest-27b-low",
            "OPENAI_API_KEY",
        ] {
            assert!(!route_arguments.contains(secretish));
        }

        fs::remove_file(root.join("probe.args")).expect("clear probe args");
        let invalid_factory_calls = Arc::new(AtomicUsize::new(0));
        let invalid_factory = |routes: &[AdmittedLocalModelRoute]| {
            invalid_factory_calls.fetch_add(1, Ordering::SeqCst);
            cli_route_publisher(
                routes,
                &semantic_response,
                true,
                TestModelBehavior::RateLimited,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            )
        };
        let mut unapproved_later = cli_route_arguments(&question);
        unapproved_later[6].clear();
        let mut rejected_output = Vec::new();
        let rejected = run(
            unapproved_later,
            Some(TestRouteInjection::Factory {
                build: &invalid_factory,
                deadline: Duration::from_secs(30),
            }),
            &mut rejected_output,
        )
        .expect_err("later unapproved model must reject the entire route set");
        assert_eq!(rejected.code, 78);
        assert_eq!(invalid_factory_calls.load(Ordering::SeqCst), 0);
        assert!(!root.join("probe.args").exists());
        assert!(rejected_output.is_empty());

        let internal_calls = Arc::new(AtomicUsize::new(0));
        let unused_fallback_calls = Arc::new(AtomicUsize::new(0));
        let internal_factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                &semantic_response,
                true,
                TestModelBehavior::Internal,
                internal_calls.clone(),
                unused_fallback_calls.clone(),
            )
        };
        let mut internal_output = Vec::new();
        assert!(run(
            cli_route_arguments(&question),
            Some(TestRouteInjection::Factory {
                build: &internal_factory,
                deadline: Duration::from_secs(30),
            }),
            &mut internal_output,
        )
        .is_err());
        assert_eq!(internal_calls.load(Ordering::SeqCst), 1);
        assert_eq!(unused_fallback_calls.load(Ordering::SeqCst), 0);
        assert!(internal_output.is_empty());

        let unauthorized_calls = Arc::new(AtomicUsize::new(0));
        let unauthorized_fallback_calls = Arc::new(AtomicUsize::new(0));
        let unauthorized_factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                &semantic_response,
                false,
                TestModelBehavior::RateLimited,
                unauthorized_calls.clone(),
                unauthorized_fallback_calls.clone(),
            )
        };
        let mut unauthorized_output = Vec::new();
        assert!(run(
            cli_route_arguments(&question),
            Some(TestRouteInjection::Factory {
                build: &unauthorized_factory,
                deadline: Duration::from_secs(30),
            }),
            &mut unauthorized_output,
        )
        .is_err());
        assert_eq!(unauthorized_calls.load(Ordering::SeqCst), 0);
        assert_eq!(unauthorized_fallback_calls.load(Ordering::SeqCst), 0);
        assert!(unauthorized_output.is_empty());

        let pending_calls = Arc::new(AtomicUsize::new(0));
        let deadline_fallback_calls = Arc::new(AtomicUsize::new(0));
        let pending_factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                &semantic_response,
                true,
                TestModelBehavior::Pending,
                pending_calls.clone(),
                deadline_fallback_calls.clone(),
            )
        };
        let deadline_start = Instant::now();
        let mut deadline_output = Vec::new();
        assert!(run(
            cli_route_arguments(&question),
            Some(TestRouteInjection::Factory {
                build: &pending_factory,
                deadline: Duration::from_millis(700),
            }),
            &mut deadline_output,
        )
        .is_err());
        assert!(deadline_start.elapsed() < Duration::from_secs(3));
        assert_eq!(pending_calls.load(Ordering::SeqCst), 1);
        assert_eq!(deadline_fallback_calls.load(Ordering::SeqCst), 0);
        assert!(deadline_output.is_empty());

        let incomplete_question = "where is exact_reuse_receipt and missing_target";
        let incomplete_probe = format!("File: {}, Lines: 1-1\n", root.join("receipt.py").display());
        let incomplete_report = verify_probe_evidence(
            &incomplete_probe,
            &root,
            incomplete_question,
            DEFAULT_MAX_RESULTS,
        )
        .expect("partial verified evidence");
        assert!(!incomplete_report.is_complete());
        assert_eq!(incomplete_report.missing_targets(), &["missing_target"]);
        let incomplete_route = test_publisher(json!({
            "answer":answer,
            "uncertainty":"Only the verified source span was inspected.",
            "citations":[{"path":"receipt.py","start_line":1,"end_line":1}]
        }));
        let mut incomplete_output = Vec::new();
        assert!(matches!(
            run(
                vec![incomplete_question.to_owned()],
                Some(TestRouteInjection::Publisher(&incomplete_route)),
                &mut incomplete_output
            ),
            Ok(0)
        ));
        assert_eq!(
            String::from_utf8(incomplete_output).expect("semantic output"),
            format!("{answer}\n")
        );

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
            let publisher = test_publisher(response);
            let mut output = Vec::new();
            let result = run(
                arguments.clone(),
                Some(TestRouteInjection::Publisher(&publisher)),
                &mut output,
            );
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
        let invalid_publisher = test_publisher(invalid);
        let mut invalid_output = Vec::new();
        assert!(matches!(
            run(
                vec![question.clone(), "--json".to_owned()],
                Some(TestRouteInjection::Publisher(&invalid_publisher)),
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
