//! Stable contracts for bounded file audio understanding and speech generation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, ExecutionMode, RequestConstraints,
    audio_embedding::{AudioEmbeddingRequest, AudioTextEmbeddingRequest},
    audio_event::EventDetectionRequest,
    string_enum,
};

pub const MAX_AUDIO_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Versioned catalog containing the Runtime-owned logical voices accepted by
/// `speech.synthesize`.
pub const SPEECH_VOICE_ALIAS_CATALOG_REVISION: &str = "infer.speech.voice-aliases@20260922.1";

/// Bright, synthetic Mandarin voice intended for general narration and
/// dialogue. This is a logical contract identity, not a provider speaker name.
pub const SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1: &str = "speech.voice.zh.bright_female.v1";
pub const SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE: &str = "Chinese";

/// Published logical presets and their preferred synthesis languages.
pub const SPEECH_VOICE_PRESETS: &[(&str, &str)] = &[
    ("speech.voice.zh.bright_female.v1", "Chinese"),
    ("speech.voice.zh.warm_female.v1", "Chinese"),
    ("speech.voice.zh.mature_male.v1", "Chinese"),
    ("speech.voice.zh.beijing_male.v1", "Chinese"),
    ("speech.voice.zh.sichuan_male.v1", "Chinese"),
    ("speech.voice.en.dynamic_male.v1", "English"),
    ("speech.voice.en.warm_male.v1", "English"),
    ("speech.voice.ja.bright_female.v1", "Japanese"),
    ("speech.voice.ko.warm_female.v1", "Korean"),
];

pub const SPEECH_LANGUAGES: &[&str] = &[
    "auto",
    "Chinese",
    "English",
    "Japanese",
    "Korean",
    "German",
    "French",
    "Russian",
    "Portuguese",
    "Spanish",
    "Italian",
];

pub fn speech_voice_language(alias: &str) -> Option<&'static str> {
    SPEECH_VOICE_PRESETS
        .iter()
        .find(|(voice, _)| *voice == alias)
        .map(|(_, language)| *language)
}

