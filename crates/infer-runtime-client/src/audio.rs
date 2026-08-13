use std::{collections::BTreeMap, path::Path};

use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Client, Error, Result,
    transport::{
        MAX_AUDIO_INPUT_BYTES, MAX_AUDIO_RESPONSE_BYTES, MAX_JSON_RESPONSE_BYTES, ensure_success,
        read_bounded, read_bounded_file,
    },
};

pub const TRANSCRIPTION_CAPABILITIES: &[&str] = &["infer.audio.transcription@20260811.1"];
pub const EVENT_DETECTION_CAPABILITIES: &[&str] = &["infer.audio.event-detection@20260813.2"];
pub const ALIGNMENT_CAPABILITIES: &[&str] = &["infer.audio.alignment@20260811.1"];
pub const SPEECH_CAPABILITIES: &[&str] = &["infer.audio.speech@20260811.1"];

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
    pub language: Option<String>,
    #[serde(default)]
    pub segments: Value,
    #[serde(default)]
    pub usage: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
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

impl Client {
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

    #[tokio::test]
    async fn unary_speech_rejects_streaming_before_discovery_or_transport() {
        let client = Client::builder().build().unwrap();
        for mode in [ExecutionMode::ServerStream, ExecutionMode::Duplex] {
            let error = client.synthesize_speech(&request(mode)).await.unwrap_err();
            assert!(matches!(error, Error::Input(message) if message.contains("unary")));
        }
    }
}
