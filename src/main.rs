use pbi_rs::semantic::{
    admit_local_routes, investigate, local_route_from_environment,
    local_route_publisher_from_cli_routes, local_route_publisher_from_environment,
    plan_search_query, AdmittedLocalModelRoute, LocalModelRoute, SemanticAnswer, SemanticError,
    SemanticRouteError,
};
#[cfg(test)]
use pbi_rs::semantic::{
    explicit_admitted_routes_from_environment, local_route_publisher_from_admitted_routes,
    DEFAULT_LOCAL_BASE_URL, DEFAULT_LOCAL_MODEL, ENV_TEST_LOCK,
};
use pbi_rs::{verify_probe_evidence, EvidenceError, SourceEvidence};

mod native_search;
mod raw_session;
use native_search::{
    candidate_symbols, search_raw_repository, search_repository, RawHit, RawSearchOptions,
    SearchFailure, SearchLimits,
};
use serde_json::json;
use std::cell::Cell;
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
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
const DEFAULT_MAX_RESULTS: usize = 8;
const SEARCH_OUTER_DEADLINE_SECONDS: u64 = 8;
const MESSAGE_OUTER_DEADLINE_SECONDS: u64 = 90;
const MAX_STAGE_ROWS: usize = 24;
const MAX_RAW_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
enum TraceStage {
    Route,
    InitialSearch,
    InitialVerify,
    Anchor,
    Candidates,
    Plan,
    RevisedSearch,
    RevisedVerify,
    Follow,
    Answer,
    Terminal,
}

impl TraceStage {
    fn label(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::InitialSearch => "initial_search",
            Self::InitialVerify => "initial_verify",
            Self::Anchor => "anchor",
            Self::Candidates => "candidates",
            Self::Plan => "plan",
            Self::RevisedSearch => "revised_search",
            Self::RevisedVerify => "revised_verify",
            Self::Follow => "follow",
            Self::Answer => "answer",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Clone, Copy)]
enum TraceStatus {
    Start,
    Ok,
    NoSource,
    Deadline,
    RouteError,
    InvalidOutput,
    OtherError,
}

impl TraceStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Ok => "ok",
            Self::NoSource => "no_source",
            Self::Deadline => "deadline",
            Self::RouteError => "route_error",
            Self::InvalidOutput => "invalid_output",
            Self::OtherError => "other_error",
        }
    }
}

struct StageTrace {
    enabled: bool,
    started: Instant,
    previous: Cell<Instant>,
    deadline: Instant,
    rows: Cell<usize>,
}

impl StageTrace {
    fn new(deadline: Instant) -> Self {
        let now = Instant::now();
        Self {
            enabled: env::var("PBI_RS_STAGE_TIMING").as_deref() == Ok("1"),
            started: now,
            previous: Cell::new(now),
            deadline,
            rows: Cell::new(0),
        }
    }

    fn point(&self, stage: TraceStage, status: TraceStatus, count: usize) {
        if !self.enabled || self.rows.get() >= MAX_STAGE_ROWS {
            return;
        }
        let now = Instant::now();
        eprintln!(
            "pbi-stage stage={} status={} elapsed_ms={} delta_ms={} remaining_ms={} count={}",
            stage.label(),
            status.label(),
            now.duration_since(self.started).as_millis(),
            now.duration_since(self.previous.replace(now)).as_millis(),
            self.deadline.saturating_duration_since(now).as_millis(),
            count,
        );
        self.rows.set(self.rows.get() + 1);
    }

    fn route(&self, index: usize, route: &AdmittedLocalModelRoute) {
        if !self.enabled || self.rows.get() >= MAX_STAGE_ROWS {
            return;
        }
        let endpoint = match route.base_url() {
            "http://gb10:18009/v1" => "gb10:18009",
            "http://localhost:18317/v1" => "localhost:18317",
            _ => "unapproved",
        };
        let model = match route.model() {
            "abliterated-qwen-latest-27b-none" => "none",
            "abliterated-qwen-latest-27b-low" => "low",
            "abliterated-qwen-latest-27b-medium" => "medium",
            _ => "unapproved",
        };
        eprintln!("pbi-stage stage=route_slot slot={index} endpoint={endpoint} model={model}");
        self.rows.set(self.rows.get() + 1);
    }
}

fn semantic_trace_status(error: &SemanticError) -> TraceStatus {
    match error {
        SemanticError::PlanningDeadlineExceeded | SemanticError::DeadlineExceeded => {
            TraceStatus::Deadline
        }
        SemanticError::Route { .. } => TraceStatus::RouteError,
        SemanticError::InvalidOutput | SemanticError::CitationMismatch => {
            TraceStatus::InvalidOutput
        }
        SemanticError::NoEvidence => TraceStatus::NoSource,
        _ => TraceStatus::OtherError,
    }
}

struct SearchOptions {
    timeout: Option<u64>,
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
    session: Option<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            timeout: None,
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
            session: None,
        }
    }
}

