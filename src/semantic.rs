use crate::{EvidenceReport, SourceEvidence};
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};
use workflow_adk::model_profiles::{
    CredentialBroker, CredentialHandle, ModelProfileRegistry, ModelRuntimeConfig,
    OpenAiCompatibleProfile,
};
use workflow_adk::{
    EscalationPolicy, InferenceBudget, ModelInvocationSpec, ModelProfileIdentity, ModelRole,
    ModelRouteAuthorization, ModelRouteCancellation, ModelRouteCandidate, ModelRoutePolicy,
    ModelRoutePublisher, ModelRouteSnapshot, ModelRouteTerminalErrorKind, PromptProtocol,
    ProviderRouteIdentity, ReasoningEffort, StructuredOutputContract, MAX_MODEL_ROUTE_CANDIDATES,
};
use workflow_runtime::TrustDomain;

pub const MAX_SEMANTIC_EVIDENCE: usize = 8;
pub const MAX_SEMANTIC_CONTEXT_BYTES: usize = 24 * 1024;
pub const MAX_SEMANTIC_OUTPUT_BYTES: usize = 16 * 1024;

const APPROVED_LOCAL_BASE_URLS: [&str; 2] = ["http://gb10:18009/v1", "http://localhost:18317/v1"];
const APPROVED_LOCAL_MODELS: [&str; 3] = [
    "abliterated-qwen-latest-27b-none",
    "abliterated-qwen-latest-27b-low",
    "abliterated-qwen-latest-27b-medium",
];
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://localhost:18317/v1";
pub const DEFAULT_LOCAL_MODEL: &str = "abliterated-qwen-latest-27b-none";
pub const MODEL_CREDENTIAL_HANDLES: [&str; 3] =
    ["CLIPROXY_API_KEY", "OPENAI_API_KEY", "LOCAL_ROUTER_API_KEY"];
const ADK_ENABLE_ENV: &str = "PBI_RS_ADK_ENABLE";

pub static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["answer", "uncertainty", "citations"],
  "properties": {
    "answer": {"type": "string", "minLength": 1, "maxLength": 4096},
    "uncertainty": {"type": "string", "minLength": 1, "maxLength": 512},
    "citations": {
      "type": "array",
      "minItems": 1,
      "maxItems": 8,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "start_line", "end_line"],
        "properties": {
          "path": {"type": "string", "minLength": 1, "maxLength": 512},
          "start_line": {"type": "integer", "minimum": 1},
          "end_line": {"type": "integer", "minimum": 1}
        }
      }
    }
  }
}"#;

const SEARCH_PLAN_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["query"],
  "properties": {"query": {"type": "string", "minLength": 1, "maxLength": 128}}
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticAnswer {
    answer: String,
    uncertainty: String,
    citations: Vec<SourceEvidence>,
    invocation_identity: String,
}

impl SemanticAnswer {
    pub fn answer(&self) -> &str {
        &self.answer
    }

    pub fn uncertainty(&self) -> &str {
        &self.uncertainty
    }

    pub fn citations(&self) -> &[SourceEvidence] {
        &self.citations
    }

    pub fn invocation_identity(&self) -> &str {
        &self.invocation_identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticError {
    EmptyQuestion,
    NoEvidence,
    SourceOutsideRoot,
    InputTooLarge,
    Protocol,
    Cancelled,
    PlanningDeadlineExceeded,
    DeadlineExceeded,
    Route {
        kind: ModelRouteTerminalErrorKind,
        attempts: usize,
    },
    InvalidOutput,
    CitationMismatch,
}

impl fmt::Display for SemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::Route { kind, attempts } = self {
            return write!(
                formatter,
                "semantic model route failed: {kind:?}; attempts={attempts}"
            );
        }
        formatter.write_str(match self {
            Self::EmptyQuestion => "semantic question is empty",
            Self::NoEvidence => "semantic investigation requires verified source evidence",
            Self::SourceOutsideRoot => "semantic citation crossed the repository boundary",
            Self::InputTooLarge => "semantic evidence exceeded the bounded context",
            Self::Protocol => "semantic invocation protocol could not be built",
            Self::Cancelled => "semantic investigation was cancelled",
            Self::PlanningDeadlineExceeded => "semantic planning exceeded its bounded deadline",
            Self::DeadlineExceeded => "semantic investigation exceeded its bounded deadline",
            Self::Route { .. } => unreachable!("route errors are formatted above"),
            Self::InvalidOutput => "semantic model output failed validation",
            Self::CitationMismatch => "semantic model returned an unverified citation",
        })
    }
}

impl std::error::Error for SemanticError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRouteError {
    InvalidEnable,
    IncompleteConfig,
    CandidateLimit,
    UnapprovedRoute,
    UnapprovedModel,
    UnapprovedCredentialHandle,
    MissingCredential,
    Profile,
    InvalidConfig,
}

impl fmt::Display for SemanticRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnable => "semantic route opt-in is invalid",
            Self::IncompleteConfig => "semantic route configuration is incomplete",
            Self::CandidateLimit => "semantic route configuration exceeds the kit candidate limit",
            Self::UnapprovedRoute => "semantic route is not an approved local route",
            Self::UnapprovedModel => "semantic model is not an approved local model",
            Self::UnapprovedCredentialHandle => "semantic credential handle is not approved",
            Self::MissingCredential => "semantic route requires an available credential handle",
            Self::Profile => "semantic model profile could not be bound",
            Self::InvalidConfig => "semantic route configuration file is invalid",
        })
    }
}

impl std::error::Error for SemanticRouteError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalModelRoute {
    base_url: String,
    model: String,
    credential_handle: String,
}

