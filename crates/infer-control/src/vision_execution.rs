//! Typed vision execution orchestration.
//!
//! Provider adapters own image/tensor semantics. This module owns the shared
//! Job/Attempt, deadline, cancellation, scheduling, reservation, and
//! provenance envelope for synchronous vision data planes.

use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};

use infer_core::{
    AttemptOutcome, AttemptTrigger, ExecutionMode, ExecutionRequirements, FaceDetectionRequest,
    FaceDetectionResponse, FaceEmbeddingRequest, FaceEmbeddingResponse, ImageEmbeddingRequest,
    ImageEmbeddingResponse, JobState, Modality, SENSITIVE_BIOMETRIC_CLASSIFICATION,
    TextEmbeddingRequest, TextEmbeddingResponse, VisionProvenance, VisionTokenizerProvenance,
};
use infer_provider::{
    DynFaceDetectionExecutor, DynFaceEmbeddingExecutor, DynImageEmbeddingExecutor,
    DynTextEmbeddingExecutor, OnnxExecutionProvenance, ProviderError,
};
use tokio::time::{sleep_until, timeout};

use super::{JobPreparation, PreparedRun, Runtime, RuntimeError, attempt_policy, unix_time_ms};

impl Runtime {
    pub async fn execute_face_detection(
        self: &Arc<Self>,
        app_id: &str,
        request: FaceDetectionRequest,
    ) -> Result<FaceDetectionResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_vision_job(app_id, &logical_model, constraints, "vision.face_detection")
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.face_detection_executor(&prepared.provider_id)?;
        let upstream = executor.detect_faces(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                self.complete_vision_success(&prepared, attempt_number)
                    .await?;
                output
            }
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt_number, error)
                    .await?);
            }
        };

        Ok(FaceDetectionResponse {
            id: prepared.job_id.clone(),
            object: "vision.face_detection".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            image: output.image,
            detections: output.detections,
            provenance: vision_provenance(&prepared, output.provenance),
        })
    }

    pub async fn execute_face_embedding(
        self: &Arc<Self>,
        app_id: &str,
        request: FaceEmbeddingRequest,
    ) -> Result<FaceEmbeddingResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_vision_job(app_id, &logical_model, constraints, "vision.face_embedding")
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.face_embedding_executor(&prepared.provider_id)?;
        let upstream = executor.embed_face(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                self.complete_vision_success(&prepared, attempt_number)
                    .await?;
                output
            }
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt_number, error)
                    .await?);
            }
        };

        Ok(FaceEmbeddingResponse {
            id: prepared.job_id.clone(),
            object: "vision.face_embedding".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            data_classification: SENSITIVE_BIOMETRIC_CLASSIFICATION.into(),
            embedding: output.embedding,
            eligibility: output.eligibility,
            provenance: vision_provenance(&prepared, output.provenance),
        })
    }

    pub async fn execute_image_embedding(
        self: &Arc<Self>,
        app_id: &str,
        request: ImageEmbeddingRequest,
    ) -> Result<ImageEmbeddingResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_vision_job(
                app_id,
                &logical_model,
                constraints,
                "vision.image_embedding",
            )
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.image_embedding_executor(&prepared.provider_id)?;
        let upstream = executor.embed_image(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                self.complete_vision_success(&prepared, attempt_number)
                    .await?;
                output
            }
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt_number, error)
                    .await?);
            }
        };

        Ok(ImageEmbeddingResponse {
            id: prepared.job_id.clone(),
            object: "vision.image_embedding".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            image: output.image,
            embedding: output.embedding,
            provenance: vision_provenance(&prepared, output.provenance),
        })
    }

    pub async fn execute_text_embedding(
        self: &Arc<Self>,
        app_id: &str,
        request: TextEmbeddingRequest,
    ) -> Result<TextEmbeddingResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let query_revision = request.query_revision.clone();
        let language = request.language.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_vision_job(app_id, &logical_model, constraints, "vision.text_embedding")
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.text_embedding_executor(&prepared.provider_id)?;
        let upstream = executor.embed_text(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                self.complete_vision_success(&prepared, attempt_number)
                    .await?;
                output
            }
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt_number, error)
                    .await?);
            }
        };

        Ok(TextEmbeddingResponse {
            id: prepared.job_id.clone(),
            object: "vision.text_embedding".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            query_revision,
            language,
            embedding: output.embedding,
            provenance: vision_provenance(&prepared, output.provenance),
        })
    }

    async fn prepare_vision_job(
        &self,
        app_id: &str,
        logical_model: &str,
        constraints: infer_core::RequestConstraints,
        expected_data_plane: &'static str,
    ) -> Result<PreparedRun, RuntimeError> {
        self.prepare_job(
            app_id,
            JobPreparation {
                logical_model,
                constraints,
                execution_requirements: ExecutionRequirements {
                    input_modalities: BTreeSet::from([
                        if expected_data_plane == "vision.text_embedding" {
                            Modality::Text
                        } else {
                            Modality::Image
                        },
                    ]),
                    execution_mode: ExecutionMode::Unary,
                    ..ExecutionRequirements::default()
                },
                reasoning_effort: None,
                estimated_tokens: 0,
                id_prefix: "vision",
                expected_data_plane,
                capability_contract: super::current_admitted_capability_contract(
                    match expected_data_plane {
                        "vision.face_detection" => "infer.vision.face-detection@20260811.1",
                        "vision.face_embedding" => "infer.vision.face-embedding@20260811.1",
                        "vision.image_embedding" => "infer.vision.image-embedding@20260811.1",
                        "vision.text_embedding" => "infer.vision.text-embedding@20260811.1",
                        _ => unreachable!("validated vision data plane"),
                    },
                ),
                durable_payload: None,
            },
        )
        .await
    }

    pub(super) async fn start_vision_attempt(
        &self,
        prepared: &PreparedRun,
    ) -> Result<usize, RuntimeError> {
        self.mark(&prepared.job_id, JobState::Running, None).await?;
        self.begin_attempt(prepared, AttemptTrigger::Initial).await
    }

    pub(super) async fn await_vision<T>(
        &self,
        prepared: &PreparedRun,
        upstream: impl Future<Output = Result<T, ProviderError>>,
    ) -> Result<T, RuntimeError> {
        tokio::pin!(upstream);
        match prepared.deadline {
            Some(deadline) => tokio::select! {
                result = &mut upstream => result.map_err(RuntimeError::Provider),
                _ = prepared.cancellation.cancelled() => {
                    // Poll the provider future after signalling cancellation so
                    // its native RunOptions termination path can run. Cleanup
                    // remains bounded; a later native result is never published.
                    let _ = timeout(Duration::from_secs(2), &mut upstream).await;
                    Err(RuntimeError::Cancelled)
                }
                _ = sleep_until(deadline) => {
                    prepared.cancellation.cancel();
                    let _ = timeout(Duration::from_secs(2), &mut upstream).await;
                    Err(RuntimeError::DeadlineExpired)
                }
            },
            None => tokio::select! {
                result = &mut upstream => result.map_err(RuntimeError::Provider),
                _ = prepared.cancellation.cancelled() => {
                    let _ = timeout(Duration::from_secs(2), &mut upstream).await;
                    Err(RuntimeError::Cancelled)
                }
            },
        }
    }

    pub(super) async fn complete_vision_success(
        &self,
        prepared: &PreparedRun,
        attempt_number: usize,
    ) -> Result<(), RuntimeError> {
        self.health.record_success(&prepared.provider_id);
        self.finish_attempt(
            prepared,
            attempt_number,
            AttemptOutcome::Succeeded,
            None,
            None,
            None,
        )
        .await?;
        self.mark(&prepared.job_id, JobState::Succeeded, None)
            .await?;
        self.metrics.succeeded();
        Ok(())
    }

    pub(super) async fn complete_vision_error(
        &self,
        prepared: &PreparedRun,
        attempt_number: usize,
        error: RuntimeError,
    ) -> Result<RuntimeError, RuntimeError> {
        match &error {
            RuntimeError::Cancelled => {
                self.finish_attempt(
                    prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("cancelled".into()),
                    Some("vision execution was cancelled".into()),
                    None,
                )
                .await?;
                self.mark(&prepared.job_id, JobState::Cancelled, None)
                    .await?;
                self.metrics.cancelled();
            }
            RuntimeError::DeadlineExpired => {
                prepared.cancellation.cancel();
                self.finish_attempt(
                    prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("deadline_exceeded".into()),
                    Some("request deadline expired during vision execution".into()),
                    None,
                )
                .await?;
                self.mark(
                    &prepared.job_id,
                    JobState::Expired,
                    Some("request deadline expired during vision execution".into()),
                )
                .await?;
                self.metrics.expired();
            }
            RuntimeError::Provider(provider) => {
                self.health.record_failure(&prepared.provider_id, provider);
                self.finish_attempt(
                    prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some(attempt_policy::kind_code(provider.kind()).into()),
                    Some(provider.public_message().into()),
                    None,
                )
                .await?;
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some(error.public_message()),
                )
                .await?;
                self.metrics.failed();
            }
            _ => {
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some(error.public_message()),
                )
                .await?;
                self.metrics.failed();
            }
        }
        Ok(error)
    }

    fn face_detection_executor(&self, id: &str) -> Result<DynFaceDetectionExecutor, RuntimeError> {
        self.face_detection_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }

    fn face_embedding_executor(&self, id: &str) -> Result<DynFaceEmbeddingExecutor, RuntimeError> {
        self.face_embedding_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }

    fn image_embedding_executor(
        &self,
        id: &str,
    ) -> Result<DynImageEmbeddingExecutor, RuntimeError> {
        self.image_embedding_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }

    fn text_embedding_executor(&self, id: &str) -> Result<DynTextEmbeddingExecutor, RuntimeError> {
        self.text_embedding_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }
}

fn vision_provenance(
    prepared: &PreparedRun,
    provenance: OnnxExecutionProvenance,
) -> VisionProvenance {
    VisionProvenance {
        job_id: prepared.job_id.clone(),
        provider: prepared.provider_id.clone(),
        deployment: prepared.deployment_id.clone(),
        model_build: provenance.model_build,
        artifact_sha256: provenance.artifact_sha256,
        preprocessing_identity: provenance.preprocessing_identity,
        postprocessing_identity: provenance.postprocessing_identity,
        tokenizer: provenance
            .tokenizer
            .map(|tokenizer| VisionTokenizerProvenance {
                identity: tokenizer.identity,
                artifact_sha256: tokenizer.artifact_sha256,
                max_length: tokenizer.max_length,
                lowercase: tokenizer.lowercase,
            }),
        runtime: provenance.runtime,
        requested_execution_provider: provenance.requested_execution_provider,
        actual_execution_provider: provenance.actual_execution_provider,
        execution_provider_fallback_reason: provenance.execution_provider_fallback_reason,
        precision: provenance.precision,
    }
}
