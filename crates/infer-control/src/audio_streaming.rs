//! Control-plane lifecycle for typed audio server streams and duplex sessions.

use std::{collections::BTreeSet, pin::Pin, sync::Arc};

use async_stream::stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use infer_core::{
    ExecutionMode, ExecutionRequirements, JobState, Modality, SpeechRequest,
    SpeechStreamDescriptor, TranscriptRevision, TranscriptionSessionRequest,
};
use infer_provider::{DynAudioDuplexSession, ProviderError};
use infer_resource::ModelReservation;
use tokio::time::timeout_at;

use crate::{
    AttemptOutcome, AttemptTrigger, JobPreparation, PreparedRun, Runtime, RuntimeError,
    ScheduledPermit,
};

pub type RuntimeAudioByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

pub struct SpeechRuntimeStream {
    pub job_id: String,
    pub logical_model: String,
    pub descriptor: SpeechStreamDescriptor,
    pub stream: RuntimeAudioByteStream,
}

pub struct RuntimeTranscriptionSession {
    runtime: Arc<Runtime>,
    job_id: String,
    prepared: Option<PreparedRun>,
    attempt_number: usize,
    provider: DynAudioDuplexSession,
    _permit: Option<ScheduledPermit>,
    _resource_reservation: Option<ModelReservation>,
}

/// Ensures dropping an HTTP response body is a control-plane cancellation,
/// not a permanently running Job. The guard is captured by the stream itself,
/// so it also runs when the body is never polled.
struct SpeechStreamTerminalGuard {
    runtime: Arc<Runtime>,
    prepared: Option<PreparedRun>,
    attempt_number: usize,
}

impl SpeechStreamTerminalGuard {
    fn prepared(&self) -> &PreparedRun {
        self.prepared.as_ref().expect("active speech stream")
    }

    fn disarm(&mut self) {
        self.prepared.take();
    }
}

impl Drop for SpeechStreamTerminalGuard {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let runtime = Arc::clone(&self.runtime);
        let attempt_number = self.attempt_number;
        tokio::spawn(async move {
            let _ = runtime
                .finish_attempt(
                    &prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("cancelled".into()),
                    Some("speech stream client disconnected".into()),
                    None,
                )
                .await;
            let _ = runtime
                .mark(&prepared.job_id, JobState::Cancelled, None)
                .await;
            runtime.metrics.cancelled();
        });
    }
}

