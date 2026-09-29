use crate::{EvidenceReport, SourceEvidence};
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use workflow_adk::model_profiles::{
    CredentialBroker, CredentialHandle, ModelBinding, ModelProfileErrorKind, ModelProfileRegistry,
    ModelRuntimeConfig, OpenAiCompatibleProfile,
};
use workflow_adk::{
    EscalationPolicy, InferenceBudget, ModelInvocationErrorKind, ModelInvocationSpec,
    PromptProtocol, ReasoningEffort, StructuredOutputContract,
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
    Invocation {
        kind: ModelInvocationErrorKind,
        model_error: Option<ModelProfileErrorKind>,
        attempts: u8,
    },
    InvalidOutput,
    CitationMismatch,
}

impl fmt::Display for SemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::Invocation {
            kind,
            model_error,
            attempts,
        } = self
        {
            return match model_error {
                Some(model_error) => write!(
                    formatter,
                    "semantic model invocation failed: {kind:?}; model_error={model_error:?}; attempts={attempts}"
                ),
                None => write!(
                    formatter,
                    "semantic model invocation failed: {kind:?}; attempts={attempts}"
                ),
            };
        }
        formatter.write_str(match self {
            Self::EmptyQuestion => "semantic question is empty",
            Self::NoEvidence => "semantic investigation requires verified source evidence",
            Self::SourceOutsideRoot => "semantic citation crossed the repository boundary",
            Self::InputTooLarge => "semantic evidence exceeded the bounded context",
            Self::Protocol => "semantic invocation protocol could not be built",
            Self::Cancelled => "semantic investigation was cancelled",
            Self::DeadlineExceeded => "semantic investigation exceeded its bounded deadline",
            Self::Invocation { .. } => unreachable!("invocation errors are formatted above"),
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
    UnapprovedRoute,
    UnapprovedModel,
    UnapprovedCredentialHandle,
    MissingCredential,
    Profile,
}

impl fmt::Display for SemanticRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnable => "semantic route opt-in is invalid",
            Self::IncompleteConfig => "semantic route configuration is incomplete",
            Self::UnapprovedRoute => "semantic route is not an approved local route",
            Self::UnapprovedModel => "semantic model is not an approved local model",
            Self::UnapprovedCredentialHandle => "semantic credential handle is not approved",
            Self::MissingCredential => {
                "semantic route requires one credential handle: CLIPROXY_API_KEY, OPENAI_API_KEY, or LOCAL_ROUTER_API_KEY"
            }
            Self::Profile => "semantic model profile could not be bound",
        })
    }
}

impl std::error::Error for SemanticRouteError {}

pub fn validate_local_route(base_url: &str, model: &str) -> Result<(), SemanticRouteError> {
    if !APPROVED_LOCAL_BASE_URLS.contains(&base_url) {
        return Err(SemanticRouteError::UnapprovedRoute);
    }
    if !APPROVED_LOCAL_MODELS.contains(&model) {
        return Err(SemanticRouteError::UnapprovedModel);
    }
    Ok(())
}

