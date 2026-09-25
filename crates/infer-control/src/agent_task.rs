//! One admitted file task produces exactly one Agent Attempt.

use std::collections::BTreeSet;

use infer_core::{
    AGENT_TASK_CAPABILITY_CONTRACT, AgentTaskProvenance, AgentTaskRequest, AgentTaskResult,
    ExecutionMode, ExecutionRequirements, Modality, ProviderCapability, RequestConstraints,
};

use super::{JobPreparation, Runtime, RuntimeError, current_admitted_capability_contract};

impl Runtime {
    pub async fn execute_agent_task(
        &self,
        app_id: &str,
        request: AgentTaskRequest,
    ) -> Result<AgentTaskResult, RuntimeError> {
        self.authorize_agent_file_task(app_id)?;
        request.validate().map_err(|error| {
            RuntimeError::Provider(infer_provider::ProviderError::InvalidInput(error.into()))
        })?;
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &request.model,
                    constraints: RequestConstraints {
                        deadline_ms: Some(300_000),
                        ..RequestConstraints::default()
                    },
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([Modality::Text]),
                        execution_mode: ExecutionMode::Unary,
                        provider_capabilities: BTreeSet::from([ProviderCapability::AgentTask]),
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "agent",
                    expected_data_plane: "agent.task",
                    capability_contract: current_admitted_capability_contract(
                        AGENT_TASK_CAPABILITY_CONTRACT,
                    ),
                    durable_payload: None,
                },
            )
            .await?;

        let _resource = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        let attempt_number = self.start_vision_attempt(&prepared).await?;
        let provider = self.provider(&prepared.provider_id)?;
        let result = self
            .await_vision(
                &prepared,
                provider.execute_agent_task(request, &prepared.physical_model),
            )
            .await;
        let execution = match result {
            Ok(execution) => execution,
            Err(error) => {
                return Err(self
                    .complete_vision_error(&prepared, attempt_number, error)
                    .await?);
            }
        };
        self.complete_vision_success(&prepared, attempt_number)
            .await?;
        let build = &self.config.deployments[&prepared.deployment_id].build;
        Ok(AgentTaskResult {
            job_id: prepared.job_id,
            state: "completed".into(),
            answer: execution.answer,
            outputs: execution.outputs,
            provenance: AgentTaskProvenance {
                capability_contract: AGENT_TASK_CAPABILITY_CONTRACT.into(),
                provider: prepared.provider_id,
                deployment: prepared.deployment_id,
                model_build: build.clone(),
                attempt_number,
                codex_thread_id: execution.thread_id,
                codex_turn_id: execution.turn_id,
                sandbox_profile: execution.sandbox_profile,
                tool_policy: execution.tool_policy,
            },
        })
    }
}