impl Runtime {
    pub async fn execute_speech_stream(
        self: &Arc<Self>,
        app_id: &str,
        request: SpeechRequest,
    ) -> Result<SpeechRuntimeStream, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let requirements = ExecutionRequirements {
            input_modalities: BTreeSet::from([Modality::Text]),
            execution_mode: ExecutionMode::ServerStream,
            ..ExecutionRequirements::default()
        };
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &logical_model,
                    constraints: request.constraints()?,
                    execution_requirements: requirements,
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "audio",
                    expected_data_plane: "audio.speech",
                    durable_payload: None,
                },
            )
            .await?;
        let resource_reservation = self.reserve_resource(&prepared).await?;
        let permit = self.acquire(&prepared).await?;
        self.mark(&prepared.job_id, JobState::Running, None).await?;
        let attempt_number = self
            .begin_attempt(&prepared, AttemptTrigger::Initial)
            .await?;
        let executor = self
            .audio_stream_executors
            .get(&prepared.provider_id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(prepared.provider_id.clone()))?;
        let setup = executor.execute_speech_stream(&prepared.physical_model, request);
        let upstream = match prepared.deadline {
            Some(deadline) => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = timeout_at(deadline, setup) => {
                    result.map_err(|_| RuntimeError::DeadlineExpired)?.map_err(RuntimeError::Provider)
                }
            },
            None => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = setup => result.map_err(RuntimeError::Provider),
            },
        };
        let output = match upstream {
            Ok(output) => output,
            Err(error) => {
                self.finish_audio_setup_error(&prepared, attempt_number, &error)
                    .await?;
                return Err(error);
            }
        };
        let job_id = prepared.job_id.clone();
        let runtime = Arc::clone(self);
        let mut provider_stream = output.stream;
        let terminal_guard = SpeechStreamTerminalGuard {
            runtime: Arc::clone(self),
            prepared: Some(prepared),
            attempt_number,
        };
        let normalized = stream! {
            let _permit = permit;
            let _resource_reservation = resource_reservation;
            let mut terminal = terminal_guard;
            loop {
                let prepared = terminal.prepared();
                let next = tokio::select! {
                    _ = prepared.cancellation.cancelled() => {
                        let error = ProviderError::Protocol("speech stream was cancelled".into());
                        let _ = runtime.finish_attempt(prepared, attempt_number, AttemptOutcome::Failed, Some("cancelled".into()), Some(error.to_string()), None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Cancelled, None).await;
                        runtime.metrics.cancelled();
                        terminal.disarm();
                        yield Err(error);
                        return;
                    }
                    _ = async { if let Some(deadline) = prepared.deadline { tokio::time::sleep_until(deadline).await; } }, if prepared.deadline.is_some() => {
                        let error = ProviderError::Protocol("speech stream deadline expired".into());
                        let _ = runtime.finish_attempt(prepared, attempt_number, AttemptOutcome::Failed, Some("deadline_exceeded".into()), Some(error.to_string()), None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Expired, Some(error.to_string())).await;
                        runtime.metrics.expired();
                        terminal.disarm();
                        yield Err(error);
                        return;
                    }
                    item = provider_stream.next() => item,
                };
                match next {
                    Some(Ok(chunk)) => yield Ok(chunk),
                    Some(Err(error)) => {
                        runtime.health.record_failure(&prepared.provider_id, &error);
                        let _ = runtime.finish_attempt(prepared, attempt_number, AttemptOutcome::Failed, Some(crate::attempt_policy::kind_code(error.kind()).into()), Some(error.to_string()), None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Failed, Some(error.to_string())).await;
                        runtime.metrics.failed();
                        terminal.disarm();
                        yield Err(error);
                        return;
                    }
                    None => {
                        runtime.health.record_success(&prepared.provider_id);
                        let _ = runtime.finish_attempt(prepared, attempt_number, AttemptOutcome::Succeeded, None, None, None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Succeeded, None).await;
                        runtime.metrics.succeeded();
                        terminal.disarm();
                        return;
                    }
                }
            }
        };
        Ok(SpeechRuntimeStream {
            job_id,
            logical_model,
            descriptor: output.descriptor,
            stream: Box::pin(normalized),
        })
    }

    pub async fn open_transcription_session(
        self: &Arc<Self>,
        app_id: &str,
        request: TranscriptionSessionRequest,
    ) -> Result<RuntimeTranscriptionSession, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &logical_model,
                    constraints: request.constraints()?,
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([Modality::Audio]),
                        execution_mode: ExecutionMode::Duplex,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "audio_session",
                    expected_data_plane: "audio.transcription",
                    durable_payload: None,
                },
            )
            .await?;
        let resource_reservation = self.reserve_resource(&prepared).await?;
        let permit = self.acquire(&prepared).await?;
        self.mark(&prepared.job_id, JobState::Running, None).await?;
        let attempt_number = self
            .begin_attempt(&prepared, AttemptTrigger::Initial)
            .await?;
        let executor = self
            .audio_duplex_executors
            .get(&prepared.provider_id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(prepared.provider_id.clone()))?;
        let setup = executor.open_transcription_session(&prepared.physical_model, request);
        let provider = match prepared.deadline {
            Some(deadline) => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = timeout_at(deadline, setup) => {
                    result.map_err(|_| RuntimeError::DeadlineExpired)?.map_err(RuntimeError::Provider)
                }
            },
            None => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = setup => result.map_err(RuntimeError::Provider),
            },
        };
        let provider = match provider {
            Ok(provider) => provider,
            Err(error) => {
                self.finish_audio_setup_error(&prepared, attempt_number, &error)
                    .await?;
                return Err(error);
            }
        };
        Ok(RuntimeTranscriptionSession {
            runtime: Arc::clone(self),
            job_id: prepared.job_id.clone(),
            prepared: Some(prepared),
            attempt_number,
            provider,
            _permit: Some(permit),
            _resource_reservation: resource_reservation,
        })
    }

    async fn finish_audio_setup_error(
        &self,
        prepared: &PreparedRun,
        attempt_number: usize,
        error: &RuntimeError,
    ) -> Result<(), RuntimeError> {
        let (state, code) = match error {
            RuntimeError::Cancelled => (JobState::Cancelled, "cancelled"),
            RuntimeError::DeadlineExpired => (JobState::Expired, "deadline_exceeded"),
            RuntimeError::Provider(error) => {
                self.health.record_failure(&prepared.provider_id, error);
                (
                    JobState::Failed,
                    crate::attempt_policy::kind_code(error.kind()),
                )
            }
            _ => (JobState::Failed, "setup_failed"),
        };
        self.finish_attempt(
            prepared,
            attempt_number,
            AttemptOutcome::Failed,
            Some(code.into()),
            Some(error.to_string()),
            None,
        )
        .await?;
        self.mark(prepared.job_id.as_str(), state, Some(error.to_string()))
            .await?;
        match state {
            JobState::Cancelled => self.metrics.cancelled(),
            JobState::Expired => self.metrics.expired(),
            _ => self.metrics.failed(),
        }
        Ok(())
    }
}

