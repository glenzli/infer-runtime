//! Typed orchestration for bounded generative image understanding.
//!
//! This module reuses the common Job/Attempt scheduler and lifecycle owner,
//! while the provider adapter exclusively owns prompt construction, native
//! multimodal wire details, and structured-output parsing.

use std::{collections::BTreeSet, sync::Arc};

use infer_core::{
    ClassificationReviewRequest, ClassificationReviewResponse, ExecutionMode,
    ExecutionRequirements, ImageDescriptionRequest, ImageDescriptionResponse,
    ImageUnderstandingProvenance, Modality,
};
use infer_provider::{DynImageUnderstandingExecutor, OllamaVisionProvenance};

use super::{JobPreparation, PreparedRun, Runtime, RuntimeError, unix_time_ms};

impl Runtime {
    pub async fn execute_image_description(
        self: &Arc<Self>,
        app_id: &str,
        request: ImageDescriptionRequest,
    ) -> Result<ImageDescriptionResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let language = request.language.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_image_understanding_job(
                app_id,
                &logical_model,
                constraints,
                "vision.image_description",
            )
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.image_understanding_executor(&prepared.provider_id)?;
        let upstream = executor.describe_image(
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

        Ok(ImageDescriptionResponse {
            id: prepared.job_id.clone(),
            object: "vision.image_description".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            language,
            image: output.image,
            result: output.result,
            provenance: self.image_understanding_provenance(&prepared, output.provenance),
        })
    }

