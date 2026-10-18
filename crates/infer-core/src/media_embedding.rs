//! Bounded local media retrieval. Consumers own segmentation and indexes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, EmbeddingSpaceConfig, Fallback, Modality, PlacementScope, RequestConstraints,
};

pub const MEDIA_EMBEDDING_CONTRACT: &str = "infer.media.embedding@20261013.1";
pub const MAX_MEDIA_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_MEDIA_QUERY_BYTES: usize = 16 * 1024;
pub const MEDIA_QUERY_INSTRUCTION: &str = "embeddinggemma2-search-query-v1";
pub const MEDIA_PREPROCESSING: &str = "embeddinggemma2-processor@e880ddc5:image-rgb:audio-mono16k-max30s:video-visual1fps-max16s-16frames:v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaEmbeddingKind {
    Text,
    Image,
    Audio,
    Video,
}

impl MediaEmbeddingKind {
    pub fn modality(self) -> Modality {
        match self {
            Self::Text => Modality::Text,
            Self::Image => Modality::Image,
            Self::Audio => Modality::Audio,
            Self::Video => Modality::Video,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaQueryEmbeddingRequest {
    pub model: String,
    pub text: String,
    pub query_revision: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub enum MediaEmbeddingInput {
    Query(String),
    File {
        kind: MediaEmbeddingKind,
        content_type: String,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct MediaEmbeddingRequest {
    pub model: String,
    pub input: MediaEmbeddingInput,
    pub input_revision: String,
    pub metadata: BTreeMap<String, String>,
}

impl From<MediaQueryEmbeddingRequest> for MediaEmbeddingRequest {
    fn from(value: MediaQueryEmbeddingRequest) -> Self {
        Self {
            model: value.model,
            input: MediaEmbeddingInput::Query(value.text),
            input_revision: value.query_revision,
            metadata: value.metadata,
        }
    }
}

impl MediaEmbeddingRequest {
    pub fn kind(&self) -> MediaEmbeddingKind {
        match &self.input {
            MediaEmbeddingInput::Query(_) => MediaEmbeddingKind::Text,
            MediaEmbeddingInput::File { kind, .. } => *kind,
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty()
            || self.model.len() > 128
            || self.input_revision.trim().is_empty()
            || self.input_revision.len() > 256
        {
            return Err(invalid("model and bounded input revision are required"));
        }
        match &self.input {
            MediaEmbeddingInput::Query(text)
                if text.trim().is_empty() || text.len() > MAX_MEDIA_QUERY_BYTES =>
            {
                return Err(invalid("query text must contain 1..16384 UTF-8 bytes"));
            }
            MediaEmbeddingInput::File {
                kind,
                content_type,
                bytes,
            } => {
                if bytes.is_empty()
                    || bytes.len() > MAX_MEDIA_UPLOAD_BYTES
                    || !media_content_type_allowed(*kind, content_type)
                {
                    return Err(invalid(
                        "media requires one bounded file with a supported content type",
                    ));
                }
            }
            _ => {}
        }
        let constraints = self.constraints()?;
        if constraints.placement != Some(PlacementScope::LocalOnly)
            || constraints.offline_required != Some(true)
            || constraints.fallback != Some(Fallback::None)
        {
            return Err(invalid(
                "media retrieval requires local_only, offline_required=true and fallback=none",
            ));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

pub fn media_content_type_allowed(kind: MediaEmbeddingKind, content_type: &str) -> bool {
    match kind {
        MediaEmbeddingKind::Text => false,
        MediaEmbeddingKind::Image => matches!(content_type, "image/jpeg" | "image/png"),
        MediaEmbeddingKind::Audio => matches!(
            content_type,
            "audio/wav" | "audio/x-wav" | "audio/flac" | "audio/mpeg" | "audio/ogg" | "audio/mp4"
        ),
        MediaEmbeddingKind::Video => {
            matches!(content_type, "video/mp4" | "video/quicktime" | "video/webm")
        }
    }
}

fn invalid(message: &str) -> ContractError {
    ContractError::InvalidVision(format!("invalid media embedding request: {message}"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub model: String,
    pub input_kind: MediaEmbeddingKind,
    pub input_revision: String,
    pub embedding: Vec<f32>,
    pub embedding_space: EmbeddingSpaceConfig,
    pub processing: MediaEmbeddingProcessing,
    pub provenance: MediaEmbeddingProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingProcessing {
    pub input_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_frames: Option<usize>,
    /// This version embeds visual frames only; audio has its own typed route.
    pub video_audio_included: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub model_revision: String,
    pub artifact_set_sha256: String,
    pub runtime: String,
    pub precision: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub tokenizer_identity: String,
    pub instruction_revision: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_rejects_paths_wrong_modalities_and_relaxed_placement() {
        let query = r#"{"model":"semantic.media_query","text":"海浪","query_revision":"q1","path":"/tmp/private"}"#;
        assert!(serde_json::from_str::<MediaQueryEmbeddingRequest>(query).is_err());
        assert!(!media_content_type_allowed(
            MediaEmbeddingKind::Video,
            "image/jpeg"
        ));
        let mut request: MediaEmbeddingRequest = MediaQueryEmbeddingRequest {
            model: "semantic.media_query".into(),
            text: "海浪".into(),
            query_revision: "q1".into(),
            metadata: BTreeMap::new(),
        }
        .into();
        assert!(request.validate().is_err());
        request.metadata = BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ]);
        request.validate().unwrap();
    }
}
