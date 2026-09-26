//! Adapter from an admitted local Job Attempt to an explicitly paired node.
use crate::{
    Provider, ProviderAttemptContext, ProviderByteStream, ProviderError, ProviderFailureKind,
};
use async_trait::async_trait;
use infer_core::{NodePeerConfig, ResponsesRequest};
use infer_node::{NodeClient, NodeError, TaskKey};
use serde_json::Value;
use std::collections::BTreeSet;

pub struct TrustedNodeProvider {
    id: String,
    config: NodePeerConfig,
    client: NodeClient,
}
impl TrustedNodeProvider {
    pub fn new(id: String, config: NodePeerConfig) -> Result<Self, ProviderError> {
        let client = NodeClient::new(config.clone()).map_err(map_error)?;
        Ok(Self { id, config, client })
    }
}

pub(crate) fn map_error(error: NodeError) -> ProviderError {
    if error == NodeError::OutcomeUnknown {
        return ProviderError::RemoteOutcomeUnknown;
    }
    ProviderError::Classified {
        kind: match error {
            NodeError::Forbidden => ProviderFailureKind::Authentication,
            NodeError::Busy => ProviderFailureKind::RateLimited,
            NodeError::Unavailable => ProviderFailureKind::Unavailable,
            NodeError::Protocol | NodeError::Missing => ProviderFailureKind::InvalidRequest,
            _ => ProviderFailureKind::Protocol,
        },
        message: error.to_string(),
    }
}

#[async_trait]
impl Provider for TrustedNodeProvider {
    fn id(&self) -> &str {
        &self.id
    }
    async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
        Err(ProviderError::InvalidInput(
            "trusted nodes require an admitted Job Attempt".into(),
        ))
    }
    async fn execute_attempt(
        &self,
        context: ProviderAttemptContext,
        request: ResponsesRequest,
    ) -> Result<Value, ProviderError> {
        let deployment = request.model.clone();
        self.client
            .execute(
                infer_node::NodeAttempt {
                    key: TaskKey {
                        job_id: context.job_id,
                        attempt: context.attempt,
                    },
                    app_id: context.app_id,
                    intent: context.intent,
                },
                deployment,
                request,
                context.remaining,
            )
            .await
            .map_err(map_error)
    }
    async fn execute_stream(
        &self,
        _request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        Err(ProviderError::InvalidInput(
            "trusted text nodes currently support unary execution".into(),
        ))
    }
    async fn available_models(
        &self,
        intent: &str,
    ) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let (_, catalog) = self.client.catalog().await.map_err(map_error)?;
        if catalog.available_admissions == 0 {
            return Ok(Some(BTreeSet::new()));
        }
        Ok(Some(
            catalog
                .offers
                .into_iter()
                .filter(|offer| {
                    offer.intent == intent
                        && self.config.imports.get(&offer.deployment_id)
                            == Some(&offer.contract_digest)
                })
                .map(|offer| offer.deployment_id)
                .collect(),
        ))
    }
}
