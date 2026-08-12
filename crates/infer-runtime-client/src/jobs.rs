use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Payload-free Job projection. Capability modules may expose more specific
/// result bodies, while Job/Attempt/routing provenance stays Core-owned.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobSnapshot {
    pub id: String,
    pub app_id: String,
    pub intent: String,
    pub consumer_core_contract: String,
    pub capability_contract: Option<String>,
    pub provider: String,
    pub deployment: String,
    pub model_profile: String,
    pub model_build: String,
    pub physical_model: String,
    pub placement: String,
    pub capability_level: String,
    pub evaluation_status: String,
    pub resource_class: String,
    pub state: String,
    pub policy: String,
    pub priority: String,
    pub constraints: Value,
    pub routing: RoutingDecision,
    #[serde(default)]
    pub attempts: Vec<AttemptSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttemptSnapshot {
    pub number: usize,
    pub provider: String,
    pub deployment: String,
    pub outcome: String,
    pub trigger: String,
    pub error_kind: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingDecision {
    pub capability_floor: String,
    pub named_route: Option<NamedRouteDecision>,
    pub candidates: Vec<CandidateDecision>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NamedRouteDecision {
    pub kind: String,
    pub ordered_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandidateDecision {
    pub deployment: String,
    pub provider: String,
    pub status: String,
    pub rank: Option<usize>,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobListItem {
    pub id: String,
    pub app_id: String,
    pub intent: String,
    pub provider: String,
    pub deployment: String,
    pub state: String,
    pub priority: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobListPage {
    pub jobs: Vec<JobListItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CancelResult {
    pub id: String,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExplainResult {
    pub response_id: String,
    pub intent: String,
    pub consumer_core_contract: String,
    pub capability_contract: Option<String>,
    pub routing: RoutingDecision,
    pub attempts: Vec<AttemptSnapshot>,
    pub state: String,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}
