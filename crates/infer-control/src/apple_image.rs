//! Apple image execution uses the same Job, ACL, deadline, quota and cancellation owner as other data planes.
use super::{JobPreparation, Runtime, RuntimeError};
use infer_core::*;
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;
impl Runtime {
    pub async fn execute_apple_image(
        self: &Arc<Self>,
        app_id: &str,
        request: AppleImageRequest,
        cancellation: CancellationToken,
    ) -> Result<AppleImageResponse, RuntimeError> {
        request.validate()?;
        let source_revision = request.parameters.source_revision.clone();
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &request.parameters.model,
                    constraints: RequestConstraints::from_metadata(&request.parameters.metadata)?,
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([Modality::Image]),
                        execution_mode: ExecutionMode::Unary,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "apple_image",
                    expected_data_plane: request.parameters.options.data_plane(),
                    capability_contract: super::current_admitted_capability_contract(
                        APPLE_IMAGE_CONTRACT,
                    ),
                    durable_payload: None,
                },
            )
            .await?;
        // A detached owner finishes the Job even if the HTTP waiter is dropped.
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            let execute = async {
                let runtime = &runtime;
                let _reservation = runtime.reserve_resource(&prepared).await?;
                let _permit = runtime.acquire(&prepared).await?;
                let attempt = runtime.start_vision_attempt(&prepared).await?;
                let provider = &runtime.config.providers[&prepared.provider_id];
                let output = if provider.kind == ProviderKind::TrustedNode {
                    let context = infer_provider::ProviderAttemptContext {
                        job_id: prepared.job_id.clone(),
                        attempt,
                        app_id: prepared.app_id.clone(),
                        intent: prepared.logical_model.clone(),
                        remaining: prepared
                            .deadline
                            .map(|d| d.saturating_duration_since(tokio::time::Instant::now()))
                            .unwrap_or(std::time::Duration::from_secs(120)),
                    };
                    runtime
                        .await_vision(
                            &prepared,
                            infer_provider::AppleImageExecutor::execute_on_node(
                                provider.node.clone().expect("validated node"),
                                context,
                                prepared.physical_model.clone(),
                                request,
                            ),
                        )
                        .await
                } else {
                    match runtime.apple_image_executors.get(&prepared.provider_id) {
                        Some(executor) => {
                            runtime
                                .await_vision(
                                    &prepared,
                                    executor.execute(
                                        &prepared.physical_model,
                                        request,
                                        prepared.cancellation.clone(),
                                    ),
                                )
                                .await
                        }
                        None => Err(RuntimeError::Provider(
                            infer_provider::ProviderError::Protocol(
                                "Apple image provider unavailable".into(),
                            ),
                        )),
                    }
                };
                // Cancellation must remain a terminal Job outcome even when the worker
                // notices it before await_vision's cancellation branch is selected.
                let output = if prepared
                    .deadline
                    .is_some_and(|d| tokio::time::Instant::now() >= d)
                {
                    Err(RuntimeError::DeadlineExpired)
                } else if prepared.cancellation.is_cancelled() {
                    Err(RuntimeError::Cancelled)
                } else {
                    output
                };
                match output {
                    Ok(output) => {
                        runtime.complete_vision_success(&prepared, attempt).await?;
                        Ok(output)
                    }
                    Err(error) => Err(runtime
                        .complete_vision_error(&prepared, attempt, error)
                        .await?),
                }
            };
            tokio::pin!(execute);
            let output = tokio::select! {
                biased;
                _=cancellation.cancelled()=>{
                    prepared.cancellation.cancel();
                    execute.await?
                },
                output=&mut execute=>output?,
            };
            Ok(AppleImageResponse {
                id: prepared.job_id.clone(),
                source_revision,
                provider: prepared.provider_id.clone(),
                deployment: prepared.deployment_id.clone(),
                model_build: runtime.config.deployments[&prepared.deployment_id]
                    .build
                    .clone(),
                result: output.result,
                provenance: output.provenance,
            })
        })
        .await
        .map_err(|_| {
            RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                "Apple task owner failed".into(),
            ))
        })?
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;
    fn fixture() -> Arc<Runtime> {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/apple-image.example.toml")).unwrap();
        let mut runtime = Runtime::with_providers(config, BTreeMap::new());
        let worker = infer_provider::AppleImageExecutor::new(
            "/bin/sh".into(),
            vec!["-c".into(), "cat >/dev/null; exec sleep 60".into()],
        )
        .unwrap();
        Arc::get_mut(&mut runtime)
            .unwrap()
            .apple_image_executors
            .insert("apple".into(), Arc::new(worker));
        runtime
    }
    fn request() -> AppleImageRequest {
        AppleImageRequest {
            parameters: AppleImageParameters {
                model: "apple.ocr".into(),
                source_revision: "r1".into(),
                options: AppleImageOperation::Ocr {},
                metadata: BTreeMap::from([
                    ("infer.placement".into(), "local_only".into()),
                    ("infer.offline_required".into(), "true".into()),
                    ("infer.fallback".into(), "none".into()),
                ]),
            },
            bytes: vec![1],
        }
    }
    #[tokio::test]
    async fn disconnected_waiter_still_finishes_cancelled_job() {
        let runtime = fixture();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let token = cancellation.clone();
            async move {
                runtime
                    .execute_apple_image("apple-example", request(), token)
                    .await
            }
        });
        let id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(id) = runtime
                    .jobs
                    .lock()
                    .await
                    .values()
                    .find(|j| j.snapshot.state == JobState::Running)
                    .map(|j| j.snapshot.id.clone())
                {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
        task.abort();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if runtime.snapshot(&id).await.unwrap().unwrap().state == JobState::Cancelled {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn native_deadline_finishes_expired_job() {
        let runtime = fixture();
        let mut request = request();
        request
            .parameters
            .metadata
            .insert("infer.deadline_ms".into(), "100".into());
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            runtime.execute_apple_image("apple-example", request, CancellationToken::new()),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(RuntimeError::DeadlineExpired)));
        assert!(
            runtime
                .jobs
                .lock()
                .await
                .values()
                .all(|j| j.snapshot.state == JobState::Expired)
        );
    }
}
