//! Fail-closed admission for a future file-bearing Agent data plane.
//!
//! Codex App Server currently advertises a write boundary but no per-task
//! read boundary. Until an isolated executor can prove that only staged input
//! files are readable, this route never creates a Job or dispatches a turn.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use infer_core::AgentTaskRequest;

use crate::{ApiError, ApiState, authenticate, contract};

pub(super) async fn create_agent_task(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<AgentTaskRequest>, JsonRejection>,
) -> Result<axum::response::Response, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    state.runtime.authorize_agent_file_task(&app_id)?;
    let Json(request) = body.map_err(|_| ApiError::bad_request("invalid Agent task JSON"))?;
    request.validate().map_err(ApiError::bad_request)?;
    Err(ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: contract::error_code::AGENT_TASK_UNAVAILABLE,
        message: "Agent file execution is unavailable until an enforced input read boundary and Codex approval protocol are verified".into(),
    })
}
