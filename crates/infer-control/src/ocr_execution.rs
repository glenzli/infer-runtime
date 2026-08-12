//! Typed document OCR orchestration through shared admission and Job state.

use std::{collections::BTreeSet, sync::Arc};

use infer_core::{
    DocumentOcrRequest, DocumentOcrResponse, ExecutionMode, ExecutionRequirements, Modality,
    OcrProvenance,
};
use infer_provider::{DynOcrExecutor, OcrBuildContract};

use super::{JobPreparation, Runtime, RuntimeError, unix_time_ms};

impl Runtime {
    pub async fn execute_document_ocr(
        self: &Arc<Self>,
        app_id: &str,
        request: DocumentOcrRequest,
    ) -> Result<DocumentOcrResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &logical_model,
                    constraints,
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([Modality::Image]),
                        execution_mode: ExecutionMode::Unary,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "ocr",
                    expected_data_plane: "document.ocr",
                    capability_contract: super::current_admitted_capability_contract(
                        "infer.document.ocr@20260812.1",
                    ),
                    durable_payload: None,
                },
            )
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.ocr_executor(&prepared.provider_id)?;
        let upstream = executor.recognize(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
        let output = match self.await_vision(&prepared, upstream).await {
            Ok(output) => {
                if let Err(error) = validate_ocr_output(&output) {
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
        Ok(DocumentOcrResponse {
            id: prepared.job_id.clone(),
            object: "document.ocr".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            image: output.image,
            lines: output.lines,
            provenance: ocr_provenance(&prepared, output.provenance),
        })
    }

    fn ocr_executor(&self, id: &str) -> Result<DynOcrExecutor, RuntimeError> {
        self.ocr_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }
}

fn validate_ocr_output(output: &infer_provider::OcrExecutionOutput) -> Result<(), RuntimeError> {
    let width = output.image.width as f32;
    let height = output.image.height as f32;
    let total_text_bytes = output
        .lines
        .iter()
        .map(|line| line.text.len())
        .sum::<usize>();
    let valid = output.image.width > 0
        && output.image.height > 0
        && output.lines.len() <= infer_core::MAX_OCR_LINES
        && total_text_bytes <= infer_core::MAX_OCR_RESULT_TEXT_BYTES
        && output.image.orientation == infer_core::VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS
        && output.lines.iter().all(|line| {
            line.confidence.is_finite()
                && (0.0..=1.0).contains(&line.confidence)
                && !line.text.is_empty()
                && line.polygon.iter().all(|point| {
                    point.x.is_finite()
                        && point.y.is_finite()
                        && (0.0..=width).contains(&point.x)
                        && (0.0..=height).contains(&point.y)
                })
        });
    if !valid {
        return Err(RuntimeError::Provider(
            infer_provider::ProviderError::Protocol(
                "OCR worker returned invalid geometry or confidence".into(),
            ),
        ));
    }
    Ok(())
}

fn ocr_provenance(prepared: &super::PreparedRun, provenance: OcrBuildContract) -> OcrProvenance {
    OcrProvenance {
        job_id: prepared.job_id.clone(),
        provider: prepared.provider_id.clone(),
        deployment: prepared.deployment_id.clone(),
        model_build: provenance.model_build,
        detection_revision: provenance.detection_revision,
        detection_artifact_sha256: provenance.detection_artifact_sha256,
        recognition_revision: provenance.recognition_revision,
        recognition_artifact_sha256: provenance.recognition_artifact_sha256,
        preprocessing_identity: provenance.preprocessing_identity,
        postprocessing_identity: provenance.postprocessing_identity,
        runtime: provenance.runtime,
        requested_execution_provider: provenance.requested_execution_provider,
        actual_execution_provider: provenance.actual_execution_provider,
        precision: provenance.precision,
    }
}
