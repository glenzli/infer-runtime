use std::{collections::BTreeMap, path::Path};

use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    Client, Error, Result,
    transport::{
        MAX_AUDIO_INPUT_BYTES, MAX_AUDIO_RESPONSE_BYTES, MAX_JSON_RESPONSE_BYTES, ensure_success,
        read_bounded, read_bounded_file,
    },
};

pub const TRANSCRIPTION_CAPABILITIES: &[&str] = &["infer.audio.transcription@20260814.1"];
pub const EVENT_DETECTION_CAPABILITIES: &[&str] = &["infer.audio.event-detection@20260813.2"];
pub const ALIGNMENT_CAPABILITIES: &[&str] = &["infer.audio.alignment@20260811.1"];
pub const SPEECH_CAPABILITIES: &[&str] = &["infer.audio.speech@20260811.1"];
pub const SOUND_GENERATION_CAPABILITIES: &[&str] = &[
    "infer.audio.sound-generation@20260926.2",
    "infer.audio.sound-generation@20260926.1",
];
pub const AUDIO_EMBEDDING_CAPABILITIES: &[&str] = &["infer.audio.embedding@20260815.2"];

#[derive(Debug, Clone, Copy)]
pub enum TranscriptionFormat {
    Json,
    Text,
    VerboseJson,
}

impl TranscriptionFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Text => "text",
            Self::VerboseJson => "verbose_json",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpeechFormat {
    #[default]
    Wav,
    Mp3,
    Flac,
    Opus,
    Pcm,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Unary,
    ServerStream,
    Duplex,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRequest {
    pub model: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default = "default_speed")]
    pub speed: f64,
    #[serde(default)]
    pub response_format: SpeechFormat,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

