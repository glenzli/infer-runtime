//! Four typed routes; no model paths, arbitrary tensors or remote media URLs.

use crate::{ApiError, ApiState, authenticate, response, strict_json};
use axum::{
    Json,
    extract::{Multipart, State, rejection::JsonRejection},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::{
    MAX_MEDIA_UPLOAD_BYTES, MediaEmbeddingInput, MediaEmbeddingKind, MediaEmbeddingRequest,
    MediaQueryEmbeddingRequest, VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn create_query(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<MediaQueryEmbeddingRequest>, JsonRejection>,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    request.metadata = crate::vision::fail_closed_metadata(request.metadata, "media retrieval")?;
    let result = state
        .runtime
        .execute_media_embedding(&app_id, request.into())
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("media response serializes"),
    )
}

pub(super) async fn create_image(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    create_file(state, headers, multipart, MediaEmbeddingKind::Image).await
}
pub(super) async fn create_audio(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    create_file(state, headers, multipart, MediaEmbeddingKind::Audio).await
}
pub(super) async fn create_video(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    create_file(state, headers, multipart, MediaEmbeddingKind::Video).await
}

async fn create_file(
    state: ApiState,
    headers: HeaderMap,
    mut multipart: Multipart,
    kind: MediaEmbeddingKind,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut seen = BTreeSet::new();
    let mut fields = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::bad_request("invalid media multipart"))?
    {
        let name = field
            .name()
            .ok_or_else(|| ApiError::bad_request("multipart field needs a name"))?
            .to_owned();
        if !seen.insert(name.clone()) {
            return Err(ApiError::bad_request("duplicate media multipart field"));
        }
        if name == "file" {
            let content_type = field
                .content_type()
                .ok_or_else(|| ApiError::bad_request("media content type required"))?
                .to_owned();
            if !infer_core::media_content_type_allowed(kind, &content_type) {
                return Err(ApiError::bad_request("unsupported media content type"));
            }
            let bytes = field
                .bytes()
                .await
                .map_err(|_| ApiError::bad_request("invalid media file"))?;
            if bytes.len() > MAX_MEDIA_UPLOAD_BYTES {
                return Err(ApiError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    code: "invalid_request_error",
                    message: "media file exceeds 25 MiB".into(),
                });
            }
            file = Some(MediaEmbeddingInput::File {
                kind,
                content_type,
                bytes: bytes.to_vec(),
            });
        } else if matches!(name.as_str(), "model" | "source_revision" | "metadata")
            || (kind == MediaEmbeddingKind::Image && name == "image_orientation")
        {
            let bytes = field
                .bytes()
                .await
                .map_err(|_| ApiError::bad_request("invalid media field"))?;
            let limit = if name == "metadata" { 32768 } else { 256 };
            if bytes.len() > limit {
                return Err(ApiError::bad_request("media field exceeds its limit"));
            }
            let value = String::from_utf8(bytes.to_vec())
                .map_err(|_| ApiError::bad_request("media field requires UTF-8"))?;
            if name == "metadata" {
                metadata = serde_json::from_str(&value)
                    .map_err(|_| ApiError::bad_request("invalid media metadata"))?;
            } else {
                fields.insert(name, value);
            }
        } else {
            return Err(ApiError::bad_request("unknown media multipart field"));
        }
    }
    if kind == MediaEmbeddingKind::Image
        && fields.get("image_orientation").map(String::as_str)
            != Some(VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS)
    {
        return Err(ApiError::bad_request(
            "image requires display_pixels_orientation_normalized",
        ));
    }
    let request = MediaEmbeddingRequest {
        model: fields
            .remove("model")
            .ok_or_else(|| ApiError::bad_request("missing model"))?,
        input: file.ok_or_else(|| ApiError::bad_request("missing media file"))?,
        input_revision: fields
            .remove("source_revision")
            .ok_or_else(|| ApiError::bad_request("missing source_revision"))?,
        metadata: crate::vision::fail_closed_metadata(metadata, "media retrieval")?,
    };
    let result = state
        .runtime
        .execute_media_embedding(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("media response serializes"),
    )
}