impl RuntimeTranscriptionSession {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub async fn push_audio(&mut self, chunk: Bytes) -> Result<(), RuntimeError> {
        let prepared = self.active()?;
        let cancellation = prepared.cancellation.clone();
        let deadline = prepared.deadline;
        let result = match deadline {
            Some(deadline) => tokio::select! {
                _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = timeout_at(deadline, self.provider.push_audio(chunk)) => {
                    result.map_err(|_| RuntimeError::DeadlineExpired)?.map_err(RuntimeError::Provider)
                }
            },
            None => tokio::select! {
                _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = self.provider.push_audio(chunk) => result.map_err(RuntimeError::Provider),
            },
        };
        if let Err(error) = &result {
            self.finish_error(error).await?;
        }
        result
    }

    pub async fn commit(&mut self, is_final: bool) -> Result<TranscriptRevision, RuntimeError> {
        let prepared = self.active()?;
        let cancellation = prepared.cancellation.clone();
        let deadline = prepared.deadline;
        let result = match deadline {
            Some(deadline) => tokio::select! {
                _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = timeout_at(deadline, self.provider.commit(is_final)) => {
                    result.map_err(|_| RuntimeError::DeadlineExpired)?.map_err(RuntimeError::Provider)
                }
            },
            None => tokio::select! {
                _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = self.provider.commit(is_final) => result.map_err(RuntimeError::Provider),
            },
        };
        match result {
            Ok(revision) => {
                if is_final {
                    self.finish_success().await?;
                }
                Ok(revision)
            }
            Err(error) => {
                self.finish_error(&error).await?;
                Err(error)
            }
        }
    }

    pub async fn cancel(&mut self) -> Result<(), RuntimeError> {
        if self.prepared.is_none() {
            return Ok(());
        }
        self.finish_error(&RuntimeError::Cancelled).await
    }

    fn active(&self) -> Result<&PreparedRun, RuntimeError> {
        self.prepared.as_ref().ok_or(RuntimeError::Cancelled)
    }

    async fn finish_success(&mut self) -> Result<(), RuntimeError> {
        let prepared = self.prepared.take().expect("active session");
        self.runtime.health.record_success(&prepared.provider_id);
        self.runtime
            .finish_attempt(
                &prepared,
                self.attempt_number,
                AttemptOutcome::Succeeded,
                None,
                None,
                None,
            )
            .await?;
        self.runtime
            .mark(&prepared.job_id, JobState::Succeeded, None)
            .await?;
        self.runtime.metrics.succeeded();
        self._permit.take();
        self._resource_reservation.take();
        Ok(())
    }