fn usage() {
    println!(
        "pbi-rs {VERSION} — bounded native source search and cited answers\n\
         Usage: pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... <question...> [--timeout <SECONDS>] [--json]\n\
                pbi-rs search [--bm25] [--timeout <SECONDS>] [--max-results <N>] [--language/-l <LANGUAGE>] [--ignore/-i <PATTERN>]... <query>\n\
                pbi-rs [--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>]... --message <question> [--timeout <SECONDS>] [--json]\n\
                pbi-rs --debug-config\n\
         Model routes require explicit local opt-in. Route arguments must precede the question; credential handles are names only. --timeout bounds the entire run in seconds (default: {MESSAGE_OUTER_DEADLINE_SECONDS} for answers, {SEARCH_OUTER_DEADLINE_SECONDS} for search). Search is read-only and bounded. Normal source citations are verified; --bm25 prints raw native ranked hits without citation verification or a model."
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
    PublisherWithEvidence {
        publisher: &'a ModelRoutePublisher,
        report: &'a pbi_rs::EvidenceReport,
    },
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

fn debug_config_output(route_specs: Vec<LocalModelRoute>) -> Result<String, SemanticRouteError> {
    let (base_url, model) = if route_specs.is_empty() {
        local_route_from_environment()?
    } else {
        let route = admit_local_routes(route_specs)?
            .into_iter()
            .next()
            .ok_or(SemanticRouteError::IncompleteConfig)?;
        (route.base_url().to_owned(), route.model().to_owned())
    };
    Ok(format!(
        "search_default=native_bounded_term_frequency_no_probe\nsearch_bm25_opt_in=native_bounded_raw_no_model\nsearch_outer_deadline_seconds={SEARCH_OUTER_DEADLINE_SECONDS}\nmodel_path=adk_workflow_kit_authorized_route_snapshot\nmodel_opt_in_env=PBI_RS_ADK_ENABLE\nmodel_route_policy=approved_local_only\nmodel_route_snapshot=ordered_authorized_candidates_bounded_by_kit\nmodel_route_chain=repeatable_cli_routes_or_single_default\nmodel_route_credentials=handle_names_only_values_not_emitted\nprimary_model={model}\nbase_url={base_url}\napi_key=[REDACTED]\n",
    ))
}

fn question_code_anchor_missing(question: &str, report: &pbi_rs::EvidenceReport) -> bool {
    if !question
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("why ")
    {
        return false;
    }
    // ponytail: exact snake_case anchors; broaden only if identifier-free questions miss often.
    question
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| word.len() >= 4 && word.contains('_'))
        .any(|word| {
            !report
                .evidence()
                .iter()
                .any(|item| item.symbol() == Some(word) || item.snippet().contains(word))
        })
}

// A field question needs the named type's declaration. Prose terms such as
// "fields" and "store" otherwise outrank that short declaration in a small
// bounded search window. Keep the question itself for the model invocation.
fn type_field_subject(question: &str) -> Option<&str> {
    let tokens = question
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if !tokens.iter().any(|token| {
        matches!(
            token.to_ascii_lowercase().as_str(),
            "field" | "fields" | "member" | "members"
        )
    }) {
        return None;
    }
    let mut subjects = tokens.into_iter().filter(|token| {
        token
            .chars()
            .next()
            .is_some_and(|character| character.is_uppercase())
            && !matches!(
                token.to_ascii_lowercase().as_str(),
                "what" | "which" | "how" | "does" | "do" | "the" | "a" | "an"
            )
    });
    let subject = subjects.next()?;
    subjects.next().is_none().then_some(subject)
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
        let output = debug_config_output(route_specs).map_err(route_cli_error)?;
        #[cfg(test)]
        let writer = _semantic_output;
        #[cfg(not(test))]
        let mut writer = io::stdout();
        writer
            .write_all(output.as_bytes())
            .map_err(|_| CliError::failed("cannot write debug configuration"))?;
        return Ok(0);
    }

    if arguments[0] == "search" && !route_specs.is_empty() {
        return Err(CliError::usage(
            "--model-route is only supported for semantic questions",
        ));
    }
    let (raw, semantic, query, options, json_output, requested_timeout) = if arguments[0]
        == "search"
    {
        let (raw, query, options) = parse_search(&arguments[1..])?;
        if options.help {
            println!(
                    "pbi-rs search is bounded and in-process.\n\
                 Supported: --timeout --max-results --language/-l --ignore/-i.\n\
                 --bm25 prints bounded native ranked hits. Raw --merge-threshold merges blocks separated by at most N lines (default 5). Raw --session ID paginates with private source-fresh state. Raw formats: plain, terminal, markdown, json, xml, color, outline, outline-xml. Raw --max-bytes caps emitted bytes; --max-tokens caps lexical output tokens."
                );
            return Ok(0);
        }
        let requested_timeout = options.timeout;
        (raw, false, query, options, false, requested_timeout)
    } else {
        let (query, json_output, requested_timeout) = parse_question(&arguments)?;
        (
            false,
            true,
            query,
            SearchOptions::default(),
            json_output,
            requested_timeout,
        )
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
    let deadline_duration = match (_test_route_injection, requested_timeout) {
        (_, Some(seconds)) => Duration::from_secs(seconds),
        (Some(TestRouteInjection::Factory { deadline, .. }), None) => deadline,
        _ => Duration::from_secs(if semantic {
            MESSAGE_OUTER_DEADLINE_SECONDS
        } else {
            SEARCH_OUTER_DEADLINE_SECONDS
        }),
    };
    #[cfg(not(test))]
    let deadline_duration = Duration::from_secs(requested_timeout.unwrap_or(if semantic {
        MESSAGE_OUTER_DEADLINE_SECONDS
    } else {
        SEARCH_OUTER_DEADLINE_SECONDS
    }));
    let deadline = Instant::now()
        .checked_add(deadline_duration)
        .ok_or_else(|| CliError::usage("--timeout is too large"))?;
    let trace = StageTrace::new(deadline);
    if let Some(routes) = admitted_routes.as_deref() {
        trace.point(TraceStage::Route, TraceStatus::Ok, routes.len());
        for (index, route) in routes.iter().enumerate() {
            trace.route(index, route);
        }
    }
    if raw {
        if options.strict_elastic_syntax {
            return Err(CliError::usage(
                "--strict-elastic-syntax has no verified historical query contract",
            ));
        }
        let session = options
            .session
            .as_deref()
            .map(|id| {
                let scope = raw_session_scope(&root, &query, &options)?;
                raw_session::RawSession::open(id, scope, deadline).map_err(CliError::failed)
            })
            .transpose()?;
        let (mut hits, freshness) = search_raw_repository(
            &root,
            &query,
            &SearchLimits {
                deadline,
                max_results: options.max_results,
                language: options.language.clone(),
                ignores: options.ignores.clone(),
            },
            &RawSearchOptions {
                exact: options.exact,
                exclude_filenames: options.exclude_filenames,
                merge_threshold: options
                    .merge_threshold
                    .as_deref()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(5),
            },
        )
        .map_err(search_cli_error)?;
        if options.files_only {
            let mut seen = std::collections::HashSet::new();
            hits.retain(|hit| seen.insert(hit.file.clone()));
        }
        if hits.is_empty() {
            return Err(evidence_cli_error(EvidenceError::NoSourceLocations));
        }
        let cursor = session
            .as_ref()
            .map(|state| state.cursor(freshness).map_err(CliError::failed))
            .transpose()?
            .unwrap_or(0);
        if cursor >= hits.len() {
            return Err(CliError::failed("raw search session exhausted"));
        }
        let end = cursor.saturating_add(options.max_results).min(hits.len());
        let (output, emitted) = render_raw_hits(&hits[cursor..end], &options, deadline)?;
        if let Some(state) = session.as_ref() {
            state
                .advance(freshness, cursor + emitted)
                .map_err(CliError::failed)?;
        }
        io::stdout()
            .write_all(&output)
            .map_err(|_| CliError::failed("cannot write raw search results"))?;
        if emitted < end - cursor {
            eprintln!(
                "pbi-rs: raw results truncated: emitted {emitted} of {} page hits",
                end - cursor
            );
        }
        trace.point(TraceStage::Terminal, TraceStatus::Ok, emitted);
        return Ok(0);
    }
    if options.format.is_some()
        || options.max_bytes.is_some()
        || options.max_tokens.is_some()
        || options.merge_threshold.is_some()
        || options.files_only
        || options.exact
        || options.frequency
        || options.exclude_filenames
        || options.strict_elastic_syntax
        || options.session.is_some()
    {
        return Err(CliError::usage(
            "raw search options require --bm25; verified search prints compact citations",
        ));
    }
    #[cfg(test)]
    let injected_publisher = match _test_route_injection {
        Some(TestRouteInjection::Publisher(publisher)) => Some(publisher),
        Some(TestRouteInjection::PublisherWithEvidence { publisher, .. }) => Some(publisher),
        Some(TestRouteInjection::Factory { .. }) | None => None,
    };
    #[cfg(test)]
    let owned_publisher = if semantic {
        match _test_route_injection {
            Some(TestRouteInjection::Factory { build, .. }) => {
                let routes = match admitted_routes.as_ref() {
                    Some(routes) => routes.clone(),
                    None => explicit_admitted_routes_from_environment()
                        .map_err(route_cli_error)?
                        .ok_or_else(|| route_cli_error(SemanticRouteError::IncompleteConfig))?,
                };
                Some(build(&routes).map_err(route_cli_error)?)
            }
            Some(TestRouteInjection::Publisher(_))
            | Some(TestRouteInjection::PublisherWithEvidence { .. }) => None,
            None => match admitted_routes.as_deref() {
                Some(routes) => local_route_publisher_from_cli_routes(routes, deadline_duration)
                    .map_err(route_cli_error)?,
                None => local_route_publisher_from_environment(deadline_duration)
                    .map_err(route_cli_error)?,
            },
        }
    } else {
        None
    };
    #[cfg(not(test))]
    let owned_publisher = if semantic {
        match admitted_routes.as_deref() {
            Some(routes) => local_route_publisher_from_cli_routes(routes, deadline_duration)
                .map_err(route_cli_error)?,
            None => local_route_publisher_from_environment(deadline_duration)
                .map_err(route_cli_error)?,
        }
    } else {
        None
    };
    #[cfg(test)]
    let publisher = injected_publisher.or(owned_publisher.as_ref());
    #[cfg(not(test))]
    let publisher = owned_publisher.as_ref();

    let explanatory_question = semantic && {
        let lower = query.trim_start().to_ascii_lowercase();
        lower.starts_with("why ") || lower.starts_with("how ")
    };
    let collect_evidence =
        |search_query: &str, initial: bool| -> Result<Option<pbi_rs::EvidenceReport>, CliError> {
            let evidence_query = if semantic && initial {
                type_field_subject(search_query).unwrap_or(search_query)
            } else {
                search_query
            };
            let (search_stage, verify_stage) = if initial {
                (TraceStage::InitialSearch, TraceStage::InitialVerify)
            } else {
                (TraceStage::RevisedSearch, TraceStage::RevisedVerify)
            };
            trace.point(search_stage, TraceStatus::Start, 0);
            let found = search_repository(
                &root,
                evidence_query,
                &SearchLimits {
                    deadline,
                    max_results: options.max_results,
                    language: options.language.clone(),
                    ignores: options.ignores.clone(),
                },
            )
            .map_err(|error| {
                let status = if matches!(error, SearchFailure::Deadline) {
                    TraceStatus::Deadline
                } else {
                    TraceStatus::OtherError
                };
                trace.point(search_stage, status, 0);
                search_cli_error(error)
            })?;
            trace.point(search_stage, TraceStatus::Ok, found.lines().count());
            // Leave room for called definitions needed to prove a why answer.
            let verify_limit = if explanatory_question {
                options
                    .max_results
                    .min(pbi_rs::semantic::MAX_SEMANTIC_EVIDENCE / 2)
            } else {
                options.max_results
            };
            if Instant::now() >= deadline {
                trace.point(verify_stage, TraceStatus::Deadline, 0);
                return Err(CliError::failed(
                    "source verification exceeded its bounded deadline",
                ));
            }
            let verified = verify_probe_evidence(&found, &root, evidence_query, verify_limit);
            if Instant::now() >= deadline {
                trace.point(verify_stage, TraceStatus::Deadline, 0);
                return Err(CliError::failed(
                    "source verification exceeded its bounded deadline",
                ));
            }
            match verified {
                Ok(report) => {
                    trace.point(verify_stage, TraceStatus::Ok, report.evidence().len());
                    Ok(Some(report))
                }
                Err(EvidenceError::NoSourceLocations) => {
                    trace.point(verify_stage, TraceStatus::NoSource, 0);
                    Ok(None)
                }
                Err(error) => {
                    trace.point(verify_stage, TraceStatus::OtherError, 0);
                    Err(evidence_cli_error(error))
                }
            }
        };
    #[cfg(test)]
    let report = match _test_route_injection {
        Some(TestRouteInjection::PublisherWithEvidence { report, .. }) => Some(report.clone()),
        _ => collect_evidence(&query, true)?,
    };
    #[cfg(not(test))]
    let report = collect_evidence(&query, true)?;
    let anchor_missing = report
        .as_ref()
        .is_some_and(|report| semantic && question_code_anchor_missing(&query, report));
    trace.point(
        TraceStage::Anchor,
        if anchor_missing {
            TraceStatus::NoSource
        } else {
            TraceStatus::Ok
        },
        usize::from(anchor_missing),
    );
    let report = report.filter(|_| !anchor_missing);
    let report = match report {
        Some(report) => report,
        None if semantic => {
            let Some(publisher) = publisher else {
                trace.point(TraceStage::Terminal, TraceStatus::NoSource, 0);
                return Err(evidence_cli_error(EvidenceError::NoSourceLocations));
            };
            let policy = publisher.policy(deadline);
            let cancellation = ModelRouteCancellation::new();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| CliError::failed("semantic runtime could not be created"))?;
            trace.point(TraceStage::Candidates, TraceStatus::Start, 0);
            let candidates = candidate_symbols(
                &root,
                &query,
                &SearchLimits {
                    deadline,
                    max_results: options.max_results,
                    language: options.language.clone(),
                    ignores: options.ignores.clone(),
                },
            )
            .map_err(|error| {
                let status = if matches!(error, SearchFailure::Deadline) {
                    TraceStatus::Deadline
                } else {
                    TraceStatus::OtherError
                };
                trace.point(TraceStage::Candidates, status, 0);
                search_cli_error(error)
            })?;
            trace.point(TraceStage::Candidates, TraceStatus::Ok, candidates.len());
            trace.point(TraceStage::Plan, TraceStatus::Start, 1);
            let revised = runtime
                .block_on(plan_search_query(
                    &query,
                    &candidates,
                    &policy,
                    deadline,
                    &cancellation,
                ))
                .map_err(|error| {
                    trace.point(TraceStage::Plan, semantic_trace_status(&error), 0);
                    semantic_cli_error(error)
                })?;
            let chosen_index = candidates
                .iter()
                .position(|(_, name)| *name == revised)
                .map_or(0, |index| index + 1);
            trace.point(TraceStage::Plan, TraceStatus::Ok, chosen_index);
            let search_query = candidates
                .iter()
                .find(|(_, name)| *name == revised)
                .and_then(|(path, _)| Path::new(path).file_stem().and_then(|stem| stem.to_str()))
                .map(|module| format!("{module}::{revised}"))
                .unwrap_or(revised);
            collect_evidence(&search_query, false)?.ok_or_else(|| {
                trace.point(TraceStage::Terminal, TraceStatus::NoSource, 0);
                evidence_cli_error(EvidenceError::NoSourceLocations)
            })?
        }
        None => {
            trace.point(TraceStage::Terminal, TraceStatus::NoSource, 0);
            return Err(evidence_cli_error(EvidenceError::NoSourceLocations));
        }
    };
    if semantic {
        trace.point(
            TraceStage::Follow,
            TraceStatus::Start,
            report.evidence().len(),
        );
        if Instant::now() >= deadline {
            trace.point(TraceStage::Follow, TraceStatus::Deadline, 0);
            return Err(CliError::failed(
                "semantic investigation exceeded its bounded deadline",
            ));
        }
        let expanded_report = if explanatory_question || type_field_subject(&query).is_some() {
            Some(
                report
                    .clone()
                    .with_following_lines(&root, pbi_rs::semantic::MAX_SEMANTIC_EVIDENCE)
                    .map_err(|error| {
                        trace.point(TraceStage::Follow, TraceStatus::OtherError, 0);
                        evidence_cli_error(error)
                    })?,
            )
        } else {
            None
        };
        if Instant::now() >= deadline {
            trace.point(TraceStage::Follow, TraceStatus::Deadline, 0);
            return Err(CliError::failed(
                "semantic investigation exceeded its bounded deadline",
            ));
        }
        let report = expanded_report.as_ref().unwrap_or(&report);
        trace.point(TraceStage::Follow, TraceStatus::Ok, report.evidence().len());
        if let Some(publisher) = publisher {
            let cancellation = ModelRouteCancellation::new();
            let policy = publisher.policy(deadline);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| CliError::failed("semantic runtime could not be created"))?;
            trace.point(TraceStage::Answer, TraceStatus::Start, 1);
            let answer = runtime
                .block_on(investigate(
                    &query,
                    &root,
                    report,
                    &policy,
                    deadline,
                    &cancellation,
                ))
                .map_err(|error| {
                    trace.point(TraceStage::Answer, semantic_trace_status(&error), 0);
                    semantic_cli_error(error)
                })?;
            trace.point(
                TraceStage::Answer,
                TraceStatus::Ok,
                answer.citations().len(),
            );
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
            trace.point(TraceStage::Terminal, TraceStatus::Ok, 0);
            return Ok(0);
        }
    }
    print_evidence(&report, &root)?;
    trace.point(
        TraceStage::Terminal,
        TraceStatus::Ok,
        report.evidence().len(),
    );
    Ok(if report.is_complete() { 0 } else { 1 })
}

