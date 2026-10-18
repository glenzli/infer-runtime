//! Shared admission and payload-free jobs for the unified media retrieval space.

use super::{JobPreparation, Runtime, RuntimeError, unix_time_ms};
use infer_core::{
    AttemptOutcome, ExecutionMode, ExecutionRequirements, MEDIA_EMBEDDING_CONTRACT,
    MediaEmbeddingKind, MediaEmbeddingProvenance, MediaEmbeddingRequest, MediaEmbeddingResponse,
};
use infer_provider::{DynMediaEmbeddingExecutor, MediaEmbeddingExecutionOutput, ProviderError};
use std::{collections::BTreeSet, sync::Arc};

impl Runtime {
    pub fn with_media_embedding_executors(
        mut self: Arc<Self>,
        executors: std::collections::BTreeMap<String, DynMediaEmbeddingExecutor>,
    ) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("test runtime has no shared owners")
            .media_embedding_executors = executors;
        self
    }

    pub async fn execute_media_embedding(
        self: &Arc<Self>,
        app_id: &str,
        request: MediaEmbeddingRequest,
    ) -> Result<MediaEmbeddingResponse, RuntimeError> {
        request.validate()?;
        let model = request.model.clone();
        let input_kind = request.kind();
        let input_revision = request.input_revision.clone();
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &model,
                    constraints: request.constraints()?,
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([input_kind.modality()]),
                        execution_mode: ExecutionMode::Unary,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "media",
                    expected_data_plane: "media.embedding",
                    capability_contract: super::current_admitted_capability_contract(
                        MEDIA_EMBEDDING_CONTRACT,
                    ),
                    durable_payload: None,
                },
            )
            .await?;
        let _reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt = self.start_vision_attempt(&prepared).await?;
        let result = match self.media_embedding_executors.get(&prepared.provider_id) {
            Some(executor) => {
                let upstream = executor.embed_media(
                    &prepared.physical_model,
                    request,
                    prepared.cancellation.clone(),
                );
                self.await_vision(&prepared, upstream).await
            }
            None => {
                self.finish_attempt(
                    &prepared,
                    attempt,
                    AttemptOutcome::Failed,
                    Some("unavailable".into()),
                    Some("media executor is unavailable".into()),
                    None,
                )
                .await?;
                Err(RuntimeError::ProviderUnavailable(
                    prepared.provider_id.clone(),
                ))
            }
        };
        let output = match result {
            Ok(output) => {
                if let Err(error) = validate_output(&output, input_kind) {
                    return Err(self
                        .complete_vision_error(&prepared, attempt, error)
                        .await?);
                }
                self.complete_vision_success(&prepared, attempt).await?;
                output
            }
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt, error)
                    .await?);
            }
        };
        let build = output.provenance;
        Ok(MediaEmbeddingResponse {
            id: prepared.job_id.clone(),
            object: "media.embedding".into(),
            created_at: (unix_time_ms() / 1000).try_into().unwrap_or_default(),
            model,
            input_kind,
            input_revision,
            embedding: output.embedding,
            embedding_space: build.embedding_space,
            processing: output.processing,
            provenance: MediaEmbeddingProvenance {
                job_id: prepared.job_id,
                provider: prepared.provider_id,
                deployment: prepared.deployment_id,
                model_build: build.model_build,
                model_revision: build.model_revision,
                artifact_set_sha256: build.artifact_set_sha256,
                runtime: build.runtime,
                precision: build.precision,
                preprocessing_identity: build.preprocessing_identity,
                postprocessing_identity: build.postprocessing_identity,
                tokenizer_identity: build.tokenizer_identity,
                instruction_revision: build.instruction_revision,
            },
        })
    }
}

fn validate_output(
    output: &MediaEmbeddingExecutionOutput,
    kind: MediaEmbeddingKind,
) -> Result<(), RuntimeError> {
    let space = &output.provenance.embedding_space;
    let norm = output
        .embedding
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let processing = &output.processing;
    let duration_valid = |bound: f64| {
        processing
            .decoded_seconds
            .is_some_and(|seconds| seconds.is_finite() && seconds > 0.0 && seconds <= bound + 1e-6)
    };
    let shape_valid = match kind {
        MediaEmbeddingKind::Text | MediaEmbeddingKind::Image => {
            processing.decoded_seconds.is_none() && processing.video_frames.is_none()
        }
        MediaEmbeddingKind::Audio => duration_valid(30.0) && processing.video_frames.is_none(),
        MediaEmbeddingKind::Video => {
            duration_valid(16.0)
                && processing
                    .video_frames
                    .is_some_and(|frames| (1..=16).contains(&frames))
        }
    };
    if space.dimensions != 768
        || !space.normalized
        || space.distance_metric != "cosine"
        || space.identity.trim().is_empty()
        || output.embedding.len() != 768
        || output.embedding.iter().any(|value| !value.is_finite())
        || !norm.is_finite()
        || (norm - 1.0).abs() > 1e-4
        || !(1..=8192).contains(&processing.input_tokens)
        || processing.video_audio_included
        || !shape_valid
    {
        return Err(RuntimeError::Provider(ProviderError::Protocol(
            "media_worker_output_invalid".into(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use infer_auth::AppCredentials;
    use infer_core::{AttemptOutcome, JobState, MediaEmbeddingInput, RuntimeConfig};
    use infer_store::{ConfigSnapshot, Store};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn missing_media_executor_persists_terminal_job_and_attempt() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::new(),
            Some(store.clone()),
            AppCredentials::empty(),
        );
        let request = MediaEmbeddingRequest {
            model: "semantic.media_query".into(),
            input: MediaEmbeddingInput::Query("private query".into()),
            input_revision: "q1".into(),
            metadata: BTreeMap::from([
                ("infer.placement".into(), "local_only".into()),
                ("infer.offline_required".into(), "true".into()),
                ("infer.fallback".into(), "none".into()),
            ]),
        };
        assert!(matches!(
            runtime
                .execute_media_embedding("local-operator", request)
                .await,
            Err(RuntimeError::ProviderUnavailable(_))
        ));
        let jobs = store
            .job_page("local-operator", None, None, None, 10)
            .unwrap();
        assert_eq!(jobs.jobs.len(), 1);
        let snapshot = store.load_job(&jobs.jobs[0].id).unwrap().unwrap();
        assert_eq!(snapshot.state, JobState::Failed);
        assert_eq!(snapshot.attempts.len(), 1);
        assert_eq!(snapshot.attempts[0].outcome, AttemptOutcome::Failed);
        assert_eq!(
            snapshot.attempts[0].error_kind.as_deref(),
            Some("unavailable")
        );
        assert!(store.active_reservations().unwrap().is_empty());
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("private query"));
    }
}
