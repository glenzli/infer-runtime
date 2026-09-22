//! Destination node composition. Remote work shares this Runtime's admission,
//! resource manager, local schedulers and cancellation authority.
use crate::{CompletionMode, JobPreparation, Runtime, RuntimeError, estimate_response_tokens};
use infer_core::{ResponsesRequest, RuntimeConfig};
use infer_node::{NodeError, NodeExecutor, NodeServer, Offer};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

struct RuntimeNodeExecutor {
    runtime: Arc<Runtime>,
    exports: BTreeMap<String, infer_core::NodeExportConfig>,
}

impl Runtime {
    pub async fn start_node_server(self: &Arc<Self>) -> Result<Option<NodeServer>, NodeError> {
        let Some(config) = self.config.node_server.clone() else {
            return Ok(None);
        };
        let offers = node_offers(&self.config)?;
        let executor = Arc::new(RuntimeNodeExecutor {
            runtime: Arc::clone(self),
            exports: config.exports.clone(),
        });
        NodeServer::start(config, offers, executor).await.map(Some)
    }
}

pub fn node_offers(config: &RuntimeConfig) -> Result<Vec<Offer>, NodeError> {
    let Some(node) = &config.node_server else {
        return Ok(vec![]);
    };
    node.exports
        .iter()
        .map(|(id, export)| {
            let deployment = &config.deployments[&export.deployment];
            let build = &config.model_builds[&deployment.build];
            let profile = &config.model_profiles[&build.profile];
            let intent = &config.intents[&export.intent];
            let capabilities = &config.providers[&deployment.provider].capability_profile;
            Ok(Offer {
                deployment_id: id.clone(),
                intent: export.intent.clone(),
                build_id: deployment.build.clone(),
                contract_digest: infer_node::contract_digest(&(
                    export,
                    deployment,
                    build,
                    profile,
                    intent,
                    capabilities,
                ))?,
                resource_estimate: deployment.resource_estimate.clone(),
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl NodeExecutor for RuntimeNodeExecutor {
    async fn execute(
        &self,
        app: &str,
        export: &str,
        mut request: ResponsesRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, NodeError> {
        let export = self.exports.get(export).ok_or(NodeError::Forbidden)?;
        if request.model != export.intent {
            return Err(NodeError::Protocol);
        }
        request.metadata.clear();
        request.metadata.extend([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
            ("infer.deployment_ids".into(), export.deployment.clone()),
        ]);
        let request = self
            .runtime
            .prepare_responses_request(request)
            .map_err(map_runtime)?;
        let prepared = self
            .runtime
            .prepare_job(
                app,
                JobPreparation {
                    logical_model: &request.model,
                    constraints: request.constraints().map_err(|_| NodeError::Protocol)?,
                    execution_requirements: request.execution_requirements(),
                    reasoning_effort: request.reasoning_effort(),
                    estimated_tokens: estimate_response_tokens(&request),
                    id_prefix: "node_resp",
                    expected_data_plane: "responses",
                    capability_contract: "infer.responses@20260812.1",
                    durable_payload: None,
                },
            )
            .await
            .map_err(map_runtime)?;
        let job_id = prepared.job_id.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                let _ = self.runtime.cancel_for_app(app, &job_id).await;
                Err(NodeError::Cancelled)
            }
            result = self.runtime.execute_prepared(request, prepared, CompletionMode::Immediate) => result.map_err(map_runtime),
        }
    }
}

fn map_runtime(_error: RuntimeError) -> NodeError {
    NodeError::Execution
}