fn evidence_cli_error(error: EvidenceError) -> CliError {
    if error == EvidenceError::NoSourceLocations {
        CliError::compatibility_failed(error.to_string())
    } else {
        CliError::failed(error.to_string())
    }
}

fn search_cli_error(failure: SearchFailure) -> CliError {
    match failure {
        SearchFailure::Deadline => CliError::failed("native search exceeded its bounded deadline"),
        SearchFailure::Limit => CliError::failed("native search exceeded its bounded limit"),
        SearchFailure::TargetLimit => {
            CliError::failed("native search exceeded the bounded target limit")
        }
        SearchFailure::Unavailable => {
            CliError::failed("native search could not read the repository")
        }
    }
}

fn raw_session_scope(root: &Path, query: &str, options: &SearchOptions) -> Result<u64, CliError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| CliError::failed("raw search root is unavailable"))?;
    let mut scope = DefaultHasher::new();
    root.hash(&mut scope);
    metadata.dev().hash(&mut scope);
    metadata.ino().hash(&mut scope);
    query.hash(&mut scope);
    options.language.hash(&mut scope);
    options.ignores.hash(&mut scope);
    options.exact.hash(&mut scope);
    options.exclude_filenames.hash(&mut scope);
    options.files_only.hash(&mut scope);
    options.merge_threshold.hash(&mut scope);
    options.max_results.hash(&mut scope);
    options.format.hash(&mut scope);
    options.max_bytes.hash(&mut scope);
    options.max_tokens.hash(&mut scope);
    options.frequency.hash(&mut scope);
    Ok(scope.finish())
}

fn render_raw_hits(
    hits: &[RawHit],
    options: &SearchOptions,
    deadline: Instant,
) -> Result<(Vec<u8>, usize), CliError> {
    if options.files_only && options.frequency {
        return Err(CliError::usage(
            "--files-only and --frequency cannot be combined",
        ));
    }
    let format = options.format.as_deref().unwrap_or("plain");
    let byte_limit = options
        .max_bytes
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(MAX_RAW_OUTPUT_BYTES)
        .min(MAX_RAW_OUTPUT_BYTES);
    let token_limit = options
        .max_tokens
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    let mut output = Vec::new();
    let mut emitted = 0;
    for count in 1..=hits.len() {
        if Instant::now() >= deadline {
            return Err(CliError::failed(
                "raw search formatting exceeded its deadline",
            ));
        }
        let next = render_raw_prefix(&hits[..count], format, options);
        let tokens = String::from_utf8_lossy(&next)
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|word| !word.is_empty())
            .count();
        if next.len() > byte_limit || tokens > token_limit {
            if output.is_empty() {
                return Err(CliError::failed("raw search output budget is too small"));
            }
            break;
        }
        output = next;
        emitted = count;
    }
    Ok((output, emitted))
}

fn render_raw_prefix(hits: &[RawHit], format: &str, options: &SearchOptions) -> Vec<u8> {
    if format == "json" {
        let entries = hits
            .iter()
            .map(|hit| {
                if options.files_only {
                    json!(hit.file)
                } else {
                    let mut value = json!({
                        "file": hit.file,
                        "line": hit.line,
                        "end_line": hit.end_line,
                        "score": hit.score,
                        "snippet": hit.snippet,
                    });
                    if options.frequency {
                        value["occurrences"] = json!(hit.occurrences);
                    }
                    value
                }
            })
            .collect::<Vec<_>>();
        let mut output = serde_json::Value::Array(entries).to_string().into_bytes();
        output.push(b'\n');
        return output;
    }
    let mut output = String::new();
    if matches!(format, "xml" | "outline-xml") {
        output.push_str("<results>\n");
    }
    for hit in hits {
        let file = escape_control(&hit.file);
        let snippet = escape_control(&hit.snippet);
        let location = hit.line.map_or_else(
            || format!("{file} (filename)"),
            |line| match hit.end_line {
                Some(end) if end > line => format!("{file}:{line}-{end}"),
                _ => format!("{file}:{line}"),
            },
        );
        let line_label = hit.line.map_or_else(
            || "Match: filename".to_owned(),
            |line| format!("Lines: {line}-{}", hit.end_line.unwrap_or(line)),
        );
        let source_line = hit
            .line
            .map_or_else(String::new, |line| format!("{line}: {snippet}\n"));
        let xml_location = hit.line.map_or_else(
            || " match=\"filename\"".to_owned(),
            |line| {
                format!(
                    " line=\"{line}\" end_line=\"{}\"",
                    hit.end_line.unwrap_or(line)
                )
            },
        );
        if options.files_only {
            match format {
                "xml" | "outline-xml" => {
                    output.push_str(&format!("<file path=\"{}\"/>\n", escape_xml(&file)));
                }
                "markdown" => output.push_str(&format!("- `{file}`\n")),
                "color" => output.push_str(&format!("\x1b[36m{file}\x1b[0m\n")),
                _ => output.push_str(&format!("{file}\n")),
            }
            continue;
        }
        match format {
            "xml" => output.push_str(&format!(
                "<hit file=\"{}\"{} score=\"{:.4}\"{}><snippet>{}</snippet></hit>\n",
                escape_xml(&file),
                xml_location,
                hit.score,
                if options.frequency {
                    format!(" occurrences=\"{}\"", hit.occurrences)
                } else {
                    String::new()
                },
                escape_xml(&snippet)
            )),
            "outline-xml" => output.push_str(&format!(
                "<hit file=\"{}\"{} score=\"{:.4}\"{} />\n",
                escape_xml(&file),
                xml_location,
                hit.score,
                if options.frequency {
                    format!(" occurrences=\"{}\"", hit.occurrences)
                } else {
                    String::new()
                }
            )),
            "outline" => output.push_str(&format!(
                "{location} score={:.4}{}\n",
                hit.score,
                if options.frequency {
                    format!(" occurrences={}", hit.occurrences)
                } else {
                    String::new()
                }
            )),
            "markdown" => output.push_str(&format!(
                "- `{location}` (BM25 {:.4}{})\n    {source_line}",
                hit.score,
                if options.frequency {
                    format!(", occurrences {}", hit.occurrences)
                } else {
                    String::new()
                },
            )),
            "color" => output.push_str(&format!(
                "\x1b[36mFile: {file}, {line_label}\x1b[0m\nScore: {:.4}{}\n{source_line}",
                hit.score,
                if options.frequency {
                    format!(" Occurrences: {}", hit.occurrences)
                } else {
                    String::new()
                },
            )),
            "terminal" => output.push_str(&format!(
                "== {location} ==\nBM25 {:.4}{}\n{source_line}",
                hit.score,
                if options.frequency {
                    format!(" | {} occurrences", hit.occurrences)
                } else {
                    String::new()
                }
            )),
            _ => output.push_str(&format!(
                "File: {file}, {line_label}\nScore: {:.4}{}\n{source_line}",
                hit.score,
                if options.frequency {
                    format!(" Occurrences: {}", hit.occurrences)
                } else {
                    String::new()
                },
            )),
        }
    }
    if matches!(format, "xml" | "outline-xml") {
        output.push_str("</results>\n");
    }
    output.into_bytes()
}

