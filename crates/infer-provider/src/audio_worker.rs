//! Persistent JSON-lines bridge to typed local file-audio workers.

mod streaming;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use async_trait::async_trait;
use infer_core::{
    AlignmentRequest, AudioExecutionRequest, AudioFile, EventDetectionRequest,
    EventDetectionResult, SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1, SpeechFormat, SpeechRequest,
    TranscriptionRequest, VoiceCloneRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use uuid::Uuid;

use crate::ProviderError;

const MAX_WORKER_RESPONSE_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum AudioExecutionOutput {
    Json(Value),
    Audio {
        bytes: Vec<u8>,
        content_type: &'static str,
    },
}

#[async_trait]
pub trait AudioExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn execute(
        &self,
        physical_model: &str,
        request: AudioExecutionRequest,
    ) -> Result<AudioExecutionOutput, ProviderError>;
}

pub type DynAudioExecutor = Arc<dyn AudioExecutor>;

pub struct AudioWorkerExecutor {
    id: String,
    command: String,
    args: Vec<String>,
    admitted_model_paths: Arc<BTreeMap<String, String>>,
    process: Arc<Mutex<Option<WorkerProcess>>>,
}

impl Clone for AudioWorkerExecutor {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            admitted_model_paths: Arc::clone(&self.admitted_model_paths),
            process: Arc::clone(&self.process),
        }
    }
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Debug, Serialize)]
struct WorkerRequest {
    request_id: String,
    operation: &'static str,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_audio_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<SpeechFormat>,
}

