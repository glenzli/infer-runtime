//! Streaming execution for the MLX audio worker.
//!
//! TTS forwards native generator chunks as PCM. Qwen3-ASR currently exposes a
//! complete-file generate call, so the duplex adapter re-decodes the committed
//! PCM prefix and marks every non-final result as a revisable replacement.

use std::{collections::BTreeMap, sync::Arc};

use async_stream::stream;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use infer_core::{
    AudioExecutionRequest, AudioFile, ExecutionMode, MAX_AUDIO_UPLOAD_BYTES, SpeechFormat,
    SpeechRequest, SpeechStreamDescriptor, StreamSemantics, TranscriptRevision,
    TranscriptionFormat, TranscriptionRequest, TranscriptionSessionRequest,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

use crate::{
    AudioDuplexExecutor, AudioDuplexSession, AudioExecutionOutput, AudioExecutor,
    AudioStreamExecutor, DynAudioDuplexSession, ProviderError, SpeechStreamOutput,
};

use super::{
    AudioWorkerExecutor, PreparedWorkerRequest, WorkerProcess, WorkerRequest, prepare_speech,
    spawn_worker,
};

#[derive(Debug, Deserialize)]
struct WorkerStreamFrame {
    request_id: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    channels: Option<u16>,
    #[serde(default)]
    audio_base64: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[async_trait]
impl AudioStreamExecutor for AudioWorkerExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute_speech_stream(
        &self,
        physical_model: &str,
        request: SpeechRequest,
    ) -> Result<SpeechStreamOutput, ProviderError> {
        request
            .validate()
            .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
        if request.execution_mode != ExecutionMode::ServerStream
            || request.response_format != SpeechFormat::Pcm
        {
            return Err(ProviderError::InvalidInput(
                "MLX speech stream requires execution_mode=server_stream and response_format=pcm"
                    .into(),
            ));
        }
        let prepared = prepare_stream_request(physical_model, request)?;
        let process = Arc::clone(&self.process);
        let command = self.command.clone();
        let args = self.args.clone();
        let (descriptor_tx, descriptor_rx) = oneshot::channel();
        let (chunk_tx, mut chunk_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            run_speech_stream(process, command, args, prepared, descriptor_tx, chunk_tx).await;
        });
        let descriptor = descriptor_rx.await.map_err(|_| {
            ProviderError::Protocol("audio worker closed before stream metadata".into())
        })??;
        let output = stream! {
            while let Some(item) = chunk_rx.recv().await {
                yield item;
            }
        };
        Ok(SpeechStreamOutput {
            descriptor,
            stream: Box::pin(output),
        })
    }
}

fn prepare_stream_request(
    physical_model: &str,
    mut request: SpeechRequest,
) -> Result<PreparedWorkerRequest, ProviderError> {
    request.response_format = SpeechFormat::Pcm;
    let temporary_files = tempfile::tempdir()?;
    let request_id = Uuid::new_v4().simple().to_string();
    let (mut worker, _) = prepare_speech(request_id, physical_model, request, &temporary_files);
    worker.operation = "speech_stream";
    worker.output_path = None;
    Ok(PreparedWorkerRequest {
        request: worker,
        _temporary_files: temporary_files,
        audio_output: None,
    })
}

async fn run_speech_stream(
    process: Arc<tokio::sync::Mutex<Option<WorkerProcess>>>,
    command: String,
    args: Vec<String>,
    prepared: PreparedWorkerRequest,
    descriptor_tx: oneshot::Sender<Result<SpeechStreamDescriptor, ProviderError>>,
    chunk_tx: mpsc::Sender<Result<Bytes, ProviderError>>,
) {
    let mut descriptor_tx = Some(descriptor_tx);
    let result = run_speech_stream_inner(
        &process,
        &command,
        &args,
        &prepared.request,
        &mut descriptor_tx,
        &chunk_tx,
    )
    .await;
    if let Err(error) = result {
        let mut guard = process.lock().await;
        if let Some(worker) = guard.as_mut() {
            let _ = worker.child.start_kill();
        }
        *guard = None;
        if let Some(sender) = descriptor_tx.take() {
            let _ = sender.send(Err(error));
        } else {
            let _ = chunk_tx.send(Err(error)).await;
        }
    }
}

