//! Typed multipart transport for exact document OCR.

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::DocumentOcrRequest;

use crate::{ApiError, ApiState, authenticate, response};

pub(super) async fn create_document_ocr(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = super::vision::VisionMultipart::parse(
        multipart,
        &["model", "source_revision", "image_orientation"],
    )
    .await?;
    let request = DocumentOcrRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        metadata: super::vision::fail_closed_metadata(form.metadata, "document OCR")?,
    };
    let result = state.runtime.execute_document_ocr(&app_id, request).await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("OCR response is serializable"),
    )
}
