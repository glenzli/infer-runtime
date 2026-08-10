//! Authenticated, read-only infrastructure observation endpoint.

use super::{ApiError, ApiState, authenticate_observer};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};

pub(super) async fn get_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authenticate_observer(&state, &headers)?;
    let snapshot = state.runtime.observer_snapshot().await?;
    let mut response =
        Json(serde_json::to_value(snapshot).expect("observer snapshot is serializable"))
            .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
