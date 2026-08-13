//! Experimental typed vision transport. Promotion into the frozen Consumer
//! manifest waits for real-provider tolerance and privacy acceptance gates.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Json,
    extract::{Multipart, State, rejection::JsonRejection},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::{
    BoundingBox, FaceDetectionRequest, FaceEmbeddingRequest, FaceParsingRequest,
    FivePointLandmarks, ImageEmbeddingRequest, MAX_VISION_IMAGE_BYTES, NormalizedBoundingBox,
    SegmentationPromptPoint, SubjectSegmentationRequest, TextEmbeddingRequest, VisionImage,
};

use crate::{ApiError, ApiState, authenticate, response, strict_json};

pub(super) async fn create_subject_segmentation(
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
            "prompt_coordinate_space",
            "points",
            "box_prompt",
        ],
    )
    .await?;
    let points =
        serde_json::from_str::<Vec<SegmentationPromptPoint>>(&form.required_text("points")?)
            .map_err(|_| ApiError::bad_request("invalid points JSON"))?;
    let box_prompt = form
        .optional_text("box_prompt")
        .map(|value| {
            serde_json::from_str::<NormalizedBoundingBox>(value)
                .map_err(|_| ApiError::bad_request("invalid box_prompt JSON"))
        })
        .transpose()?;
    let request = SubjectSegmentationRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        prompt_coordinate_space: form.required_text("prompt_coordinate_space")?,
        points,
        box_prompt,
        metadata: fail_closed_metadata(form.metadata, "subject segmentation")?,
    };
    let result = state
        .runtime
        .execute_subject_segmentation(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("subject segmentation response is serializable"),
    )
}

/// Additive probability-mask endpoint.  The multipart input is deliberately
/// identical to the legacy binary endpoint; only the explicitly negotiated
/// capability and returned representation differ.
pub(super) async fn create_subject_segmentation_soft_mask(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let request = parse_subject_segmentation_request(multipart).await?;
    let result = state
        .runtime
        .execute_subject_segmentation_soft_mask(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("soft subject segmentation response is serializable"),
    )
}

async fn parse_subject_segmentation_request(
    multipart: Multipart,
) -> Result<SubjectSegmentationRequest, ApiError> {
    let mut form = VisionMultipart::parse(
        multipart,
        &[
            "model",
            "source_revision",
            "image_orientation",
            "prompt_coordinate_space",
            "points",
            "box_prompt",
        ],
    )
    .await?;
    let points =
        serde_json::from_str::<Vec<SegmentationPromptPoint>>(&form.required_text("points")?)
            .map_err(|_| ApiError::bad_request("invalid points JSON"))?;
    let box_prompt = form
        .optional_text("box_prompt")
        .map(|value| {
            serde_json::from_str::<NormalizedBoundingBox>(value)
                .map_err(|_| ApiError::bad_request("invalid box_prompt JSON"))
        })
        .transpose()?;
    Ok(SubjectSegmentationRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        prompt_coordinate_space: form.required_text("prompt_coordinate_space")?,
        points,
        box_prompt,
        metadata: fail_closed_metadata(form.metadata, "subject segmentation")?,
    })
}

pub(super) async fn create_face_parsing(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = VisionMultipart::parse(
        multipart,
        &["model", "source_revision", "image_orientation", "face_box"],
    )
    .await?;
    let face_box = serde_json::from_str::<BoundingBox>(&form.required_text("face_box")?)
        .map_err(|_| ApiError::bad_request("invalid face_box JSON"))?;
    let request = FaceParsingRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        face_box,
        metadata: fail_closed_metadata(form.metadata, "face parsing")?,
    };
    let result = state.runtime.execute_face_parsing(&app_id, request).await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("face parsing response is serializable"),
    )
}

pub(super) async fn create_face_detection(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = VisionMultipart::parse(multipart, &["model", "source_revision"]).await?;
    let model = form.required_text("model")?;
    let source_revision = form.required_text("source_revision")?;
    let image = form
        .image
        .take()
        .ok_or_else(|| ApiError::bad_request("missing image file"))?;
    let request = FaceDetectionRequest {
        model,
        image,
        source_revision,
        metadata: fail_closed_metadata(form.metadata, "face detection")?,
    };
    let result = state
        .runtime
        .execute_face_detection(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("face detection response is serializable"),
    )
}

pub(super) async fn create_face_embedding(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form =
        VisionMultipart::parse(multipart, &["model", "source_revision", "landmarks"]).await?;
    let model = form.required_text("model")?;
    let source_revision = form.required_text("source_revision")?;
    let landmarks =
        serde_json::from_str::<FivePointLandmarks>(&form.required_text("landmarks")?)
            .map_err(|error| ApiError::bad_request(format!("invalid landmarks JSON: {error}")))?;
    let image = form
        .image
        .take()
        .ok_or_else(|| ApiError::bad_request("missing image file"))?;
    let request = FaceEmbeddingRequest {
        model,
        image,
        landmarks,
        source_revision,
        metadata: fail_closed_metadata(form.metadata, "face embedding")?,
    };
    let result = state
        .runtime
        .execute_face_embedding(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("face embedding response is serializable"),
    )
}

