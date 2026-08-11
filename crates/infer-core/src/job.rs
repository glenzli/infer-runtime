//! Serializable control-plane projection of a runtime Job.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

use crate::{
    CapabilityLevel, EvaluationStatus, Placement, Priority, RequestConstraints, ResourceClass,
    string_enum,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobSnapshot {
    pub id: String,
    pub app_id: String,
    pub intent: String,
    pub provider: String,
    pub deployment: String,
    pub model_profile: String,
    pub model_build: String,
    pub physical_model: String,
    pub placement: Placement,
    pub capability_level: CapabilityLevel,
    pub evaluation_status: EvaluationStatus,
    pub resource_class: ResourceClass,
    pub state: JobState,
    pub policy: String,
    pub priority: Priority,
    pub constraints: RequestConstraints,
    /// Immutable admission-time routing record. Later provider health or queue
    /// changes never rewrite why this job selected its initial deployment.
    pub routing: RoutingDecision,
    #[serde(default)]
    pub attempts: Vec<AttemptSnapshot>,
    pub error: Option<String>,
}

/// Lightweight, payload-free projection used by bounded Job collection
/// queries. Full routing and Attempt detail remains available from the
/// single-Job endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JobListItem {
    pub id: String,
    pub app_id: String,
    pub intent: String,
    pub provider: String,
    pub deployment: String,
    pub state: JobState,
    pub priority: Priority,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JobListPage {
    pub jobs: Vec<JobListItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Stable keyset cursor. It is opaque at the HTTP boundary but remains a
/// small typed value between the transport and persistence owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPageCursor {
    pub created_at_ms: i64,
    pub id: String,
}

impl fmt::Display for JobPageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.created_at_ms, self.id)
    }
}

impl FromStr for JobPageCursor {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (created_at_ms, id) = value
            .split_once(':')
            .ok_or("cursor must contain a timestamp and Job ID")?;
        let created_at_ms = created_at_ms
            .parse::<i64>()
            .map_err(|_| "cursor timestamp is invalid")?;
        if created_at_ms < 0 || id.is_empty() {
            return Err("cursor timestamp and Job ID must be non-empty and non-negative");
        }
        Ok(Self {
            created_at_ms,
            id: id.into(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttemptSnapshot {
    pub number: usize,
    pub provider: String,
    pub deployment: String,
    pub outcome: AttemptOutcome,
    pub trigger: AttemptTrigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

string_enum!(AttemptOutcome {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    Interrupted => "interrupted"
});

string_enum!(AttemptTrigger {
    Initial => "initial",
    Retry => "retry",
    Fallback => "fallback",
    Recovery => "recovery"
});

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingDecision {
    pub capability_floor: CapabilityLevel,
    pub candidates: Vec<CandidateDecision>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandidateDecision {
    pub deployment: String,
    pub provider: String,
    pub status: CandidateDecisionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<CandidateReasonCode>,
}

string_enum!(CandidateDecisionStatus {
    Eligible => "eligible",
    FallbackEligible => "fallback_eligible",
    Rejected => "rejected"
});

string_enum!(CandidateReasonCode {
    IntentUnassessed => "intent_unassessed",
    ProviderCredentialUnavailable => "provider_credential_unavailable",
    ProviderAccessNotAllowed => "provider_access_not_allowed",
    ProviderAccessClassMismatch => "provider_access_class_mismatch",
    CloudInputModalityNotAllowed => "cloud_input_modality_not_allowed",
    ProviderCircuitOpen => "provider_circuit_open",
    DeploymentUnavailable => "deployment_unavailable",
    ProviderCapabilityMissing => "provider_capability_missing",
    ExecutionModeUnsupported => "execution_mode_unsupported",
    InputModalityMissing => "input_modality_missing",
    OutputModalityMissing => "output_modality_missing",
    RequiredFeatureMissing => "required_feature_missing",
    PlacementNotAllowed => "placement_not_allowed",
    OfflineRequired => "offline_required",
    CapabilityBelowFloor => "capability_below_floor",
    ReasoningEffortUnsupported => "reasoning_effort_unsupported",
    CostLimitExceeded => "cost_limit_exceeded"
});

string_enum!(JobState {
    Queued => "queued",
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
    Expired => "expired"
});

#[cfg(test)]
mod tests {
    use super::JobPageCursor;

    #[test]
    fn job_page_cursor_round_trips_without_hiding_its_sort_key() {
        let cursor = "1234:resp_one".parse::<JobPageCursor>().unwrap();
        assert_eq!(cursor.created_at_ms, 1234);
        assert_eq!(cursor.id, "resp_one");
        assert_eq!(cursor.to_string(), "1234:resp_one");
        assert!("-1:resp_one".parse::<JobPageCursor>().is_err());
        assert!("1234:".parse::<JobPageCursor>().is_err());
    }
}
