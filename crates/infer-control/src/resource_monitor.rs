//! Lease-gated background resource pressure monitoring.
//!
//! The monitor owns its polling lifecycle and short-lived maintenance lease.
//! It never changes planner or native lifecycle rules: every tick delegates a
//! single target to Resource Manager's existing serialized action path.

use std::{sync::Arc, time::Duration};

use infer_core::EvictionMonitorConfig;
use infer_resource::{EvictionApplyError, ResourceError};
use infer_store::ResourceAuditEventInput;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::{sync::Mutex, time::sleep};
use uuid::Uuid;

use crate::{Runtime, RuntimeError};

const MIN_LEASE_MS: u64 = 10_000;
const MAX_REASON_BYTES: usize = 512;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MaintenanceLeaseRequest {
    pub duration_ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MaintenanceLeaseRevokeRequest {
    pub lease_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceLease {
    pub id: String,
    pub actor: String,
    pub reason: String,
    pub granted_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvictionMonitorOutcome {
    NeverRun,
    Disabled,
    NoMaintenanceLease,
    NoActionableTarget,
    Applied { deployment: String },
    Failed { code: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct EvictionMonitorSnapshot {
    pub enabled: bool,
    pub poll_interval_ms: u64,
    pub max_lease_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<MaintenanceLease>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tick_unix_ms: Option<u64>,
    pub last_outcome: EvictionMonitorOutcome,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MaintenanceLeaseError {
    #[error("background eviction monitor is disabled")]
    MonitorDisabled,
    #[error("maintenance lease duration must be at least {MIN_LEASE_MS} ms")]
    DurationTooShort,
    #[error("maintenance lease duration exceeds configured maximum {max_ms} ms")]
    DurationTooLong { max_ms: u64 },
    #[error("maintenance lease needs a non-empty reason")]
    MissingReason,
    #[error("maintenance lease reason exceeds {MAX_REASON_BYTES} bytes")]
    ReasonTooLong,
    #[error("an unexpired maintenance lease already exists")]
    ActiveLease,
    #[error("maintenance lease id does not match the active lease")]
    LeaseMismatch,
}

#[derive(Debug)]
struct MonitorState {
    lease: Option<MaintenanceLease>,
    last_tick_unix_ms: Option<u64>,
    last_outcome: EvictionMonitorOutcome,
}

pub(crate) struct EvictionMonitor {
    config: EvictionMonitorConfig,
    state: Mutex<MonitorState>,
}

impl EvictionMonitor {
    pub fn new(config: EvictionMonitorConfig) -> Self {
        Self {
            config,
            state: Mutex::new(MonitorState {
                lease: None,
                last_tick_unix_ms: None,
                last_outcome: EvictionMonitorOutcome::NeverRun,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.config.poll_interval_ms)
    }

    pub async fn grant(
        &self,
        actor: &str,
        request: MaintenanceLeaseRequest,
        now_unix_ms: u64,
    ) -> Result<MaintenanceLease, MaintenanceLeaseError> {
        self.validate_request(&request)?;
        let mut state = self.state.lock().await;
        clear_expired(&mut state, now_unix_ms);
        if state.lease.is_some() {
            return Err(MaintenanceLeaseError::ActiveLease);
        }
        let lease = MaintenanceLease {
            id: format!("lease_{}", Uuid::new_v4().simple()),
            actor: actor.into(),
            reason: request.reason,
            granted_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(request.duration_ms),
        };
        state.lease = Some(lease.clone());
        Ok(lease)
    }

    pub async fn revoke(
        &self,
        lease_id: &str,
        now_unix_ms: u64,
    ) -> Result<MaintenanceLease, MaintenanceLeaseError> {
        let mut state = self.state.lock().await;
        clear_expired(&mut state, now_unix_ms);
        let Some(lease) = state.lease.take() else {
            return Err(MaintenanceLeaseError::LeaseMismatch);
        };
        if lease.id != lease_id {
            state.lease = Some(lease);
            return Err(MaintenanceLeaseError::LeaseMismatch);
        }
        Ok(lease)
    }

    pub async fn active_lease(&self, now_unix_ms: u64) -> Option<MaintenanceLease> {
        let mut state = self.state.lock().await;
        clear_expired(&mut state, now_unix_ms);
        state.lease.clone()
    }

    pub async fn record(&self, now_unix_ms: u64, outcome: EvictionMonitorOutcome) {
        let mut state = self.state.lock().await;
        clear_expired(&mut state, now_unix_ms);
        state.last_tick_unix_ms = Some(now_unix_ms);
        state.last_outcome = outcome;
    }

    pub async fn snapshot(&self, now_unix_ms: u64) -> EvictionMonitorSnapshot {
        let mut state = self.state.lock().await;
        clear_expired(&mut state, now_unix_ms);
        EvictionMonitorSnapshot {
            enabled: self.config.enabled,
            poll_interval_ms: self.config.poll_interval_ms,
            max_lease_ms: self.config.max_lease_ms,
            lease: state.lease.clone(),
            last_tick_unix_ms: state.last_tick_unix_ms,
            last_outcome: state.last_outcome.clone(),
        }
    }

    fn validate_request(
        &self,
        request: &MaintenanceLeaseRequest,
    ) -> Result<(), MaintenanceLeaseError> {
        if !self.config.enabled {
            return Err(MaintenanceLeaseError::MonitorDisabled);
        }
        if request.duration_ms < MIN_LEASE_MS {
            return Err(MaintenanceLeaseError::DurationTooShort);
        }
        if request.duration_ms > self.config.max_lease_ms {
            return Err(MaintenanceLeaseError::DurationTooLong {
                max_ms: self.config.max_lease_ms,
            });
        }
        if request.reason.trim().is_empty() {
            return Err(MaintenanceLeaseError::MissingReason);
        }
        if request.reason.len() > MAX_REASON_BYTES {
            return Err(MaintenanceLeaseError::ReasonTooLong);
        }
        Ok(())
    }
}

fn clear_expired(state: &mut MonitorState, now_unix_ms: u64) {
    if state
        .lease
        .as_ref()
        .is_some_and(|lease| lease.expires_at_unix_ms <= now_unix_ms)
    {
        state.lease = None;
    }
}

pub(crate) fn spawn(runtime: &Arc<Runtime>) {
    if !runtime.resource_monitor.enabled() {
        return;
    }
    let weak = Arc::downgrade(runtime);
    let interval = runtime.resource_monitor.poll_interval();
    tokio::spawn(async move {
        loop {
            sleep(interval).await;
            let Some(runtime) = weak.upgrade() else {
                break;
            };
            runtime.run_resource_monitor_once().await;
        }
    });
}

impl Runtime {
    pub async fn grant_eviction_maintenance(
        &self,
        actor: &str,
        request: MaintenanceLeaseRequest,
    ) -> Result<MaintenanceLease, RuntimeError> {
        let lease = self
            .resource_monitor
            .grant(actor, request, unix_ms())
            .await?;
        if let Err(error) = self.record_resource_audit(ResourceAuditEventInput {
            actor: actor.into(),
            kind: "eviction.maintenance_lease_granted".into(),
            deployment: "*".into(),
            details: serde_json::to_value(&lease).expect("maintenance lease is serializable"),
        }) {
            let _ = self.resource_monitor.revoke(&lease.id, unix_ms()).await;
            return Err(error);
        }
        Ok(lease)
    }

    pub async fn revoke_eviction_maintenance(
        &self,
        actor: &str,
        request: MaintenanceLeaseRevokeRequest,
    ) -> Result<MaintenanceLease, RuntimeError> {
        let lease = self
            .resource_monitor
            .revoke(&request.lease_id, unix_ms())
            .await?;
        self.record_resource_audit(ResourceAuditEventInput {
            actor: actor.into(),
            kind: "eviction.maintenance_lease_revoked".into(),
            deployment: "*".into(),
            details: json!({"lease_id": lease.id}),
        })?;
        Ok(lease)
    }

    pub async fn eviction_monitor_snapshot(&self) -> EvictionMonitorSnapshot {
        self.resource_monitor.snapshot(unix_ms()).await
    }

    pub(crate) async fn run_resource_monitor_once(&self) {
        let now = unix_ms();
        if !self.resource_monitor.enabled() {
            self.resource_monitor
                .record(now, EvictionMonitorOutcome::Disabled)
                .await;
            return;
        }
        let Some(lease) = self.resource_monitor.active_lease(now).await else {
            self.resource_monitor
                .record(now, EvictionMonitorOutcome::NoMaintenanceLease)
                .await;
            return;
        };
        let reason = format!("maintenance lease {}: {}", lease.id, lease.reason);
        match self.resources.apply_monitored_eviction(reason).await {
            Ok(action) => {
                let persisted = self
                    .record_resource_audit(ResourceAuditEventInput {
                        actor: "system:eviction-monitor".into(),
                        kind: "eviction.monitor_applied".into(),
                        deployment: action.deployment.clone(),
                        details: json!({
                            "lease_id": lease.id,
                            "approval_actor": lease.actor,
                            "audit": action.audit,
                        }),
                    })
                    .unwrap_or(false);
                let outcome = if persisted || self.store.is_none() {
                    EvictionMonitorOutcome::Applied {
                        deployment: action.deployment,
                    }
                } else {
                    EvictionMonitorOutcome::Failed {
                        code: "completion_audit_failed".into(),
                    }
                };
                self.resource_monitor.record(now, outcome).await;
            }
            Err(ResourceError::EvictionApply(EvictionApplyError::NoActionableTarget)) => {
                self.resource_monitor
                    .record(now, EvictionMonitorOutcome::NoActionableTarget)
                    .await;
            }
            Err(_) => {
                let _ = self.record_resource_audit(ResourceAuditEventInput {
                    actor: "system:eviction-monitor".into(),
                    kind: "eviction.monitor_failed".into(),
                    deployment: "*".into(),
                    details: json!({"lease_id": lease.id}),
                });
                self.resource_monitor
                    .record(
                        now,
                        EvictionMonitorOutcome::Failed {
                            code: "resource_action_failed".into(),
                        },
                    )
                    .await;
            }
        }
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_monitor() -> EvictionMonitor {
        EvictionMonitor::new(EvictionMonitorConfig {
            enabled: true,
            poll_interval_ms: 1_000,
            max_lease_ms: 60_000,
        })
    }

    #[tokio::test]
    async fn lease_is_exclusive_revocable_and_expires_fail_closed() {
        let monitor = enabled_monitor();
        let lease = monitor
            .grant(
                "operator",
                MaintenanceLeaseRequest {
                    duration_ms: 10_000,
                    reason: "maintenance-42".into(),
                },
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(
            monitor
                .grant(
                    "operator",
                    MaintenanceLeaseRequest {
                        duration_ms: 10_000,
                        reason: "second".into(),
                    },
                    2_000,
                )
                .await
                .unwrap_err(),
            MaintenanceLeaseError::ActiveLease
        );
        assert!(monitor.active_lease(10_999).await.is_some());
        assert!(monitor.active_lease(11_000).await.is_none());

        let replacement = monitor
            .grant(
                "operator",
                MaintenanceLeaseRequest {
                    duration_ms: 10_000,
                    reason: "replacement".into(),
                },
                12_000,
            )
            .await
            .unwrap();
        assert_eq!(
            monitor.revoke("wrong", 12_001).await.unwrap_err(),
            MaintenanceLeaseError::LeaseMismatch
        );
        assert_eq!(
            monitor.revoke(&replacement.id, 12_002).await.unwrap().id,
            replacement.id
        );
        assert!(monitor.active_lease(12_003).await.is_none());
        assert_ne!(lease.id, replacement.id);
    }

    #[tokio::test]
    async fn disabled_monitor_never_grants_a_lease() {
        let monitor = EvictionMonitor::new(EvictionMonitorConfig::default());
        assert_eq!(
            monitor
                .grant(
                    "operator",
                    MaintenanceLeaseRequest {
                        duration_ms: 10_000,
                        reason: "maintenance".into(),
                    },
                    1,
                )
                .await
                .unwrap_err(),
            MaintenanceLeaseError::MonitorDisabled
        );
    }
}
