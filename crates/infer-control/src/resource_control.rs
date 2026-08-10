//! Control-plane composition for local resource inspection and actions.
//!
//! Resource Manager owns policy and native lifecycle mutation; Store owns
//! durable audit. This facade coordinates them without adding resource action
//! branches to the ordinary Job execution pipeline.

use infer_resource::{
    EvictionActionResult, EvictionApplyRequest, LifecycleActionResult, ReloadBenchmarkRequest,
    ReloadBenchmarkResult, ResourceError, ResourceSnapshot,
};
use infer_store::{ResourceAuditEvent, ResourceAuditEventInput};
use serde_json::json;

use crate::{Runtime, RuntimeError};

#[derive(Debug, serde::Serialize)]
pub struct AuditedEvictionActionResult {
    pub action: EvictionActionResult,
    /// The request record is written before native mutation; a persistence
    /// failure therefore prevents the action from starting.
    pub approval_audit_persisted: bool,
    /// Completion recording is best effort because the native unload cannot
    /// be rolled back safely after it has succeeded.
    pub completion_audit_persisted: bool,
}

impl Runtime {
    /// Last observed native local-model inventories. Observations are not
    /// persisted and begin as `unknown` after every daemon start.
    pub async fn resource_snapshot(&self) -> ResourceSnapshot {
        self.resources.snapshot().await
    }

    /// Refresh configured local model inventories. This is an explicit
    /// operator action: it does not load, unload, or otherwise mutate models.
    pub async fn refresh_resources(&self) -> ResourceSnapshot {
        self.resources.refresh().await
    }

    /// Explicitly load one configured native model. This control-plane action
    /// is never selected by ordinary request routing.
    pub async fn load_resource_deployment(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<LifecycleActionResult, RuntimeError> {
        Ok(self.resources.load_deployment(provider, deployment).await?)
    }

    /// Explicitly unload one configured native model after draining it. New
    /// provider attempts are rejected while the native control call is open.
    pub async fn unload_resource_deployment(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<LifecycleActionResult, RuntimeError> {
        Ok(self
            .resources
            .unload_deployment(provider, deployment)
            .await?)
    }

    /// Execute one fresh eviction target after an authenticated operator has
    /// confirmed its deployment. The approval is durably recorded before the
    /// native action begins.
    pub async fn apply_resource_eviction(
        &self,
        actor: &str,
        request: EvictionApplyRequest,
    ) -> Result<AuditedEvictionActionResult, RuntimeError> {
        request.validate().map_err(ResourceError::from)?;
        let expected_deployment = request.expected_deployment.clone();
        let reason = request.reason.clone();
        let approval_audit_persisted = self.record_resource_audit(ResourceAuditEventInput {
            actor: actor.into(),
            kind: "eviction.apply_requested".into(),
            deployment: expected_deployment.clone(),
            details: json!({"reason": reason}),
        })?;
        match self.resources.apply_eviction(request).await {
            Ok(action) => {
                let completion_audit_persisted = self
                    .record_resource_audit(ResourceAuditEventInput {
                        actor: actor.into(),
                        kind: "eviction.apply_completed".into(),
                        deployment: action.deployment.clone(),
                        details: serde_json::to_value(&action.audit)
                            .expect("eviction audit is serializable"),
                    })
                    .unwrap_or(false);
                Ok(AuditedEvictionActionResult {
                    action,
                    approval_audit_persisted,
                    completion_audit_persisted,
                })
            }
            Err(error) => {
                let _ = self.record_resource_audit(ResourceAuditEventInput {
                    actor: actor.into(),
                    kind: "eviction.apply_failed".into(),
                    deployment: expected_deployment,
                    details: json!({"reason": reason}),
                });
                Err(error.into())
            }
        }
    }

    pub fn resource_audit_events(
        &self,
        limit: usize,
    ) -> Result<Vec<ResourceAuditEvent>, RuntimeError> {
        self.store
            .as_ref()
            .map(|store| store.resource_audit_events(limit))
            .transpose()
            .map(|events| events.unwrap_or_default())
            .map_err(Into::into)
    }

    /// Measure a configured, currently non-resident model's native reload
    /// cost. The caller receives a copyable registry record; runtime never
    /// edits its own TOML configuration.
    pub async fn benchmark_resource_reload(
        &self,
        provider: &str,
        deployment: &str,
        request: ReloadBenchmarkRequest,
    ) -> Result<ReloadBenchmarkResult, RuntimeError> {
        Ok(self
            .resources
            .benchmark_reload(provider, deployment, request)
            .await?)
    }

    pub(crate) fn record_resource_audit(
        &self,
        event: ResourceAuditEventInput,
    ) -> Result<bool, RuntimeError> {
        let Some(store) = &self.store else {
            return Ok(false);
        };
        store.record_resource_audit_event(event)?;
        Ok(true)
    }
}
