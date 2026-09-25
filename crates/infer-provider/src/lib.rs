//! Provider contracts and protocol-family adapters.

mod trusted_node;
pub use trusted_node::TrustedNodeProvider;
mod audio_stream;
mod audio_worker;
mod codex_app_server;
mod coreml_sam;
mod ocr_worker;
mod ollama_vision;
mod onnx;
mod probe;
mod runtime_dependency;
mod text_retrieval_worker;

use std::{pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use infer_core::{AgentTaskInputFile, AgentTaskRequest, ResponsesRequest};
use reqwest::Client;
use serde_json::Value;
use thiserror::Error;

pub use audio_stream::{
    AudioDuplexExecutor, AudioDuplexSession, AudioStreamExecutor, DynAudioDuplexExecutor,
    DynAudioDuplexSession, DynAudioStreamExecutor, ProviderAudioByteStream, SpeechStreamOutput,
};
pub use audio_worker::{
    AudioExecutionOutput, AudioExecutor, AudioTextQueryNormalizer, AudioWorkerExecutor,
    DynAudioExecutor,
};
pub use codex_app_server::CodexAppServerProvider;
pub use coreml_sam::{
    CoremlSamExecutor, DynSubjectSegmentationExecutor, SamBuildContract,
    SubjectSegmentationExecutionOutput, SubjectSegmentationExecutor,
    SubjectSegmentationSoftMaskExecutionOutput,
};
pub use ocr_worker::{
    DynOcrExecutor, OcrBuildContract, OcrExecutionOutput, OcrExecutor, OcrWorkerExecutor,
};
pub use ollama_vision::{
    ClassificationReviewExecutionOutput, DynImageUnderstandingExecutor,
    ImageDescriptionExecutionOutput, ImageUnderstandingExecutor, OllamaVisionExecutor,
    OllamaVisionProvenance,
};
pub use onnx::{
    DynFaceDetectionExecutor, DynFaceEmbeddingExecutor, DynFaceParsingExecutor,
    DynImageCompletionExecutor, DynImageEmbeddingExecutor, DynSemanticGroundingExecutor,
    DynTextEmbeddingExecutor, FaceDetectionExecutionOutput, FaceDetectionExecutor,
    FaceEmbeddingExecutionOutput, FaceEmbeddingExecutor, FaceParsingExecutionOutput,
    FaceParsingExecutor, ImageCompletionExecutionOutput, ImageCompletionExecutor,
    ImageEmbeddingExecutionOutput, ImageEmbeddingExecutor, OnnxExecutionProvenance,
    OnnxProviderRuntime, SemanticGroundingExecutionOutput, SemanticGroundingExecutor,
    TextEmbeddingExecutionOutput, TextEmbeddingExecutor, VisionExecutionProvenance,
};
pub use probe::{
    ProviderProbeCheck, ProviderProbeReport, ProviderProbeStatus, probe_responses_provider,
    probe_responses_provider_with_effort,
};
pub use runtime_dependency::{
    ProviderReadinessStatus, ProviderRuntimeReadiness, ResolvedProviderProcess,
    RuntimeDependencyCheck, preflight_provider_runtime, provider_requires_ffmpeg,
    resolve_provider_process, verify_clap_worker, verify_coreml_sam_worker, verify_yamnet_worker,
};
pub use text_retrieval_worker::{
    DynRetrievalExecutor, RetrievalBuildContract, RetrievalEmbeddingExecutionOutput,
    RetrievalEmbeddingKind, RetrievalExecutor, RetrievalRerankExecutionOutput,
    RetrievalWorkerExecutor,
};

pub type ProviderByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

/// One Provider-native model group observed by the operator plane.
///
/// Dynamic discovery never mutates routing admission. `admitted` is derived
/// exclusively from version-controlled Build/Deployment configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderModelCatalog {
    pub provider: String,
    pub models: Vec<ProviderModelInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderModelInfo {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub input_modalities: Vec<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: String,
    pub is_default: bool,
    pub hidden: bool,
    pub upgrade: Option<String>,
    /// Discovery is not admission. Only version-controlled Deployments may be
    /// selected by the runtime router.
    pub admitted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    Authentication,
    RateLimited,
    Timeout,
    Unavailable,
    InvalidRequest,
    Protocol,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("remote node outcome unknown; automatic replay prohibited")]
    RemoteOutcomeUnknown,
    #[error("provider transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// Keep the upstream body for failure classification and local debugging,
    /// but never place it in Display. It can contain provider echoes of a
    /// prompt or other sensitive request context and therefore must not enter
    /// default API errors, logs, Job snapshots, or the usage ledger.
    #[error("provider returned HTTP {status}")]
    Upstream {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("provider returned malformed JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("local executor I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("local executor protocol error: {0}")]
    Protocol(String),
    #[error("invalid local inference input: {0}")]
    InvalidInput(String),
    #[error("native runtime error: {0}")]
    NativeRuntime(String),
    /// Whitelisted App Server stage/code; safe for public errors and audit.
    #[error("{code}")]
    CodexTurn {
        kind: ProviderFailureKind,
        code: &'static str,
    },
    #[error("provider bridge error: {message}")]
    Classified {
        kind: ProviderFailureKind,
        message: String,
    },
    #[error("configured model {model} is absent from the provider inventory")]
    ModelMissing { model: String },
}

impl ProviderError {
    /// Stable failure semantics for routing and public error normalization.
    /// Retry/fallback policy is deliberately not decided here.
    pub fn kind(&self) -> ProviderFailureKind {
        match self {
            Self::Upstream { status, .. } => match status {
                401 | 403 => ProviderFailureKind::Authentication,
                408 | 504 => ProviderFailureKind::Timeout,
                429 => ProviderFailureKind::RateLimited,
                400 | 404 | 409 | 413 | 422 => ProviderFailureKind::InvalidRequest,
                500..=599 => ProviderFailureKind::Unavailable,
                _ => ProviderFailureKind::Protocol,
            },
            Self::Transport(error) if error.is_timeout() => ProviderFailureKind::Timeout,
            Self::Transport(_) | Self::Io(_) => ProviderFailureKind::Unavailable,
            Self::InvalidInput(_) => ProviderFailureKind::InvalidRequest,
            Self::Classified { kind, .. } | Self::CodexTurn { kind, .. } => *kind,
            Self::ModelMissing { .. } => ProviderFailureKind::Unavailable,
            Self::RemoteOutcomeUnknown
            | Self::Malformed(_)
            | Self::Protocol(_)
            | Self::NativeRuntime(_) => ProviderFailureKind::Protocol,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Upstream { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Payload-free summary safe for public responses, Job snapshots, audit,
    /// and ordinary logs. `Display` remains an internal diagnostic surface.
    pub fn replay_allowed(&self) -> bool {
        !matches!(self, Self::RemoteOutcomeUnknown)
    }

    pub fn model_missing(&self) -> bool {
        matches!(self, Self::ModelMissing { .. })
    }

    pub fn public_message(&self) -> &'static str {
        if matches!(self, Self::RemoteOutcomeUnknown) {
            return "remote execution outcome unknown; automatic replay prohibited";
        }
        if let Self::CodexTurn { code, .. } = self {
            return code;
        }
        if self.model_missing() {
            return "configured model is absent from provider inventory";
        }
        match self.kind() {
            ProviderFailureKind::Authentication => "provider authentication failed",
            ProviderFailureKind::RateLimited => "provider rate limit exceeded",
            ProviderFailureKind::Timeout => "provider timed out",
            ProviderFailureKind::Unavailable => "provider is unavailable",
            ProviderFailureKind::InvalidRequest => "provider rejected the request",
            ProviderFailureKind::Protocol => "provider protocol failed",
        }
    }
}

pub struct ProviderAttemptContext {
    pub job_id: String,
    pub app_id: String,
    pub intent: String,
    pub attempt: usize,
    pub remaining: Duration,
}

pub struct AgentTaskExecution {
    pub answer: String,
    pub outputs: Vec<AgentTaskInputFile>,
    pub thread_id: String,
    pub turn_id: String,
    pub sandbox_profile: String,
    pub tool_policy: String,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError>;
    async fn execute_agent_task(
        &self,
        _request: AgentTaskRequest,
        _model: &str,
    ) -> Result<AgentTaskExecution, ProviderError> {
        Err(ProviderError::InvalidInput(
            "provider does not support Agent file tasks".into(),
        ))
    }
    async fn execute_stream(
        &self,
        request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError>;
    async fn execute_attempt(
        &self,
        _context: ProviderAttemptContext,
        request: ResponsesRequest,
    ) -> Result<Value, ProviderError> {
        self.execute(request).await
    }
    /// None preserves static local admission. Nodes return live, approved offers.
    async fn available_models(
        &self,
        _intent: &str,
    ) -> Result<Option<std::collections::BTreeSet<String>>, ProviderError> {
        Ok(None)
    }
    /// Optional dynamic model inventory. Discovery never grants routing
    /// admission; callers must inspect the `admitted` marker.
    async fn model_catalog(&self) -> Result<Option<ProviderModelCatalog>, ProviderError> {
        Ok(None)
    }
}

/// Executes the stateless `/v1/responses` profile shared by Ollama and cloud providers.
pub struct ResponsesProvider {
    id: String,
    endpoint: String,
    api_key: Option<String>,
    client: Client,
}

impl ResponsesProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: &str,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
        Ok(Self {
            id: id.into(),
            endpoint,
            api_key,
            client: Client::new(),
        })
    }

    fn request(&self, request: &ResponsesRequest) -> reqwest::RequestBuilder {
        let builder = self.client.post(&self.endpoint).json(request);
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    async fn checked(
        &self,
        request: &ResponsesRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self.request(request).send().await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            let status = response.status().as_u16();
            let retry_after = parse_retry_after(response.headers());
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable provider error body>".into());
            Err(ProviderError::Upstream {
                status,
                body,
                retry_after,
            })
        }
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()?
        .duration_since(std::time::SystemTime::now())
        .ok()
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
        Ok(self.checked(&request).await?.json::<Value>().await?)
    }

    async fn execute_stream(
        &self,
        request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        let stream = self
            .checked(&request)
            .await?
            .bytes_stream()
            .map(|item| item.map_err(ProviderError::Transport));
        Ok(Box::pin(stream))
    }
}

pub type DynProvider = Arc<dyn Provider>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_upstream_failures_without_provider_specific_strings() {
        assert_eq!(
            ProviderError::Upstream {
                status: 429,
                body: String::new(),
                retry_after: None,
            }
            .kind(),
            ProviderFailureKind::RateLimited
        );
        assert_eq!(
            ProviderError::Upstream {
                status: 503,
                body: String::new(),
                retry_after: None,
            }
            .kind(),
            ProviderFailureKind::Unavailable
        );
        assert_eq!(
            ProviderError::Upstream {
                status: 400,
                body: String::new(),
                retry_after: None,
            }
            .kind(),
            ProviderFailureKind::InvalidRequest
        );
    }

    #[test]
    fn upstream_display_never_exposes_the_provider_body() {
        let error = ProviderError::Upstream {
            status: 400,
            body: "prompt=private meeting notes".into(),
            retry_after: None,
        };
        assert_eq!(error.to_string(), "provider returned HTTP 400");
    }
}