impl LocalModelRoute {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        credential_handle: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            credential_handle: credential_handle.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedLocalModelRoute {
    base_url: String,
    model: String,
    credential_handle: String,
    profile_name: String,
}

impl AdmittedLocalModelRoute {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn candidate(&self) -> ModelRouteCandidate {
        ModelRouteCandidate::new(ModelRole::Worker, self.profile_name.clone(), "1")
    }
}

pub fn admit_local_routes(
    routes: Vec<LocalModelRoute>,
) -> Result<Vec<AdmittedLocalModelRoute>, SemanticRouteError> {
    if routes.is_empty() {
        return Err(SemanticRouteError::IncompleteConfig);
    }
    if routes.len() > MAX_MODEL_ROUTE_CANDIDATES {
        return Err(SemanticRouteError::CandidateLimit);
    }
    routes
        .into_iter()
        .enumerate()
        .map(|(index, route)| {
            validate_local_route(&route.base_url, &route.model)?;
            select_explicit_credential_handle(&route.credential_handle)?;
            Ok(AdmittedLocalModelRoute {
                base_url: route.base_url,
                model: route.model,
                credential_handle: route.credential_handle,
                profile_name: local_route_profile_name(index),
            })
        })
        .collect()
}

fn local_route_profile_name(index: usize) -> String {
    if index == 0 {
        "pbi-rs-local".to_owned()
    } else {
        format!("pbi-rs-local-fallback-{}", index + 1)
    }
}

pub fn validate_local_route(base_url: &str, model: &str) -> Result<(), SemanticRouteError> {
    if !APPROVED_LOCAL_BASE_URLS.contains(&base_url) {
        return Err(SemanticRouteError::UnapprovedRoute);
    }
    if !APPROVED_LOCAL_MODELS.contains(&model) {
        return Err(SemanticRouteError::UnapprovedModel);
    }
    Ok(())
}

/// Select and validate public route fields without probing credentials or enabling ADK.
/// Only an explicit PBI_CONFIG_FILE is read; complete environment fields bypass it.
pub fn local_route_from_environment() -> Result<(String, String), SemanticRouteError> {
    let env_base = first_value(&["CLIPROXY_BASE_URL", "LOCAL_ROUTER_BASEURL"])?;
    let env_model = first_value(&["LOCAL_MODEL", "LLM_MODEL"])?;
    let (base_url, model) = if let (Some(base_url), Some(model)) = (&env_base, &env_model) {
        (base_url.clone(), model.clone())
    } else {
        let configured = explicit_config_route()?;
        match (env_base, env_model, configured) {
            (Some(base_url), None, Some(config)) => (base_url, config.primary),
            (None, Some(model), Some(config)) => {
                endpoint_base_for_model(&config.endpoints, &model)?
            }
            (None, None, Some(config)) => {
                endpoint_base_for_model(&config.endpoints, &config.primary)?
            }
            (base_url, model, None) => local_route_from_values(base_url, model)?,
            (Some(_), Some(_), Some(_)) => unreachable!("both fields returned before config"),
        }
    };
    validate_local_route(&base_url, &model)?;
    Ok((base_url, model))
}

pub fn explicit_admitted_routes_from_environment(
) -> Result<Option<Vec<AdmittedLocalModelRoute>>, SemanticRouteError> {
    if !semantic_route_opted_in()? {
        return Ok(None);
    }
    let (base_url, model) = local_route_from_environment()?;
    let credential_name = if let Some(name) = env::var_os("PBI_RS_CREDENTIAL_HANDLE") {
        let name = name
            .into_string()
            .map_err(|_| SemanticRouteError::UnapprovedCredentialHandle)?;
        select_explicit_credential_handle(&name)?.to_owned()
    } else {
        MODEL_CREDENTIAL_HANDLES
            .into_iter()
            .find(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
            .ok_or(SemanticRouteError::MissingCredential)?
            .to_owned()
    };
    let routes = admit_local_routes(vec![LocalModelRoute::new(base_url, model, credential_name)])?;
    Ok(Some(routes))
}

pub fn local_route_publisher_from_environment(
) -> Result<Option<ModelRoutePublisher>, SemanticRouteError> {
    explicit_admitted_routes_from_environment()?
        .map(|routes| local_route_publisher_from_admitted_routes(&routes))
        .transpose()
}

pub fn local_route_publisher_from_cli_routes(
    routes: &[AdmittedLocalModelRoute],
) -> Result<Option<ModelRoutePublisher>, SemanticRouteError> {
    if !semantic_route_opted_in()? {
        return Ok(None);
    }
    Ok(Some(local_route_publisher_from_admitted_routes(routes)?))
}

fn semantic_route_opted_in() -> Result<bool, SemanticRouteError> {
    match env::var(ADK_ENABLE_ENV).as_deref() {
        Err(_) | Ok("0") => Ok(false),
        Ok("1") => Ok(true),
        Ok(_) => Err(SemanticRouteError::InvalidEnable),
    }
}

pub fn local_route_publisher_from_admitted_routes(
    routes: &[AdmittedLocalModelRoute],
) -> Result<ModelRoutePublisher, SemanticRouteError> {
    if routes.is_empty() {
        return Err(SemanticRouteError::IncompleteConfig);
    }
    if routes.len() > MAX_MODEL_ROUTE_CANDIDATES {
        return Err(SemanticRouteError::CandidateLimit);
    }
    for route in routes {
        validate_local_route(&route.base_url, &route.model)?;
        select_explicit_credential_handle(&route.credential_handle)?;
    }

    let make_profile = |route: &AdmittedLocalModelRoute| {
        OpenAiCompatibleProfile::new(
            route.profile_name.clone(),
            "1",
            route.model.clone(),
            route.base_url.clone(),
            CredentialHandle::environment(route.credential_handle.clone()),
        )
        .with_provider("openai")
        .with_runtime(ModelRuntimeConfig::default().with_timeout(Duration::from_secs(30)))
    };
    let mut profiles = ModelProfileRegistry::new()
        .with_worker(make_profile(&routes[0]))
        .map_err(|_| SemanticRouteError::Profile)?;
    for route in &routes[1..] {
        profiles
            .register(make_profile(route))
            .map_err(|_| SemanticRouteError::Profile)?;
    }
    let candidates = routes
        .iter()
        .map(AdmittedLocalModelRoute::candidate)
        .collect::<Vec<_>>();
    let snapshot = ModelRouteSnapshot::new(
        profiles,
        candidates.clone(),
        ModelRouteAuthorization::new(candidates),
    )
    .map_err(|_| SemanticRouteError::Profile)?;
    Ok(ModelRoutePublisher::new(snapshot))
}

fn local_route_from_values(
    base_url: Option<String>,
    model: Option<String>,
) -> Result<(String, String), SemanticRouteError> {
    let base_url = base_url.unwrap_or_else(|| DEFAULT_LOCAL_BASE_URL.to_owned());
    let model = model.unwrap_or_else(|| DEFAULT_LOCAL_MODEL.to_owned());
    validate_local_route(&base_url, &model)?;
    Ok((base_url, model))
}

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const O_NONBLOCK: i32 = 0x800;

fn set_blocking(fd: i32) -> Result<(), SemanticRouteError> {
    let flags = unsafe { libc_fcntl(fd, 3, 0) };
    if flags < 0 || unsafe { libc_fcntl(fd, 4, flags & !O_NONBLOCK) } < 0 {
        return Err(SemanticRouteError::InvalidConfig);
    }
    Ok(())
}

unsafe fn libc_fcntl(fd: i32, cmd: i32, arg: i32) -> i32 {
    // SAFETY: fd is owned by the caller; F_GETFL/F_SETFL only read or clear O_NONBLOCK.
    unsafe {
        extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        }
        fcntl(fd, cmd, arg)
    }
}

struct ExplicitConfig {
    primary: String,
    endpoints: Vec<(String, String)>,
}

fn explicit_config_route() -> Result<Option<ExplicitConfig>, SemanticRouteError> {
    let Some(path) = env::var_os("PBI_CONFIG_FILE") else {
        return Ok(None);
    };
    if path.is_empty() {
        return Ok(None);
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(&path)
        .map_err(|_| SemanticRouteError::InvalidConfig)?;
    let metadata = file
        .metadata()
        .map_err(|_| SemanticRouteError::InvalidConfig)?;
    let kind = metadata.file_type();
    if !kind.is_file() || kind.is_fifo() {
        return Err(SemanticRouteError::InvalidConfig);
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(SemanticRouteError::InvalidConfig);
    }
    set_blocking(file.as_raw_fd())?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SemanticRouteError::InvalidConfig)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(SemanticRouteError::InvalidConfig);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| SemanticRouteError::InvalidConfig)?;
    let table: toml::Table = text
        .parse()
        .map_err(|_| SemanticRouteError::InvalidConfig)?;
    let selected = table
        .get("primary_model")
        .or_else(|| table.get("model"))
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(SemanticRouteError::InvalidConfig)?;
    let endpoints = table
        .get("endpoints")
        .and_then(toml::Value::as_array)
        .ok_or(SemanticRouteError::InvalidConfig)?;
    let parsed = endpoints
        .iter()
        .filter_map(toml::Value::as_table)
        .filter_map(|endpoint| {
            let model = endpoint.get("model").and_then(toml::Value::as_str)?;
            let base_url = endpoint.get("base_url").and_then(toml::Value::as_str)?;
            (!model.is_empty() && !base_url.is_empty())
                .then(|| (model.to_owned(), base_url.to_owned()))
        })
        .collect::<Vec<_>>();
    if parsed.is_empty() {
        return Err(SemanticRouteError::InvalidConfig);
    }
    Ok(Some(ExplicitConfig {
        primary: selected.to_owned(),
        endpoints: parsed,
    }))
}

fn endpoint_base_for_model(
    endpoints: &[(String, String)],
    model: &str,
) -> Result<(String, String), SemanticRouteError> {
    endpoints
        .iter()
        .find(|(endpoint_model, _)| endpoint_model == model)
        .map(|(_, base_url)| (base_url.clone(), model.to_owned()))
        .ok_or(SemanticRouteError::InvalidConfig)
}

fn select_explicit_credential_handle(name: &str) -> Result<&str, SemanticRouteError> {
    MODEL_CREDENTIAL_HANDLES
        .contains(&name)
        .then_some(name)
        .ok_or(SemanticRouteError::UnapprovedCredentialHandle)
}