#[derive(Debug, Clone)]
pub struct AudioFile {
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl AudioFile {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.bytes.is_empty() {
            return Err(ContractError::InvalidAudio("audio file is empty".into()));
        }
        if self.bytes.len() > MAX_AUDIO_UPLOAD_BYTES {
            return Err(ContractError::InvalidAudio(format!(
                "audio file exceeds the {} byte limit",
                MAX_AUDIO_UPLOAD_BYTES
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    pub model: String,
    pub file: AudioFile,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub response_format: TranscriptionFormat,
    pub temperature: Option<f64>,
    pub metadata: BTreeMap<String, String>,
}

impl TranscriptionRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        self.file.validate()?;
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

#[derive(Debug, Clone)]
pub struct AlignmentRequest {
    pub model: String,
    pub file: AudioFile,
    pub text: String,
    pub language: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

impl AlignmentRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        self.file.validate()?;
        if self.text.trim().is_empty() {
            return Err(ContractError::InvalidAudio(
                "alignment text is required".into(),
            ));
        }
        self.constraints().map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRequest {
    /// Stable intent: `speech.synthesize` or `speech.design_voice`.
    pub model: String,
    pub input: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default = "default_speed")]
    pub speed: f64,
    #[serde(default)]
    pub response_format: SpeechFormat,
    /// Transport shape, independent of audio encoding. Streaming currently
    /// uses raw PCM so chunks can be forwarded without container finalization.
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

fn default_speed() -> f64 {
    1.0
}

impl SpeechRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        if self.input.trim().is_empty() {
            return Err(ContractError::InvalidAudio(
                "speech input is required".into(),
            ));
        }
        if !self.speed.is_finite() || !(0.25..=4.0).contains(&self.speed) {
            return Err(ContractError::InvalidAudio(
                "speed must be between 0.25 and 4.0".into(),
            ));
        }
        if self.model == "speech.synthesize" && self.voice.as_deref().is_none_or(str::is_empty) {
            return Err(ContractError::InvalidAudio(
                "speech.synthesize requires voice".into(),
            ));
        }
        // The versioned Runtime alias has a stricter semantic contract than
        // legacy provider speaker strings. Per-App allowlists decide whether
        // a Consumer may use only aliases without breaking existing callers.
        if self
            .voice
            .as_deref()
            .and_then(speech_voice_language)
            .is_some()
            && !SPEECH_LANGUAGES.contains(&self.language.as_deref().unwrap_or("auto"))
        {
            return Err(ContractError::InvalidAudio(
                "unsupported speech language".into(),
            ));
        }
        if self.model == "speech.design_voice"
            && self.instructions.as_deref().is_none_or(str::is_empty)
        {
            return Err(ContractError::InvalidAudio(
                "speech.design_voice requires instructions".into(),
            ));
        }
        match self.execution_mode {
            ExecutionMode::Unary => {}
            ExecutionMode::ServerStream if self.response_format == SpeechFormat::Pcm => {}
            ExecutionMode::ServerStream => {
                return Err(ContractError::InvalidAudio(
                    "server_stream speech requires response_format=pcm".into(),
                ));
            }
            ExecutionMode::Duplex => {
                return Err(ContractError::InvalidAudio(
                    "speech synthesis does not use duplex execution".into(),
                ));
            }
        }
        self.constraints().map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone)]
pub struct VoiceCloneRequest {
    pub model: String,
    pub input: String,
    pub reference_audio: AudioFile,
    pub reference_text: String,
    pub language: Option<String>,
    pub response_format: SpeechFormat,
    pub metadata: BTreeMap<String, String>,
}

impl VoiceCloneRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        self.reference_audio.validate()?;
        if self.input.trim().is_empty() || self.reference_text.trim().is_empty() {
            return Err(ContractError::InvalidAudio(
                "input and reference_text are required".into(),
            ));
        }
        self.constraints().map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone)]
pub enum AudioExecutionRequest {
    Transcription(TranscriptionRequest),
    Alignment(AlignmentRequest),
    EventDetection(EventDetectionRequest),
    Embedding(AudioEmbeddingRequest),
    TextEmbedding(AudioTextEmbeddingRequest),
    Speech(SpeechRequest),
    VoiceClone(VoiceCloneRequest),
}

impl AudioExecutionRequest {
    pub fn model(&self) -> &str {
        match self {
            Self::Transcription(request) => &request.model,
            Self::Alignment(request) => &request.model,
            Self::EventDetection(request) => &request.model,
            Self::Embedding(request) => &request.model,
            Self::TextEmbedding(request) => &request.model,
            Self::Speech(request) => &request.model,
            Self::VoiceClone(request) => &request.model,
        }
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        match self {
            Self::Transcription(request) => request.constraints(),
            Self::Alignment(request) => request.constraints(),
            Self::EventDetection(request) => request.constraints(),
            Self::Embedding(request) => request.constraints(),
            Self::TextEmbedding(request) => request.constraints(),
            Self::Speech(request) => request.constraints(),
            Self::VoiceClone(request) => request.constraints(),
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Transcription(request) => request.validate(),
            Self::Alignment(request) => request.validate(),
            Self::EventDetection(request) => request.validate(),
            Self::Embedding(request) => request.validate(),
            Self::TextEmbedding(request) => request.validate(),
            Self::Speech(request) => request.validate(),
            Self::VoiceClone(request) => request.validate(),
        }
    }
}

pub(crate) fn validate_model(model: &str) -> Result<(), ContractError> {
    if model.trim().is_empty() {
        Err(ContractError::MissingModel)
    } else {
        Ok(())
    }
}

string_enum!(TranscriptionFormat {
    Json => "json",
    Text => "text",
    VerboseJson => "verbose_json"
});

#[allow(clippy::derivable_impls)]
impl Default for TranscriptionFormat {
    fn default() -> Self {
        Self::Json
    }
}

string_enum!(SpeechFormat {
    Wav => "wav",
    Mp3 => "mp3",
    Flac => "flac",
    Opus => "opus",
    Pcm => "pcm"
});

#[allow(clippy::derivable_impls)]
impl Default for SpeechFormat {
    fn default() -> Self {
        Self::Wav
    }
}

impl SpeechFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::Opus => "audio/ogg",
            Self::Pcm => "application/octet-stream",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_audio_before_dispatch() {
        let request = TranscriptionRequest {
            model: "audio.transcribe".into(),
            file: AudioFile {
                filename: "empty.wav".into(),
                content_type: Some("audio/wav".into()),
                bytes: vec![],
            },
            language: None,
            prompt: None,
            response_format: TranscriptionFormat::Json,
            temperature: None,
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn specialized_speech_intents_require_their_discriminator() {
        let request = SpeechRequest {
            model: "speech.design_voice".into(),
            input: "hello".into(),
            voice: None,
            instructions: None,
            language: None,
            speed: 1.0,
            response_format: SpeechFormat::Wav,
            execution_mode: ExecutionMode::Unary,
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn speech_json_rejects_unknown_fields() {
        let error = serde_json::from_value::<SpeechRequest>(serde_json::json!({
            "model": "speech.synthesize",
            "input": "hello",
            "voice": "default",
            "formt": "wav"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `formt`"));
    }

    #[test]
    fn speech_execution_mode_is_independent_of_codec_but_streaming_requires_pcm() {
        let mut request = SpeechRequest {
            model: "speech.synthesize".into(),
            input: "hello".into(),
            voice: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1.into()),
            instructions: None,
            language: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE.into()),
            speed: 1.0,
            response_format: SpeechFormat::Wav,
            execution_mode: ExecutionMode::Unary,
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_ok());

        request.execution_mode = ExecutionMode::ServerStream;
        assert!(request.validate().is_err());
        request.response_format = SpeechFormat::Pcm;
        assert!(request.validate().is_ok());

        request.execution_mode = ExecutionMode::Duplex;
        assert!(request.validate().is_err());
    }

    #[test]
    fn versioned_runtime_voices_support_auto_and_explicit_languages() {
        let mut request = SpeechRequest {
            model: "speech.synthesize".into(),
            input: "hello".into(),
            voice: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1.into()),
            instructions: None,
            language: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE.into()),
            speed: 1.0,
            response_format: SpeechFormat::Wav,
            execution_mode: ExecutionMode::Unary,
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_ok());

        for (alias, language) in SPEECH_VOICE_PRESETS {
            request.voice = Some((*alias).into());
            request.language = Some((*language).into());
            assert!(request.validate().is_ok(), "{alias}");
            request.language = Some("invalid".into());
            assert!(request.validate().is_err(), "{alias}");
        }
        request.language = Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE.into());

        // Existing callers remain compatible until their App opts
        // into an alias-only allowlist.
        request.voice = Some("Vivian".into());
        assert!(request.validate().is_ok());

        request.voice = Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1.into());
        request.language = Some("English".into());
        assert!(request.validate().is_ok());
        request.language = Some("auto".into());
        request.input = "你好，welcome to Shape。".into();
        assert!(request.validate().is_ok());
        request.language = None;
        assert!(request.validate().is_ok());
    }
}
