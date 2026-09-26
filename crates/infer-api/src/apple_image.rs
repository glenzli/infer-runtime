//! Bounded multipart transport; host paths and model preparation are never Consumer inputs.
use crate::{ApiError, ApiState, authenticate, response};
use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::{AppleImageParameters, AppleImageRequest, MAX_APPLE_IMAGE_BYTES};
use tokio_util::sync::CancellationToken;
pub(super) async fn execute(
    State(state): State<ApiState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app = authenticate(&state, &headers)?;
    let mut parameters = None;
    let mut image = None;
    while let Some(mut field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::bad_request("invalid multipart"))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let limit = match name.as_str() {
            "request" if parameters.is_none() => 16 * 1024,
            "image" if image.is_none() => MAX_APPLE_IMAGE_BYTES,
            _ => {
                return Err(ApiError::bad_request(
                    "unknown or duplicate Apple image field",
                ));
            }
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|_| ApiError::bad_request("invalid multipart field"))?
        {
            if bytes.len() + chunk.len() > limit {
                return Err(ApiError::bad_request("Apple image field exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if name == "request" {
            parameters = Some(
                serde_json::from_slice::<AppleImageParameters>(&bytes)
                    .map_err(|_| ApiError::bad_request("invalid Apple image parameters"))?,
            );
        } else {
            image = Some(bytes);
        }
    }
    let mut parameters =
        parameters.ok_or_else(|| ApiError::bad_request("missing request field"))?;
    let placement = parameters
        .metadata
        .entry("infer.placement".into())
        .or_insert_with(|| "local_only".into());
    if !matches!(placement.as_str(), "local_only" | "private") {
        return Err(ApiError::bad_request(
            "Apple images require local or trusted private execution",
        ));
    }
    for (key, value) in [
        ("infer.offline_required", "true"),
        ("infer.fallback", "none"),
    ] {
        if parameters
            .metadata
            .get(key)
            .is_some_and(|actual| actual != value)
        {
            return Err(ApiError::bad_request(
                "Apple image privacy constraints cannot be relaxed",
            ));
        }
        parameters.metadata.insert(key.into(), value.into());
    }
    let request = AppleImageRequest {
        parameters,
        bytes: image.ok_or_else(|| ApiError::bad_request("missing image field"))?,
    };
    let cancellation = CancellationToken::new();
    let guard = cancellation.clone().drop_guard();
    let result = state
        .runtime
        .execute_apple_image(&app, request, cancellation)
        .await?;
    guard.disarm();
    let mut result = response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("typed native response"),
    )?;
    result.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(result)
}