fn first_value(names: &[&str]) -> Result<Option<String>, SemanticRouteError> {
    for name in names {
        if env::var_os(name).is_some() {
            let value = env::var(name).map_err(|_| SemanticRouteError::IncompleteConfig)?;
            if value.trim().is_empty() {
                return Err(SemanticRouteError::IncompleteConfig);
            }
            return Ok(Some(value));
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AllowedCitation {
    path: String,
    start_line: usize,
    end_line: usize,
    evidence_index: usize,
}

#[cfg(test)]
#[path = "explicit_config_route_tests.rs"]
mod explicit_config_route_tests;

/// Ask the authorized kit route for one short source search when literal retrieval has no hits.
/// The resulting text is only a query; repository reads and citation admission stay in Rust.
pub async fn plan_search_query(
    question: &str,
    candidates: &[(String, String)],
    policy: &ModelRoutePolicy,
    deadline: Instant,
    cancellation: &ModelRouteCancellation,
) -> Result<String, SemanticError> {
    if question.trim().is_empty() {
        return Err(SemanticError::EmptyQuestion);
    }
    if Instant::now() >= deadline {
        return Err(SemanticError::PlanningDeadlineExceeded);
    }
    let schema: Value =
        serde_json::from_str(SEARCH_PLAN_SCHEMA).map_err(|_| SemanticError::Protocol)?;
    let protocol = PromptProtocol::new(
        "A literal search did not find trustworthy implementation evidence. If candidate_functions are supplied, choose exactly one listed function name and return it verbatim as query. For why/stop questions, prefer a state-classifying predicate over the function that drains or records attempts. Prefer implementation functions over metrics and tests. If there are no candidates, choose a different likely identifier without repeating a snake_case name from the question. Do not answer or invent citations.",
        Vec::new(),
        schema.clone(),
        json!({
            "question": question,
            "candidate_functions": candidates.iter().map(|(path, name)| json!({"path":path,"name":name})).collect::<Vec<_>>(),
        }),
        TrustDomain::ConditionallyTrustedContent,
    )
    .map_err(|_| SemanticError::Protocol)?;
    let output = StructuredOutputContract::new(schema, 512).map_err(|_| SemanticError::Protocol)?;
    let budget = InferenceBudget::new(ReasoningEffort::Low, 512, 0)
        .map(|budget| budget.with_escalation(EscalationPolicy::None))
        .map_err(|_| SemanticError::Protocol)?;
    let route_placeholder = ProviderRouteIdentity::new(
        ModelProfileIdentity::new("pbi-rs-route-placeholder", "1"),
        "openai",
        "pbi-rs-route-placeholder",
        "pbi-rs-route-placeholder",
        "pbi-rs-route-placeholder",
    );
    let spec = ModelInvocationSpec::new(
        protocol,
        question.to_owned(),
        route_placeholder,
        budget,
        output,
    )
    .map_err(|_| SemanticError::Protocol)?;
    let broker = CredentialBroker::new();
    let result = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        policy.invoke(&spec, &broker, cancellation),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            return Err(match error.kind() {
                ModelRouteTerminalErrorKind::Cancelled => SemanticError::Cancelled,
                ModelRouteTerminalErrorKind::DeadlineExceeded => {
                    SemanticError::PlanningDeadlineExceeded
                }
                kind => SemanticError::Route {
                    kind,
                    attempts: error.attempts().len(),
                },
            });
        }
        Err(_) => return Err(SemanticError::PlanningDeadlineExceeded),
    };
    let value = result.into_output();
    let query = value
        .as_object()
        .and_then(|object| object.get("query"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| {
            !query.is_empty()
                && query.len() <= 128
                && !query.chars().any(char::is_control)
                && *query != question.trim()
                && (candidates.is_empty() || candidates.iter().any(|(_, name)| name == query))
        })
        .ok_or(SemanticError::InvalidOutput)?;
    Ok(query.to_owned())
}

pub async fn investigate(
    question: &str,
    root: &Path,
    report: &EvidenceReport,
    policy: &ModelRoutePolicy,
    deadline: Instant,
    cancellation: &ModelRouteCancellation,
) -> Result<SemanticAnswer, SemanticError> {
    if question.trim().is_empty() {
        return Err(SemanticError::EmptyQuestion);
    }
    if report.evidence().is_empty() {
        return Err(SemanticError::NoEvidence);
    }
    if Instant::now() >= deadline {
        return Err(SemanticError::DeadlineExceeded);
    }

    let root = std::fs::canonicalize(root).map_err(|_| SemanticError::SourceOutsideRoot)?;
    let evidence = report
        .evidence()
        .iter()
        .take(MAX_SEMANTIC_EVIDENCE)
        .collect::<Vec<_>>();
    let mut allowed = Vec::with_capacity(evidence.len());
    let mut evidence_json = Vec::with_capacity(evidence.len());
    for (evidence_index, item) in evidence.iter().enumerate() {
        let path = item
            .location()
            .path()
            .strip_prefix(&root)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or(SemanticError::SourceOutsideRoot)?;
        allowed.push(AllowedCitation {
            path: path.clone(),
            start_line: item.location().start_line(),
            end_line: item.location().end_line(),
            evidence_index,
        });
        evidence_json.push(json!({
            "id": evidence_index,
            "path": path,
            "start_line": item.location().start_line(),
            "end_line": item.location().end_line(),
            "target": item.target(),
            "symbol": item.symbol(),
            "snippet": item.snippet(),
            "relevance": item.relevance(),
        }));
    }
    let common_data = json!({
        "question": question,
        "verified_evidence": evidence_json,
        "missing_targets": report.missing_targets(),
    });
    let common_data_bytes =
        serde_json::to_vec(&common_data).map_err(|_| SemanticError::Protocol)?;
    if common_data_bytes.len() > MAX_SEMANTIC_CONTEXT_BYTES {
        return Err(SemanticError::InputTooLarge);
    }

    let output_schema: Value =
        serde_json::from_str(OUTPUT_SCHEMA).map_err(|_| SemanticError::Protocol)?;
    let protocol = PromptProtocol::new(
        "Answer only from VERIFIED_EVIDENCE. For why questions, state the direct stop condition and cite its return branch; also state how the caller uses that return value and cite the caller. If you explain a budget, cite the definition that computes it, not just a call to that definition. Distinguish later branches and independent limits, respecting short-circuit expressions. An unmatched name does not imply a separate implementation; do not speculate about one. Return one compact answer, explicit uncertainty, and citations that exactly match a verified path and line span. Do not invent files, lines, symbols, or facts. No repository tools are available to this model.",
        Vec::new(),
        output_schema.clone(),
        common_data,
        TrustDomain::ConditionallyTrustedContent,
    )
    .map_err(|_| SemanticError::Protocol)?;
    let output = StructuredOutputContract::new(output_schema, MAX_SEMANTIC_OUTPUT_BYTES)
        .map_err(|_| SemanticError::Protocol)?;
    let budget = InferenceBudget::new(ReasoningEffort::Low, 512, 0)
        .map(|budget| budget.with_escalation(EscalationPolicy::None))
        .map_err(|_| SemanticError::Protocol)?;
    let route_placeholder = ProviderRouteIdentity::new(
        ModelProfileIdentity::new("pbi-rs-route-placeholder", "1"),
        "openai",
        "pbi-rs-route-placeholder",
        "pbi-rs-route-placeholder",
        "pbi-rs-route-placeholder",
    );
    let spec = ModelInvocationSpec::new(
        protocol,
        question.to_owned(),
        route_placeholder,
        budget,
        output,
    )
    .map_err(|_| SemanticError::Protocol)?;

    let broker = CredentialBroker::new();
    let result = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        policy.invoke(&spec, &broker, cancellation),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            return Err(match error.kind() {
                ModelRouteTerminalErrorKind::Cancelled => SemanticError::Cancelled,
                ModelRouteTerminalErrorKind::DeadlineExceeded => SemanticError::DeadlineExceeded,
                kind => SemanticError::Route {
                    kind,
                    attempts: error.attempts().len(),
                },
            });
        }
        Err(_) => return Err(SemanticError::DeadlineExceeded),
    };
    if Instant::now() >= deadline {
        return Err(SemanticError::DeadlineExceeded);
    }
    let invocation_identity = result.provenance().invocation_identity().to_owned();
    decode_answer(
        result.into_output(),
        &allowed,
        &evidence,
        report,
        invocation_identity,
    )
}

