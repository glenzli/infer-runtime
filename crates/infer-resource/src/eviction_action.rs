//! Approval-gated execution of one prepared eviction target.
//!
//! The planner remains pure and `ResourceManager::refresh` remains
//! observational. This owner serializes apply requests and verifies that an
//! operator-approved deployment is still the first target of a freshly
//! computed recommendation. Native lifecycle mutation stays in Resource
//! Manager so provider protocol details do not leak into policy code.

use tokio::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{EvictionRecommendation, EvictionTarget, SystemPressureLevel};

const MAX_APPROVAL_REASON_BYTES: usize = 512;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvictionApplyRequest {
    /// The operator must confirm the exact first target they inspected. A
    /// changed recommendation is rejected instead of silently evicting a
    /// different model.
    pub expected_deployment: String,
    /// Human-readable maintenance or incident reference. It is returned in
    /// the action audit and must not contain request payloads or credentials.
    pub reason: String,
}

impl EvictionApplyRequest {
    pub fn validate(&self) -> Result<(), EvictionApplyError> {
        validate_request(self)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvictionApplyError {
    #[error("eviction approval needs a non-empty expected deployment")]
    MissingExpectedDeployment,
    #[error("eviction approval needs a non-empty reason")]
    MissingReason,
    #[error("eviction approval reason exceeds {MAX_APPROVAL_REASON_BYTES} bytes")]
    ReasonTooLong,
    #[error("the fresh eviction recommendation has no actionable target")]
    NoActionableTarget,
    #[error(
        "fresh eviction target changed: operator approved `{expected}`, current target is `{actual}`"
    )]
    TargetChanged { expected: String, actual: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct EvictionActionAudit {
    pub reason: String,
    pub selected_at_unix_ms: u64,
    pub completed_at_unix_ms: u64,
    pub pressure: SystemPressureLevel,
    pub target_free_memory_percent: u8,
    pub current_free_memory_bytes: u64,
    pub target_free_memory_bytes: u64,
    pub requested_bytes: u64,
    pub projected_freed_bytes: u64,
    pub plan_shortfall_bytes: u64,
    pub selected: EvictionTarget,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedEvictionAction {
    pub reason: String,
    pub pressure: SystemPressureLevel,
    pub target_free_memory_percent: u8,
    pub current_free_memory_bytes: u64,
    pub target_free_memory_bytes: u64,
    pub requested_bytes: u64,
    pub projected_freed_bytes: u64,
    pub plan_shortfall_bytes: u64,
    pub selected: EvictionTarget,
}

/// Serializes apply operations without turning inventory refresh into an
/// action trigger. A future monitor can call this same owner after it obtains
/// an explicit policy lease.
#[derive(Default)]
pub(crate) struct EvictionActionCoordinator {
    action_lock: Mutex<()>,
}

impl EvictionActionCoordinator {
    pub async fn acquire(&self) -> MutexGuard<'_, ()> {
        self.action_lock.lock().await
    }

    pub fn prepare(
        &self,
        recommendation: &EvictionRecommendation,
        request: EvictionApplyRequest,
    ) -> Result<PreparedEvictionAction, EvictionApplyError> {
        request.validate()?;
        let prepared = self.prepare_current(recommendation, request.reason)?;
        if prepared.selected.deployment != request.expected_deployment {
            return Err(EvictionApplyError::TargetChanged {
                expected: request.expected_deployment,
                actual: prepared.selected.deployment,
            });
        }
        Ok(prepared)
    }

    pub fn prepare_current(
        &self,
        recommendation: &EvictionRecommendation,
        reason: String,
    ) -> Result<PreparedEvictionAction, EvictionApplyError> {
        validate_reason(&reason)?;
        let EvictionRecommendation::Planned {
            pressure,
            target_free_memory_percent,
            current_free_memory_bytes,
            target_free_memory_bytes,
            plan,
        } = recommendation
        else {
            return Err(EvictionApplyError::NoActionableTarget);
        };
        let selected = plan
            .targets
            .first()
            .cloned()
            .ok_or(EvictionApplyError::NoActionableTarget)?;
        Ok(PreparedEvictionAction {
            reason,
            pressure: *pressure,
            target_free_memory_percent: *target_free_memory_percent,
            current_free_memory_bytes: *current_free_memory_bytes,
            target_free_memory_bytes: *target_free_memory_bytes,
            requested_bytes: plan.requested_bytes,
            projected_freed_bytes: plan.projected_freed_bytes,
            plan_shortfall_bytes: plan.shortfall_bytes,
            selected,
        })
    }
}

fn validate_request(request: &EvictionApplyRequest) -> Result<(), EvictionApplyError> {
    if request.expected_deployment.trim().is_empty() {
        return Err(EvictionApplyError::MissingExpectedDeployment);
    }
    validate_reason(&request.reason)
}

fn validate_reason(reason: &str) -> Result<(), EvictionApplyError> {
    if reason.trim().is_empty() {
        return Err(EvictionApplyError::MissingReason);
    }
    if reason.len() > MAX_APPROVAL_REASON_BYTES {
        return Err(EvictionApplyError::ReasonTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use infer_core::ResourceClass;

    use super::*;
    use crate::{EvictionPlan, EvictionTarget};

    fn recommendation(deployment: &str) -> EvictionRecommendation {
        EvictionRecommendation::Planned {
            pressure: SystemPressureLevel::Critical,
            target_free_memory_percent: 25,
            current_free_memory_bytes: 1,
            target_free_memory_bytes: 10,
            plan: EvictionPlan {
                requested_bytes: 9,
                projected_freed_bytes: 12,
                shortfall_bytes: 0,
                targets: vec![EvictionTarget {
                    deployment: deployment.into(),
                    resource_class: ResourceClass::Light,
                    minimum_resident_ms: 300_000,
                    resident_memory_bytes: 12,
                    reload_cost_ms: 636,
                }],
                skipped: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn approval_must_match_the_fresh_first_target() {
        let coordinator = EvictionActionCoordinator::default();
        let _lease = coordinator.acquire().await;
        let error = coordinator
            .prepare(
                &recommendation("current"),
                EvictionApplyRequest {
                    expected_deployment: "stale".into(),
                    reason: "maintenance-42".into(),
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            EvictionApplyError::TargetChanged {
                expected: "stale".into(),
                actual: "current".into(),
            }
        );
    }

    #[test]
    fn approval_requires_a_bounded_reason_and_actionable_plan() {
        let coordinator = EvictionActionCoordinator::default();
        assert_eq!(
            coordinator
                .prepare(
                    &recommendation("local"),
                    EvictionApplyRequest {
                        expected_deployment: "local".into(),
                        reason: " ".into(),
                    },
                )
                .unwrap_err(),
            EvictionApplyError::MissingReason
        );
        assert_eq!(
            coordinator
                .prepare(
                    &EvictionRecommendation::Disabled,
                    EvictionApplyRequest {
                        expected_deployment: "local".into(),
                        reason: "maintenance-42".into(),
                    },
                )
                .unwrap_err(),
            EvictionApplyError::NoActionableTarget
        );
    }
}
