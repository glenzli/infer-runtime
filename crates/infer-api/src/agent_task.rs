//! File-bearing Agent task admission and dispatch.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use infer_core::AgentTaskRequest;

use infer_control::RuntimeError;

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
    let result = state
        .runtime
        .execute_agent_task(&app_id, request)
        .await
        .map_err(|error| {
            if matches!(
                error,
                RuntimeError::NoCandidate | RuntimeError::ProviderUnavailable(_)
            ) {
                ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: contract::error_code::AGENT_TASK_UNAVAILABLE,
                    message: "No Agent file task executor is available".into(),
                }
            } else {
                ApiError::from(error)
            }
        })?;
    Ok((StatusCode::OK, Json(result)).into_response())
}