fn decode_answer(
    value: Value,
    allowed: &[AllowedCitation],
    evidence: &[&SourceEvidence],
    report: &EvidenceReport,
    invocation_identity: String,
) -> Result<SemanticAnswer, SemanticError> {
    let object = value.as_object().ok_or(SemanticError::InvalidOutput)?;
    let answer = object
        .get("answer")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SemanticError::InvalidOutput)?
        .to_owned();
    let uncertainty = object
        .get("uncertainty")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SemanticError::InvalidOutput)?
        .to_owned();
    let citation_values = object
        .get("citations")
        .and_then(Value::as_array)
        .ok_or(SemanticError::InvalidOutput)?;
    if citation_values.is_empty() || citation_values.len() > MAX_SEMANTIC_EVIDENCE {
        return Err(SemanticError::InvalidOutput);
    }
    let mut citations = Vec::with_capacity(citation_values.len());
    let mut selected = Vec::with_capacity(citation_values.len());
    for citation in citation_values {
        let citation = citation
            .as_object()
            .ok_or(SemanticError::CitationMismatch)?;
        let path = citation
            .get("path")
            .and_then(Value::as_str)
            .ok_or(SemanticError::CitationMismatch)?;
        let start_line = citation
            .get("start_line")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(SemanticError::CitationMismatch)?;
        let end_line = citation
            .get("end_line")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(SemanticError::CitationMismatch)?;
        let matched = allowed
            .iter()
            .find(|allowed| {
                allowed.path == path
                    && start_line <= end_line
                    && allowed.start_line <= start_line
                    && end_line <= allowed.end_line
            })
            .ok_or(SemanticError::CitationMismatch)?;
        if citations
            .iter()
            .any(|item: &SourceEvidence| item == evidence[matched.evidence_index])
        {
            return Err(SemanticError::CitationMismatch);
        }
        citations.push(evidence[matched.evidence_index].clone());
        selected.push(AllowedCitation {
            path: matched.path.clone(),
            start_line,
            end_line,
            evidence_index: matched.evidence_index,
        });
    }
    answer_body_citations_match(&answer, &selected.iter().collect::<Vec<_>>())?;
    // A followed definition is admitted only through a parsed call in its
    // parent window. Keep that call site and its ancestors with the model's
    // selected citation so causal claims retain their executable path.
    let mut cited_indices = selected
        .iter()
        .map(|citation| citation.evidence_index)
        .collect::<Vec<_>>();
    for citation in &selected {
        let mut child = citation.evidence_index;
        while let Some(parent) = report.followed_from(child) {
            if parent >= child || parent >= evidence.len() {
                return Err(SemanticError::CitationMismatch);
            }
            if !cited_indices.contains(&parent) {
                citations.push(evidence[parent].clone());
                cited_indices.push(parent);
            }
            child = parent;
        }
    }
    Ok(SemanticAnswer {
        answer,
        uncertainty,
        citations,
        invocation_identity,
    })
}

/// One contextual boundary: retain filename punctuation until the whole token
/// is classified. Paired outer wrappers may enclose a citation list; a colon
/// inside the pair distinguishes `(a.py:1)` from the filename `(a.py):1`.
/// A comma/semicolon follows a complete numeric location or excluded atom;
/// a malformed prefix cannot manufacture a list boundary.
/// Fixed linear punctuation-index passes, then one atom/owner scan.
/// O(answer bytes) storage; each whole token is classified once.
fn answer_body_citations_match(
    answer: &str,
    selected: &[&AllowedCitation],
) -> Result<(), SemanticError> {
    let wrappers = BodyWrappers::new(answer);
    let mut owners = BodyOwners::default();
    let mut start = 0;
    let mut has_colon = false;
    let mut separator_checked = false;
    let mut after_wrapper = false;
    let mut cursor = 0;
    while cursor <= answer.len() {
        let ch = answer[cursor..].chars().next().unwrap_or('\n');
        let width = ch.len_utf8();
        let closing = owners.limit(answer.len()) == cursor && !owners.groups.is_empty();
        if cursor == start && !closing {
            if let Some(content) = owners.bracket_prefix(&wrappers, answer, cursor) {
                cursor = content;
                start = cursor;
                continue;
            }
            let limit = owners.limit(answer.len());
            if let Some((end, marker_width)) = wrappers.pair(answer, cursor, limit) {
                owners.push(body_marker(answer.as_bytes()[cursor]), end, marker_width);
                cursor += marker_width;
                start = cursor;
                continue;
            }
        }
        let separator =
            if matches!(ch, ',' | ';') && (has_colon || after_wrapper) && !separator_checked {
                separator_checked = true;
                after_wrapper || body_separator_after(&answer[start..cursor])
            } else {
                false
            };
        if ch.is_whitespace() || closing || separator {
            let token = answer[start..cursor].trim_end_matches(['.', '!', '?', '。']);
            body_citation(token, selected)?;
            let consumed = if closing { owners.pop() } else { width };
            cursor += consumed;
            start = cursor;
            has_colon = false;
            separator_checked = false;
            after_wrapper = closing;
        } else {
            after_wrapper = false;
            has_colon |= ch == ':';
            cursor += width;
        }
    }
    Ok(())
}

/// Test at most the first prospective separator of an atom. On failure the
/// remainder stays in that atom, preventing both suffix rescue and rescanning.
fn body_separator_after(token: &str) -> bool {
    if excluded_body_context(token) {
        return true;
    }
    let Some((_, spec)) = token.rsplit_once(':') else {
        return false;
    };
    let (first, last) = spec.split_once('-').unwrap_or((spec, spec));
    parse_body_line(first).is_some() && parse_body_line(last).is_some()
}

fn body_wrapper_end(tail: &str) -> bool {
    let mut chars = tail.chars();
    let Some(ch) = chars.next() else {
        return true;
    };
    ch.is_whitespace()
        || matches!(ch, ',' | ';' | ')' | ']' | '}' | '*' | '`' | '\'' | '"')
        || matches!(ch, '.' | '!' | '?' | '。') && chars.next().is_none_or(|ch| ch.is_whitespace())
}

/// Index punctuation, not speculative atoms. Only the shared boundary scan
/// admits an opener; its ordered owner stack reserves each enclosing close.
struct BodyWrappers {
    brackets: Vec<usize>,
    runs: Vec<usize>,
    next_close: Vec<[usize; 4]>,
    colons: Vec<usize>,
    // Neutral candidates only; the forward owner scan still admits brackets.
    enclosing_atom_bracket: Vec<usize>,
    previous_prefix_marker: Vec<usize>,
}

fn body_marker(byte: u8) -> Option<usize> {
    match byte {
        b'*' => Some(0),
        b'`' => Some(1),
        b'\'' => Some(2),
        b'"' => Some(3),
        _ => None,
    }
}

/// Consecutive same-kind owners share a width sum. Relocating their reserved
/// run does not walk the owners: old entries use the shared end, newer entries
/// retain their explicit end until the next relocation.
struct BodyOwnerGroup {
    kind: Option<usize>,
    entries: Vec<(usize, usize)>,
    width: usize,
    moved: usize,
    after: usize,
}

impl BodyOwnerGroup {
    fn end(&self) -> usize {
        if self.entries.len() <= self.moved {
            self.after - self.width
        } else {
            self.entries.last().expect("nonempty owner group").0
        }
    }
}

#[derive(Default)]
struct BodyOwners {
    groups: Vec<BodyOwnerGroup>,
    brackets: Vec<usize>,
    #[cfg(test)]
    examined: usize,
}