pub fn local_binding_from_environment() -> Result<Option<ModelBinding>, SemanticRouteError> {
    match env::var(ADK_ENABLE_ENV).as_deref() {
        Err(_) | Ok("0") => return Ok(None),
        Ok("1") => {}
        Ok(_) => return Err(SemanticRouteError::InvalidEnable),
    }
    let (base_url, model) = local_route_from_values(
        first_value(&["CLIPROXY_BASE_URL", "LOCAL_ROUTER_BASEURL"])?,
        first_value(&["LOCAL_MODEL", "LLM_MODEL"])?,
    )?;
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
    let profile = OpenAiCompatibleProfile::new(
        "pbi-rs-local",
        "1",
        model,
        base_url,
        CredentialHandle::environment(credential_name),
    )
    .with_provider("openai")
    .with_runtime(ModelRuntimeConfig::default().with_timeout(Duration::from_secs(30)));
    let registry = ModelProfileRegistry::new()
        .with_worker(profile)
        .map_err(|_| SemanticRouteError::Profile)?;
    registry
        .bind_worker(&CredentialBroker::new())
        .map(Some)
        .map_err(|_| SemanticRouteError::Profile)
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

pub async fn investigate(
    question: &str,
    root: &Path,
    report: &EvidenceReport,
    binding: &ModelBinding,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<SemanticAnswer, SemanticError> {
    if question.trim().is_empty() {
        return Err(SemanticError::EmptyQuestion);
    }
    if report.evidence().is_empty() {
        return Err(SemanticError::NoEvidence);
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(SemanticError::Cancelled);
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
    let spec = ModelInvocationSpec::new(
        protocol,
        question.to_owned(),
        workflow_adk::ProviderRouteIdentity::from_binding(binding),
        budget,
        output,
    )
    .map_err(|_| SemanticError::Protocol)?;

    let invocation = async {
        tokio::select! {
            result = spec.invoke(binding) => result.map_err(|error| SemanticError::Invocation {
                kind: error.kind(),
                model_error: error.model_error(),
                attempts: error.attempts(),
            }),
            _ = wait_for_cancellation(cancellation) => Err(SemanticError::Cancelled),
        }
    };
    let result =
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), invocation).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(SemanticError::DeadlineExceeded),
        };
    if cancellation.load(Ordering::Acquire) {
        return Err(SemanticError::Cancelled);
    }
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

async fn wait_for_cancellation(cancellation: &AtomicBool) {
    while !cancellation.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
    }
    Ok(SemanticAnswer {
        answer,
        uncertainty,
        citations,
        invocation_identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_probe_evidence;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::time::{SystemTime, UNIX_EPOCH};
    use workflow_adk::model_profiles::{CredentialBroker, FakeModelProfile, ModelProfileRegistry};

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

    fn binding(response: Value) -> workflow_adk::model_profiles::ModelBinding {
        let profile = FakeModelProfile::new("pbi-test", "1", "fake-model", [response.to_string()]);
        let registry = ModelProfileRegistry::new()
            .with_worker(profile)
            .expect("fake profile");
        registry
            .bind_worker(&CredentialBroker::new())
            .expect("fake binding")
    }

    #[test]
    fn adk_binding_returns_cited_answer() {
        let fixture = Fixture::new();
        let response = json!({
            "answer": "The parser converts the error at the verified span.",
            "uncertainty": "Only the supplied source span was inspected.",
            "citations": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        });
        let binding = binding(response);
        let cancelled = AtomicBool::new(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let answer = runtime
            .block_on(investigate(
                "where is answer parse error",
                &fixture.root,
                &fixture.report,
                &binding,
                Instant::now() + Duration::from_secs(2),
                &cancelled,
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
        let binding = binding(response);
        let cancelled = AtomicBool::new(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &binding,
            Instant::now() + Duration::from_secs(2),
            &cancelled,
        ));
        assert_eq!(result, Err(SemanticError::CitationMismatch));
    }

    #[test]
    fn semantic_invocation_error_display_exposes_safe_typed_metadata() {
        assert_eq!(
            SemanticError::Invocation {
                kind: ModelInvocationErrorKind::ModelProfile,
                model_error: Some(ModelProfileErrorKind::Provider),
                attempts: 1,
            }
            .to_string(),
            "semantic model invocation failed: ModelProfile; model_error=Provider; attempts=1"
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
    fn semantic_path_fails_closed_on_cancel_and_expired_deadline() {
        let fixture = Fixture::new();
        let binding = binding(json!({
            "answer": "unused",
            "uncertainty": "unused",
            "citations": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let cancelled = AtomicBool::new(true);
        let cancelled_result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &binding,
            Instant::now() + Duration::from_secs(2),
            &cancelled,
        ));
        assert_eq!(cancelled_result, Err(SemanticError::Cancelled));
        let active = AtomicBool::new(false);
        let expired_result = runtime.block_on(investigate(
            "where is answer parse error",
            &fixture.root,
            &fixture.report,
            &binding,
            Instant::now(),
            &active,
        ));
        assert_eq!(expired_result, Err(SemanticError::DeadlineExceeded));
    }
}
