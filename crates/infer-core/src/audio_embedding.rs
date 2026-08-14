//! Typed contracts for local audio-text embedding spaces.
//!
//! This is intentionally separate from transcript, alignment, and AudioSet
//! evidence: an embedding is rebuildable retrieval evidence, not a detected
//! event or a semantic fact about the source audio.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, EmbeddingSpaceConfig, RequestConstraints,
    audio::{AudioFile, validate_model},
    vision::validate_revision,
};

pub const MAX_AUDIO_EMBEDDING_TEXT_BYTES: usize = 16 * 1024;
pub const AUDIO_EMBEDDING_MAX_SECONDS: u64 = 10;

#[derive(Debug, Clone)]
pub struct AudioEmbeddingRequest {
    /// Runtime-owned logical intent. Physical models remain selected by routing.
    pub model: String,
    pub file: AudioFile,
    /// Exact Consumer-owned identity of the audio artifact/chunk.
    pub source_revision: String,
    pub metadata: BTreeMap<String, String>,
}

impl AudioEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        self.file.validate()?;
        validate_revision(&self.source_revision, "source_revision")
            .map_err(|error| ContractError::InvalidAudio(error.to_string()))?;
        self.local_only_constraints()
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }

    fn local_only_constraints(&self) -> Result<(), ContractError> {
        let constraints = self.constraints()?;
        if constraints.placement != Some(crate::PlacementScope::LocalOnly)
            || constraints.offline_required != Some(true)
            || constraints.fallback != Some(crate::Fallback::None)
        {
            return Err(ContractError::InvalidAudio(
                "audio embedding requires local_only, offline_required=true, and fallback=none"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTextEmbeddingRequest {
    pub model: String,
    pub text: String,
    /// Consumer-owned identity of this query revision. It is not logged by
    /// Runtime and allows Consumers to reject late rebuildable results.
    pub query_revision: String,
    pub language: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl AudioTextEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        if self.text.trim().is_empty() || self.text.len() > MAX_AUDIO_EMBEDDING_TEXT_BYTES {
            return Err(ContractError::InvalidAudio(format!(
                "text must contain between 1 and {MAX_AUDIO_EMBEDDING_TEXT_BYTES} UTF-8 bytes"
            )));
        }
        validate_revision(&self.query_revision, "query_revision")
            .map_err(|error| ContractError::InvalidAudio(error.to_string()))?;
        if self.language.is_empty()
            || self.language.len() > 35
            || !self
                .language
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ContractError::InvalidAudio(
                "language must be a BCP-47-shaped tag no longer than 35 ASCII bytes".into(),
            ));
        }
        let constraints = self.constraints()?;
        if constraints.placement != Some(crate::PlacementScope::LocalOnly)
            || constraints.offline_required != Some(true)
            || constraints.fallback != Some(crate::Fallback::None)
        {
            return Err(ContractError::InvalidAudio(
                "audio text embedding requires local_only, offline_required=true, and fallback=none"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEmbeddingProvenance {
    pub build: String,
    pub artifact_set_sha256: String,
    pub runtime: String,
    pub precision: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub preprocessing_identity: String,
    pub tokenizer_identity: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_revision: Option<String>,
    pub embedding: Vec<f32>,
    pub embedding_space: EmbeddingSpaceConfig,
    pub provenance: AudioEmbeddingProvenance,
}

impl AudioEmbeddingResponse {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.object != "audio.embedding"
            || self.id.is_empty()
            || self.model.is_empty()
            || self.embedding_space.dimensions == 0
            || !self.embedding_space.normalized
            || self.embedding_space.distance_metric != "cosine"
            || self.embedding.len() != self.embedding_space.dimensions
            || self.embedding.iter().any(|value| !value.is_finite())
            || self.provenance.build.is_empty()
            || self.provenance.artifact_set_sha256.len() != 64
            || self.provenance.runtime.is_empty()
            || self.provenance.precision.is_empty()
            || self.provenance.requested_execution_provider.is_empty()
            || self.provenance.actual_execution_provider.is_empty()
            || self.provenance.preprocessing_identity.is_empty()
            || self.provenance.tokenizer_identity.is_empty()
        {
            return Err(ContractError::InvalidAudio(
                "audio embedding response has invalid embedding-space or provenance identity"
                    .into(),
            ));
        }
        let expected_revisions = match self.model.as_str() {
            "audio.embed" => self.source_revision.is_some() && self.query_revision.is_none(),
            "audio.embed_text_query" => {
                self.source_revision.is_none() && self.query_revision.is_some()
            }
            _ => false,
        };
        if !expected_revisions {
            return Err(ContractError::InvalidAudio(
                "audio embedding response has an invalid input revision shape".into(),
            ));
        }
        let norm = self
            .embedding
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if (norm - 1.0).abs() > 1e-4 {
            return Err(ContractError::InvalidAudio(
                "audio embedding response must be L2 normalized".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_metadata() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ])
    }

    #[test]
    fn audio_embedding_requires_bounded_local_only_input() {
        let request = AudioEmbeddingRequest {
            model: "audio.embed".into(),
            file: AudioFile {
                filename: "chunk.wav".into(),
                content_type: Some("audio/wav".into()),
                bytes: vec![1],
            },
            source_revision: "echo:chunk:1".into(),
            metadata: local_metadata(),
        };
        assert!(request.validate().is_ok());
        let invalid = AudioEmbeddingRequest {
            metadata: BTreeMap::new(),
            ..request
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn audio_text_embedding_rejects_implicit_placement() {
        let request = AudioTextEmbeddingRequest {
            model: "audio.embed_text_query".into(),
            text: "a sustained tone".into(),
            query_revision: "echo:query:1".into(),
            language: "en".into(),
            metadata: local_metadata(),
        };
        assert!(request.validate().is_ok());
        let invalid = AudioTextEmbeddingRequest {
            language: "zh_CN".into(),
            ..request
        };
        assert!(invalid.validate().is_err());
    }
}