impl BodyOwners {
    fn limit(&self, length: usize) -> usize {
        self.groups.last().map_or(length, BodyOwnerGroup::end)
    }

    fn push(&mut self, kind: Option<usize>, end: usize, width: usize) {
        if kind.is_none() {
            self.brackets.push(end);
        }
        if kind.is_none() || self.groups.last().is_none_or(|group| group.kind != kind) {
            self.groups.push(BodyOwnerGroup {
                kind,
                entries: Vec::new(),
                width: 0,
                moved: 0,
                after: 0,
            });
        }
        let group = self.groups.last_mut().expect("new or existing group");
        group.entries.push((end, width));
        group.width += width;
    }

    fn pop(&mut self) -> usize {
        let group = self.groups.last_mut().expect("active wrapper");
        let (_, width) = group.entries.pop().expect("active wrapper entry");
        group.width -= width;
        group.moved = group.moved.min(group.entries.len());
        if group.kind.is_none() {
            self.brackets.pop();
        }
        if group.entries.is_empty() {
            self.groups.pop();
        }
        width
    }

    /// Admit a bracket only at an actual atom start, possibly behind a fully
    /// consumed marker prefix. Existing owners keep their opening widths.
    /// Earlier owners reserve trailing close bytes first, before the bracket
    /// gets authority to hide any close. Failure leaves all reservations intact.
    ///
    /// Every marker kind uses its first candidate after this bracket. Those
    /// candidate offsets must decrease inward; a nonconsecutive repeated kind
    /// is therefore impossible. At most four groups/prefix runs are examined,
    /// including failed proposals. Same-kind depth is summarized by width, not
    /// rescanned. Along with the punctuation indexes and one forward traversal
    /// this bounds total work and storage by O(answer bytes), including S_m.
    fn bracket_prefix(
        &mut self,
        wrappers: &BodyWrappers,
        answer: &str,
        start: usize,
    ) -> Option<usize> {
        #[cfg(test)]
        {
            self.examined += 1;
        }
        let structural_limit = self.brackets.last().copied().unwrap_or(answer.len());
        let mut cursor = start;
        let mut prefix = Vec::new();
        let mut seen = [false; 4];
        while let Some(kind) = answer.as_bytes().get(cursor).copied().and_then(body_marker) {
            #[cfg(test)]
            {
                self.examined += 1;
            }
            if seen[kind] {
                return None;
            }
            seen[kind] = true;
            let (_, width) = wrappers.pair(answer, cursor, structural_limit)?;
            if width != wrappers.runs[cursor] {
                return None; // Literal remainder wins; never bootstrap a wider prefix.
            }
            prefix.push((kind, width));
            cursor += width;
        }
        let (bracket_end, _) = wrappers.bracket_pair(answer, cursor, structural_limit)?;

        let mut first = self.groups.len();
        seen = [false; 4];
        while first > 0 && self.groups[first - 1].end() <= bracket_end {
            #[cfg(test)]
            {
                self.examined += 1;
            }
            let kind = self.groups[first - 1].kind?;
            if seen[kind] {
                return None;
            }
            seen[kind] = true;
            first -= 1;
        }
        let mut proposed: Vec<(usize, usize, Option<usize>)> = self.groups[first..]
            .iter()
            .enumerate()
            .map(|(index, group)| {
                (
                    group.kind.expect("marker group"),
                    group.width,
                    Some(first + index),
                )
            })
            .collect();
        for &(kind, width) in &prefix {
            if let Some(group) = proposed.last_mut().filter(|group| group.0 == kind) {
                group.1 += width;
            } else {
                proposed.push((kind, width, None));
            }
        }
        let mut limit = first
            .checked_sub(1)
            .map_or(answer.len(), |index| self.groups[index].end());
        let mut moves = Vec::new();
        seen = [false; 4];
        for (kind, width, group) in proposed {
            #[cfg(test)]
            {
                self.examined += 1;
            }
            if seen[kind] {
                return None;
            }
            seen[kind] = true;
            let close = wrappers.next_close[bracket_end + 1][kind];
            if close >= limit {
                return None;
            }
            let after = (close + wrappers.runs[close]).min(limit);
            if after - close < width {
                return None;
            }
            if let Some(group) = group {
                moves.push((group, after));
            }
            limit = after - width;
        }
        for (index, after) in moves {
            let group = &mut self.groups[index];
            group.after = after;
            group.moved = group.entries.len();
        }
        for (kind, width) in prefix {
            let close = wrappers.next_close[bracket_end + 1][kind];
            let after = (close + wrappers.runs[close]).min(self.limit(answer.len()));
            self.push(Some(kind), after - width, width);
        }
        self.push(None, bracket_end, 1);
        Some(cursor + 1)
    }
}