    pub async fn execute_classification_review(
        self: &Arc<Self>,
        app_id: &str,
        request: ClassificationReviewRequest,
    ) -> Result<ClassificationReviewResponse, RuntimeError> {
        request.validate()?;
        let logical_model = request.model.clone();
        let source_revision = request.source_revision.clone();
        let taxonomy_revision = request.taxonomy_revision.clone();
        let constraints = request.constraints()?;
        let prepared = self
            .prepare_image_understanding_job(
                app_id,
                &logical_model,
                constraints,
                "vision.classification_review",
            )
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let executor = self.image_understanding_executor(&prepared.provider_id)?;
        let upstream = executor.review_classification(
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

        Ok(ClassificationReviewResponse {
            id: prepared.job_id.clone(),
            object: "vision.classification_review".into(),
            created_at: (unix_time_ms() / 1_000).try_into().unwrap_or_default(),
            status: "completed".into(),
            source_revision,
            taxonomy_revision,
            image: output.image,
            suggestion: output.suggestion,
            provenance: self.image_understanding_provenance(&prepared, output.provenance),
        })
    }

    async fn prepare_image_understanding_job(
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
                    input_modalities: BTreeSet::from([Modality::Image]),
                    execution_mode: ExecutionMode::Unary,
                    ..ExecutionRequirements::default()
                },
                reasoning_effort: None,
                estimated_tokens: 0,
                id_prefix: "vision",
                expected_data_plane,
                durable_payload: None,
            },
        )
        .await
    }

    fn image_understanding_executor(
        &self,
        id: &str,
    ) -> Result<DynImageUnderstandingExecutor, RuntimeError> {
        self.image_understanding_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }

    fn image_understanding_provenance(
        &self,
        prepared: &PreparedRun,
        native: OllamaVisionProvenance,
    ) -> ImageUnderstandingProvenance {
        let deployment = &self.config.deployments[&prepared.deployment_id];
        let build = &self.config.model_builds[&deployment.build];
        ImageUnderstandingProvenance {
            job_id: prepared.job_id.clone(),
            provider: prepared.provider_id.clone(),
            deployment: prepared.deployment_id.clone(),
            model_profile: build.profile.clone(),
            model_build: deployment.build.clone(),
            physical_model: prepared.physical_model.clone(),
            runtime: native.runtime,
            schema_revision: native.schema_revision,
            prompt_revision: native.prompt_revision,
            total_duration_ms: native.total_duration_ms,
            load_duration_ms: native.load_duration_ms,
            prompt_eval_count: native.prompt_eval_count,
            eval_count: native.eval_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use async_trait::async_trait;
    use infer_auth::AppCredentials;
    use infer_core::{
        ClassificationCategory, ClassificationDisposition, ClassificationSuggestion,
        ImageDescriptionResult, ImageGeometry, RuntimeConfig,
        VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS, VisionImage,
    };
    use infer_provider::{
        ClassificationReviewExecutionOutput, ImageDescriptionExecutionOutput,
        ImageUnderstandingExecutor, OllamaVisionProvenance, ProviderError,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        models: Mutex<Vec<String>>,
    }

    impl FakeExecutor {
        fn provenance(schema_revision: &str, prompt_revision: &str) -> OllamaVisionProvenance {
            OllamaVisionProvenance {
                runtime: "fake_ollama".into(),
                schema_revision: schema_revision.into(),
                prompt_revision: prompt_revision.into(),
                total_duration_ms: Some(1),
                load_duration_ms: None,
                prompt_eval_count: None,
                eval_count: None,
            }
        }
    }

    #[async_trait]
    impl ImageUnderstandingExecutor for FakeExecutor {
        fn id(&self) -> &str {
            "ollama-local"
        }

        async fn describe_image(
            &self,
            physical_model: &str,
            _request: ImageDescriptionRequest,
            _cancellation: CancellationToken,
        ) -> Result<ImageDescriptionExecutionOutput, ProviderError> {
            self.models.lock().unwrap().push(physical_model.into());
            Ok(ImageDescriptionExecutionOutput {
                image: ImageGeometry {
                    width: 2,
                    height: 2,
                    orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                },
                result: ImageDescriptionResult {
                    description: "private-result-marker".into(),
                    keyword_suggestions: vec!["private-keyword-marker".into()],
                },
                provenance: Self::provenance("schema:description", "prompt:description"),
            })
        }

        async fn review_classification(
            &self,
            physical_model: &str,
            request: ClassificationReviewRequest,
            _cancellation: CancellationToken,
        ) -> Result<ClassificationReviewExecutionOutput, ProviderError> {
            self.models.lock().unwrap().push(physical_model.into());
            Ok(ClassificationReviewExecutionOutput {
                image: ImageGeometry {
                    width: 2,
                    height: 2,
                    orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                },
                suggestion: ClassificationSuggestion {
                    disposition: ClassificationDisposition::Matched,
                    category_id: Some(request.categories[0].id.clone()),
                },
                provenance: Self::provenance("schema:classification", "prompt:classification"),
            })
        }
    }

    fn config(allowed_intents: &[&str]) -> RuntimeConfig {
        let mut config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let app = config
            .apps
            .get_mut("example-local-consumer")
            .expect("example app");
        app.allowed_intents = Some(
            allowed_intents
                .iter()
                .map(|intent| (*intent).to_owned())
                .collect(),
        );
        config.validate().unwrap();
        config
    }

    fn runtime(allowed_intents: &[&str]) -> (Arc<Runtime>, Arc<FakeExecutor>) {
        let executor = Arc::new(FakeExecutor::default());
        let executors = BTreeMap::from([(
            "ollama-local".into(),
            Arc::clone(&executor) as DynImageUnderstandingExecutor,
        )]);
        (
            Runtime::with_image_understanding_executors(
                config(allowed_intents),
                BTreeMap::new(),
                AppCredentials::empty(),
                executors,
            ),
            executor,
        )
    }

    fn metadata(capability: Option<&str>) -> BTreeMap<String, String> {
        let mut metadata = BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ]);
        if let Some(capability) = capability {
            metadata.insert("infer.capability_floor".into(), capability.into());
        }
        metadata
    }

    fn description_request(capability: Option<&str>) -> ImageDescriptionRequest {
        ImageDescriptionRequest {
            model: "vision.describe_image".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: b"private-image-marker".to_vec(),
            },
            source_revision: "photo:1".into(),
            image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            language: "zh-CN".into(),
            metadata: metadata(capability),
        }
    }

    fn classification_request() -> ClassificationReviewRequest {
        ClassificationReviewRequest {
            model: "vision.classify_closed_set".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: b"private-classification-image".to_vec(),
            },
            source_revision: "photo:2".into(),
            image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            taxonomy_revision: "taxonomy:2".into(),
            categories: vec![ClassificationCategory {
                id: "travel".into(),
                name: "旅行".into(),
                description: Some("旅行或度假照片".into()),
            }],
            metadata: metadata(None),
        }
    }

    #[tokio::test]
    async fn capability_floor_selects_4b_for_bulk_and_8b_for_explicit_capability() {
        let (runtime, executor) = runtime(&["vision.describe_image"]);
        let foundational = runtime
            .execute_image_description(
                "example-local-consumer",
                description_request(Some("foundational")),
            )
            .await
            .unwrap();
        assert_eq!(foundational.provenance.model_build, "qwen3_vl_4b");
        let capable = runtime
            .execute_image_description(
                "example-local-consumer",
                description_request(Some("capable")),
            )
            .await
            .unwrap();
        assert_eq!(capable.provenance.model_build, "qwen3_vl_8b");
        assert_eq!(
            *executor.models.lock().unwrap(),
            ["qwen3-vl:4b", "qwen3-vl:8b"]
        );

        let snapshot = runtime.snapshot(&foundational.id).await.unwrap().unwrap();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for private in [
            "private-image-marker",
            "private-result-marker",
            "private-keyword-marker",
        ] {
            assert!(!serialized.contains(private));
        }
    }

    #[tokio::test]
    async fn classification_review_defaults_to_capable_8b_and_stays_acl_scoped() {
        let (runtime, _) = runtime(&["vision.classify_closed_set"]);
        let response = runtime
            .execute_classification_review("example-local-consumer", classification_request())
            .await
            .unwrap();
        assert_eq!(response.provenance.model_build, "qwen3_vl_8b");
        assert_eq!(response.suggestion.category_id.as_deref(), Some("travel"));

        let denied = runtime
            .execute_image_description(
                "example-local-consumer",
                description_request(Some("foundational")),
            )
            .await;
        assert!(matches!(denied, Err(RuntimeError::IntentNotAllowed { .. })));
    }
}
