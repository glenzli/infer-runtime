//! Typed text retrieval orchestration through shared admission and Job state.

use std::{collections::BTreeSet, sync::Arc};

use infer_core::{
    ExecutionMode, ExecutionRequirements, Modality, RetrievalEmbeddingItem,
    RetrievalEmbeddingRequest, RetrievalEmbeddingResponse, RetrievalEmbeddingVector,
    RetrievalProvenance, RetrievalRerankRequest, RetrievalRerankResponse,
};
use infer_provider::{
    DynRetrievalExecutor, RetrievalBuildContract, RetrievalEmbeddingExecutionOutput,
    RetrievalEmbeddingKind, RetrievalRerankExecutionOutput,
};

use super::{JobPreparation, Runtime, RuntimeError, unix_time_ms};

impl Runtime {
    pub async fn execute_retrieval_embedding(
        self: &Arc<Self>,
        app_id: &str,
        request: RetrievalEmbeddingRequest,
        kind: RetrievalEmbeddingKind,
    ) -> Result<RetrievalEmbeddingResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let identities = request
            .inputs
            .iter()
            .map(|input| (input.id.clone(), input.source_revision.clone()))
            .collect::<Vec<_>>();
        let constraints = request.constraints()?;
        let data_plane = match kind {
            RetrievalEmbeddingKind::Query | RetrievalEmbeddingKind::Documents => "text.embedding",
        };
        let prepared = self
            .prepare_retrieval_job(app_id, &logical_model, constraints, data_plane)
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.retrieval_executor(&prepared.provider_id)?;
        let upstream = executor.embed(
            &prepared.physical_model,
            request,
            kind,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                if let Err(error) = validate_embeddings(&output, identities.len()) {
                    return Err(self
                        .complete_vision_error(&prepared, attempt_number, error)
                        .await?);
                }
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
        let space = output.provenance.embedding_space.clone().ok_or_else(|| {
            RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                "embedding Build omitted its space identity".into(),
            ))
        })?;
        let data = identities
            .into_iter()
            .zip(output.embeddings)
            .map(|((id, source_revision), values)| RetrievalEmbeddingItem {
                id,
                source_revision,
                embedding: RetrievalEmbeddingVector {
                    values,
                    dimensions: output.dimensions,
                    normalized: output.normalized,
                    distance_metric: "cosine".into(),
                    space: space.clone(),
                },
            })
            .collect();
        Ok(RetrievalEmbeddingResponse {
            id: prepared.job_id.clone(),
            object: data_plane.into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            data,
            provenance: retrieval_provenance(
                &prepared,
                output.provenance,
                output.instruction_revision,
            ),
        })
    }

    pub async fn execute_retrieval_rerank(
        self: &Arc<Self>,
        app_id: &str,
        request: RetrievalRerankRequest,
    ) -> Result<RetrievalRerankResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let query_revision = request.query.source_revision.clone();
        let available_results = request.candidates.len();
        let expected_results = request.top_n.unwrap_or(available_results);
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_retrieval_job(app_id, &logical_model, constraints, "text.rerank")
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.retrieval_executor(&prepared.provider_id)?;
        let upstream = executor.rerank(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let RetrievalRerankExecutionOutput {
            mut results,
            instruction_revision,
            score_semantics,
            provenance,
        } = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                if let Err(error) = validate_rerank(&output, expected_results, available_results) {
                    return Err(self
                        .complete_vision_error(&prepared, attempt_number, error)
                        .await?);
                }
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
        results.truncate(expected_results);
        Ok(RetrievalRerankResponse {
            id: prepared.job_id.clone(),
            object: "text.rerank".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            query_revision,
            results,
            score_semantics,
            provenance: retrieval_provenance(&prepared, provenance, Some(instruction_revision)),
        })
    }

    async fn prepare_retrieval_job(
        &self,
        app_id: &str,
        logical_model: &str,
        constraints: infer_core::RequestConstraints,
        expected_data_plane: &'static str,
    ) -> Result<super::PreparedRun, RuntimeError> {
        self.prepare_job(
            app_id,
            JobPreparation {
                logical_model,
                constraints,
                execution_requirements: ExecutionRequirements {
                    input_modalities: BTreeSet::from([Modality::Text]),
                    execution_mode: ExecutionMode::Unary,
                    ..ExecutionRequirements::default()
                },
                reasoning_effort: None,
                estimated_tokens: 0,
                id_prefix: "semantic",
                expected_data_plane,
                capability_contract: super::current_admitted_capability_contract(
                    match expected_data_plane {
                        "text.embedding" => "infer.text.embedding@20260812.1",
                        "text.rerank" => "infer.text.rerank@20260812.1",
                        _ => unreachable!("validated retrieval data plane"),
                    },
                ),
                durable_payload: None,
            },
        )
        .await
    }

    fn retrieval_executor(&self, id: &str) -> Result<DynRetrievalExecutor, RuntimeError> {
        self.retrieval_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }
}

fn validate_embeddings(
    output: &RetrievalEmbeddingExecutionOutput,
    expected_count: usize,
) -> Result<(), RuntimeError> {
    if !output.normalized
        || output.dimensions == 0
        || output.provenance.embedding_dimensions != Some(output.dimensions)
        || output.embeddings.len() != expected_count
        || output.embeddings.iter().any(|embedding| {
            embedding.len() != output.dimensions
                || embedding.iter().any(|value| !value.is_finite())
                || !unit_norm_within_tolerance(embedding)
        })
    {
        return Err(RuntimeError::Provider(
            infer_provider::ProviderError::Protocol(
                "retrieval worker returned an invalid embedding tensor".into(),
            ),
        ));
    }
    Ok(())
}

fn unit_norm_within_tolerance(embedding: &[f32]) -> bool {
    let squared_norm = embedding
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    (squared_norm.sqrt() - 1.0).abs() <= 1e-3
}

fn validate_rerank(
    output: &RetrievalRerankExecutionOutput,
    expected_results: usize,
    available_results: usize,
) -> Result<(), RuntimeError> {
    let mut identities = BTreeSet::new();
    let valid = output.results.len() >= expected_results
        && output.results.len() <= available_results
        && output.results.iter().enumerate().all(|(index, result)| {
            identities.insert(result.candidate_id.as_str())
                && result.rank == index + 1
                && result.score.is_finite()
                && (0.0..=1.0).contains(&result.score)
                && (index == 0 || output.results[index - 1].score >= result.score)
        });
    if !valid {
        return Err(RuntimeError::Provider(
            infer_provider::ProviderError::Protocol(
                "retrieval worker returned an invalid ranking".into(),
            ),
        ));
    }
    Ok(())
}

fn retrieval_provenance(
    prepared: &super::PreparedRun,
    provenance: RetrievalBuildContract,
    instruction_revision: Option<String>,
) -> RetrievalProvenance {
    RetrievalProvenance {
        job_id: prepared.job_id.clone(),
        provider: prepared.provider_id.clone(),
        deployment: prepared.deployment_id.clone(),
        model_build: provenance.model_build,
        model_revision: provenance.model_revision,
        artifact_sha256: provenance.artifact_sha256,
        tokenizer_identity: provenance.tokenizer_identity,
        instruction_revision,
        runtime: provenance.runtime,
        precision: provenance.precision,
        embedding_space: provenance.embedding_space,
    }
}