fn default_speed() -> f64 {
    1.0
}
#[derive(Debug, Clone)]
pub struct AudioBytesResponse {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub job_id: String,
    pub logical_model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundGenerationRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_choice: Option<SoundModelChoice>,
    pub prompt: String,
    pub duration_seconds: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum SoundModelChoice {
    #[default]
    #[serde(rename = "stable_audio_3_small_sfx")]
    SmallSfx,
    #[serde(rename = "stable_audio_3_small_music")]
    SmallMusic,
    #[serde(rename = "stable_audio_open_small")]
    OpenSmall,
}

impl SoundModelChoice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SmallSfx => "stable_audio_3_small_sfx",
            Self::SmallMusic => "stable_audio_3_small_music",
            Self::OpenSmall => "stable_audio_open_small",
        }
    }

    pub const fn physical_model(self) -> Option<&'static str> {
        match self {
            Self::SmallSfx => Some(
                "stabilityai/stable-audio-3-optimized@da6edc54ddba10bfd79a077102ded687f80e882b:sm-sfx",
            ),
            Self::SmallMusic => Some(
                "stabilityai/stable-audio-3-optimized@da6edc54ddba10bfd79a077102ded687f80e882b:sm-music",
            ),
            Self::OpenSmall => None,
        }
    }

    pub const fn max_duration_seconds(self) -> u8 {
        match self {
            Self::OpenSmall => 11,
            Self::SmallSfx | Self::SmallMusic => 30,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SoundGenerationResponse {
    /// Exactly one stereo 44.1 kHz PCM16 WAV artifact.
    pub wav: Vec<u8>,
    pub sha256: String,
    pub job_id: String,
    pub logical_model: String,
    pub model_choice: SoundModelChoice,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub physical_model: String,
    pub placement: String,
    pub seed: u32,
    pub duration_seconds: u8,
}

pub struct SpeechByteStream {
    response: reqwest::Response,
    received: usize,
    pub content_type: String,
    pub job_id: String,
    pub logical_model: String,
}

impl SpeechByteStream {
    pub async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        let chunk = self.response.chunk().await?;
        if let Some(chunk) = &chunk {
            self.received = self.received.saturating_add(chunk.len());
            if self.received > MAX_AUDIO_RESPONSE_BYTES {
                return Err(Error::MalformedResponse(format!(
                    "speech stream exceeds {MAX_AUDIO_RESPONSE_BYTES} bytes"
                )));
            }
        }
        Ok(chunk)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptionResponse {
    pub text: String,
    /// A single document-level language only when the provider reported one
    /// unambiguous value. For mixed-language audio, use `language_evidence`.
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_evidence: Option<TranscriptionLanguageEvidence>,
    #[serde(default)]
    pub segments: Value,
    #[serde(default)]
    pub usage: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Provider-reported language evidence. `InputSet` preserves an unordered
/// whole-input set without inventing a dominant language or time boundaries;
/// `Segments` is reserved for providers that supply those boundaries.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptionLanguageEvidence {
    InputSet {
        source: TranscriptionLanguageEvidenceSource,
        languages: Vec<String>,
    },
    Segments {
        source: TranscriptionLanguageEvidenceSource,
        segments: Vec<TranscriptionLanguageSegment>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionLanguageEvidenceSource {
    ProviderReported,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionLanguageSegment {
    pub language: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlignmentResponse {
    pub text: String,
    pub language: String,
    pub items: Vec<AlignmentItem>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlignmentItem {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEventDetectionResponse {
    pub id: String,
    pub model: String,
    pub object: String,
    pub events: Vec<DetectedSoundEvent>,
    pub speech_presence: SpeechPresence,
    pub coverage: AudioAnalysisCoverage,
    pub ontology: SoundEventOntology,
    pub policy: SoundEventDetectionPolicy,
    pub provenance: SoundEventProvenance,
}

/// A bounded text query for the paired local audio retrieval space. `language`
/// describes the query supplied by the Consumer; it is not a model-quality
/// assertion and must not be used to infer multilingual support.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTextEmbeddingRequest {
    pub model: String,
    pub text: String,
    pub query_revision: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudioEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_revision: Option<String>,
    pub embedding: Vec<f32>,
    pub embedding_space: AudioEmbeddingSpace,
    pub provenance: AudioEmbeddingProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_normalizer: Option<AudioTextQueryNormalizerProvenance>,
    /// Forward-compatible capability fields are deliberately retained rather
    /// than rejected by the SDK.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEmbeddingSpace {
    pub identity: String,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
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
pub struct AudioTextQueryNormalizerProvenance {
    pub deployment: String,
    pub build: String,
    pub prompt_revision: String,
    pub source_language: String,
    pub target_language: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DetectedSoundEvent {
    pub class_id: String,
    pub label: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpeechPresenceStatus {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechPresence {
    pub status: SpeechPresenceStatus,
    pub max_score: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioCoverageStatus {
    Full,
    Partial,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioAnalysisCoverage {
    pub status: AudioCoverageStatus,
    pub input_duration_seconds: f64,
    pub analyzed_start_seconds: f64,
    pub analyzed_end_seconds: f64,
    pub analyzed_seconds: f64,
    pub ratio: f64,
    pub window_count: usize,
    pub window_seconds: f64,
    pub hop_seconds: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventOntology {
    pub id: String,
    pub revision: String,
    pub class_id_namespace: String,
    pub class_count: usize,
    pub artifact_sha256: String,
    pub license_spdx: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventSmoothingPolicy {
    pub method: String,
    pub window_frames: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventDetectionPolicy {
    pub revision: String,
    pub score_kind: String,
    pub event_score_threshold: f64,
    pub smoothing: SoundEventSmoothingPolicy,
    pub max_classes_per_window: usize,
    pub max_events: usize,
    pub speech_class_set_revision: String,
    pub speech_present_threshold: f64,
    pub speech_absent_threshold: f64,
    pub max_audio_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventProvenance {
    pub model: String,
    pub model_archive_sha256: String,
    pub artifact_set_sha256: String,
    pub model_license_spdx: String,
    pub training_data_license_spdx: String,
    pub runtime: String,
    pub runtime_version: String,
    pub decoder: String,
    pub decoder_version: String,
    pub preprocessing_identity: String,
}

impl AudioEventDetectionResponse {
    fn validate(&self) -> Result<()> {
        let unit = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
        let digest =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        let valid = !self.id.is_empty()
            && self.model == "audio.detect_events"
            && self.object == "audio.event_detection"
            && self.events.len() <= self.policy.max_events
            && self.ontology.id == "audioset"
            && self.ontology.class_id_namespace == "audioset_mid"
            && self.ontology.class_count > 0
            && digest(&self.ontology.artifact_sha256)
            && digest(&self.provenance.model_archive_sha256)
            && digest(&self.provenance.artifact_set_sha256)
            && unit(self.policy.event_score_threshold)
            && unit(self.policy.speech_present_threshold)
            && unit(self.policy.speech_absent_threshold)
            && self.policy.speech_absent_threshold < self.policy.speech_present_threshold
            && self.policy.smoothing.window_frames > 0
            && !self.policy.smoothing.window_frames.is_multiple_of(2)
            && !self.policy.speech_class_set_revision.is_empty()
            && unit(self.speech_presence.max_score)
            && self.coverage.input_duration_seconds > 0.0
            && self.coverage.input_duration_seconds <= self.policy.max_audio_seconds as f64 + 1e-6
            && unit(self.coverage.ratio);
        if !valid {
            return Err(Error::MalformedResponse(
                "audio event evidence violates the dated capability contract".into(),
            ));
        }
        if self.speech_presence.status == SpeechPresenceStatus::Absent
            && (self.coverage.status != AudioCoverageStatus::Full
                || self.speech_presence.max_score > self.policy.speech_absent_threshold)
        {
            return Err(Error::MalformedResponse(
                "speech absence requires complete low-score model evidence".into(),
            ));
        }
        for event in &self.events {
            if !(event.class_id.starts_with("/m/") || event.class_id.starts_with("/t/"))
                || event.label.is_empty()
                || event.start_seconds < self.coverage.analyzed_start_seconds
                || event.end_seconds <= event.start_seconds
                || event.end_seconds > self.coverage.analyzed_end_seconds + 1e-6
                || !unit(event.score)
                || event.score < self.policy.event_score_threshold
            {
                return Err(Error::MalformedResponse(
                    "audio event interval or AudioSet evidence is invalid".into(),
                ));
            }
        }
        Ok(())
    }
}

impl AudioEmbeddingResponse {
    fn validate(&self) -> Result<()> {
        let digest =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        let expected_revision = match self.model.as_str() {
            "audio.embed" => self.source_revision.is_some() && self.query_revision.is_none(),
            "audio.embed_text_query" => {
                self.source_revision.is_none() && self.query_revision.is_some()
            }
            _ => false,
        };
        let norm = self
            .embedding
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if self.id.is_empty()
            || self.object != "audio.embedding"
            || !expected_revision
            || self.embedding_space.dimensions != 512
            || !self.embedding_space.normalized
            || self.embedding_space.distance_metric != "cosine"
            || self.embedding.len() != 512
            || self.embedding.iter().any(|value| !value.is_finite())
            || (norm - 1.0).abs() > 1e-4
            || self.embedding_space.identity.is_empty()
            || !digest(&self.provenance.artifact_set_sha256)
            || self.provenance.build.is_empty()
            || self.provenance.runtime.is_empty()
            || self.provenance.precision.is_empty()
            || self.provenance.requested_execution_provider.is_empty()
            || self.provenance.actual_execution_provider.is_empty()
            || self.provenance.preprocessing_identity.is_empty()
            || self.provenance.tokenizer_identity.is_empty()
        {
            return Err(Error::MalformedResponse(
                "audio embedding violates the dated capability contract".into(),
            ));
        }
        Ok(())
    }
}

impl Client {
    pub async fn embed_audio_file(
        &self,
        path: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<AudioEmbeddingResponse> {
        let bytes = read_bounded_file(path, MAX_AUDIO_INPUT_BYTES, "audio").await?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.bin")
            .to_owned();
        let source_revision = source_revision.to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let response = self
            .send_capability_with(AUDIO_EMBEDDING_CAPABILITIES, move |http, endpoint| {
                let file = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                http.post(format!("{endpoint}/v1/audio/embeddings"))
                    .multipart(
                        Form::new()
                            .text("model", "audio.embed")
                            .text("source_revision", source_revision.clone())
                            .text("metadata", metadata.clone())
                            .part("file", file),
                    )
            })
            .await?;
        let response = ensure_success(response).await?;
        let parsed: AudioEmbeddingResponse =
            serde_json::from_slice(&read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?)
                .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub async fn embed_audio_text(
        &self,
        request: &AudioTextEmbeddingRequest,
    ) -> Result<AudioEmbeddingResponse> {
        if request.model != "audio.embed_text_query" {
            return Err(Error::Input(
                "audio text embedding requires model=audio.embed_text_query".into(),
            ));
        }
        let response = self
            .send_capability_with(AUDIO_EMBEDDING_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/v1/audio/text-embeddings"))
                    .json(request)
            })
            .await?;
        let response = ensure_success(response).await?;
        let parsed: AudioEmbeddingResponse =
            serde_json::from_slice(&read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?)
                .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub async fn detect_audio_events_file(
        &self,
        path: &Path,
        content_type: &'static str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<AudioEventDetectionResponse> {
        let bytes = read_bounded_file(path, MAX_AUDIO_INPUT_BYTES, "audio").await?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.bin")
            .to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let response = self
            .send_capability_with(EVENT_DETECTION_CAPABILITIES, move |http, endpoint| {
                let file = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                http.post(format!("{endpoint}/v1/audio/event-detections"))
                    .multipart(
                        Form::new()
                            .text("model", "audio.detect_events")
                            .text("metadata", metadata.clone())
                            .part("file", file),
                    )
            })
            .await?;
        let response = ensure_success(response).await?;
        let parsed: AudioEventDetectionResponse =
            serde_json::from_slice(&read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?)
                .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub async fn transcribe_file(
        &self,
        path: &Path,
        content_type: &'static str,
        language: Option<&str>,
        format: TranscriptionFormat,
        metadata: &BTreeMap<String, String>,
    ) -> Result<TranscriptionResponse> {
        if matches!(format, TranscriptionFormat::Text) {
            return Err(Error::MalformedResponse(
                "use a JSON transcription format with the typed SDK".into(),
            ));
        }
        let bytes = read_bounded_file(path, MAX_AUDIO_INPUT_BYTES, "audio").await?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.bin")
            .to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let response = self
            .send_capability_with(TRANSCRIPTION_CAPABILITIES, move |http, endpoint| {
                let file = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                let mut form = Form::new()
                    .text("model", "audio.transcribe")
                    .text("response_format", format.as_str())
                    .text("metadata", metadata.clone())
                    .part("file", file);
                if let Some(language) = language {
                    form = form.text("language", language.to_owned());
                }
                http.post(format!("{endpoint}/v1/audio/transcriptions"))
                    .multipart(form)
            })
            .await?;
        let response = ensure_success(response).await?;
        serde_json::from_slice(&read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?)
            .map_err(|error| Error::MalformedResponse(error.to_string()))
    }

    pub async fn align_file(
        &self,
        path: &Path,
        content_type: &'static str,
        text: &str,
        language: Option<&str>,
        metadata: &BTreeMap<String, String>,
    ) -> Result<AlignmentResponse> {
        let bytes = read_bounded_file(path, MAX_AUDIO_INPUT_BYTES, "audio").await?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.bin")
            .to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let text = text.to_owned();
        let response = self
            .send_capability_with(ALIGNMENT_CAPABILITIES, move |http, endpoint| {
                let file = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                let mut form = Form::new()
                    .text("model", "audio.align")
                    .text("text", text.clone())
                    .text("metadata", metadata.clone())
                    .part("file", file);
                if let Some(language) = language {
                    form = form.text("language", language.to_owned());
                }
                http.post(format!("{endpoint}/v1/audio/alignments"))
                    .multipart(form)
            })
            .await?;
        let response = ensure_success(response).await?;
        serde_json::from_slice(&read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?)
            .map_err(|error| Error::MalformedResponse(error.to_string()))
    }

    pub async fn generate_sound_effect(
        &self,
        request: &SoundGenerationRequest,
    ) -> Result<SoundGenerationResponse> {
        if request.model != "audio.generate_sound"
            || request.prompt.trim().is_empty()
            || request.prompt.len() > 2000
            || request.prompt.chars().any(char::is_control)
            || !(1..=request
                .model_choice
                .unwrap_or_default()
                .max_duration_seconds())
                .contains(&request.duration_seconds)
        {
            return Err(Error::Input(
                "invalid bounded sound generation request".into(),
            ));
        }
        let response = self
            .send_capability_with(SOUND_GENERATION_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/v1/audio/sound-generations"))
                    .json(request)
            })
            .await?;
        let response = ensure_success(response).await?;
        if required_header(&response, reqwest::header::CONTENT_TYPE.as_str())? != "audio/wav" {
            return Err(Error::MalformedResponse(
                "sound generation requires audio/wav".into(),
            ));
        }
        let job_id = required_header(&response, "x-infer-job-id")?;
        let logical_model = required_header(&response, "x-infer-model")?;
        let model_choice = match response.headers().get("x-infer-model-choice") {
            Some(header) => serde_json::from_value::<SoundModelChoice>(Value::String(
                header
                    .to_str()
                    .map_err(|_| Error::MalformedResponse("invalid sound model choice".into()))?
                    .to_owned(),
            ))
            .map_err(|_| Error::MalformedResponse("invalid sound model choice".into()))?,
            None if request.model_choice.unwrap_or_default() == SoundModelChoice::SmallSfx => {
                SoundModelChoice::SmallSfx
            }
            None => {
                return Err(Error::MalformedResponse(
                    "missing sound model choice".into(),
                ));
            }
        };
        let provider = required_header(&response, "x-infer-provider")?;
        let deployment = required_header(&response, "x-infer-deployment")?;
        let model_build = required_header(&response, "x-infer-model-build")?;
        let physical_model = required_header(&response, "x-infer-physical-model")?;
        let placement = required_header(&response, "x-infer-placement")?;
        let sha256 = required_header(&response, "x-infer-artifact-sha256")?;
        let seed = required_header(&response, "x-infer-seed")?
            .parse::<u32>()
            .map_err(|_| Error::MalformedResponse("invalid sound seed".into()))?;
        let duration_seconds = required_header(&response, "x-infer-duration-seconds")?
            .parse::<u8>()
            .map_err(|_| Error::MalformedResponse("invalid sound duration".into()))?;
        let wav = read_bounded(response, 6 * 1024 * 1024).await?;
        if logical_model != request.model
            || model_choice != request.model_choice.unwrap_or_default()
            || model_choice
                .physical_model()
                .is_some_and(|expected| physical_model != expected)
            || duration_seconds != request.duration_seconds
            || request.seed.is_some_and(|expected| expected != seed)
            || placement != "local"
            || provider.is_empty()
            || deployment.is_empty()
            || model_build.is_empty()
            || physical_model.is_empty()
            || sha256 != format!("{:x}", Sha256::digest(&wav))
            || wav.len() != 44 + usize::from(duration_seconds) * 44_100 * 4
            || &wav[..4] != b"RIFF"
            || &wav[8..12] != b"WAVE"
            || &wav[12..16] != b"fmt "
            || u16::from_le_bytes([wav[20], wav[21]]) != 1
            || u16::from_le_bytes([wav[22], wav[23]]) != 2
            || u32::from_le_bytes(wav[24..28].try_into().expect("bounded WAV header")) != 44_100
            || u16::from_le_bytes([wav[34], wav[35]]) != 16
            || &wav[36..40] != b"data"
            || u32::from_le_bytes(wav[40..44].try_into().expect("bounded WAV header")) as usize
                != wav.len() - 44
        {
            return Err(Error::MalformedResponse(
                "sound artifact identity or WAV is invalid".into(),
            ));
        }
        Ok(SoundGenerationResponse {
            wav,
            sha256,
            job_id,
            logical_model,
            model_choice,
            provider,
            deployment,
            model_build,
            physical_model,
            placement,
            seed,
            duration_seconds,
        })
    }

    pub async fn synthesize_speech(&self, request: &SpeechRequest) -> Result<AudioBytesResponse> {
        if request.execution_mode != ExecutionMode::Unary {
            return Err(Error::Input(
                "synthesize_speech is unary; use a dedicated streaming client when available"
                    .into(),
            ));
        }
        let response = self
            .send_capability_with(SPEECH_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/v1/audio/speech"))
                    .json(request)
            })
            .await?;
        let response = ensure_success(response).await?;
        let content_type = required_header(&response, reqwest::header::CONTENT_TYPE.as_str())?;
        let job_id = required_header(&response, "x-infer-job-id")?;
        let logical_model = required_header(&response, "x-infer-model")?;
        Ok(AudioBytesResponse {
            bytes: read_bounded(response, MAX_AUDIO_RESPONSE_BYTES).await?,
            content_type,
            job_id,
            logical_model,
        })
    }

    pub async fn stream_speech(&self, request: &SpeechRequest) -> Result<SpeechByteStream> {
        if request.execution_mode != ExecutionMode::ServerStream
            || request.response_format != SpeechFormat::Pcm
        {
            return Err(Error::Input(
                "stream_speech requires execution_mode=server_stream and response_format=pcm"
                    .into(),
            ));
        }
        let response = self
            .send_capability_with(SPEECH_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/v1/audio/speech"))
                    .json(request)
            })
            .await?;
        let response = ensure_success(response).await?;
        let content_type = required_header(&response, reqwest::header::CONTENT_TYPE.as_str())?;
        let job_id = required_header(&response, "x-infer-job-id")?;
        let logical_model = required_header(&response, "x-infer-model")?;
        Ok(SpeechByteStream {
            response,
            received: 0,
            content_type,
            job_id,
            logical_model,
        })
    }
}

fn required_header(response: &reqwest::Response, name: &str) -> Result<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::MalformedResponse(format!("missing {name} header")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(execution_mode: ExecutionMode) -> SpeechRequest {
        SpeechRequest {
            model: "speech.synthesize".into(),
            input: "bounded fixture".into(),
            voice: Some("speech.voice.test.v1".into()),
            instructions: None,
            language: None,
            speed: 1.0,
            response_format: SpeechFormat::Wav,
            execution_mode,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn execution_modes_have_the_exact_wire_spelling() {
        assert_eq!(
            serde_json::to_value(ExecutionMode::ServerStream).unwrap(),
            "server_stream"
        );
        assert_eq!(serde_json::to_value(SpeechFormat::Pcm).unwrap(), "pcm");
    }

    #[test]
    fn dated_audio_event_fixtures_are_typed_and_absence_is_fail_closed() {
        let present: AudioEventDetectionResponse = serde_json::from_str(include_str!(
            "../../../contracts/capabilities/infer.audio.event-detection/20260813.2/fixtures/event-present.json"
        ))
        .unwrap();
        present.validate().unwrap();

        let mut partial: AudioEventDetectionResponse = serde_json::from_str(include_str!(
            "../../../contracts/capabilities/infer.audio.event-detection/20260813.2/fixtures/partial-unknown.json"
        ))
        .unwrap();
        partial.validate().unwrap();
        partial.speech_presence.status = SpeechPresenceStatus::Absent;
        assert!(partial.validate().is_err());
    }

    #[test]
    fn transcription_language_evidence_preserves_a_mixed_input_set() {
        assert_eq!(
            TRANSCRIPTION_CAPABILITIES,
            &["infer.audio.transcription@20260814.1"]
        );
        let response: TranscriptionResponse = serde_json::from_value(serde_json::json!({
            "text": "provider output",
            "language": null,
            "language_evidence": {
                "kind": "input_set",
                "source": "provider_reported",
                "languages": ["Chinese", "English"]
            }
        }))
        .unwrap();
        assert_eq!(response.language, None);
        assert!(matches!(
            response.language_evidence,
            Some(TranscriptionLanguageEvidence::InputSet {
                source: TranscriptionLanguageEvidenceSource::ProviderReported,
                languages,
            }) if languages == vec!["Chinese".to_owned(), "English".to_owned()]
        ));
    }

    #[test]
    fn audio_embedding_response_is_space_bound_and_forward_compatible() {
        let response: AudioEmbeddingResponse = serde_json::from_value(serde_json::json!({
            "id": "audio_test",
            "object": "audio.embedding",
            "model": "audio.embed_text_query",
            "query_revision": "query:1",
            "embedding": std::iter::once(1.0f32)
                .chain(std::iter::repeat_n(0.0f32, 511))
                .collect::<Vec<_>>(),
            "embedding_space": {
                "identity": "test-space",
                "dimensions": 512,
                "normalized": true,
                "distance_metric": "cosine"
            },
            "provenance": {
                "build": "exact-build",
                "artifact_set_sha256": "a".repeat(64),
                "runtime": "pytorch-mps",
                "precision": "fp32",
                "requested_execution_provider": "mps",
                "actual_execution_provider": "mps",
                "preprocessing_identity": "fixed-preprocess",
                "tokenizer_identity": "exact-tokenizer"
            },
            "future_additive_field": true
        }))
        .unwrap();
        response.validate().unwrap();
        assert_eq!(
            response.extra.get("future_additive_field"),
            Some(&Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn unary_speech_rejects_streaming_before_discovery_or_transport() {
        let client = Client::builder().build().unwrap();
        for mode in [ExecutionMode::ServerStream, ExecutionMode::Duplex] {
            let error = client.synthesize_speech(&request(mode)).await.unwrap_err();
            assert!(matches!(error, Error::Input(message) if message.contains("unary")));
        }
    }
}