    async fn finish_error(&mut self, error: &RuntimeError) -> Result<(), RuntimeError> {
        let Some(prepared) = self.prepared.take() else {
            return Ok(());
        };
        let (state, code) = match error {
            RuntimeError::Cancelled => (JobState::Cancelled, "cancelled"),
            RuntimeError::DeadlineExpired => (JobState::Expired, "deadline_exceeded"),
            RuntimeError::Provider(provider) => {
                self.runtime
                    .health
                    .record_failure(&prepared.provider_id, provider);
                (
                    JobState::Failed,
                    crate::attempt_policy::kind_code(provider.kind()),
                )
            }
            _ => (JobState::Failed, "session_failed"),
        };
        self.runtime
            .finish_attempt(
                &prepared,
                self.attempt_number,
                AttemptOutcome::Failed,
                Some(code.into()),
                Some(error.to_string()),
                None,
            )
            .await?;
        self.runtime
            .mark(&prepared.job_id, state, Some(error.to_string()))
            .await?;
        match state {
            JobState::Cancelled => self.runtime.metrics.cancelled(),
            JobState::Expired => self.runtime.metrics.expired(),
            _ => self.runtime.metrics.failed(),
        }
        self._permit.take();
        self._resource_reservation.take();
        Ok(())
    }
}

impl Drop for RuntimeTranscriptionSession {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let runtime = Arc::clone(&self.runtime);
        let attempt_number = self.attempt_number;
        tokio::spawn(async move {
            let _ = runtime
                .finish_attempt(
                    &prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("cancelled".into()),
                    Some("duplex client disconnected".into()),
                    None,
                )
                .await;
            let _ = runtime
                .mark(&prepared.job_id, JobState::Cancelled, None)
                .await;
            runtime.metrics.cancelled();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use infer_core::{ExecutionMode, JobState, RuntimeConfig, SpeechFormat};

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_unpolled_speech_body_cancels_its_job() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake_audio_worker.py");
        std::fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    request_id = request["request_id"]
    print(json.dumps({"request_id": request_id, "ok": True, "event": "started", "sample_rate": 24000, "channels": 1}), flush=True)
    print(json.dumps({"request_id": request_id, "ok": True, "event": "audio_chunk", "audio_base64": "AQI="}), flush=True)
    print(json.dumps({"request_id": request_id, "ok": True, "event": "completed"}), flush=True)
"#,
        )
        .unwrap();
        let persistence = temp.path().join("runtime.sqlite3");
        let credentials = temp.path().join("credentials");
        let config: RuntimeConfig = toml::from_str(&format!(
            r#"
            [server]
            bind = "127.0.0.1:0"
            [auth]
            managed_credentials_directory = "{}"
            [defaults]
            policy = "balanced"
            [persistence]
            path = "{}"
            [providers.audio]
            kind = "audio_worker"
            command = "python3"
            args = ["{}"]
            placement = "local"
            max_concurrency = 1
            max_queue = 1
            [providers.audio.capability_profile]
            version = 1
            protocol = "audio_worker"
            [profiles.balanced]
            order = ["cost"]
            [intents."speech.synthesize"]
            data_plane = "audio.speech"
            input_modalities = ["text"]
            output_modalities = ["audio"]
            required_features = ["built_in_voices"]
            default_quality_floor = "basic"
            [model_profiles.tts]
            family = "fake"
            [model_profiles.tts.ratings."speech.synthesize"]
            grade = "basic"
            status = "benchmarked"
            eval_profile = "fake-v1"
            score = 1.0
            [model_builds.tts]
            profile = "tts"
            model_id = "fake"
            input_modalities = ["text"]
            output_modalities = ["audio"]
            features = ["built_in_voices"]
            [deployments.tts]
            provider = "audio"
            build = "tts"
            estimated_cost_usd = 0.0
            supported_execution_modes = ["unary", "server_stream"]
            [apps.test]
            credential = {{ source = "managed" }}
            allowed_intents = ["speech.synthesize"]
            allowed_policies = ["balanced"]
            "#,
            credentials.display(),
            persistence.display(),
            script.display(),
        ))
        .unwrap();
        config.validate().unwrap();
        let runtime = Runtime::from_config(config).await.unwrap();
        let result = runtime
            .execute_speech_stream(
                "test",
                SpeechRequest {
                    model: "speech.synthesize".into(),
                    input: "hello".into(),
                    voice: Some("default".into()),
                    instructions: None,
                    language: None,
                    speed: 1.0,
                    response_format: SpeechFormat::Pcm,
                    execution_mode: ExecutionMode::ServerStream,
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        let job_id = result.job_id.clone();
        drop(result.stream);

        for _ in 0..50 {
            if runtime.snapshot(&job_id).await.unwrap().unwrap().state == JobState::Cancelled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("dropped speech body did not settle the Job as cancelled");
    }
}
