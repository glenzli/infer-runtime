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

impl Client {
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

    #[tokio::test]
    async fn unary_speech_rejects_streaming_before_discovery_or_transport() {
        let client = Client::builder().build().unwrap();
        for mode in [ExecutionMode::ServerStream, ExecutionMode::Duplex] {
            let error = client.synthesize_speech(&request(mode)).await.unwrap_err();
            assert!(matches!(error, Error::Input(message) if message.contains("unary")));
        }
    }
}