impl BodyWrappers {
    fn new(answer: &str) -> Self {
        let length = answer.len();
        let mut brackets = vec![length; length];
        let mut colons = vec![0; length + 1];
        let mut stack = Vec::new();
        for (index, byte) in answer.bytes().enumerate() {
            colons[index + 1] = colons[index] + usize::from(byte == b':');
            match byte {
                b'(' | b'[' | b'{' => stack.push((index, byte)),
                b')' | b']' | b'}' => {
                    let open = match byte {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if stack.last().is_some_and(|&(_, byte)| byte == open) {
                        let (start, _) = stack.pop().expect("matching bracket");
                        brackets[start] = index;
                        brackets[index] = start;
                    }
                }
                _ => {}
            }
        }
        let mut runs = vec![0; length];
        let mut closing = vec![false; length];
        let mut prefix_markers = vec![false; length];
        let mut atom_brackets = vec![false; length];
        let mut cursor = 0;
        let mut content_before = false;
        let mut prefix_only = true;
        let mut start = 0;
        let mut has_colon = false;
        let mut separator_checked = false;
        let mut after_close = false;
        while cursor < length {
            let ch = answer[cursor..].chars().next().expect("character boundary");
            let byte = answer.as_bytes()[cursor];
            let mut width = ch.len_utf8();
            if body_marker(byte).is_some() {
                while answer.as_bytes().get(cursor + width) == Some(&byte) {
                    width += 1;
                }
                runs[cursor] = width;
                closing[cursor] =
                    has_colon && content_before && body_wrapper_end(&answer[cursor + width..]);
                prefix_markers[cursor] = prefix_only && !closing[cursor];
                prefix_only &= !closing[cursor];
                content_before = closing[cursor];
                after_close = closing[cursor];
            } else {
                atom_brackets[cursor] = prefix_only && matches!(byte, b'(' | b'[' | b'{');
                let separator = matches!(ch, ',' | ';')
                    && !separator_checked
                    && (after_close || has_colon && body_separator_after(&answer[start..cursor]));
                separator_checked |= matches!(ch, ',' | ';') && (has_colon || after_close);
                if ch.is_whitespace() || separator {
                    start = cursor + width;
                    has_colon = false;
                    separator_checked = false;
                    prefix_only = true;
                } else {
                    prefix_only = false;
                }
                has_colon |= ch == ':';
                // A closing run can include bracket bytes before its list
                // separator; those bytes do not begin another filename.
                after_close = after_close && brackets[cursor] < cursor;
                content_before = !ch.is_whitespace() && !matches!(ch, ',' | ';' | '(' | '[' | '{');
            }
            cursor += width;
        }
        let mut next_close = vec![[length; 4]; length + 1];
        for index in (0..length).rev() {
            next_close[index] = next_close[index + 1];
            if closing[index] {
                let kind = body_marker(answer.as_bytes()[index]).expect("closing marker");
                next_close[index][kind] = index;
            }
        }
        let mut wrappers = Self {
            brackets,
            runs,
            next_close,
            colons,
            enclosing_atom_bracket: vec![length; length],
            previous_prefix_marker: vec![length; length + 1],
        };
        let mut active = Vec::new();
        for index in 0..length {
            while active
                .last()
                .is_some_and(|&open| wrappers.brackets[open] <= index)
            {
                active.pop();
            }
            if atom_brackets[index] && wrappers.bracket_pair(answer, index, length).is_some() {
                active.push(index);
            }
            wrappers.enclosing_atom_bracket[index] = active.last().copied().unwrap_or(length);
            wrappers.previous_prefix_marker[index + 1] = if prefix_markers[index] {
                index
            } else {
                wrappers.previous_prefix_marker[index]
            };
        }
        wrappers
    }

    fn pair(&self, answer: &str, start: usize, limit: usize) -> Option<(usize, usize)> {
        let byte = *answer.as_bytes().get(start)?;
        if let Some(kind) = body_marker(byte) {
            let run = self.runs[start];
            if run == 0 {
                return None; // Unconsumed opening-run bytes are filename data.
            }
            // Direct bracket prefixes retain the baseline full-width feasibility
            // rule. A shorter close cannot bootstrap a wider opening prefix.
            let content = start + run;
            let close = self
                .bracket_pair(answer, content, limit)
                .map(|(end, _)| self.next_close[end + 1][kind])
                .filter(|&close| close < limit && self.runs[close].min(limit - close) >= run)
                .unwrap_or(self.next_close[content][kind]);
            if close >= limit {
                return None;
            }
            // Earlier enclosing-list interpretation wins ambiguous spellings,
            // even with balanced filename markers before the first colon.
            // Reserve trailing close bytes for this owner; later nested pairs
            // get only their own bytes before this limit. Width is fixed here,
            // never shrunk by a later opener or another owner's closer.
            let after = (close + self.runs[close]).min(limit);
            let width = run.min(after - close);
            let end = after - width;
            // A different marker before an atom-start bracket can keep this
            // close inside the bracket. Both later closes must fit their full
            // opening widths; a partial run retains the literal reading.
            if width == run {
                let bracket = self.enclosing_atom_bracket[close];
                if bracket < answer.len() && bracket > content {
                    let marker = self.previous_prefix_marker[bracket];
                    if marker > start && marker < bracket {
                        if let Some(inner_kind) = body_marker(answer.as_bytes()[marker]) {
                            if inner_kind != kind {
                                let bracket_end = self.brackets[bracket];
                                let inner_close = self.next_close[bracket_end + 1][inner_kind];
                                let inner_width = self.runs[marker];
                                if inner_close < limit
                                    && self.runs[inner_close].min(limit - inner_close)
                                        >= inner_width
                                    && self.next_close[marker + inner_width][inner_kind]
                                        == inner_close
                                    && self.colons[inner_close] > self.colons[marker]
                                {
                                    let outer_close =
                                        self.next_close[inner_close + inner_width][kind];
                                    if outer_close < limit
                                        && self.runs[outer_close].min(limit - outer_close) >= run
                                    {
                                        return Some((
                                            outer_close
                                                + self.runs[outer_close].min(limit - outer_close)
                                                - run,
                                            run,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return (self.colons[end] > self.colons[start]).then_some((end, width));
        }
        self.bracket_pair(answer, start, limit)
    }

    fn bracket_pair(&self, answer: &str, start: usize, limit: usize) -> Option<(usize, usize)> {
        let byte = *answer.as_bytes().get(start)?;
        let end = self.brackets[start];
        if end <= start || end >= limit {
            return None;
        }
        let after = end + 1;
        let link = byte == b'[' && answer[after..].starts_with('(');
        let destination = byte == b'(' && start > 0 && answer.as_bytes()[start - 1] == b']';
        (link
            || (destination || body_wrapper_end(&answer[after..]))
                && self.colons[end] > self.colons[start])
            .then_some((end, 1))
    }
}

/// None is prose; a recognized but invalid whole reference is an error.
fn body_citation<'a>(
    token: &'a str,
    selected: &[&AllowedCitation],
) -> Result<Option<(&'a str, usize, usize)>, SemanticError> {
    let Some((path, spec)) = token.rsplit_once(':') else {
        return Ok(None);
    };
    let (start_text, end_text) = spec.split_once('-').unwrap_or((spec, spec));
    let span = parse_body_line(start_text).zip(parse_body_line(end_text));
    // One bounded pass over selected evidence, including disjoint same-file spans.
    let mut known = false;
    let mut contained = false;
    for item in selected {
        if token
            .strip_prefix(&item.path)
            .is_some_and(|tail| tail.starts_with(':'))
        {
            known = true;
            contained |= item.path == path
                && span.is_some_and(|(start, end)| {
                    start <= end && start >= item.start_line && end <= item.end_line
                });
        }
    }
    if excluded_body_context(token)
        || !(known
            || path.contains('/')
            || path.contains('\\')
            || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
            || path
                .rsplit_once('.')
                .is_some_and(|(_, extension)| !extension.is_empty()))
    {
        return Ok(None);
    }
    if path.contains(['\\', ':', '`'])
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(SemanticError::CitationMismatch);
    }
    if !contained {
        return Err(SemanticError::CitationMismatch);
    }
    let (start, end) = span.ok_or(SemanticError::CitationMismatch)?;
    Ok(Some((path, start, end)))
}

fn excluded_body_context(token: &str) -> bool {
    if token.split_once("://").is_some_and(|(scheme, rest)| {
        // Single-letter slash forms are drives, not URI escape hatches.
        scheme.len() > 1
            && !rest.is_empty()
            && scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    }) {
        return true;
    }
    if token.split_once(':').is_some_and(|(host, port)| {
        (host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::Ipv4Addr>().is_ok())
            && !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok()
    }) {
        return true;
    }
    // Whole numeric ISO-like timestamp: YYYY-MM-DDThh:mm[:ss][Z].
    let time = token.strip_suffix('Z').unwrap_or(token).as_bytes();
    matches!(time.len(), 16 | 19)
        && time.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        })
}

fn parse_body_line(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = text.parse::<usize>().ok()?;
    (value >= 1).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_probe_evidence;
    use adk_rust::{AdkError, ErrorCategory, ErrorComponent, Llm, LlmRequest};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use workflow_adk::model_profiles::{FakeModelProfile, ModelProfileRegistry};

    #[test]
    fn body_owner_shared_suffix_work_is_linear() {
        let citation = AllowedCitation {
            path: "a.py".to_owned(),
            start_line: 1,
            end_line: 1,
            evidence_index: 0,
        };
        for depth in [1, 32, 1024, 4096] {
            let mut answer = String::new();
            let mut starts = Vec::new();
            for index in 0..depth {
                starts.push(answer.len());
                answer.push_str("'a.py:1");
                if index + 1 < depth {
                    answer.push_str(", ");
                }
            }
            answer.push_str(&"'".repeat(depth));
            let wrappers = BodyWrappers::new(&answer);
            let mut owners = BodyOwners::default();
            for &start in &starts {
                assert_eq!(owners.bracket_prefix(&wrappers, &answer, start), None);
                let (end, width) = wrappers
                    .pair(&answer, start, owners.limit(answer.len()))
                    .expect("distinct reserved byte");
                owners.push(Some(2), end, width);
            }
            assert_eq!(owners.groups.len(), 1);
            assert_eq!(owners.groups[0].entries.len(), depth);
            assert!(
                owners.examined <= 3 * depth,
                "actual prefix/group checks: {}",
                owners.examined
            );
            assert!(answer_body_citations_match(&answer, &[&citation]).is_ok());
            answer.truncate(answer.len() - depth);
            answer.push_str(", ");
            let bracket = answer.len();
            let markers = "'".repeat(depth);
            answer.push_str(&format!("({markers}a.py:1{markers})"));
            for enough in [false, true] {
                if enough {
                    answer.push_str(&markers);
                }
                let wrappers = BodyWrappers::new(&answer);
                let mut owners = BodyOwners::default();
                for &start in &starts {
                    let (end, width) = wrappers
                        .pair(&answer, start, owners.limit(answer.len()))
                        .expect("reserved byte inside bracket");
                    owners.push(Some(2), end, width);
                }
                assert_eq!(
                    owners.bracket_prefix(&wrappers, &answer, bracket),
                    enough.then_some(bracket + 1)
                );
                assert!(
                    owners.examined <= 3,
                    "one shared group, regardless of depth"
                );
                assert_eq!(
                    answer_body_citations_match(&answer, &[&citation]).is_ok(),
                    enough
                );
            }
        }
    }

    struct FailingAdapter {
        calls: AtomicUsize,
        pending: bool,
    }

    #[adk_rust::async_trait]
    impl Llm for FailingAdapter {
        fn name(&self) -> &str {
            "offline-route-test"
        }

        async fn generate_content(
            &self,
            _request: LlmRequest,
            _stream: bool,
        ) -> adk_rust::Result<adk_rust::LlmResponseStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.pending {
                std::future::pending().await
            } else {
                Err(AdkError::new(
                    ErrorComponent::Model,
                    ErrorCategory::RateLimited,
                    "offline",
                    "offline",
                ))
            }
        }
    }

    struct Fixture {
        root: std::path::PathBuf,
        report: EvidenceReport,
    }

    impl Fixture {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("pbi-rs-semantic-{suffix}"));
            fs::create_dir_all(root.join("src")).expect("fixture directory");
            fs::write(
                root.join("src/lib.rs"),
                "pub fn answer() { parse_value().map_err(|error| error)?; }\n",
            )
            .expect("fixture source");
            let source = root.join("src/lib.rs");
            let probe = format!("File: {}, Lines: 1-1\n", source.display());
            let report = verify_probe_evidence(&probe, &root, "answer parse error", 8)
                .expect("verified fixture evidence");
            Self { root, report }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn publisher(response: Value) -> ModelRoutePublisher {
        let profile = FakeModelProfile::new("pbi-test", "1", "fake-model", [response.to_string()]);
        let registry = ModelProfileRegistry::new()
            .with_worker(profile)
            .expect("fake profile");
        let candidate = ModelRouteCandidate::new(ModelRole::Worker, "pbi-test", "1");
        let snapshot = ModelRouteSnapshot::new(
            registry,
            vec![candidate.clone()],
            ModelRouteAuthorization::new(vec![candidate]),
        )
        .expect("single authorized route snapshot");
        ModelRoutePublisher::new(snapshot)
    }

    #[test]
    fn adk_binding_returns_cited_answer() {
        let fixture = Fixture::new();
        let response = json!({
            "answer": "The parser converts the error at the verified span.",
            "uncertainty": "Only the supplied source span was inspected.",
            "citations": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        });
        let publisher = publisher(response);
        let deadline = Instant::now() + Duration::from_secs(2);
        let policy = publisher.policy(deadline);
        let cancellation = ModelRouteCancellation::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let answer = runtime
            .block_on(investigate(
                "where is answer parse error",
                &fixture.root,
                &fixture.report,
                &policy,
                deadline,
                &cancellation,
            ))
            .expect("semantic answer");
        assert_eq!(answer.citations().len(), 1);
        assert!(answer.answer().contains("parser"));
        assert!(!answer.uncertainty().is_empty());
        assert!(!answer.invocation_identity().is_empty());
    }

    #[test]
    fn model_citation_within_verified_span_is_grounded() {
        let fixture = Fixture::new();
        let source = fixture.root.join("src/lib.rs");
        fs::write(
            &source,
            "fn empty_guard() {\n    if empty { return Err(SourceOutsideRoot); }\n}\n",
        )
        .expect("source");
        let report = verify_probe_evidence(
            &format!("File: {}, Lines: 1-3\n", source.display()),
            &fixture.root,
            "empty_guard",
            8,
        )
        .expect("verified span");
        let item = &report.evidence()[0];
        assert_eq!(item.location().start_line(), 1);
        assert_eq!(item.location().end_line(), 2);
        let allowed = [AllowedCitation {
            path: "src/lib.rs".to_owned(),
            start_line: 1,
            end_line: 2,
            evidence_index: 0,
        }];
        let response = json!({
            "answer":"The guard at src/lib.rs:2 returns SourceOutsideRoot for an empty path.",
            "uncertainty":"Only the verified source span was inspected.",
            "citations":[{"path":"src/lib.rs","start_line":2,"end_line":2}]
        });
        let answer = decode_answer(
            response,
            &allowed,
            &[item],
            &report,
            "test-route".to_owned(),
        )
        .expect("the precise subspan remains verified");
        assert_eq!(answer.citations().len(), 1);
        let outside = json!({
            "answer":"The guard returns SourceOutsideRoot.",
            "uncertainty":"Only the verified source span was inspected.",
            "citations":[{"path":"src/lib.rs","start_line":3,"end_line":3}]
        });
        assert_eq!(
            decode_answer(outside, &allowed, &[item], &report, "test-route".to_owned()),
            Err(SemanticError::CitationMismatch)
        );
    }

    #[test]
    fn cited_callee_retains_verified_caller_chain() {
        let fixture = Fixture::new();
        let source = fixture.root.join("src/lib.rs");
        fs::write(
            &source,
            "fn caller() -> bool { let retry = wait(); retry }\n\n\n\n\n\n\n\n\
             fn wait() -> bool { remaining() > 0 }\n\n\n\n\n\n\n\n\
             fn remaining() -> u64 { 0 }\n",
        )
        .expect("source");
        let report = verify_probe_evidence(
            &format!("File: {}, Lines: 1-1\n", source.display()),
            &fixture.root,
            "caller retry",
            3,
        )
        .expect("caller evidence")
        .with_following_lines(&fixture.root, 3)
        .expect("verified call chain");
        assert_eq!(report.evidence().len(), 3, "{:?}", report.evidence());
        let callee = report.evidence().last().expect("budget definition");
        assert_eq!(callee.symbol(), Some("remaining"));
        let response = json!({
            "answer": "The caller stops when wait observes zero remaining budget.",
            "uncertainty": "Only the verified source was inspected.",
            "citations": [{"path": "src/lib.rs", "start_line": callee.location().start_line(),
                "end_line": callee.location().end_line()}]
        });
        let publisher = publisher(response);
        let deadline = Instant::now() + Duration::from_secs(2);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let answer = runtime
            .block_on(investigate(
                "where is the remaining budget",
                &fixture.root,
                &report,
                &publisher.policy(deadline),
                deadline,
                &ModelRouteCancellation::new(),
            ))
            .expect("semantic answer");
        assert_eq!(answer.citations().len(), 3);
        assert_eq!(answer.citations()[0].symbol(), Some("remaining"));
        assert_eq!(answer.citations()[1].symbol(), Some("wait"));
        assert_eq!(answer.citations()[2].location().start_line(), 1);
        assert!(answer.citations()[2]
            .snippet()
            .contains("let retry = wait()"));
    }

    #[test]
    fn fabricated_citation_is_rejected_after_adk_output_validation() {
        let fixture = Fixture::new();
        let response = json!({
            "answer": "untrusted",
            "uncertainty": "not enough evidence",
            "citations": [{"path": "../outside.rs", "start_line": 1, "end_line": 1}]
        });
        let publisher = publisher(response);
        let deadline = Instant::now() + Duration::from_secs(2);
        let policy = publisher.policy(deadline);
        let cancellation = ModelRouteCancellation::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &policy,
            deadline,
            &cancellation,
        ));
        assert_eq!(result, Err(SemanticError::CitationMismatch));
    }

    #[test]
    fn semantic_invocation_error_display_exposes_safe_typed_metadata() {
        assert_eq!(
            SemanticError::Route {
                kind: ModelRouteTerminalErrorKind::Provider,
                attempts: 1,
            }
            .to_string(),
            "semantic model route failed: Provider; attempts=1"
        );
    }

    #[test]
    fn explicit_credential_handle_selects_only_source_approved_name() {
        assert_eq!(
            select_explicit_credential_handle("CLIPROXY_API_KEY"),
            Ok("CLIPROXY_API_KEY")
        );
        assert_eq!(
            select_explicit_credential_handle("UNAPPROVED_KEY"),
            Err(SemanticRouteError::UnapprovedCredentialHandle)
        );
        assert_eq!(
            select_explicit_credential_handle(""),
            Err(SemanticRouteError::UnapprovedCredentialHandle)
        );
    }

    #[test]
    fn route_admission_rejects_cloud_and_unapproved_models() {
        assert_eq!(
            validate_local_route(
                "https://cloud.example/v1",
                "abliterated-qwen-latest-27b-none"
            ),
            Err(SemanticRouteError::UnapprovedRoute)
        );
        assert_eq!(
            validate_local_route("http://gb10:18009/v1", "cloud-model"),
            Err(SemanticRouteError::UnapprovedModel)
        );
    }

    #[test]
    fn local_route_defaults_match_the_approved_pbi_source_route() {
        assert_eq!(
            local_route_from_values(None, None),
            Ok((
                "http://localhost:18317/v1".to_owned(),
                "abliterated-qwen-latest-27b-none".to_owned()
            ))
        );
    }

    #[test]
    fn admitted_local_candidates_build_one_ordered_kit_publisher_and_enforce_limit() {
        let routes = vec![
            LocalModelRoute::new(
                DEFAULT_LOCAL_BASE_URL,
                DEFAULT_LOCAL_MODEL,
                "CLIPROXY_API_KEY",
            ),
            LocalModelRoute::new(
                APPROVED_LOCAL_BASE_URLS[0],
                APPROVED_LOCAL_MODELS[1],
                "OPENAI_API_KEY",
            ),
        ];
        let admitted = admit_local_routes(routes.clone()).expect("approved ordered routes");
        assert_eq!(admitted[0].profile_name(), "pbi-rs-local");
        assert_eq!(admitted[1].profile_name(), "pbi-rs-local-fallback-2");
        assert!(local_route_publisher_from_admitted_routes(&admitted).is_ok());

        let invalid_later = vec![
            routes[0].clone(),
            LocalModelRoute::new(DEFAULT_LOCAL_BASE_URL, "", "OPENAI_API_KEY"),
        ];
        assert_eq!(
            admit_local_routes(invalid_later),
            Err(SemanticRouteError::UnapprovedModel)
        );
        assert_eq!(
            admit_local_routes(vec![routes[0].clone(); MAX_MODEL_ROUTE_CANDIDATES + 1]),
            Err(SemanticRouteError::CandidateLimit)
        );
    }

    #[test]
    fn ordered_route_retry_denial_and_deadline_stay_inside_one_policy() {
        let fixture = Fixture::new();
        let response = json!({
            "answer": "Fallback verified the parser.",
            "uncertainty": "Only the supplied source span was inspected.",
            "citations": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        });
        let first = ModelRouteCandidate::new(ModelRole::Worker, "first", "1");
        let second = ModelRouteCandidate::new(ModelRole::Worker, "second", "1");
        let first_adapter = Arc::new(FailingAdapter {
            calls: AtomicUsize::new(0),
            pending: false,
        });
        let registry = ModelProfileRegistry::new()
            .with_worker(FakeModelProfile::new(
                "first",
                "1",
                "fake-first",
                ["unused"],
            ))
            .expect("first profile");
        let mut registry = registry;
        registry
            .register(FakeModelProfile::new(
                "second",
                "1",
                "fake-second",
                [response.to_string()],
            ))
            .expect("second profile");
        let snapshot = ModelRouteSnapshot::new(
            registry.clone(),
            [first.clone(), second.clone()],
            ModelRouteAuthorization::new([first.clone(), second.clone()]),
        )
        .expect("authorized ordered snapshot")
        .with_test_llm(first.clone(), first_adapter.clone())
        .expect("injected first adapter");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = runtime
            .block_on(investigate(
                "where is answer parse error",
                &fixture.root,
                &fixture.report,
                &ModelRoutePublisher::new(snapshot).policy(deadline),
                deadline,
                &ModelRouteCancellation::new(),
            ))
            .expect("authorized retry reaches second route");
        assert_eq!(result.answer(), "Fallback verified the parser.");
        assert_eq!(result.citations().len(), 1);
        assert_eq!(first_adapter.calls.load(Ordering::SeqCst), 1);

        let denied = ModelRouteSnapshot::new(
            registry.clone(),
            [first.clone(), second.clone()],
            ModelRouteAuthorization::new([second.clone()]),
        )
        .expect("denied candidate remains in snapshot")
        .with_test_llm(first.clone(), first_adapter.clone())
        .expect("injected first adapter");
        let deadline = Instant::now() + Duration::from_secs(2);
        let error = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &ModelRoutePublisher::new(denied).policy(deadline),
            deadline,
            &ModelRouteCancellation::new(),
        ));
        assert_eq!(
            error,
            Err(SemanticError::Route {
                kind: ModelRouteTerminalErrorKind::AuthorizationDenied,
                attempts: 1,
            })
        );
        assert_eq!(first_adapter.calls.load(Ordering::SeqCst), 1);

        let pending = Arc::new(FailingAdapter {
            calls: AtomicUsize::new(0),
            pending: true,
        });
        let deadline_snapshot = ModelRouteSnapshot::new(
            registry,
            [first.clone(), second],
            ModelRouteAuthorization::new([
                first.clone(),
                ModelRouteCandidate::new(ModelRole::Worker, "second", "1"),
            ]),
        )
        .expect("authorized deadline snapshot")
        .with_test_llm(first, pending.clone())
        .expect("injected pending adapter");
        let deadline = Instant::now() + Duration::from_millis(30);
        let error = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &ModelRoutePublisher::new(deadline_snapshot).policy(deadline),
            deadline,
            &ModelRouteCancellation::new(),
        ));
        assert_eq!(error, Err(SemanticError::DeadlineExceeded));
        assert_eq!(pending.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn semantic_path_fails_closed_on_cancel_and_expired_deadline() {
        let fixture = Fixture::new();
        let publisher = publisher(json!({
            "answer": "unused",
            "uncertainty": "unused",
            "citations": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let cancelled = ModelRouteCancellation::new();
        cancelled.cancel();
        let deadline = Instant::now() + Duration::from_secs(2);
        let policy = publisher.policy(deadline);
        let cancelled_result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &policy,
            deadline,
            &cancelled,
        ));
        assert_eq!(cancelled_result, Err(SemanticError::Cancelled));
        let deadline = Instant::now();
        let policy = publisher.policy(deadline);
        let active = ModelRouteCancellation::new();
        let expired_result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &policy,
            deadline,
            &active,
        ));
        assert_eq!(expired_result, Err(SemanticError::DeadlineExceeded));
    }

