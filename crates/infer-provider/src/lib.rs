//! Provider contracts and protocol-family adapters.

mod audio_stream;
mod audio_worker;
mod codex_app_server;
mod ollama_vision;
mod onnx;
mod probe;

use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use infer_core::ResponsesRequest;
use reqwest::Client;
use serde_json::Value;
use thiserror::Error;

pub use audio_stream::{
    AudioDuplexExecutor, AudioDuplexSession, AudioStreamExecutor, DynAudioDuplexExecutor,
    DynAudioDuplexSession, DynAudioStreamExecutor, ProviderAudioByteStream, SpeechStreamOutput,
};
pub use audio_worker::{
    AudioExecutionOutput, AudioExecutor, AudioWorkerExecutor, DynAudioExecutor,
};
pub use codex_app_server::{CodexAppServerProvider, ProviderModelCatalog, ProviderModelInfo};
pub use ollama_vision::{
    ClassificationReviewExecutionOutput, DynImageUnderstandingExecutor,
    ImageDescriptionExecutionOutput, ImageUnderstandingExecutor, OllamaVisionExecutor,
    OllamaVisionProvenance,
};
pub use onnx::{
    DynFaceDetectionExecutor, DynFaceEmbeddingExecutor, DynImageEmbeddingExecutor,
    DynTextEmbeddingExecutor, FaceDetectionExecutionOutput, FaceDetectionExecutor,
    FaceEmbeddingExecutionOutput, FaceEmbeddingExecutor, ImageEmbeddingExecutionOutput,
    ImageEmbeddingExecutor, OnnxExecutionProvenance, OnnxProviderRuntime,
    TextEmbeddingExecutionOutput, TextEmbeddingExecutor,
};
pub use probe::{
    ProviderProbeCheck, ProviderProbeReport, ProviderProbeStatus, probe_responses_provider,
    probe_responses_provider_with_effort,
};

pub type ProviderByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

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
    #[error("provider transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// Keep the upstream body for failure classification and local debugging,
    /// but never place it in Display. It can contain provider echoes of a
    /// prompt or other sensitive request context and therefore must not enter
    /// default API errors, logs, Job snapshots, or the usage ledger.
    #[error("provider returned HTTP {status}")]
    Upstream { status: u16, body: String },
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
    #[error("provider bridge error: {message}")]
    Classified {
        kind: ProviderFailureKind,
        message: String,
    },
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
            Self::Classified { kind, .. } => *kind,
            Self::Malformed(_) | Self::Protocol(_) | Self::NativeRuntime(_) => {
                ProviderFailureKind::Protocol
            }
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError>;
    async fn execute_stream(
        &self,
        request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError>;
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
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable provider error body>".into());
            Err(ProviderError::Upstream { status, body })
        }
    }
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
            }
            .kind(),
            ProviderFailureKind::RateLimited
        );
        assert_eq!(
            ProviderError::Upstream {
                status: 503,
                body: String::new(),
            }
            .kind(),
            ProviderFailureKind::Unavailable
        );
        assert_eq!(
            ProviderError::Upstream {
                status: 400,
                body: String::new(),
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
        };
        assert_eq!(error.to_string(), "provider returned HTTP 400");
    }
}
