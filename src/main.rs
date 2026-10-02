use pbi_rs::semantic::{
    admit_local_routes, investigate, local_route_publisher_from_cli_routes,
    local_route_publisher_from_environment, LocalModelRoute, SemanticAnswer, SemanticError,
    SemanticRouteError,
};
#[cfg(test)]
use pbi_rs::semantic::{
    explicit_admitted_routes_from_environment, local_route_publisher_from_admitted_routes,
    AdmittedLocalModelRoute, DEFAULT_LOCAL_BASE_URL, DEFAULT_LOCAL_MODEL, ENV_TEST_LOCK,
};
use pbi_rs::{verify_probe_evidence, SourceEvidence};

mod probe_scope;
use probe_scope::{
    evidence_cli_error, exit_status, invoke_probe, probe_base_command, relay_probe_output,
    run_probe_command, MAX_SCOPED_PROBE_TARGETS,
};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
#[cfg(test)]
use std::process::Command;
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

struct SearchOptions {
    timeout: String,
    max_results: usize,
    max_bytes: Option<String>,
    max_tokens: Option<String>,
    merge_threshold: Option<String>,
    help: bool,
    language: Option<String>,
    ignores: Vec<String>,
    format: Option<String>,
    files_only: bool,
    exact: bool,
    frequency: bool,
    exclude_filenames: bool,
    strict_elastic_syntax: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT.to_owned(),
            max_results: DEFAULT_MAX_RESULTS,
            max_bytes: None,
            max_tokens: None,
            merge_threshold: None,
            help: false,
            language: None,
            ignores: Vec::new(),
            format: None,
            files_only: false,
            exact: false,
            frequency: false,
            exclude_filenames: false,
            strict_elastic_syntax: false,
        }
    }
}

