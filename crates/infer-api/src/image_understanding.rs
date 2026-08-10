//! Experimental typed HTTP transport for local image understanding.

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::{
    ClassificationCategory, ClassificationReviewRequest, ImageDescriptionRequest,
    MAX_CLASSIFICATION_CANDIDATES_JSON_BYTES,
};

use crate::{ApiError, ApiState, authenticate, response};

use crate::vision::{VisionMultipart, fail_closed_metadata};

pub(super) async fn create_image_description(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = VisionMultipart::parse(
        multipart,
        &["model", "source_revision", "image_orientation", "language"],
    )
    .await?;
    let request = ImageDescriptionRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        language: form.required_text("language")?,
        metadata: fail_closed_metadata(form.metadata, "image description")?,
    };
    let result = state
        .runtime
        .execute_image_description(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("image description response is serializable"),
    )
}

pub(super) async fn create_classification_review(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = VisionMultipart::parse(
        multipart,
        &[
            "model",
            "source_revision",
            "image_orientation",
            "taxonomy_revision",
            "categories",
        ],
    )
    .await?;
    let categories = parse_categories(&form.required_text("categories")?)?;
    let request = ClassificationReviewRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        taxonomy_revision: form.required_text("taxonomy_revision")?,
        categories,
        metadata: fail_closed_metadata(form.metadata, "classification review")?,
    };
    let result = state
        .runtime
        .execute_classification_review(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("classification review response is serializable"),
    )
}

fn parse_categories(value: &str) -> Result<Vec<ClassificationCategory>, ApiError> {
    if value.len() > MAX_CLASSIFICATION_CANDIDATES_JSON_BYTES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "vision_payload_too_large",
            message: format!(
                "categories JSON must not exceed {MAX_CLASSIFICATION_CANDIDATES_JSON_BYTES} UTF-8 bytes"
            ),
        });
    }
    serde_json::from_str(value)
        .map_err(|error| ApiError::bad_request(format!("invalid categories JSON: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_request_rejects_unknown_fields() {
        let error =
            parse_categories(r#"[{"id":"travel","name":"Travel","confidence":0.9}]"#).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn category_request_has_a_separate_small_payload_bound() {
        let error = parse_categories(&"x".repeat(MAX_CLASSIFICATION_CANDIDATES_JSON_BYTES + 1))
            .unwrap_err();
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.code, "vision_payload_too_large");
    }
}