async fn run_speech_stream_inner(
    process: &Arc<tokio::sync::Mutex<Option<WorkerProcess>>>,
    command: &str,
    args: &[String],
    request: &WorkerRequest,
    descriptor_tx: &mut Option<oneshot::Sender<Result<SpeechStreamDescriptor, ProviderError>>>,
    chunk_tx: &mpsc::Sender<Result<Bytes, ProviderError>>,
) -> Result<(), ProviderError> {
    let mut guard = process.lock().await;
    if guard.is_none() {
        *guard = Some(spawn_worker(command, args).await?);
    }
    if guard
        .as_mut()
        .expect("worker initialized")
        .child
        .try_wait()?
        .is_some()
    {
        *guard = Some(spawn_worker(command, args).await?);
    }
    let worker = guard.as_mut().expect("worker restarted");
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    worker.stdin.write_all(&line).await?;
    worker.stdin.flush().await?;

    loop {
        let mut line = String::new();
        if worker.stdout.read_line(&mut line).await? == 0 {
            return Err(ProviderError::Protocol(
                "audio worker exited during speech stream".into(),
            ));
        }
        let frame: WorkerStreamFrame = serde_json::from_str(&line)?;
        if frame.request_id != request.request_id {
            continue;
        }
        if !frame.ok {
            return Err(ProviderError::Protocol(
                frame
                    .error
                    .unwrap_or_else(|| "unknown streaming worker error".into()),
            ));
        }
        match frame.event.as_deref() {
            Some("started") => {
                let descriptor = SpeechStreamDescriptor {
                    format: "pcm_s16le".into(),
                    sample_rate_hz: frame.sample_rate.ok_or_else(|| {
                        ProviderError::Protocol("speech stream omitted sample_rate".into())
                    })?,
                    channels: frame.channels.unwrap_or(1),
                    semantics: StreamSemantics::AppendOnly,
                };
                descriptor_tx
                    .take()
                    .ok_or_else(|| ProviderError::Protocol("duplicate stream start".into()))?
                    .send(Ok(descriptor))
                    .map_err(|_| ProviderError::Protocol("speech stream consumer closed".into()))?;
            }
            Some("audio_chunk") => {
                if descriptor_tx.is_some() {
                    return Err(ProviderError::Protocol(
                        "speech chunk arrived before stream metadata".into(),
                    ));
                }
                let encoded = frame.audio_base64.ok_or_else(|| {
                    ProviderError::Protocol("speech chunk omitted audio_base64".into())
                })?;
                let bytes = STANDARD.decode(encoded).map_err(|_| {
                    ProviderError::Protocol("speech chunk carried invalid base64".into())
                })?;
                if chunk_tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                    return Err(ProviderError::Protocol(
                        "speech stream consumer closed".into(),
                    ));
                }
            }
            Some("completed") => return Ok(()),
            Some(other) => {
                return Err(ProviderError::Protocol(format!(
                    "unknown audio worker stream event {other}"
                )));
            }
            None => {
                return Err(ProviderError::Protocol(
                    "audio worker stream frame omitted event".into(),
                ));
            }
        }
    }
}

#[async_trait]
impl AudioDuplexExecutor for AudioWorkerExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn open_transcription_session(
        &self,
        physical_model: &str,
        request: TranscriptionSessionRequest,
    ) -> Result<DynAudioDuplexSession, ProviderError> {
        request
            .validate()
            .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
        Ok(Box::new(WorkerTranscriptionSession {
            executor: self.clone(),
            physical_model: physical_model.into(),
            request,
            pcm: Vec::new(),
            revision: 0,
            finished: false,
        }))
    }
}

struct WorkerTranscriptionSession {
    executor: AudioWorkerExecutor,
    physical_model: String,
    request: TranscriptionSessionRequest,
    pcm: Vec<u8>,
    revision: u64,
    finished: bool,
}

#[async_trait]
impl AudioDuplexSession for WorkerTranscriptionSession {
    async fn push_audio(&mut self, chunk: Bytes) -> Result<(), ProviderError> {
        if self.finished {
            return Err(ProviderError::InvalidInput(
                "transcription session is already final".into(),
            ));
        }
        let frame_bytes = usize::from(self.request.channels) * 2;
        if chunk.is_empty() || !chunk.len().is_multiple_of(frame_bytes) {
            return Err(ProviderError::InvalidInput(
                "PCM chunk must contain complete signed-16-bit sample frames".into(),
            ));
        }
        if self.pcm.len().saturating_add(chunk.len()) > MAX_AUDIO_UPLOAD_BYTES - 44 {
            return Err(ProviderError::InvalidInput(
                "transcription session exceeds the bounded audio limit".into(),
            ));
        }
        self.pcm.extend_from_slice(&chunk);
        Ok(())
    }