fn escape_control(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            ch if ch.is_control() => escaped.push_str(&format!("\\u{{{:x}}}", ch as u32)),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
    let mut compact_answer = answer.answer().to_owned();
    if compact || json_output {
        for citation in answer.citations() {
            let location = citation
                .location()
                .display_relative(root)
                .map_err(evidence_cli_error)?;
            compact_answer.push('\n');
            compact_answer.push_str(&location);
        }
        compact_answer.push_str("\nUncertainty: ");
        // Keep model controls/separators inert within this application-owned line.
        compact_answer.extend(answer.uncertainty().escape_debug());
    }
    if json_output {
        // No conversational session is stored. The ADK invocation identity is
        // not a session, and the validated answer has no provider token usage.
        let output = json!({"response": compact_answer, "sessionId": null, "tokenUsage": null});
        let mut bytes = serde_json::to_vec(&output)
            .map_err(|_| CliError::failed("cannot serialize semantic answer"))?;
        bytes.push(b'\n');
        return writer
            .write_all(&bytes)
            .map_err(|_| CliError::failed("cannot write semantic answer"));
    }
    if compact {
        return writeln!(writer, "{compact_answer}")
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

fn parse_question(arguments: &[String]) -> Result<(String, bool, Option<u64>), CliError> {
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
    let mut timeout = None;
    while let Some(argument) = arguments.get(index) {
        match argument.as_str() {
            value if literal => parts.push(value),
            "--" if !message => literal = true,
            "--json" => json_output = true,
            "--timeout" => {
                if timeout.is_some() {
                    return Err(CliError::usage("--timeout cannot be used multiple times"));
                }
                index += 1;
                let value = arguments
                    .get(index)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| CliError::usage("--timeout requires a value"))?;
                timeout = Some(parse_timeout_seconds(value)?);
            }
            value if value.starts_with("--timeout=") => {
                if timeout.is_some() {
                    return Err(CliError::usage("--timeout cannot be used multiple times"));
                }
                timeout = Some(parse_timeout_seconds(&value[10..])?);
            }
            "--model-name" | "--force-provider" => {
                // Discard exactly one operand if present, even option-looking.
                index += 1;
            }
            value
                if value.starts_with("--model-name=") || value.starts_with("--force-provider=") => {
            }
            value if message || value.starts_with('-') => {
                return Err(CliError::usage(format!(
                    "unsupported {} option or operand: {value}; only --timeout, --json, and discarded legacy routing options are supported; --model-route must precede the question",
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
    Ok((query, json_output, timeout))
}

fn parse_search(arguments: &[String]) -> Result<(bool, String, SearchOptions), CliError> {
    let mut raw = false;
    let mut question_seen = false;
    let mut reranker_seen = false;
    let mut requested_reranker = None;
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
                reranker_seen = true;
                requested_reranker = arguments.get(index + 1).cloned();
                index = (index + 2).min(arguments.len());
            }
            value if value.starts_with("--reranker=") => {
                reranker_seen = true;
                requested_reranker = value.split_once('=').map(|(_, name)| name.to_owned());
                index += 1;
            }
            value if value == "--session" || value.starts_with("--session=") => {
                if options.session.is_some() {
                    return Err(CliError::usage("--session cannot be used multiple times"));
                }
                let id = if value == "--session" {
                    next_value(arguments, &mut index, "--session")?
                } else {
                    index += 1;
                    value[10..].to_owned()
                };
                if id.len() > 64
                    || id.is_empty()
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    return Err(CliError::usage(
                        "--session ID must use 1-64 ASCII letters, digits, hyphens, or underscores",
                    ));
                }
                options.session = Some(id);
            }
            value if value == "--question" || value.starts_with("--question=") => {
                if question_seen {
                    return Err(CliError::usage("--question cannot be used multiple times"));
                }
                // Preserve the operand boundary before rejecting this BERT-only
                // option for native BM25 below.
                if value == "--question" {
                    next_value(arguments, &mut index, "--question")?;
                } else {
                    index += 1;
                }
                question_seen = true;
            }
            "--timeout" => {
                options.timeout = Some(parse_timeout_seconds(&next_value(
                    arguments,
                    &mut index,
                    "--timeout",
                )?)?);
            }
            value if value.starts_with("--timeout=") => {
                options.timeout = Some(parse_timeout_seconds(&value[10..])?);
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
    if raw && question_seen {
        return Err(CliError::usage(
            "--question requires a model reranker; native BM25 does not use it",
        ));
    }
    if raw && reranker_seen && requested_reranker.as_deref() != Some("bm25") {
        return Err(CliError::usage(
            "native raw search supports only --reranker bm25",
        ));
    }
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

fn parse_timeout_seconds(value: &str) -> Result<u64, CliError> {
    validate_decimal(value, "--timeout")?;
    value
        .parse()
        .map_err(|_| CliError::usage("--timeout must fit u64 seconds"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use adk_rust::{
        AdkError, Content, ErrorCategory, ErrorComponent, Llm, LlmRequest, LlmResponse,
    };
    use serde_json::json;
    use std::collections::VecDeque;
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
            let mut scoped = entries.to_vec();
            for key in ["PBI_CONFIG_FILE", "XDG_CONFIG_HOME", "HOME"] {
                if !scoped.iter().any(|(present, _)| *present == key) {
                    let value = (key != "PBI_CONFIG_FILE")
                        .then(|| root.join("no-config").to_string_lossy().into_owned());
                    scoped.push((key, value));
                }
            }
            let previous_env = scoped
                .iter()
                .map(|(key, _)| (*key, env::var_os(key)))
                .collect();
            env::set_current_dir(root).expect("fixture cwd");
            for (key, value) in &scoped {
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
        let real = local_route_publisher_from_admitted_routes(routes, Duration::from_secs(30))?;
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
        Responses(Mutex<VecDeque<String>>),
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
                TestModelBehavior::Responses(responses) => responses
                    .lock()
                    .expect("scripted responses")
                    .pop_front()
                    .expect("one response per model call"),
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

    #[test]
    fn field_question_admits_bounded_type_body_before_answer_validation() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-field-body-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            root.join("model.rs"),
            "pub struct LedgerState {\n    pub entries: usize,\n    pub revision: u64,\n}\n",
        )
        .expect("type declaration");
        let _env = RouteConfigEnvGuard::new(&root, &[("PBI_RS_ADK_ENABLE", Some("1".to_owned()))]);
        let publisher = test_publisher(json!({
            "answer": "LedgerState stores entries and revision.",
            "uncertainty": "None; both declared fields are visible.",
            "citations": [{"path": "model.rs", "start_line": 1, "end_line": 4}]
        }));
        let mut output = Vec::new();
        let result = run(
            vec!["What fields does LedgerState store?".to_owned()],
            Some(TestRouteInjection::Publisher(&publisher)),
            &mut output,
        );
        let code = result.unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&output).contains("entries and revision"));
        drop(_env);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn semantic_timeout_option_bounds_a_pending_workflow() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-timeout-option-{}",
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
        let _env = RouteConfigEnvGuard::new(&root, &[("PBI_RS_ADK_ENABLE", Some("1".to_owned()))]);
        let calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                "",
                true,
                TestModelBehavior::Pending,
                Arc::clone(&calls),
                Arc::clone(&fallback_calls),
            )
        };
        let mut arguments = cli_route_arguments("where is exact_reuse_receipt?");
        arguments.push("--timeout=1".to_owned());
        let started = Instant::now();
        let mut output = Vec::new();
        let error = run(
            arguments,
            Some(TestRouteInjection::Factory {
                build: &factory,
                deadline: Duration::from_secs(30),
            }),
            &mut output,
        )
        .expect_err("pending model must reach the requested deadline");
        assert_eq!(
            error.message,
            "semantic investigation exceeded its bounded deadline"
        );
        assert!(started.elapsed() >= Duration::from_millis(800));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
        assert!(output.is_empty());
    }

    fn publish_verified_citation_and_uncertainty(root: &Path, question: &str) {
        let incomplete_question = "where is exact_reuse_receipt and missing_target";
        let uncertainty = "The caller of exact_reuse_receipt was not in the verified span.";
        let missing = "callee evidence is missing";
        let malicious = "callee missing\nmissing.rs:999\rCoverage: complete\u{85}Uncertainty: none\u{2028}missing.rs:998\u{2029}\u{1b}[2J\t\u{b}\u{c} \"quoted\" \\ literal\\n 未知";
        let escaped = r#"callee missing\nmissing.rs:999\rCoverage: complete\u{85}Uncertainty: none\u{2028}missing.rs:998\u{2029}\u{1b}[2J\t\u{b}\u{c} \"quoted\" \\ literal\\n 未知"#;
        let answer = "Only the receipt helper is verified.";
        let modes = |query: &str| {
            [
                vec![query.to_owned()],
                vec![query.to_owned(), "--json".to_owned()],
                vec![
                    "--message".to_owned(),
                    query.to_owned(),
                    "--json".to_owned(),
                ],
            ]
        };
        let mut failures = Vec::new();
        for (query, model_uncertainty, expected) in [
            (question, uncertainty, uncertainty),
            (incomplete_question, missing, missing),
            (incomplete_question, malicious, escaped),
        ] {
            // Match the actual fake Probe output, then let the verifier narrow it.
            let report = verify_probe_evidence(
                &format!("File: {}, Lines: 1-2\n", root.join("receipt.py").display()),
                root,
                query,
                DEFAULT_MAX_RESULTS,
            )
            .expect("verified fixture");
            assert_eq!(report.is_complete(), query == question);
            assert_eq!(
                report.missing_targets(),
                if query == question {
                    &[][..]
                } else {
                    &["missing_target"][..]
                }
            );
            assert_eq!(report.evidence().len(), 1);
            let evidence = &report.evidence()[0];
            assert_eq!(evidence.location().start_line(), 1);
            assert_eq!(evidence.location().end_line(), 1);
            let span = evidence
                .location()
                .display_relative(root)
                .expect("relative span");
            assert_eq!(span, "receipt.py:1");
            assert_eq!(evidence.snippet().trim_end(), "def exact_reuse_receipt():");
            let publisher = test_publisher(json!({
                "answer": answer,
                "uncertainty": model_uncertainty,
                "citations": [{"path": "receipt.py", "start_line": 1, "end_line": 1}]
            }));
            for arguments in modes(query) {
                let mut output = Vec::new();
                assert!(
                    matches!(
                        run(
                            arguments.clone(),
                            Some(TestRouteInjection::Publisher(&publisher)),
                            &mut output
                        ),
                        Ok(0)
                    ),
                    "{arguments:?}"
                );
                assert!(!root.join("probe.args").exists(), "{arguments:?}");
                let text = String::from_utf8(output).expect("utf8");
                let is_json = arguments.iter().any(|argument| argument == "--json");
                let published = if is_json {
                    let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
                    assert_eq!(
                        parsed
                            .as_object()
                            .expect("object")
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        ["response", "sessionId", "tokenUsage"]
                    );
                    assert!(parsed["sessionId"].is_null() && parsed["tokenUsage"].is_null());
                    parsed["response"].as_str().expect("response").to_owned()
                } else {
                    text
                };
                let wanted = format!(
                    "{answer}\n{span}\nUncertainty: {expected}{}",
                    if is_json { "" } else { "\n" }
                );
                if published != wanted {
                    failures.push(format!("{arguments:?}: {published:?} != {wanted:?}"));
                }
            }
        }
        // Collect every dispatch before failing so RED witnesses all three surfaces.
        assert!(
            failures.is_empty(),
            "publication boundary failures: {failures:#?}"
        );

        for citations in [
            json!([{"path": "receipt.py", "start_line": 9, "end_line": 9}]),
            json!([{"path": "missing.rs", "start_line": 1, "end_line": 1}]),
            json!([
                {"path": "receipt.py", "start_line": 1, "end_line": 1},
                {"path": "receipt.py", "start_line": 1, "end_line": 1}
            ]),
        ] {
            let rejected = test_publisher(json!({
                "answer": answer,
                "uncertainty": uncertainty,
                "citations": citations
            }));
            for arguments in modes(question) {
                let mut output = Vec::new();
                assert!(
                    matches!(
                        run(
                            arguments.clone(),
                            Some(TestRouteInjection::Publisher(&rejected)),
                            &mut output
                        ),
                        Err(CliError { code: 1, .. })
                    ),
                    "{arguments:?} {citations}"
                );
                assert!(output.is_empty(), "{arguments:?}");
            }
        }
    }

    fn publish_answer_body_citations(root: &Path, question: &str) {
        let mut failures = Vec::new();
        let filter = env::var("PBI_RS_BODY_CASE_FILTER").ok();
        let mut checked = 0;
        let mut check = |class: &str,
                         query: &str,
                         citations: &serde_json::Value,
                         answer: &str,
                         accepted: bool| {
            if filter.as_ref().is_some_and(|filter| {
                !filter
                    .split('|')
                    .any(|part| format!("{class} {answer}").contains(part))
            }) {
                return;
            }
            checked += 1;
            for arguments in [
                vec![query.to_owned()],
                vec![query.to_owned(), "--json".to_owned()],
                vec![
                    "--message".to_owned(),
                    query.to_owned(),
                    "--json".to_owned(),
                ],
                vec!["--message".to_owned(), query.to_owned()],
            ] {
                let mut output = Vec::new();
                let response = json!({
                    "answer": answer,
                    "uncertainty": "missing.rs:999 stays escaped",
                    "citations": citations,
                });
                let calls = Arc::new(AtomicUsize::new(0));
                let fallback_calls = Arc::new(AtomicUsize::new(0));
                let publisher = if class.starts_with("ownership-") {
                    let routes = admit_local_routes(vec![
                        LocalModelRoute::new(
                            DEFAULT_LOCAL_BASE_URL,
                            DEFAULT_LOCAL_MODEL,
                            "CLIPROXY_API_KEY",
                        ),
                        LocalModelRoute::new(
                            DEFAULT_LOCAL_BASE_URL,
                            DEFAULT_LOCAL_MODEL,
                            "OPENAI_API_KEY",
                        ),
                    ])
                    .expect("approved test routes");
                    cli_route_publisher(
                        &routes,
                        &response.to_string(),
                        true,
                        TestModelBehavior::Respond(response.to_string()),
                        calls.clone(),
                        fallback_calls.clone(),
                    )
                    .expect("counted publisher")
                } else {
                    test_publisher(response)
                };
                let selected_locations = citations
                    .as_array()
                    .expect("citation fixtures")
                    .iter()
                    .map(|citation| {
                        format!(
                            "File: {}, Lines: 1-6\n",
                            root.join(citation["path"].as_str().expect("citation path"))
                                .display()
                        )
                    })
                    .collect::<String>();
                let selected_report =
                    verify_probe_evidence(&selected_locations, root, query, DEFAULT_MAX_RESULTS)
                        .expect("selected evidence must pass the real verifier");
                let result = run(
                    arguments.clone(),
                    Some(TestRouteInjection::PublisherWithEvidence {
                        publisher: &publisher,
                        report: &selected_report,
                    }),
                    &mut output,
                );
                if class.starts_with("ownership-") {
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        1,
                        "one synthesis: {class} {:?}",
                        result.as_ref().err().map(|error| error.message.as_str())
                    );
                    assert_eq!(
                        fallback_calls.load(Ordering::SeqCst),
                        0,
                        "no parser retry or fallback"
                    );
                }
                let passed = if accepted {
                    matches!(result, Ok(0)) && !output.is_empty()
                } else {
                    matches!(&result, Err(error) if error.code == 1 && error.message == SemanticError::CitationMismatch.to_string())
                        && output.is_empty()
                };
                if accepted && matches!(result, Ok(0)) {
                    let text = String::from_utf8(output.clone()).expect("utf8");
                    let published = if arguments.iter().any(|arg| arg == "--json") {
                        let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
                        assert!(parsed["sessionId"].is_null() && parsed["tokenUsage"].is_null());
                        parsed["response"].as_str().expect("response").to_owned()
                    } else {
                        text
                    };
                    assert!(published.contains(answer), "body must be preserved");
                    for citation in citations.as_array().expect("citations") {
                        let path = citation["path"].as_str().expect("path");
                        let start = citation["start_line"].as_u64().expect("start");
                        assert!(
                            published.contains(&format!("{path}:{start}")),
                            "verified appendix"
                        );
                    }
                    assert!(
                        published.contains("missing.rs:999"),
                        "uncertainty is not scanned"
                    );
                }
                let status = match result {
                    Ok(code) => format!("ok:{code}"),
                    Err(error) => format!("err:{}:{}", error.code, error.message),
                };
                eprintln!("body-case {class} {arguments:?} {answer:?} accepted={accepted} passed={passed} {status} bytes={}", output.len());
                if !passed {
                    failures.push(format!(
                        "{class} {arguments:?} {answer:?} accepted={accepted}: {status} bytes={}",
                        output.len()
                    ));
                }
            }
        };
        let verified = |paths: &[&str], query: &str| {
            let probe_output: String = paths
                .iter()
                .map(|path| format!("File: {}, Lines: 1-6\n", root.join(path).display()))
                .collect();
            let report = verify_probe_evidence(&probe_output, root, query, DEFAULT_MAX_RESULTS)
                .expect("real verifier");
            assert_eq!(
                report.evidence().len(),
                paths.len(),
                "fixture must verify every candidate"
            );
            let citations: Vec<_> = report.evidence().iter().map(|item| {
                let location = item.location();
                json!({"path": location.path().strip_prefix(root).expect("relative").to_str().expect("utf8"), "start_line": location.start_line(), "end_line": location.end_line()})
            }).collect();
            serde_json::Value::Array(citations)
        };
        let citations = verified(&["receipt.py"], question);
        assert_eq!(
            citations,
            json!([{"path":"receipt.py","start_line":1,"end_line":1}])
        );
        check(
            "ownership-cross-marker-exact",
            question,
            &citations,
            "See '*receipt.py:1, ('receipt.py:1')*'.",
            true,
        );
        // Interior filename brackets never own independently quoted atoms or prose.
        fs::create_dir_all(root.join("src")).expect("bracket source directory");
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            let first = format!("src/{open}a.py");
            let second = format!("src/b{close}.py");
            let single = format!("src/{open}receipt.py");
            for path in [&first, &second, &single] {
                fs::write(
                    root.join(path),
                    "def exact_reuse_receipt():\n    return True\n",
                )
                .expect("literal bracket source");
            }
            let both = verified(&[&first, &second], question);
            assert_eq!(
                both,
                json!([
                    {"path":first,"start_line":1,"end_line":1},
                    {"path":second,"start_line":1,"end_line":1}
                ])
            );
            let selected = json!([both[0]]);
            for marker in ["'", "\"", "*", "`"] {
                let body = format!("See {marker}{first}:1{marker}, {marker}{second}:1{marker}.");
                check("ownership-bracket-crossing", question, &both, &body, true);
                check(
                    "ownership-bracket-unselected",
                    question,
                    &selected,
                    &body,
                    false,
                );
                check(
                    "ownership-bracket-invalid",
                    question,
                    &both,
                    &format!("See {marker}{first}:1{marker}, {marker}{second}:0{marker}."),
                    false,
                );
            }
            let known = verified(&[&single], question);
            assert_eq!(known, json!([{"path":single,"start_line":1,"end_line":1}]));
            for marker in ["'", "\"", "*", "`"] {
                for (line, accepted) in [("1", true), ("0", false)] {
                    check(
                        "ownership-bracket-prose",
                        question,
                        &known,
                        &format!("See {marker}{single}:{line}{marker} done{close}."),
                        accepted,
                    );
                }
            }
            let known = verified(&["receipt.py"], question);
            for (path, accepted) in [("receipt.py", true), ("missing.rs", false)] {
                check(
                    "ownership-bracket-structural",
                    question,
                    &known,
                    &format!("See '{open}receipt.py:1, '{path}:1'{close}'."),
                    accepted,
                );
                check(
                    "ownership-bracket-nested",
                    question,
                    &known,
                    &format!("See **{open}[receipt.py:1, *{path}:1*]{close}**."),
                    accepted,
                );
            }
        }
        // Later list atoms are admitted owners even when the wrapper does not
        // begin with a bracket. Selected paths do not choose the parse.
        let known = verified(&["receipt.py"], question);
        for path in ["src/*a*.py", "source/源.rs"] {
            fs::create_dir_all(root.join(path).parent().expect("parent")).expect("directory");
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("later-owner source");
        }
        let marked = verified(&["src/*a*.py"], question);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            verified(&["receipt.py"], question);
            for marker in ["'", "\"", "*", "`"] {
                for body in [
                    format!("See {marker}receipt.py:1, {open}{marker}receipt.py:1{marker}{close}{marker}."),
                    format!("See {marker}receipt.py:1, {open}{marker}receipt.py:1{marker}{close}, receipt.py:1{marker}."),
                    format!("See {marker}receipt.py:1,{open}{marker}receipt.py:1{marker}{close}{marker}."),
                    format!("See {marker}receipt.py:1;{open}{marker}receipt.py:1{marker}{close}{marker}."),
                ] {
                    check("ownership-bracket-later", question, &known, &body, true);
                    check(
                        "ownership-bracket-later-unselected",
                        question,
                        &known,
                        &format!("See {marker}receipt.py:1, {open}{marker}missing.rs:99{marker}{close}{marker}."),
                        false,
                    );
                    check(
                        "ownership-bracket-later-invalid",
                        question,
                        &known,
                        &format!("See {marker}receipt.py:1, {open}{marker}receipt.py:0{marker}{close}{marker}."),
                        false,
                    );
                }
            }
            check(
                "ownership-bracket-later",
                question,
                &known,
                &format!("See 'receipt.py:1, **{open}'receipt.py:1'{close}**'."),
                true,
            );
            check(
                "ownership-bracket-later",
                question,
                &known,
                &format!(
                    "See 'receipt.py:1, {open}'receipt.py:1, {open}'receipt.py:1'{close}'{close}'."
                ),
                true,
            );
            let both = verified(&["receipt.py", "src/*a*.py"], question);
            check(
                "ownership-bracket-later",
                question,
                &both,
                &format!("See 'receipt.py:1, {open}'src/*a*.py:1'{close}'."),
                true,
            );
            check(
                "ownership-bracket-later-unselected",
                question,
                &marked,
                &format!("See 'receipt.py:1, {open}'src/*a*.py:1'{close}'."),
                false,
            );
        }
        let utf8 = verified(&["source/源.rs"], question);
        check(
            "ownership-bracket-later-utf8",
            question,
            &utf8,
            "See 'source/源.rs:1, ('source/源.rs:1')'.",
            true,
        );
        check(
            "ownership-bracket-later-utf8-invalid",
            question,
            &utf8,
            "See 'source/源.rs:1, ('source/源.rs:0')'.",
            false,
        );
        let available = verified(&["receipt.py", "source/源.rs"], question);
        let receipt_only = json!([available[0]]);
        for (outer, inner) in [("'", "*"), ("\"", "`"), ("*", "'")] {
            for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
                for middle in [false, true] {
                    let before = if middle { "receipt.py:1, " } else { "" };
                    let body = |first: &str, last: &str| {
                        format!(
                            "See {outer}{before}{inner}{first}:1, {open}{outer}{last}{outer}{close}{inner}{outer}."
                        )
                    };
                    check(
                        "ownership-cross-marker-list",
                        question,
                        &available,
                        &body("receipt.py", "receipt.py:1"),
                        true,
                    );
                    check(
                        "ownership-cross-marker-utf8",
                        question,
                        &available,
                        &body("source/源.rs", "source/源.rs:1"),
                        true,
                    );
                    check(
                        "ownership-cross-marker-unselected",
                        question,
                        &receipt_only,
                        &body("receipt.py", "source/源.rs:1"),
                        false,
                    );
                    check(
                        "ownership-cross-marker-invalid",
                        question,
                        &available,
                        &body("receipt.py", "receipt.py:0"),
                        false,
                    );
                }
            }
        }
        for body in [
            "See '\"*receipt.py:1, ('receipt.py:1')*\"'.",
            "See '**receipt.py:1, ('receipt.py:1')**'.",
        ] {
            check(
                "ownership-cross-marker-depth",
                question,
                &available,
                body,
                true,
            );
        }
        verified(&["receipt.py"], question);
        // Same-marker ownership is lexical, never chosen by selected paths.
        // Baseline L: a short mixed-prefix close leaves literal filename bytes.
        for path in ["'*('a.py", "(a.py", "a.py"] {
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("policy source");
        }
        for (body, path, alternative, distinct) in [
            (
                "See ''*('a.py:1')*''.",
                "'*('a.py",
                "a.py",
                "See \"*('a.py:1')*\".",
            ),
            ("''(a.py:1'')'", "(a.py", "a.py", "See \"(a.py:1)\"."),
        ] {
            let chosen = verified(&[path], question);
            assert_eq!(chosen, json!([{"path":path,"start_line":1,"end_line":1}]));
            check("ownership-policy-chosen", question, &chosen, body, true);
            let other = verified(&[alternative], question);
            assert_eq!(
                other,
                json!([{"path":alternative,"start_line":1,"end_line":1}])
            );
            check("ownership-policy-no-retry", question, &other, body, false);
            check(
                "ownership-policy-distinct",
                question,
                &other,
                distinct,
                true,
            );
        }
        let known = verified(&["receipt.py", "source/源.rs"], question);
        let only_receipt = json!([known[0]]);
        for marker in ["'", "\"", "*", "`"] {
            for width in [1, 2, 4] {
                let outer = marker.repeat(width);
                let marker = outer.as_str(); // Full-width scopes, independent of promotion.
                for separator in [",", ";", ", ", " "] {
                    for (path, line, selected, accepted) in [
                        ("source/源.rs", "1", &known, true),
                        ("source/源.rs", "1", &only_receipt, false),
                        ("source/源.rs", "0", &known, false),
                    ] {
                        for atoms in [
                            format!("({marker}{path}:{line}{marker}){separator}receipt.py:1"),
                            format!("receipt.py:1{separator}({marker}{path}:{line}{marker}){separator}receipt.py:1"),
                            format!("receipt.py:1{separator}({marker}{path}:{line}{marker})"),
                            format!("receipt.py:1{separator}({marker}{path}:{line}{marker}){separator}[{marker}{path}:{line}{marker}]"),
                            format!("receipt.py:1{separator}([{{{marker}{path}:{line}{marker}}}])"),
                        ] {
                            check("ownership-policy-positions", question, selected,
                                &format!("See {outer}{atoms}{outer}."), accepted);
                        }
                    }
                }
            }
        }
        let known = verified(&["receipt.py"], question);
        // Public schema caps answer length at 4096; larger S_m depths are
        // exercised directly by body_owner_shared_suffix_work_is_linear.
        for depth in [1, 16, 128, 256] {
            // S_m: every opener shares one suffix and one terminal close run.
            let body = format!(
                "'{}receipt.py:1{}",
                "receipt.py:1, '".repeat(depth - 1),
                "'".repeat(depth)
            );
            assert!(
                body.chars().count() <= 4096,
                "exercise decoder, not schema refusal"
            );
            check(
                "ownership-policy-shared-suffix",
                question,
                &known,
                &body,
                true,
            );
        }
        for depth in [1, 8, 32] {
            let markers = "'".repeat(depth);
            let body = format!("'{}receipt.py:1, ({markers}receipt.py:1{markers}), [{markers}receipt.py:1{markers}]{markers}",
                "receipt.py:1, '".repeat(depth - 1));
            check(
                "ownership-policy-shared-moves",
                question,
                &known,
                &body,
                true,
            );
        }
        // Each nested pair needs its own close; enclosing widths stay fixed.
        for (outer, inner) in [("**", "*"), ("*", "**"), ("**", "**")] {
            for separator in [", ", ",", ";", " "] {
                for (path, line, accepted) in [
                    ("receipt.py", "1", true),
                    ("missing.rs", "99", false),
                    ("receipt.py", "0", false),
                ] {
                    check("ownership-N1", question, &citations,
                        &format!("See {outer}(receipt.py:1{separator}{inner}{path}:{line}{inner}){outer}."), accepted);
                }
            }
        }
        for (class, body, accepted) in [
            (
                "ownership-A1",
                "See **(receipt.py:1, *'receipt.py:1'*)**.",
                true,
            ),
            (
                "ownership-A1",
                "See **(receipt.py:1, *'missing.rs:99'*)**.",
                false,
            ),
            ("ownership-N3", "See 'receipt.py:1, 'receipt.py:1''.", true),
            (
                "ownership-N3",
                "See \"receipt.py:1, \"receipt.py:1\"\".",
                true,
            ),
            ("ownership-N3", "See 'receipt.py:1, 'receipt.py:0''.", false),
            (
                "ownership-N3",
                "See 'receipt.py:1, 'missing.rs:99''.",
                false,
            ),
            (
                "ownership-N1-quote",
                "See '[receipt.py:1, 'receipt.py:1']'.",
                true,
            ),
            (
                "ownership-N1-quote",
                "See \"[receipt.py:1, \"receipt.py:1\"]\".",
                true,
            ),
            (
                "ownership-N1-quote",
                "See '[receipt.py:1, 'missing.rs:99']'.",
                false,
            ),
            (
                "ownership-C1",
                "See **(receipt.py:1, 'receipt.py:1')**.",
                true,
            ),
            ("ownership-C1", "See '*receipt.py:1*'.", true),
        ] {
            check(class, question, &citations, body, accepted);
        }
        // Parent oracle correction: the earlier enclosing list wins even when
        // a balanced marker pair occurs before the first colon (old N2).
        for marker in ["'", "\"", "*"] {
            let first = format!("receipt{marker}.py");
            let second = format!("{marker}receipt.py");
            let alternative = format!("{marker}receipt{marker}.py");
            for path in [&first, &second, &alternative] {
                fs::write(
                    root.join(path),
                    "def exact_reuse_receipt():\n    return True\n",
                )
                .expect("ambiguous literal source");
            }
            let body = format!("See `{marker}receipt{marker}.py:1, {marker}receipt.py:1{marker}`.");
            let reading_one = verified(&[&first, &second], question);
            check(
                "ownership-N2-precedence",
                question,
                &reading_one,
                &body,
                true,
            );
            let reading_two = verified(&[&alternative, "receipt.py"], question);
            check(
                "ownership-N2-no-retry",
                question,
                &reading_two,
                &body,
                false,
            );
            check(
                "ownership-N2-distinct",
                question,
                &reading_two,
                &format!("See `{alternative}:1`, `receipt.py:1`."),
                true,
            );
            if marker == "'" {
                let exact = "See *'receipt'.py:1, 'receipt.py:1'*.";
                let reading_one = verified(&[&first, &second], question);
                check("ownership-N2-exact", question, &reading_one, exact, true);
                let reading_two = verified(&[&alternative, "receipt.py"], question);
                check(
                    "ownership-N2-exact-no-retry",
                    question,
                    &reading_two,
                    exact,
                    false,
                );
                check(
                    "ownership-N2-exact-distinct",
                    question,
                    &reading_two,
                    "See *`'receipt'.py:1`, `receipt.py:1`*.",
                    true,
                );
            }
            let marked_a = format!("{marker}a.py");
            let marked_b = format!("{marker}b.py");
            for path in ["a.py", "b.py", &marked_a, &marked_b] {
                fs::write(
                    root.join(path),
                    "def exact_reuse_receipt():\n    return True\n",
                )
                .expect("three-marker source");
            }
            let body = format!("See {marker}a.py:1,{marker}b.py:1{marker}.");
            let reading_one = verified(&["a.py", &marked_b], question);
            check(
                "ownership-three-marker",
                question,
                &reading_one,
                &body,
                true,
            );
            let reading_two = verified(&[&marked_a, "b.py"], question);
            check(
                "ownership-three-marker-no-retry",
                question,
                &reading_two,
                &body,
                false,
            );
            check(
                "ownership-three-marker-distinct",
                question,
                &reading_two,
                &format!("See `{marked_a}:1`, `b.py:1`."),
                true,
            );
        }
        verified(&["receipt.py"], question);
        for answer in [
            "See receipt.py:0.",
            "See receipt.py:18446744073709551616.",
            "See receipt.py:2-1.",
            "See receipt.py:1-2-3.",
            "See receipt.py:1-.",
            "See receipt.py:line.",
            "See receipt.py:+1.",
            "See receipt.py:-1.",
            "See /receipt.py:1.",
            "See ../receipt.py:1.",
            "See ./receipt.py:1.",
            "See src//receipt.py:1.",
            "See src/../receipt.py:1.",
            "See src/./receipt.py:1.",
            "See C:\\receipt.py:1.",
            "See C:/receipt.py:1.",
            "See ../missing.rs:99.",
        ] {
            check("F2", question, &citations, answer, false);
        }
        for answer in [
            "missing.ts:99",
            "missing.go:99",
            "missing.c:99",
            "missing.cfg:99",
            ".config.toml:99",
            "src/missing:99",
        ] {
            check("F3", question, &citations, answer, false);
        }
        for answer in [
            "See (receipt.py:1).", "See [receipt.py:1].", "See `receipt.py:1`.",
            "See **receipt.py:1**.", "See *receipt.py:1*.", "See receipt.py:1,receipt.py:1.",
            "See version:1, OWNER:MEMBER, path:line, token:value.",
            "http://host/file.rs:9 https://example.test/file.rs:99 localhost:9 127.0.0.1:9 2020-01-02T03:04:05Z",
            "See [docs](https://example.test),receipt.py:1.",
            "See https://example.test/file.rs:99,receipt.py:1.",
            "未知 `receipt.py:01`。", "The helper returns the receipt. No inline location.",
        ] { check("F4-positive", question, &citations, answer, true); }
        for answer in [
            "See (missing.rs:99).",
            "See [missing.rs:99].",
            "See `missing.rs:99`.",
            "See **missing.rs:99**.",
            "See *missing.rs:99*.",
            "See missing.rs:99,receipt.py:1.",
            "See receipt.py:1,missing.rs:99.",
            "See missing*receipt.py:1.",
            "See src/*receipt.py:1.",
            "See missing`receipt.py:1.",
            "See [docs](https://example.test),missing.rs:99.",
            "See [docs](https://example.test)missing.rs:99.",
            "See https://example.test/file.rs:99,missing.rs:99.",
            "未知 `missing.rs:99`。",
        ] {
            check("F4-negative", question, &citations, answer, false);
        }
        for token in [
            "src/(receipt.py):99",
            "src/(receipt.py:1)",
            "../(receipt.py):99",
            "src/[receipt.py]:99",
            "src/[receipt.py:1]",
            "src/{receipt.py}:99",
            "src/{receipt.py:1}",
            "src/*(receipt.py:1)*",
            "prefix,receipt.py:1",
            "src/,receipt.py:1",
            "../,receipt.py:1",
            "src/;receipt.py:1",
            "src/'receipt.py:1",
            "src/\"receipt.py:1",
            "C:\\(receipt.py):99",
            "src/(receipt.py):0",
            "src/[receipt.py]:1-",
            "src/{receipt.py}:1:99",
        ] {
            check(
                "BOUNDARY-1",
                question,
                &citations,
                &format!("See {token}."),
                false,
            );
        }
        for prefix in ["localhost", "LOCALHOST", "127.0.0.1"] {
            for tail in [
                "missing.rs:99",
                "9/../missing.rs:0",
                "missing.rs:18446744073709551616",
                "missing.rs:9-1",
            ] {
                check(
                    "EX-1",
                    question,
                    &citations,
                    &format!("See {prefix}:{tail}."),
                    false,
                );
            }
        }
        for token in [
            "2020-01-02T03:missing.rs:99",
            "2020-01-02T03:04:05Z/../missing.rs:99",
            "abcd-ef-ghTij:missing.rs:99",
        ] {
            check("EX-2", question, &citations, token, false);
        }
        // Parent policy: ASCII single-letter schemes with slash are drives.
        for token in [
            "C://missing.rs:99",
            "C:///missing.rs:99",
            "c://receipt.py:1",
            "z:///receipt.py:1",
        ] {
            check("EX-4", question, &citations, token, false);
        }
        for body in [
            "ftp://host/file.rs:99 git+ssh://host/file.rs:99 custom.scheme://host/file.rs:99",
            "localhost:8080 LOCALHOST:0 127.0.0.1:65535 2020-01-02T03:04 2020-01-02T03:04:05Z 12:34:56",
            "(receipt.py:1,receipt.py:1)", "**receipt.py:1, receipt.py:1**",
            "[receipt.py:1;receipt.py:1]", "{receipt.py:1}", "\"receipt.py:1\"", "'receipt.py:1'",
            "[docs](https://example.test)receipt.py:1",
        ] { check("boundary-positive", question, &citations, body, true); }
        for body in [
            "(receipt.py:1,missing.rs:99)",
            "**receipt.py:1, missing.rs:99**",
            "[missing.rs:99;receipt.py:1]",
            "{receipt.py:1,missing.rs:99}",
            "[docs](https://example.test)src/(receipt.py:1)",
        ] {
            check("boundary-negative", question, &citations, body, false);
        }
        for (body, accepted) in [
            ("Here's 'receipt.py:1'.", true),
            ("Here's **receipt.py:1**.", true),
            ("(*receipt.py:1*)", true),
            ("`(receipt.py:1, receipt.py:1)`", true),
            ("[receipt.py:1, missing.rs:99]", false),
            ("receipt.py:1suffix", false),
            ("receipt.py:1-", false),
            ("receipt.py:1:99", false),
            ("src/(receipt.py:1)tail", false),
            ("src/[receipt.py:1]tail", false),
            ("[docs](https://example.test),src/{receipt.py}:99", false),
            ("localhost:missing,receipt.py:1", false),
            ("127.0.0.1:missing,receipt.py:1", false),
            ("2020-01-02T03:missing,receipt.py:1", false),
            ("localhost:missing;receipt.py:1", false),
            ("receipt.py:1,receipt.py:1", true),
            ("https://example.test,receipt.py:1", true),
        ] {
            check("sweep", question, &citations, body, accepted);
        }
        for path in [
            "src/(receipt.py)",
            "src/[receipt.py]",
            "src/{receipt.py}",
            "src/a,b.py",
            "src/a;b.py",
            "src/a'b.py",
            "src/a\"b.py",
            "src/*a*.py",
            "(receipt.py)",
            "src/源,(a).rs",
        ] {
            fs::create_dir_all(root.join(path).parent().expect("parent")).expect("directory");
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("punctuated source");
            let known = verified(&[path], question);
            for (left, right) in [
                ("(", ")"),
                ("[", "]"),
                ("*", "*"),
                ("**", "**"),
                ("'", "'"),
                ("`", "`"),
            ] {
                check(
                    "punctuated-wrapped",
                    question,
                    &known,
                    &format!("See {left}{path}:1{right}."),
                    true,
                );
            }
            for (suffix, accepted) in [("1", true), ("99", false), ("0", false), ("1-", false)] {
                check(
                    "punctuated-selected",
                    question,
                    &known,
                    &format!("See {path}:{suffix}."),
                    accepted,
                );
            }
        }
        // Quotes/stars are legal filename bytes, including after punctuation;
        // backticks are not. Obtain every selected span from the real verifier.
        for path in [
            "src/a,'b.py",
            "src/a,\"b.py",
            "'receipt'.py",
            "\"receipt\".py",
            "src/a;'b.py",
            "src/a;\"b.py",
            "src/('b.py)",
            "src/[\"b.py]",
            "src/a,*b.py",
            "*receipt*.py",
            "src/源,'b.py",
            "src/a,'b',c.py",
            "src/[\"b\"].py",
            "src/a;*b.py",
            "src/(*b.py)",
            "src/a,\"b\",c.py",
        ] {
            fs::create_dir_all(root.join(path).parent().expect("parent")).expect("directory");
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("quote source");
            let known = verified(&[path], question);
            assert_eq!(known, json!([{"path":path,"start_line":1,"end_line":1}]));
            for (left, right) in [
                ("", ""),
                ("'", "'"),
                ("\"", "\""),
                ("*", "*"),
                ("**", "**"),
                ("`", "`"),
                ("['", "']"),
                ("(\"", "\")"),
            ] {
                for (suffix, accepted) in [
                    ("1", true),
                    ("01", true),
                    ("1-1", true),
                    ("99", false),
                    ("0", false),
                    ("1-", false),
                    ("1-2", false),
                    ("2-1", false),
                    ("+1", false),
                    ("18446744073709551616", false),
                    ("１", false),
                ] {
                    check(
                        "quote-context",
                        question,
                        &known,
                        &format!("See {left}{path}:{suffix}{right}."),
                        accepted,
                    );
                }
                for body in [
                    format!("See {left}{path}:1{right},{left}{path}:1{right}."),
                    format!("See {left}{path}:1;{path}:1{right}."),
                    format!("See `{left}{path}:1{right}`."),
                ] {
                    check("quote-adjacent", question, &known, &body, true);
                }
                for (tail, accepted) in [
                    (format!("{path}:1"), true),
                    ("missing.rs:99".to_owned(), false),
                ] {
                    check(
                        "quote-list",
                        question,
                        &known,
                        &format!("See {left}{path}:1,{tail}{right}."),
                        accepted,
                    );
                }
            }
            for prefix in ["../", "missing,", "src/`"] {
                check(
                    "quote-whole-path",
                    question,
                    &known,
                    &format!("See '{prefix}{path}:1'."),
                    false,
                );
            }
        }
        for path in ["src/a,`b.py", "src/a;`b.py", "src/[`b.py]", "`receipt`.py"] {
            fs::create_dir_all(root.join(path).parent().expect("parent")).expect("directory");
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("backtick source");
            let known = verified(&[path], question);
            for marker in ["", "'", "\"", "*", "`"] {
                check(
                    "quote-forbidden",
                    question,
                    &known,
                    &format!("See {marker}{path}:1{marker}."),
                    false,
                );
            }
        }
        verified(&["receipt.py"], question);
        fs::write(
            root.join("other.py"),
            "def secondary_receipt():\n    return True\n",
        )
        .expect("second candidate");
        let two_question = "where is exact_reuse_receipt and secondary_receipt";
        let both = verified(&["receipt.py", "other.py"], two_question);
        let selected = json!([both
            .as_array()
            .expect("array")
            .iter()
            .find(|item| item["path"] == "receipt.py")
            .expect("selected A")]);
        let other = both
            .as_array()
            .expect("array")
            .iter()
            .find(|item| item["path"] == "other.py")
            .expect("verified B");
        let body = format!("See other.py:{}.", other["start_line"]);
        check("F1", two_question, &selected, &body, false);
        check("F1-positive", two_question, &both, &body, true);
        for path in [
            "Makefile",
            ".config.toml",
            "notes.md",
            "receipt.cfg",
            "source/源.rs",
        ] {
            fs::create_dir_all(root.join(path).parent().expect("parent"))
                .expect("parent directory");
            fs::write(
                root.join(path),
                "def exact_reuse_receipt():\n    return True\n",
            )
            .expect("approved source");
            let known = verified(&[path], question);
            if path == "Makefile" {
                for tail in ["1:99", ":99", "0:99", "1-:99", "18446744073709551616:99"] {
                    check(
                        "EX-3",
                        question,
                        &known,
                        &format!("See Makefile:{tail}."),
                        false,
                    );
                }
                check("EX-4-selected", question, &known, "C:Makefile:1", false);
                check(
                    "EX-3-prose",
                    question,
                    &known,
                    "MakefileX:1:99 version:1 OWNER:MEMBER",
                    true,
                );
            }
            check(
                "F3-positive",
                question,
                &known,
                &format!("See {path}:1."),
                true,
            );
            check("F3", question, &known, &format!("See {path}:99."), false);
            check(
                "F2-drive",
                question,
                &known,
                &format!("See C:{path}:1."),
                false,
            );
        }
        let source = "fn decode(line: &str) {\n    let decoded = parse_jsonl_value(line).map_err(|error| {\n        Error::new(JsonlDecodeError { source: error })\n    })?;\n    use_value(decoded);\n}\n";
        fs::write(root.join("receipt.py"), source).expect("multiline source");
        let multiline_question = "where is JSONL parser error conversion";
        let multiline = verified(&["receipt.py"], multiline_question);
        let start = multiline[0]["start_line"].as_u64().expect("start");
        let end = multiline[0]["end_line"].as_u64().expect("end");
        assert!(
            end > start && end < 6,
            "real bounded multiline window: {multiline}"
        );
        check(
            "window-positive",
            multiline_question,
            &multiline,
            &format!("See `receipt.py:{start}-{end}` and receipt.py:{start}."),
            true,
        );
        for body in [
            format!("See receipt.py:{}.", end + 1),
            format!("See receipt.py:{start}-{}.", end + 1),
            "See receipt.py:0.".to_owned(),
        ] {
            check(
                "window-negative",
                multiline_question,
                &multiline,
                &body,
                false,
            );
        }
        fs::write(
            root.join("receipt.py"),
            "def exact_reuse_receipt():\n    return True\n",
        )
        .expect("restore source");
        verified(&["receipt.py"], question);
        assert!(checked > 0, "body case filter must execute an assertion");
        assert!(
            failures.is_empty(),
            "independent answer-body cases: {failures:#?}"
        );
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
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
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
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
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
        let _env = RouteConfigEnvGuard::new(&root, &[]);
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
        assert_semantic_message_output(&route_output, answer);
        assert!(!root.join("probe.args").exists());

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
        assert!(!root.join("probe.args").exists());

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
        assert!(!root.join("probe.args").exists());
        let parsed: serde_json::Value =
            serde_json::from_slice(&override_output).expect("explicit JSON");
        let response = parsed["response"].as_str().expect("response");
        assert!(response.starts_with(answer));
        assert!(response.contains("receipt.py:1"));
        assert!(response.contains("Only the verified source span was inspected."));
        assert!(parsed["tokenUsage"].is_null());
        assert_eq!(first_calls.load(Ordering::SeqCst), 2);
        assert_eq!(second_calls.load(Ordering::SeqCst), 2);

        publish_verified_citation_and_uncertainty(&root, &question);

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
                    let response = parsed["response"].as_str().expect("response");
                    assert!(response.starts_with(answer));
                    assert!(response.contains("receipt.py:1"));
                    assert!(response.contains("Only the verified source was inspected."));
                    // No conversational session exists; invocation identity is not one.
                    assert!(parsed["sessionId"].is_null());
                    assert!(parsed["tokenUsage"].is_null());
                } else {
                    let text = String::from_utf8(output).expect("utf8");
                    assert!(text.starts_with(&format!("{answer}\n")));
                    assert!(text.contains("receipt.py:1\n"));
                    assert!(text.contains("Uncertainty: Only the verified source was inspected.\n"));
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
            let text = String::from_utf8(output).expect("utf8");
            assert!(text.starts_with(&format!("{answer}\n")));
            assert!(text.contains("receipt.py:1\n"));
            assert!(text.contains("Uncertainty: Only the verified source was inspected.\n"));
            assert!(
                !text.starts_with('{'),
                "literal JSON must not activate output mode"
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
        publish_answer_body_citations(&root, &question);
        drop(_env);
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn semantic_question_replans_one_no_hit_with_the_same_kit_route() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-replan-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture directory");
        fs::create_dir_all(root.join("src/proxy")).expect("module directory");
        fs::write(
            root.join("src/proxy/outage_hold.rs"),
            "fn is_outage() -> bool { wait() }\n\n\n\n\n\n\n\n\
             fn wait() -> bool { if remaining() == 0 { return false; } true }\n\n\n\n\n\n\n\n\
             fn remaining() -> u64 { 0 }\n",
        )
        .expect("source");
        fs::write(
            root.join("src/proxy.rs"),
            "mod outage_hold;\nfn attempt() { let _ = outage_hold::wait(); }\n",
        )
        .expect("caller");
        let found = search_repository(
            &root,
            "outage_hold::wait",
            &SearchLimits {
                deadline: Instant::now() + Duration::from_secs(2),
                max_results: DEFAULT_MAX_RESULTS,
                language: None,
                ignores: Vec::new(),
            },
        )
        .expect("native search");
        let report = verify_probe_evidence(&found, &root, "outage_hold::wait", DEFAULT_MAX_RESULTS)
            .expect("verified search")
            .with_following_lines(&root, pbi_rs::semantic::MAX_SEMANTIC_EVIDENCE)
            .expect("verified call chain");
        let (stop_id, stop) = report
            .evidence()
            .iter()
            .enumerate()
            .find(|(_, item)| item.snippet().contains("return false"))
            .expect("direct stop branch");
        let stop_citation = json!({"path":"src/proxy/outage_hold.rs",
            "start_line": stop.location().start_line(),
            "end_line": stop.location().end_line()});
        let _env = RouteConfigEnvGuard::new(&root, &[]);
        let profile = FakeModelProfile::new("pbi-test", "1", "fake-model", ["unused"]);
        let registry = ModelProfileRegistry::new()
            .with_worker(profile)
            .expect("profile");
        let candidate = ModelRouteCandidate::new(ModelRole::Worker, "pbi-test", "1");
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = ModelRouteSnapshot::new(
            registry,
            vec![candidate.clone()],
            ModelRouteAuthorization::new(vec![candidate.clone()]),
        )
        .expect("authorized test snapshot")
        .with_test_llm(
            candidate,
            Arc::new(TestRouteLlm {
                calls: Arc::clone(&calls),
                behavior: TestModelBehavior::Responses(Mutex::new(VecDeque::from([
                    json!({"query":"wait"}).to_string(),
                    json!({
                        "answer":"The caller stops when wait returns false after the budget reaches zero.",
                        "uncertainty":"Only the verified function was inspected.",
                        "citations":[stop_citation.clone()],
                        "stop_evidence_id": stop_id
                    })
                    .to_string(),
                ]))),
                seen: None,
            }),
        )
        .expect("scripted kit route");
        let publisher = ModelRoutePublisher::new(snapshot);
        let mut output = Vec::new();
        let result = run(
            vec!["Why does persistent_outage_hold stop with attempts left?".to_owned()],
            Some(TestRouteInjection::Publisher(&publisher)),
            &mut output,
        );
        if let Err(error) = &result {
            panic!("replanned answer failed: {}", error.message);
        }
        assert!(matches!(result, Ok(0)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            String::from_utf8_lossy(&output).contains("The caller stops when wait returns false")
        );
        drop(_env);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn planning_deadline_is_named_and_never_starts_answer() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-plan-deadline-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            root.join("outage_hold.rs"),
            "fn is_outage() -> bool { true }\n",
        )
        .expect("source");
        let _env = RouteConfigEnvGuard::new(&root, &[]);
        let planning_calls = Arc::new(AtomicUsize::new(0));
        let answer_calls = Arc::new(AtomicUsize::new(0));
        let factory = |routes: &[AdmittedLocalModelRoute]| {
            cli_route_publisher(
                routes,
                "unused",
                true,
                TestModelBehavior::Pending,
                Arc::clone(&planning_calls),
                Arc::clone(&answer_calls),
            )
        };
        let started = Instant::now();
        let mut output = Vec::new();
        let result = run(
            cli_route_arguments("Why does persistent_outage_hold stop with attempts left?"),
            Some(TestRouteInjection::Factory {
                build: &factory,
                deadline: Duration::from_millis(80),
            }),
            &mut output,
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            result.err().map(|error| error.message),
            Some("semantic planning exceeded its bounded deadline".to_owned())
        );
        assert_eq!(planning_calls.load(Ordering::SeqCst), 1);
        assert_eq!(answer_calls.load(Ordering::SeqCst), 0);
        assert!(output.is_empty());
        drop(_env);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn semantic_question_replans_when_its_code_anchor_is_missing_from_evidence() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-anchor-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture directory");
        let source = root.join("metrics.rs");
        fs::write(&source, "fn exact_reuse_receipt() { }\n").expect("source");
        let report = verify_probe_evidence(
            &format!("File: {}, Lines: 1-1\n", source.display()),
            &root,
            "where is exact_reuse_receipt?",
            DEFAULT_MAX_RESULTS,
        )
        .expect("verified but unrelated evidence");
        assert!(question_code_anchor_missing(
            "Why does persistent_outage_hold stop with attempts left?",
            &report
        ));
        assert!(!question_code_anchor_missing(
            "Why does exact_reuse_receipt() return?",
            &report
        ));
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn candidate_tool_exposes_implementation_names_from_matching_source_files() {
        let root = std::path::PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp").join(format!(
            "pbi-rs-candidates-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("fixture directory");
        fs::write(
            root.join("src/outage_hold.rs"),
            "fn start() {}\nfn classify() {}\nfn remember() {}\nfn finish() {}\nfn probe() {}\nfn reload() {}\nfn count() {}\nfn wait() -> bool { if remaining() == 0 { return false; } true }\nfn remaining() -> u64 { 0 }\nfn is_outage() -> bool { true }\nfn drain_attempts() -> bool { confirmed() }\n",
        )
        .expect("implementation");
        fs::write(root.join("src/metrics.rs"), "fn unrelated() {}\n").expect("noise");
        let candidates = candidate_symbols(
            &root,
            "Why does persistent_outage_hold stop with attempts left?",
            &SearchLimits {
                deadline: Instant::now() + Duration::from_secs(2),
                max_results: DEFAULT_MAX_RESULTS,
                language: None,
                ignores: Vec::new(),
            },
        )
        .expect("bounded source names");
        assert_eq!(
            candidates,
            [("src/outage_hold.rs".to_owned(), "wait".to_owned())]
        );
        assert!(candidates.len() <= 8);
        assert!(candidates.iter().all(|(path, _)| path != "src/metrics.rs"));
        fs::remove_dir_all(root).expect("remove fixture");
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
        let guard = RouteConfigEnvGuard::new(
            &root,
            &[("PBI_RS_PROBE", Some(probe.to_string_lossy().into_owned()))],
        );
        // The guard captured the baseline under the same lock as every writer.
        let outside = guard.previous_dir.clone();
        let saved_probe = guard.previous_env.first().expect("saved probe").1.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _env = guard;
            assert_eq!(env::current_dir().expect("cwd"), root);
            assert_eq!(env::var_os("PBI_RS_PROBE"), Some(probe.into_os_string()));
            panic!("controlled unwind");
        }));
        let panic = result.expect_err("controlled unwind must panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"controlled unwind"));
        let check = RouteConfigEnvGuard::lock();
        assert_eq!(env::current_dir().expect("cwd"), outside);
        assert_eq!(env::var_os("PBI_RS_PROBE"), saved_probe);
        drop(check);
        let _ = fs::remove_dir_all(root);
    }

    mod debug_config_route {
        include!("debug_config_route_tests.rs");
    }
}
