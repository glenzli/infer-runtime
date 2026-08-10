//! Cross-modal execution and streaming contracts.
//!
//! The control plane reasons about execution shape without forcing text,
//! audio, and transcript payloads into one wire schema. Each typed data plane
//! keeps its own payload while sharing these lifecycle and revision semantics.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ContractError, RequestConstraints, string_enum};

string_enum!(ExecutionMode {
    Unary => "unary",
    ServerStream => "server_stream",
    Duplex => "duplex"
});

// `string_enum!` cannot attach `#[default]` to one generated variant.
#[allow(clippy::derivable_impls)]
impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Unary
    }
}

string_enum!(StreamSemantics {
    AppendOnly => "append_only",
    Revisable => "revisable"
});

string_enum!(InputAudioFormat {
    PcmS16le => "pcm_s16le"
});

/// The first message sent by a Consumer after opening the transcription
/// WebSocket. The session is intentionally raw-PCM-only: file/container
/// decoding remains in the bounded transcription endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionSessionRequest {
    /// Stable intent, normally `audio.transcribe`.
    pub model: String,
    pub input_audio_format: InputAudioFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl TranscriptionSessionRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty() {
            return Err(ContractError::MissingModel);
        }
        if !(8_000..=48_000).contains(&self.sample_rate_hz) {
            return Err(ContractError::InvalidAudio(
                "sample_rate_hz must be between 8000 and 48000".into(),
            ));
        }
        if !(1..=2).contains(&self.channels) {
            return Err(ContractError::InvalidAudio(
                "channels must be 1 or 2".into(),
            ));
        }
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err(ContractError::InvalidAudio(
                "temperature must be finite and non-negative".into(),
            ));
        }
        self.constraints().map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

/// One provider-produced transcript revision. `revision` is monotonically
/// increasing within a session; a partial revision replaces the prior partial
/// in full. Only `is_final=true` is safe to persist as the terminal transcript.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptRevision {
    pub revision: u64,
    pub is_final: bool,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub segments: Value,
    pub semantics: StreamSemantics,
    /// Discloses whether the provider is natively incremental or re-decodes a
    /// committed prefix. Consumers must not infer latency guarantees from the
    /// duplex transport alone.
    pub transcription_mode: String,
}

/// Metadata fixed before the first streamed speech byte is exposed.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpeechStreamDescriptor {
    pub format: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub semantics: StreamSemantics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplex_session_rejects_unbounded_or_ambiguous_pcm_shapes() {
        let mut request = TranscriptionSessionRequest {
            model: "audio.transcribe".into(),
            input_audio_format: InputAudioFormat::PcmS16le,
            sample_rate_hz: 16_000,
            channels: 1,
            language: None,
            prompt: None,
            temperature: None,
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_ok());
        request.channels = 3;
        assert!(request.validate().is_err());
    }
}