    async fn commit(&mut self, is_final: bool) -> Result<TranscriptRevision, ProviderError> {
        if self.finished {
            return Err(ProviderError::InvalidInput(
                "transcription session is already final".into(),
            ));
        }
        if self.pcm.is_empty() {
            return Err(ProviderError::InvalidInput(
                "cannot commit an empty transcription session".into(),
            ));
        }
        let wav = pcm_s16le_wav(
            &self.pcm,
            self.request.sample_rate_hz,
            self.request.channels,
        )?;
        let output = self
            .executor
            .execute(
                &self.physical_model,
                AudioExecutionRequest::Transcription(TranscriptionRequest {
                    model: self.request.model.clone(),
                    file: AudioFile {
                        filename: "duplex-prefix.wav".into(),
                        content_type: Some("audio/wav".into()),
                        bytes: wav,
                    },
                    language: self.request.language.clone(),
                    prompt: self.request.prompt.clone(),
                    response_format: TranscriptionFormat::VerboseJson,
                    temperature: self.request.temperature,
                    metadata: BTreeMap::new(),
                }),
            )
            .await?;
        let AudioExecutionOutput::Json(value) = output else {
            return Err(ProviderError::Protocol(
                "transcription worker returned audio".into(),
            ));
        };
        self.revision += 1;
        self.finished = is_final;
        Ok(TranscriptRevision {
            revision: self.revision,
            is_final,
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            language: value
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_owned),
            segments: value.get("segments").cloned().unwrap_or(Value::Null),
            semantics: StreamSemantics::Revisable,
            transcription_mode: "commit_redecode".into(),
        })
    }
}

fn pcm_s16le_wav(pcm: &[u8], sample_rate_hz: u32, channels: u16) -> Result<Vec<u8>, ProviderError> {
    let data_len = u32::try_from(pcm.len())
        .map_err(|_| ProviderError::InvalidInput("PCM payload is too large".into()))?;
    let byte_rate = sample_rate_hz
        .checked_mul(u32::from(channels))
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| ProviderError::InvalidInput("PCM descriptor overflows WAV".into()))?;
    let block_align = channels
        .checked_mul(2)
        .ok_or_else(|| ProviderError::InvalidInput("PCM channels overflow WAV".into()))?;
    let riff_len = data_len
        .checked_add(36)
        .ok_or_else(|| ProviderError::InvalidInput("PCM payload overflows WAV".into()))?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn wraps_pcm_in_a_bounded_little_endian_wav() {
        let wav = pcm_s16le_wav(&[0, 0, 1, 0], 16_000, 1).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[44..], &[0, 0, 1, 0]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_worker_proves_pcm_stream_and_revisable_duplex_contracts() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake_audio_worker.py");
        std::fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    request_id = request["request_id"]
    if request["operation"] == "speech_stream":
        print(json.dumps({"request_id": request_id, "ok": True, "event": "started", "sample_rate": 24000, "channels": 1}), flush=True)
        print(json.dumps({"request_id": request_id, "ok": True, "event": "audio_chunk", "audio_base64": "AQI="}), flush=True)
        print(json.dumps({"request_id": request_id, "ok": True, "event": "completed"}), flush=True)
    elif request["operation"] == "transcribe":
        print(json.dumps({"request_id": request_id, "ok": True, "result": {"text": "partial words", "language": "English", "segments": []}}), flush=True)
"#,
        )
        .unwrap();
        let executor = AudioWorkerExecutor::new(
            "fake-audio",
            "python3".into(),
            vec![script.to_string_lossy().into_owned()],
        );
        let output = executor
            .execute_speech_stream(
                "fake-tts",
                SpeechRequest {
                    model: "speech.synthesize".into(),
                    input: "hello".into(),
                    voice: Some("voice".into()),
                    instructions: None,
                    language: None,
                    speed: 1.0,
                    response_format: SpeechFormat::Pcm,
                    execution_mode: ExecutionMode::ServerStream,
                    metadata: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.descriptor.sample_rate_hz, 24_000);
        let chunks = output
            .stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(chunks, vec![Bytes::from_static(&[1, 2])]);

        let mut session = executor
            .open_transcription_session(
                "fake-asr",
                TranscriptionSessionRequest {
                    model: "audio.transcribe".into(),
                    input_audio_format: infer_core::InputAudioFormat::PcmS16le,
                    sample_rate_hz: 16_000,
                    channels: 1,
                    language: None,
                    prompt: None,
                    temperature: None,
                    metadata: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        session
            .push_audio(Bytes::from_static(&[0, 0, 1, 0]))
            .await
            .unwrap();
        let revision = session.commit(false).await.unwrap();
        assert_eq!(revision.revision, 1);
        assert!(!revision.is_final);
        assert_eq!(revision.semantics, StreamSemantics::Revisable);
        assert_eq!(revision.transcription_mode, "commit_redecode");
    }
}