fn usage() {
    println!(
        "pbi-rs {VERSION} — Probe-backed source evidence\n\
         Usage: pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... <question...> [--json]\n\
                pbi-rs search [--bm25 [--files-only/-f] [--exact/-e] [--frequency/-s] [--exclude-filenames/-n] [--strict-elastic-syntax] [--format/-o <FORMAT>]] [--timeout <SECONDS>] [--max-results <N>] [--max-bytes <N>] [--max-tokens <N>] [--merge-threshold <N>] [--language/-l <LANGUAGE>] [--ignore/-i <PATTERN>]... <query>\n\
                pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... --message <question> [--json]\n\
                pbi-rs --debug-config\n\
         Repeat --model-route in order for approved local candidates (maximum 8). Flags must precede the question. Names only; the credential broker resolves secrets. Search does not accept model routes. Positional questions use source-verified synthesis when explicitly opted in; search remains BM25-only and expands OWNER:MEMBER to OWNER MEMBER; --bm25 relays raw Probe output without that expansion. Search --help/-h relays native Probe help under the same bounded deadline. Legacy --reranker/-r operands are discarded; BM25 is always forced. Search --question accepts one split/inline operand (including empty), consumed without inference because BM25 ignores it; BERT reranking is not enabled. Search --session refuses durable cache writes, not Chat resumability; ambient PROBE_SESSION_ID is removed from Probe children. Question --model-name/--force-provider operands (split or inline) are discarded, not activated. --message takes exactly one question operand; only --json and discarded routing options are supported afterward, not Chat sessions or arbitrary Chat flags. Positional -- preserves literal question text; top-level --help/-h must be first (search help may follow the command). Language is a single Probe language/alias; ignores are repeatable Probe patterns. Mandatory scope exclusions cannot be overridden. Filtered or budget/merge-controlled searches use the same bounded scope and pass those options to every Probe call. Code byte/token limits and merge distance accept zero and optional leading +, once per option, in split or inline syntax; they never raise wrapper deadline/output/citation caps. Probe limits code before merging, not the final formatted stream; verified evidence retains its own snippet limits. Search help is parsed after supported operand validation; -- preserves literal query operands."
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
    if arguments
        .get(index)
        .is_some_and(|argument| argument == "search")
        && arguments[index..]
            .iter()
            .take_while(|argument| argument.as_str() != "--")
            .any(|argument| argument == "--model-route")
    {
        return Err(CliError::usage(
            "--model-route is only supported for semantic questions",
        ));
    }
    // Non-prefix question routes are rejected after discard operands have been
    // consumed; an ignored legacy value may itself be --model-route.
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
    if matches!(arguments[0].as_str(), "--help" | "-h") {
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
    let (raw, semantic, query, options, json_output) = if arguments[0] == "search" {
        let (raw, query, options) = parse_search(&arguments[1..])?;
        if options.help {
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
        (raw, false, query, options, false)
    } else {
        let (query, json_output) = parse_question(&arguments)?;
        (false, true, query, SearchOptions::default(), json_output)
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
                let routes = match admitted_routes.as_ref() {
                    Some(routes) => routes.clone(),
                    None => explicit_admitted_routes_from_environment()
                        .map_err(route_cli_error)?
                        .ok_or_else(|| route_cli_error(SemanticRouteError::IncompleteConfig))?,
                };
                Some(build(&routes).map_err(route_cli_error)?)
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
    print_evidence(&report, &root)?;
    Ok(if report.is_complete() { 0 } else { 1 })
}

fn print_evidence(report: &pbi_rs::EvidenceReport, root: &Path) -> Result<(), CliError> {
    let mut output = Vec::new();
    for (index, item) in report.evidence().iter().enumerate() {
        let relative = item
            .location()
            .path()
            .strip_prefix(root)
            .map_err(|_| CliError::failed("source location is outside the repository"))?;
        let line = report
            .cited_line(index)
            .unwrap_or_else(|| item.location().start_line());
        writeln!(output, "{}:{line}", relative.to_string_lossy())
            .map_err(|_| CliError::failed("cannot write source evidence"))?;
    }
    io::stdout()
        .write_all(&output)
        .map_err(|_| CliError::failed("cannot write source evidence"))
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
        // No conversational session is stored. The ADK invocation identity is
        // not a session, and the validated answer has no provider token usage.
        let output = json!({"response": answer.answer(), "sessionId": null, "tokenUsage": null});
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

fn parse_question(arguments: &[String]) -> Result<(String, bool), CliError> {
    let message = arguments.first().is_some_and(|arg| arg == "--message");
    let mut parts = Vec::new();
    let mut index = 0;
    if message {
        // Legacy pbi:4769 captures exactly one question before parsing Chat args.
        if let Some(question) = arguments.get(1) {
            parts.push(question.as_str());
        }
        index = 2;
    }
    let mut literal = false;
    let mut json_output = false;
    while let Some(argument) = arguments.get(index) {
        match argument.as_str() {
            value if literal => parts.push(value),
            "--" if !message => literal = true,
            "--json" => json_output = true,
            "--model-name" | "--force-provider" => {
                // Discard exactly one operand if present, even option-looking.
                index += 1;
            }
            value
                if value.starts_with("--model-name=") || value.starts_with("--force-provider=") => {
            }
            value if message || value.starts_with('-') => {
                return Err(CliError::usage(format!(
                    "unsupported {} option or operand: {value}; only --json and discarded legacy routing options are supported; --model-route must precede the question",
                    if message { "Chat" } else { "question" }
                )));
            }
            value => parts.push(value),
        }
        index += 1;
    }
    let query = parts.join(" ");
    if query.trim().is_empty() {
        return Err(CliError::usage(
            "question is required; interactive mode is disabled",
        ));
    }
    Ok((query, json_output))
}

fn parse_search(arguments: &[String]) -> Result<(bool, String, SearchOptions), CliError> {
    let mut raw = false;
    let mut question_seen = false;
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
            "--help" | "-h" => {
                options.help = true;
                index += 1;
            }
            "--reranker" | "-r" => {
                // Legacy discards any next operand, even an invalid/empty option.
                // Probe remains forced to BM25; this is not reranker activation.
                index = (index + 2).min(arguments.len());
            }
            value if value.starts_with("--reranker=") => index += 1,
            value if value == "--session" || value.starts_with("--session=") => {
                return Err(CliError::usage(
                    "--session requires durable Probe cache writes; search session storage is not supported",
                ));
            }
            value if value == "--question" || value.starts_with("--question=") => {
                if question_seen {
                    return Err(CliError::usage("--question cannot be used multiple times"));
                }
                // Probe result_ranking.rs:138-145 uses question only with BERT.
                // BM25 compatibility consumes it without inference or forwarding.
                if value == "--question" {
                    next_value(arguments, &mut index, "--question")?;
                } else {
                    index += 1;
                }
                question_seen = true;
            }
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
            "--max-bytes" | "--max-tokens" | "--merge-threshold" => {
                let value = next_value(arguments, &mut index, argument)?;
                set_search_budget(&mut options, argument, value)?;
            }
            value
                if value.starts_with("--max-bytes=")
                    || value.starts_with("--max-tokens=")
                    || value.starts_with("--merge-threshold=") =>
            {
                if let Some((option, operand)) = value.split_once('=') {
                    set_search_budget(&mut options, option, operand.to_owned())?;
                }
                index += 1;
            }
            "--format" | "-o" => {
                let value = next_value(arguments, &mut index, argument)?;
                set_search_format(&mut options, value)?;
            }
            value if value.starts_with("--format=") || value.starts_with("-o") => {
                let operand = value
                    .strip_prefix("--format=")
                    .or_else(|| {
                        value
                            .strip_prefix("-o")
                            .map(|value| value.strip_prefix('=').unwrap_or(value))
                    })
                    .ok_or_else(|| CliError::usage("--format requires a value"))?;
                set_search_format(&mut options, operand.to_owned())?;
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
            "--files-only"
            | "-f"
            | "--exact"
            | "-e"
            | "--frequency"
            | "-s"
            | "--exclude-filenames"
            | "-n"
            | "--strict-elastic-syntax" => {
                set_raw_safe_flag(&mut options, argument)?;
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
    if !raw && !options.help {
        for (enabled, name) in [
            (options.files_only, "--files-only"),
            (options.exact, "--exact"),
            (options.frequency, "--frequency"),
            (options.exclude_filenames, "--exclude-filenames"),
            (options.strict_elastic_syntax, "--strict-elastic-syntax"),
        ] {
            if enabled {
                return Err(CliError::usage(format!(
                    "unsupported search option: {name}"
                )));
            }
        }
    }
    // Legacy pbi:4598 appends plain even to a user format. Installed Probe
    // rejects that duplicate; validate here rather than launching doomed work.
    if !raw && options.format.is_some() && !options.help {
        return Err(CliError::usage(
            "--format cannot be used multiple times; verified search requires plain",
        ));
    }
    if query.trim().is_empty() && !options.help {
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

fn set_raw_safe_flag(options: &mut SearchOptions, option: &str) -> Result<(), CliError> {
    let (slot, canonical) = match option {
        "--files-only" | "-f" => (&mut options.files_only, "--files-only"),
        "--exact" | "-e" => (&mut options.exact, "--exact"),
        "--frequency" | "-s" => (&mut options.frequency, "--frequency"),
        "--exclude-filenames" | "-n" => (&mut options.exclude_filenames, "--exclude-filenames"),
        "--strict-elastic-syntax" => (
            &mut options.strict_elastic_syntax,
            "--strict-elastic-syntax",
        ),
        _ => {
            return Err(CliError::usage(format!(
                "unsupported search option: {option}"
            )))
        }
    };
    if *slot {
        return Err(CliError::usage(format!(
            "{canonical} cannot be used multiple times"
        )));
    }
    *slot = true;
    Ok(())
}

fn set_search_format(options: &mut SearchOptions, value: String) -> Result<(), CliError> {
    if options.format.is_some() {
        return Err(CliError::usage("--format cannot be used multiple times"));
    }
    // Probe v0.6.0-rc339 src/cli.rs:211-214, not arbitrary forwarding.
    if !matches!(
        value.as_str(),
        "terminal" | "markdown" | "plain" | "json" | "xml" | "color" | "outline" | "outline-xml"
    ) {
        return Err(CliError::usage(
            "--format requires one supported Probe output format",
        ));
    }
    options.format = Some(value);
    Ok(())
}

fn set_search_budget(
    options: &mut SearchOptions,
    option: &str,
    value: String,
) -> Result<(), CliError> {
    // Probe's Option<usize> accepts zero and leading +, but not duplicates.
    value.parse::<usize>().map_err(|_| {
        CliError::usage(format!(
            "{option} must be a non-negative integer fitting usize"
        ))
    })?;
    let slot = match option {
        "--max-bytes" => &mut options.max_bytes,
        "--max-tokens" => &mut options.max_tokens,
        "--merge-threshold" => &mut options.merge_threshold,
        _ => return Err(CliError::usage("unsupported search budget option")),
    };
    if slot.is_some() {
        return Err(CliError::usage(format!(
            "{option} cannot be used multiple times"
        )));
    }
    *slot = Some(value);
    Ok(())
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
        Arc, Mutex,
    };
    use std::time::SystemTime;

    struct RouteConfigEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_dir: std::path::PathBuf,
        previous_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl RouteConfigEnvGuard {
        fn new(root: &std::path::Path, entries: &[(&'static str, Option<String>)]) -> Self {
            let lock = Self::lock();
            let previous_dir = env::current_dir().expect("cwd");
            let previous_env = entries
                .iter()
                .map(|(key, _)| (*key, env::var_os(key)))
                .collect();
            env::set_current_dir(root).expect("fixture cwd");
            for (key, value) in entries {
                if let Some(value) = value {
                    env::set_var(key, value);
                } else {
                    env::remove_var(key);
                }
            }
            Self {
                _lock: lock,
                previous_dir,
                previous_env,
            }
        }

        fn set(&self, key: &'static str, value: &std::path::Path) {
            let _held = &self._lock;
            env::set_var(key, value);
        }

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    impl Drop for RouteConfigEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.previous_env {
                if let Some(value) = value {
                    env::set_var(key, value);
                } else {
                    env::remove_var(key);
                }
            }
            let _ = env::set_current_dir(&self.previous_dir);
        }
    }

    fn owning_snapshot_publisher(
        routes: &[AdmittedLocalModelRoute],
        response: &str,
        calls: &Arc<AtomicUsize>,
        seen: &Arc<Mutex<Vec<String>>>,
    ) -> Result<ModelRoutePublisher, SemanticRouteError> {
        let route = routes.first().ok_or(SemanticRouteError::IncompleteConfig)?;
        let real = local_route_publisher_from_admitted_routes(routes)?;
        let policy = real.policy(Instant::now());
        let real_snapshot = policy.snapshot();
        let candidate = route.candidate();
        assert!(
            real_snapshot.candidates().contains(&candidate),
            "owning snapshot must keep the selected route identity"
        );
        let seen = Arc::clone(seen);
        let calls = Arc::clone(calls);
        let profile = FakeModelProfile::new(
            candidate.profile().name(),
            candidate.profile().version(),
            route.model(),
            [response.to_owned()],
        )
        .with_resolved_model(route.model());
        let registry = ModelProfileRegistry::new()
            .with_worker(profile)
            .map_err(|_| SemanticRouteError::Profile)?;
        let snapshot = ModelRouteSnapshot::new(
            registry,
            vec![candidate.clone()],
            ModelRouteAuthorization::new(vec![candidate.clone()]),
        )
        .map_err(|_| SemanticRouteError::Profile)?
        .with_test_llm(
            candidate,
            Arc::new(TestRouteLlm {
                calls,
                behavior: TestModelBehavior::Respond(response.to_owned()),
                seen: Some(seen),
            }),
        )
        .map_err(|_| SemanticRouteError::Profile)?;
        Ok(ModelRoutePublisher::new(snapshot))
    }

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
        seen: Option<Arc<Mutex<Vec<String>>>>,
    }

    #[adk_rust::async_trait]
    impl Llm for TestRouteLlm {
        fn name(&self) -> &str {
            "pbi-rs-cli-route-test"
        }

        async fn generate_content(
            &self,
            request: LlmRequest,
            _stream: bool,
        ) -> adk_rust::Result<adk_rust::LlmResponseStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(seen) = &self.seen {
                seen.lock().expect("seen models").push(request.model);
            }
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
        let test_profile = |route: &AdmittedLocalModelRoute, response: String| {
            FakeModelProfile::new(route.profile_name(), "1", "fake-model", [response])
        };
        let mut registry = ModelProfileRegistry::new()
            .with_worker(test_profile(&routes[0], response.to_owned()))
            .map_err(|_| SemanticRouteError::Profile)?;
        registry
            .register(test_profile(&routes[1], response.to_owned()))
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
                seen: None,
            }),
        )
        .map_err(|_| SemanticRouteError::Profile)?
        .with_test_llm(
            candidates[1].clone(),
            Arc::new(TestRouteLlm {
                calls: second_calls,
                behavior: TestModelBehavior::Respond(response.to_owned()),
                seen: None,
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
    fn env_toml_route_reaches_adk_snapshot_and_cli_route_wins() {
        let root = env::temp_dir().join(format!(
            "pbi-rs-route-contract-{}",
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

        let toml_path = root.join("models.toml");
        let selected_model = "abliterated-qwen-latest-27b-low";
        let decoy_model = "abliterated-qwen-latest-27b-none";
        fs::write(
            &toml_path,
            format!(
                "primary_model = \"{selected_model}\"\n\n[[endpoints]]\nmodel = \"{decoy_model}\"\nbase_url = \"http://localhost:18317/v1\"\n\n[[endpoints]]\nmodel = \"{selected_model}\"\nbase_url = \"http://gb10:18009/v1\"\n"
            ),
        )
        .expect("isolated config fixture");
        let _env = RouteConfigEnvGuard::new(
            &root,
            &[
                ("PBI_RS_ADK_ENABLE", Some("1".to_owned())),
                (
                    "PBI_RS_CREDENTIAL_HANDLE",
                    Some("CLIPROXY_API_KEY".to_owned()),
                ),
                (
                    "PBI_CONFIG_FILE",
                    Some(toml_path.to_string_lossy().into_owned()),
                ),
                ("PBI_RS_PROBE", Some(probe.to_string_lossy().into_owned())),
                ("LOCAL_MODEL", None),
                ("LLM_MODEL", None),
                ("CLIPROXY_BASE_URL", None),
                ("LOCAL_ROUTER_BASEURL", None),
            ],
        );

        let question = "where is exact_reuse_receipt?".to_owned();
        let answer = "Resolved route identity reached the ADK test model.";
        let response = json!({
            "answer": answer,
            "uncertainty": "Only the verified source span was inspected.",
            "citations": [{"path": "receipt.py", "start_line": 1, "end_line": 1}]
        })
        .to_string();
        let observed = Mutex::new(Vec::<(String, String)>::new());
        let env_calls = Arc::new(AtomicUsize::new(0));
        let env_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let env_factory = |routes: &[AdmittedLocalModelRoute]| {
            let route = routes.first().ok_or(SemanticRouteError::IncompleteConfig)?;
            observed
                .lock()
                .expect("route observations")
                .push((route.base_url().to_owned(), route.model().to_owned()));
            assert_eq!(routes.len(), 1, "owning env/TOML selector emits one route");
            owning_snapshot_publisher(routes, &response, &env_calls, &env_seen)
        };
        let mut env_output = Vec::new();
        assert!(matches!(
            run(
                vec!["--message".to_owned(), question.clone()],
                Some(TestRouteInjection::Factory {
                    build: &env_factory,
                    deadline: Duration::from_secs(30),
                }),
                &mut env_output,
            ),
            Ok(0)
        ));
        assert_semantic_message_output(&env_output, answer);
        assert_eq!(
            observed.lock().expect("route observations").as_slice(),
            &[("http://gb10:18009/v1".to_owned(), selected_model.to_owned())]
        );
        assert_eq!(env_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            env_seen.lock().expect("seen").as_slice(),
            &[selected_model.to_owned()]
        );

        let invalid_toml = root.join("invalid.toml");
        fs::write(&invalid_toml, "primary_model = [\n").expect("invalid config fixture");
        _env.set("PBI_CONFIG_FILE", &invalid_toml);
        let cli_calls = Arc::new(AtomicUsize::new(0));
        let cli_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let cli_factory = |routes: &[AdmittedLocalModelRoute]| {
            let route = routes.first().ok_or(SemanticRouteError::IncompleteConfig)?;
            observed
                .lock()
                .expect("route observations")
                .push((route.base_url().to_owned(), route.model().to_owned()));
            assert_eq!(routes.len(), 1, "owning CLI selector emits one route");
            owning_snapshot_publisher(routes, &response, &cli_calls, &cli_seen)
        };
        let cli_arguments = [
            "--model-route",
            DEFAULT_LOCAL_BASE_URL,
            DEFAULT_LOCAL_MODEL,
            "CLIPROXY_API_KEY",
            "--message",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(question.clone()))
        .collect();
        let mut cli_output = Vec::new();
        assert!(matches!(
            run(
                cli_arguments,
                Some(TestRouteInjection::Factory {
                    build: &cli_factory,
                    deadline: Duration::from_secs(30),
                }),
                &mut cli_output,
            ),
            Ok(0)
        ));
        assert_semantic_message_output(&cli_output, answer);
        assert_eq!(
            observed.lock().expect("route observations").as_slice(),
            &[
                ("http://gb10:18009/v1".to_owned(), selected_model.to_owned()),
                (
                    DEFAULT_LOCAL_BASE_URL.to_owned(),
                    DEFAULT_LOCAL_MODEL.to_owned()
                ),
            ]
        );
        assert_eq!(cli_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            cli_seen.lock().expect("seen").as_slice(),
            &[DEFAULT_LOCAL_MODEL.to_owned()]
        );

        let mut failed_output = Vec::new();
        let failure = run(
            vec!["--message".to_owned(), question],
            Some(TestRouteInjection::Factory {
                build: &env_factory,
                deadline: Duration::from_secs(30),
            }),
            &mut failed_output,
        )
        .expect_err("an explicitly malformed config must fail closed");
        assert_eq!(failure.code, 78);
        assert!(failed_output.is_empty());
        assert_eq!(
            observed.lock().expect("route observations").len(),
            2,
            "invalid config must be rejected before publisher construction"
        );

        drop(_env);
        fs::remove_dir_all(root).expect("remove isolated fixture");
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
        let _env = RouteConfigEnvGuard::new(
            &root,
            &[("PBI_RS_PROBE", Some(probe.to_string_lossy().into_owned()))],
        );
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
            .unwrap_or_else(|error| panic!(
                "authorized local route fallback failed: {}",
                error.message
            )),
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

        let mut override_question = cli_route_arguments(&question);
        override_question.extend(
            [
                "--model-name=unapproved-model",
                "--force-provider",
                "remote",
                "--json",
            ]
            .map(str::to_owned),
        );
        let override_factory = |routes: &[AdmittedLocalModelRoute]| {
            assert_eq!(routes[0].model(), DEFAULT_LOCAL_MODEL);
            assert_eq!(routes[1].model(), "abliterated-qwen-latest-27b-low");
            cli_route_publisher(
                routes,
                &semantic_response,
                true,
                TestModelBehavior::RateLimited,
                first_calls.clone(),
                second_calls.clone(),
            )
        };
        let mut override_output = Vec::new();
        assert_eq!(
            run(
                override_question,
                Some(TestRouteInjection::Factory {
                    build: &override_factory,
                    deadline: Duration::from_secs(30)
                }),
                &mut override_output
            )
            .unwrap_or_else(|error| panic!("override discard failed: {}", error.message)),
            0
        );
        assert!(fs::read_to_string(root.join("probe.args"))
            .expect("probe args")
            .ends_with(&format!("--\n{question}\n")));
        let parsed: serde_json::Value =
            serde_json::from_slice(&override_output).expect("explicit JSON");
        assert_eq!(parsed["response"], answer);
        assert!(parsed["tokenUsage"].is_null());
        assert_eq!(first_calls.load(Ordering::SeqCst), 2);
        assert_eq!(second_calls.load(Ordering::SeqCst), 2);

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
                    // No conversational session exists; invocation identity is not one.
                    assert!(parsed["sessionId"].is_null());
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
        for arguments in [
            vec![
                question.clone(),
                "--model-name".to_owned(),
                "--json".to_owned(),
            ],
            vec![question.clone(), "--".to_owned(), "--json".to_owned()],
        ] {
            let publisher = test_publisher(
                json!({"answer":answer,"uncertainty":"Only the verified source was inspected.","citations":[{"path":"receipt.py","start_line":1,"end_line":1}]}),
            );
            let mut output = Vec::new();
            assert!(matches!(
                run(
                    arguments,
                    Some(TestRouteInjection::Publisher(&publisher)),
                    &mut output
                ),
                Ok(0)
            ));
            assert_eq!(
                output,
                format!("{answer}\n").as_bytes(),
                "discarded/literal JSON must not activate output mode"
            );
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
        drop(_env);
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn route_config_guard_restores_env_and_cwd_on_unwind() {
        let root = env::temp_dir().join(format!(
            "pbi-rs-unwind-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("dir");
        let probe = root.join("probe");
        let outside = env::current_dir().expect("cwd");
        let saved_probe = env::var_os("PBI_RS_PROBE");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _env = RouteConfigEnvGuard::new(
                &root,
                &[("PBI_RS_PROBE", Some(probe.to_string_lossy().into_owned()))],
            );
            assert_eq!(env::current_dir().expect("cwd"), root);
            panic!("controlled unwind");
        }));
        assert!(result.is_err());
        let check = RouteConfigEnvGuard::lock();
        assert_eq!(env::current_dir().expect("cwd"), outside);
        assert_eq!(env::var_os("PBI_RS_PROBE"), saved_probe);
        drop(check);
        let _ = fs::remove_dir_all(root);
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