#[derive(Debug, Deserialize)]
struct WorkerResponse {
    request_id: String,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

struct PreparedWorkerRequest {
    request: WorkerRequest,
    _temporary_files: TempDir,
    audio_output: Option<(PathBuf, SpeechFormat)>,
}

impl AudioWorkerExecutor {
    pub fn new(id: impl Into<String>, command: String, args: Vec<String>) -> Self {
        Self {
            id: id.into(),
            command,
            args,
            admitted_model_paths: Arc::new(BTreeMap::new()),
            process: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_admitted_model_paths(
        id: impl Into<String>,
        command: String,
        args: Vec<String>,
        admitted_model_paths: BTreeMap<String, String>,
    ) -> Self {
        Self {
            id: id.into(),
            command,
            args,
            admitted_model_paths: Arc::new(admitted_model_paths),
            process: Arc::new(Mutex::new(None)),
        }
    }

    async fn spawn(&self) -> Result<WorkerProcess, ProviderError> {
        spawn_worker(&self.command, &self.args).await
    }

    async fn round_trip(&self, request: &WorkerRequest) -> Result<Value, ProviderError> {
        let mut guard = self.process.lock().await;
        let mut process = match guard.take() {
            Some(mut process) => {
                if process.child.try_wait()?.is_none() {
                    process
                } else {
                    self.spawn().await?
                }
            }
            None => self.spawn().await?,
        };
        let line = serde_json::to_vec(request)?;
        process.stdin.write_all(&line).await?;
        process.stdin.write_all(b"\n").await?;
        process.stdin.flush().await?;

        loop {
            let mut response_line = String::new();
            let bytes = (&mut process.stdout)
                .take((MAX_WORKER_RESPONSE_LINE_BYTES + 1) as u64)
                .read_line(&mut response_line)
                .await?;
            if bytes == 0 {
                return Err(ProviderError::Protocol(
                    "audio worker exited without a response".into(),
                ));
            }
            if bytes > MAX_WORKER_RESPONSE_LINE_BYTES || !response_line.ends_with('\n') {
                return Err(ProviderError::Protocol(
                    "audio worker response exceeds the bounded protocol frame".into(),
                ));
            }
            let response: WorkerResponse = serde_json::from_str(&response_line)?;
            // A timed-out caller can leave one completed response buffered. The
            // request id makes it safe for the next caller to drain that frame.
            if response.request_id != request.request_id {
                continue;
            }
            if response.ok {
                *guard = Some(process);
                return Ok(response.result.unwrap_or(Value::Null));
            }
            return Err(ProviderError::Protocol(
                response
                    .error
                    .unwrap_or_else(|| "unknown worker error".into()),
            ));
        }
    }
}

async fn spawn_worker(command: &str, args: &[String]) -> Result<WorkerProcess, ProviderError> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::Protocol("worker stdin is unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("worker stdout is unavailable".into()))?;
    Ok(WorkerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

#[async_trait]
impl AudioExecutor for AudioWorkerExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute(
        &self,
        physical_model: &str,
        request: AudioExecutionRequest,
    ) -> Result<AudioExecutionOutput, ProviderError> {
        if matches!(
            &request,
            AudioExecutionRequest::Speech(request)
                if request.execution_mode != infer_core::ExecutionMode::Unary
        ) {
            return Err(ProviderError::InvalidInput(
                "streaming speech must use execute_speech_stream".into(),
            ));
        }
        let physical_model = if self.admitted_model_paths.is_empty() {
            physical_model
        } else {
            self.admitted_model_paths
                .get(physical_model)
                .map(String::as_str)
                .ok_or_else(|| {
                    ProviderError::InvalidInput(
                        "physical audio model is not admitted by the typed Build manifest".into(),
                    )
                })?
        };
        let prepared = prepare_worker_request(physical_model, request).await?;
        let mut result = self.round_trip(&prepared.request).await?;
        if prepared.request.operation == "detect_events" {
            let typed: EventDetectionResult = serde_json::from_value(result)?;
            typed
                .validate()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?;
            result = serde_json::to_value(typed)?;
        }
        if let Some((path, format)) = prepared.audio_output {
            let bytes = fs::read(path).await?;
            Ok(AudioExecutionOutput::Audio {
                bytes,
                content_type: format.content_type(),
            })
        } else {
            Ok(AudioExecutionOutput::Json(result))
        }
    }
}

async fn prepare_worker_request(
    physical_model: &str,
    request: AudioExecutionRequest,
) -> Result<PreparedWorkerRequest, ProviderError> {
    let temporary_files = tempfile::tempdir()?;
    let request_id = Uuid::new_v4().simple().to_string();
    let (request, audio_output) = match request {
        AudioExecutionRequest::Transcription(request) => {
            prepare_transcription(request_id, physical_model, request, &temporary_files).await?
        }
        AudioExecutionRequest::Alignment(request) => {
            prepare_alignment(request_id, physical_model, request, &temporary_files).await?
        }
        AudioExecutionRequest::EventDetection(request) => {
            prepare_event_detection(request_id, physical_model, request, &temporary_files).await?
        }
        AudioExecutionRequest::Speech(request) => {
            prepare_speech(request_id, physical_model, request, &temporary_files)
        }
        AudioExecutionRequest::VoiceClone(request) => {
            prepare_voice_clone(request_id, physical_model, request, &temporary_files).await?
        }
    };
    Ok(PreparedWorkerRequest {
        request,
        _temporary_files: temporary_files,
        audio_output,
    })
}

async fn prepare_event_detection(
    request_id: String,
    model: &str,
    request: EventDetectionRequest,
    temporary_files: &TempDir,
) -> Result<(WorkerRequest, Option<(PathBuf, SpeechFormat)>), ProviderError> {
    let audio_path = write_audio_file(temporary_files, "input", &request.file).await?;
    Ok((
        worker_request(request_id, "detect_events", model, Some(audio_path)),
        None,
    ))
}

async fn prepare_transcription(
    request_id: String,
    model: &str,
    request: TranscriptionRequest,
    temporary_files: &TempDir,
) -> Result<(WorkerRequest, Option<(PathBuf, SpeechFormat)>), ProviderError> {
    let audio_path = write_audio_file(temporary_files, "input", &request.file).await?;
    Ok((
        WorkerRequest {
            language: request.language,
            prompt: request.prompt,
            temperature: request.temperature,
            ..worker_request(request_id, "transcribe", model, Some(audio_path))
        },
        None,
    ))
}

async fn prepare_alignment(
    request_id: String,
    model: &str,
    request: AlignmentRequest,
    temporary_files: &TempDir,
) -> Result<(WorkerRequest, Option<(PathBuf, SpeechFormat)>), ProviderError> {
    let audio_path = write_audio_file(temporary_files, "input", &request.file).await?;
    Ok((
        WorkerRequest {
            text: Some(request.text),
            language: request.language,
            ..worker_request(request_id, "align", model, Some(audio_path))
        },
        None,
    ))
}

fn prepare_speech(
    request_id: String,
    model: &str,
    request: SpeechRequest,
    temporary_files: &TempDir,
) -> (WorkerRequest, Option<(PathBuf, SpeechFormat)>) {
    // Public callers use a Runtime-owned, versioned voice identity. Provider
    // speaker names remain an adapter detail and must never become Consumer
    // contract values.
    let provider_voice = match request.voice.as_deref() {
        Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1) => Some("Vivian".to_owned()),
        _ => request.voice.clone(),
    };
    let output_path = temporary_files.path().join(format!(
        "output.{}",
        request.response_format.to_string_value()
    ));
    (
        WorkerRequest {
            output_path: Some(output_path.to_string_lossy().into_owned()),
            text: Some(request.input),
            language: request.language,
            voice: provider_voice,
            instructions: request.instructions,
            speed: Some(request.speed),
            format: Some(request.response_format),
            ..worker_request(request_id, "speech", model, None)
        },
        Some((output_path, request.response_format)),
    )
}

async fn prepare_voice_clone(
    request_id: String,
    model: &str,
    request: VoiceCloneRequest,
    temporary_files: &TempDir,
) -> Result<(WorkerRequest, Option<(PathBuf, SpeechFormat)>), ProviderError> {
    let reference_audio_path =
        write_audio_file(temporary_files, "reference", &request.reference_audio).await?;
    let output_path = temporary_files.path().join(format!(
        "output.{}",
        request.response_format.to_string_value()
    ));
    Ok((
        WorkerRequest {
            output_path: Some(output_path.to_string_lossy().into_owned()),
            text: Some(request.input),
            reference_text: Some(request.reference_text),
            reference_audio_path: Some(reference_audio_path),
            language: request.language,
            format: Some(request.response_format),
            ..worker_request(request_id, "voice_clone", model, None)
        },
        Some((output_path, request.response_format)),
    ))
}

fn worker_request(
    request_id: String,
    operation: &'static str,
    model: &str,
    audio_path: Option<String>,
) -> WorkerRequest {
    WorkerRequest {
        request_id,
        operation,
        model: model.into(),
        audio_path,
        output_path: None,
        text: None,
        reference_text: None,
        reference_audio_path: None,
        language: None,
        prompt: None,
        voice: None,
        instructions: None,
        speed: None,
        temperature: None,
        format: None,
    }
}

async fn write_audio_file(
    temporary_files: &TempDir,
    stem: &str,
    file: &AudioFile,
) -> Result<String, ProviderError> {
    let extension = safe_extension(&file.filename);
    let path = temporary_files.path().join(format!("{stem}.{extension}"));
    fs::write(&path, &file.bytes).await?;
    Ok(path.to_string_lossy().into_owned())
}

fn safe_extension(filename: &str) -> &str {
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 8
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or("wav")
}

trait StringEnumValue {
    fn to_string_value(self) -> &'static str;
}

impl StringEnumValue for SpeechFormat {
    fn to_string_value(self) -> &'static str {
        match self {
            SpeechFormat::Wav => "wav",
            SpeechFormat::Mp3 => "mp3",
            SpeechFormat::Flac => "flac",
            SpeechFormat::Opus => "opus",
            SpeechFormat::Pcm => "pcm",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use infer_core::{ExecutionMode, SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE};

    #[test]
    fn uploaded_filename_cannot_escape_the_temporary_directory() {
        assert_eq!(safe_extension("../../voice.wav"), "wav");
        assert_eq!(safe_extension("voice.bad/path"), "wav");
        assert_eq!(safe_extension("voice.123456789"), "wav");
    }

    #[test]
    fn runtime_voice_alias_maps_to_the_private_worker_speaker() {
        let temporary_files = TempDir::new().expect("temporary directory");
        let request = SpeechRequest {
            model: "speech.synthesize".into(),
            input: "bounded test input".into(),
            voice: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1.into()),
            instructions: None,
            language: Some(SPEECH_VOICE_ZH_BRIGHT_FEMALE_LANGUAGE.into()),
            speed: 1.0,
            response_format: SpeechFormat::Wav,
            execution_mode: ExecutionMode::Unary,
            metadata: BTreeMap::new(),
        };

        request.validate().expect("public speech contract");
        let (worker_request, _) = prepare_speech(
            "request-1".into(),
            "physical-model",
            request,
            &temporary_files,
        );

        assert_eq!(worker_request.voice.as_deref(), Some("Vivian"));
    }
}
