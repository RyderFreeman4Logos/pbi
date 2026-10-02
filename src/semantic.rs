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
        "Answer only from VERIFIED_EVIDENCE. Return one compact answer, explicit uncertainty, and citations that exactly match a verified path and line span. Do not invent files, lines, symbols, or facts. No repository tools are available to this model.",
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
        invocation_identity,
    )
}

fn decode_answer(
    value: Value,
    allowed: &[AllowedCitation],
    evidence: &[&SourceEvidence],
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
                    && allowed.start_line == start_line
                    && allowed.end_line == end_line
            })
            .ok_or(SemanticError::CitationMismatch)?;
        if citations
            .iter()
            .any(|item: &SourceEvidence| item == evidence[matched.evidence_index])
        {
            return Err(SemanticError::CitationMismatch);
        }
        citations.push(evidence[matched.evidence_index].clone());
        selected.push(matched);
    }
    answer_body_citations_match(&answer, &selected)?;
    Ok(SemanticAnswer {
        answer,
        uncertainty,
        citations,
        invocation_identity,
    })
}

/// Check only model-selected spans. Delimiters bound both references and URL
/// exemptions; path separators and colons stay intact so invalid paths cannot
/// be rescued by scanning a valid suffix. The schema bounds answer length.
fn answer_body_citations_match(
    answer: &str,
    selected: &[&AllowedCitation],
) -> Result<(), SemanticError> {
    for token in answer.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"' | '\''
            )
    }) {
        let mut token = token.trim_end_matches(['.', '!', '?', '。']);
        // Peel only paired outer markup, never an interior path fragment.
        while let Some(marker @ (b'`' | b'*')) = token.as_bytes().first().copied() {
            if token.len() < 2 || token.as_bytes().last() != Some(&marker) {
                break;
            }
            token = &token[1..token.len() - 1];
        }
        if let Some((path, start, end)) = body_citation(token, selected)? {
            if !selected
                .iter()
                .any(|item| item.path == path && start >= item.start_line && end <= item.end_line)
            {
                return Err(SemanticError::CitationMismatch);
            }
        }
    }
    Ok(())
}

/// None is prose; a recognized but invalid whole reference is an error.
fn body_citation<'a>(
    token: &'a str,
    selected: &[&AllowedCitation],
) -> Result<Option<(&'a str, usize, usize)>, SemanticError> {
    let Some((path, spec)) = token.rsplit_once(':') else {
        return Ok(None);
    };
    if excluded_body_context(token)
        || !(path.contains('/')
            || path.contains('\\')
            || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
            || path
                .rsplit_once('.')
                .is_some_and(|(_, extension)| !extension.is_empty())
            || selected.iter().any(|item| item.path == path))
    {
        return Ok(None);
    }
    if path.contains(['\\', ':'])
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(SemanticError::CitationMismatch);
    }
    let (start_text, end_text) = spec.split_once('-').unwrap_or((spec, spec));
    let start = parse_body_line(start_text).ok_or(SemanticError::CitationMismatch)?;
    let end = parse_body_line(end_text).ok_or(SemanticError::CitationMismatch)?;
    if start > end {
        return Err(SemanticError::CitationMismatch);
    }
    Ok(Some((path, start, end)))
}

fn excluded_body_context(token: &str) -> bool {
    token.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    }) || token.split_once(':').is_some_and(|(prefix, _)| {
        prefix.eq_ignore_ascii_case("localhost")
            || prefix.parse::<std::net::Ipv4Addr>().is_ok()
            || (prefix.len() == 13
                && prefix.as_bytes()[4] == b'-'
                && prefix.as_bytes()[7] == b'-'
                && prefix.as_bytes()[10] == b'T')
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