pub(super) async fn create_image_embedding(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut form = VisionMultipart::parse(
        multipart,
        &["model", "source_revision", "image_orientation"],
    )
    .await?;
    let request = ImageEmbeddingRequest {
        model: form.required_text("model")?,
        image: form
            .image
            .take()
            .ok_or_else(|| ApiError::bad_request("missing image file"))?,
        source_revision: form.required_text("source_revision")?,
        image_orientation: form.required_text("image_orientation")?,
        metadata: fail_closed_metadata(form.metadata, "image embedding")?,
    };
    let result = state
        .runtime
        .execute_image_embedding(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("image embedding response is serializable"),
    )
}

pub(super) async fn create_text_embedding(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<TextEmbeddingRequest>, JsonRejection>,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    request.metadata = fail_closed_metadata(request.metadata, "text embedding")?;
    let result = state
        .runtime
        .execute_text_embedding(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("text embedding response is serializable"),
    )
}

#[derive(Default)]
pub(super) struct VisionMultipart {
    text: BTreeMap<String, String>,
    pub(super) metadata: BTreeMap<String, String>,
    pub(super) image: Option<VisionImage>,
}

impl VisionMultipart {
    pub(super) async fn parse(
        mut multipart: Multipart,
        allowed_text_fields: &[&str],
    ) -> Result<Self, ApiError> {
        let mut form = Self::default();
        let mut seen = BTreeSet::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|error| ApiError::bad_request(error.to_string()))?
        {
            let name = field
                .name()
                .ok_or_else(|| ApiError::bad_request("multipart field needs a name"))?
                .to_owned();
            if !seen.insert(name.clone()) {
                return Err(ApiError::bad_request(format!(
                    "duplicate multipart field `{name}`"
                )));
            }
            match name.as_str() {
                "image" => {
                    let content_type = field
                        .content_type()
                        .ok_or_else(|| ApiError::bad_request("image content type is required"))?
                        .to_owned();
                    if !matches!(content_type.as_str(), "image/jpeg" | "image/png") {
                        return Err(ApiError::bad_request(
                            "image content type must be image/jpeg or image/png",
                        ));
                    }
                    let bytes = field
                        .bytes()
                        .await
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    if bytes.is_empty() || bytes.len() > MAX_VISION_IMAGE_BYTES {
                        return Err(ApiError {
                            status: StatusCode::PAYLOAD_TOO_LARGE,
                            code: "vision_payload_too_large",
                            message: format!(
                                "image must contain between 1 byte and {MAX_VISION_IMAGE_BYTES} bytes"
                            ),
                        });
                    }
                    form.image = Some(VisionImage {
                        content_type,
                        bytes: bytes.to_vec(),
                    });
                }
                name if allowed_text_fields.contains(&name) => {
                    let value = field
                        .text()
                        .await
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    form.text.insert(name.to_owned(), value);
                }
                "metadata" => {
                    let value = field
                        .text()
                        .await
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    let metadata: BTreeMap<String, String> = serde_json::from_str(&value)
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    for (key, value) in metadata {
                        if form.metadata.insert(key.clone(), value).is_some() {
                            return Err(ApiError::bad_request(format!(
                                "duplicate metadata key `{key}`"
                            )));
                        }
                    }
                }
                name if name.starts_with("infer.") => {
                    let value = field
                        .text()
                        .await
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    if form.metadata.insert(name.into(), value).is_some() {
                        return Err(ApiError::bad_request(format!(
                            "duplicate metadata key `{name}`"
                        )));
                    }
                }
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "unknown multipart field `{name}`"
                    )));
                }
            }
        }
        Ok(form)
    }

    pub(super) fn required_text(&self, name: &'static str) -> Result<String, ApiError> {
        self.text
            .get(name)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| ApiError::bad_request(format!("missing multipart field `{name}`")))
    }

    fn optional_text(&self, name: &'static str) -> Option<&str> {
        self.text.get(name).map(String::as_str)
    }
}

pub(super) fn fail_closed_metadata(
    mut metadata: BTreeMap<String, String>,
    operation: &str,
) -> Result<BTreeMap<String, String>, ApiError> {
    for (key, required) in [
        ("infer.placement", "local_only"),
        ("infer.offline_required", "true"),
        ("infer.fallback", "none"),
    ] {
        if metadata.get(key).is_some_and(|actual| actual != required) {
            return Err(ApiError::bad_request(format!(
                "{key} is fixed to {required} for {operation}"
            )));
        }
        metadata.insert(key.into(), required.into());
    }
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_metadata_is_forced_local_without_fallback() {
        let Ok(metadata) = fail_closed_metadata(BTreeMap::new(), "vision execution") else {
            panic!("empty metadata should receive fail-closed defaults");
        };
        assert_eq!(metadata["infer.placement"], "local_only");
        assert_eq!(metadata["infer.offline_required"], "true");
        assert_eq!(metadata["infer.fallback"], "none");
    }

    #[test]
    fn caller_cannot_relax_vision_placement() {
        let metadata = BTreeMap::from([("infer.placement".into(), "anywhere".into())]);
        assert!(fail_closed_metadata(metadata, "vision execution").is_err());
    }
}