    #[test]
    fn explicit_config_selects_primary_model_endpoint_not_the_first_table() {
        let fixture = Fixture::new();
        let config = fixture.root.join("config.toml");
        fs::write(
            &config,
            "\
# comment and escaped quote must survive a real parser
primary_model = \"abliterated-qwen-latest-27b-low\"
model = \"abliterated-qwen-latest-27b-none\"

[[endpoints]]
model = \"abliterated-qwen-latest-27b-none\"
base_url = \"http://localhost:18317/v1\"
api_key = \"not-a-credential\"

[[endpoints]]
model = \"abliterated-qwen-latest-27b-low\"
base_url = \"http://gb10:18009/v1\"
key = \"also-not-read\"
",
        )
        .expect("fixture config");
        let _env = explicit_config_route_tests::enabled(&[(
            "PBI_CONFIG_FILE",
            Some(config.to_str().expect("utf8")),
        )]);
        let routes = explicit_admitted_routes_from_environment()
            .expect("explicit config route")
            .expect("publisher when ADK is enabled");
        let publisher = local_route_publisher_from_admitted_routes(&routes)
            .expect("publisher from selected route");
        let selected = routes.first().expect("selected endpoint");
        assert_eq!(selected.model(), "abliterated-qwen-latest-27b-low");
        assert_eq!(selected.base_url(), "http://gb10:18009/v1");
        let _publisher = publisher;
    }
}
