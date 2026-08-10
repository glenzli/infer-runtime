//! Typed public vision contracts. Images are intentionally not serializable
//! into generic Job metadata; API adapters construct the bounded byte payload
//! after validating multipart content.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ContractError, Fallback, PlacementScope, RequestConstraints};

pub const MAX_VISION_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_VISION_IMAGE_PIXELS: u64 = 40_000_000;
pub const SENSITIVE_BIOMETRIC_CLASSIFICATION: &str = "sensitive_biometric";
pub const VISION_ORIENTATION_INPUT_PIXELS_NO_EXIF_TRANSFORM: &str =
    "input_pixels_no_exif_transform";
pub const VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS: &str =
    "display_pixels_orientation_normalized";
pub const MAX_VISION_TEXT_BYTES: usize = 4 * 1024;

#[derive(Clone)]
pub struct FaceDetectionRequest {
    pub model: String,
    pub image: VisionImage,
    pub source_revision: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct VisionImage {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl FaceDetectionRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_local_vision_request(
            &self.model,
            &self.image,
            &self.source_revision,
            &self.metadata,
            "face detection",
        )
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Clone)]
pub struct FaceEmbeddingRequest {
    pub model: String,
    pub image: VisionImage,
    /// Five points in the decoded, unrotated input pixel coordinate space.
    pub landmarks: FivePointLandmarks,
    pub source_revision: String,
    pub metadata: BTreeMap<String, String>,
}

impl FaceEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_local_vision_request(
            &self.model,
            &self.image,
            &self.source_revision,
            &self.metadata,
            "face embedding",
        )?;
        if self
            .landmarks
            .points()
            .iter()
            .any(|point| !point.x.is_finite() || !point.y.is_finite())
        {
            return Err(ContractError::InvalidVision(
                "face embedding landmarks must contain finite coordinates".into(),
            ));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Clone)]
pub struct ImageEmbeddingRequest {
    pub model: String,
    pub image: VisionImage,
    /// Stable identity of the exact orientation-normalized encoded artifact.
    pub source_revision: String,
    pub image_orientation: String,
    pub metadata: BTreeMap<String, String>,
}

impl ImageEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_local_vision_request(
            &self.model,
            &self.image,
            &self.source_revision,
            &self.metadata,
            "image embedding",
        )?;
        if self.image_orientation != VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS {
            return Err(ContractError::InvalidVision(format!(
                "image embedding requires image_orientation={VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS}"
            )));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEmbeddingRequest {
    pub model: String,
    pub text: String,
    pub query_revision: String,
    pub language: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl TextEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty() {
            return Err(ContractError::MissingModel);
        }
        if self.text.trim().is_empty() || self.text.len() > MAX_VISION_TEXT_BYTES {
            return Err(ContractError::InvalidVision(format!(
                "text must contain between 1 and {MAX_VISION_TEXT_BYTES} UTF-8 bytes"
            )));
        }
        validate_revision(&self.query_revision, "query_revision")?;
        if self.language.as_ref().is_some_and(|language| {
            language.is_empty()
                || language.len() > 35
                || !language
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(ContractError::InvalidVision(
                "language must be a BCP-47-shaped tag no longer than 35 ASCII bytes".into(),
            ));
        }
        validate_local_constraints(&self.metadata, "text embedding")
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

fn validate_local_vision_request(
    model: &str,
    image: &VisionImage,
    source_revision: &str,
    metadata: &BTreeMap<String, String>,
    operation: &str,
) -> Result<(), ContractError> {
    if model.trim().is_empty() {
        return Err(ContractError::MissingModel);
    }
    if image.bytes.is_empty() || image.bytes.len() > MAX_VISION_IMAGE_BYTES {
        return Err(ContractError::InvalidVision(format!(
            "image must contain between 1 byte and {MAX_VISION_IMAGE_BYTES} bytes"
        )));
    }
    if !matches!(image.content_type.as_str(), "image/jpeg" | "image/png") {
        return Err(ContractError::InvalidVision(
            "image content type must be image/jpeg or image/png".into(),
        ));
    }
    validate_revision(source_revision, "source_revision")?;
    validate_local_constraints(metadata, operation)
}

fn validate_revision(revision: &str, field: &str) -> Result<(), ContractError> {
    if revision.trim().is_empty() || revision.len() > 256 {
        return Err(ContractError::InvalidVision(format!(
            "{field} is required and must not exceed 256 UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_local_constraints(
    metadata: &BTreeMap<String, String>,
    operation: &str,
) -> Result<(), ContractError> {
    let constraints = RequestConstraints::from_metadata(metadata)?;
    if constraints.placement != Some(PlacementScope::LocalOnly) {
        return Err(ContractError::InvalidVision(format!(
            "{operation} requires infer.placement=local_only"
        )));
    }
    if constraints.offline_required != Some(true) {
        return Err(ContractError::InvalidVision(format!(
            "{operation} requires infer.offline_required=true"
        )));
    }
    if constraints.fallback != Some(Fallback::None) {
        return Err(ContractError::InvalidVision(format!(
            "{operation} requires infer.fallback=none"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FaceDetectionResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub image: ImageGeometry,
    pub detections: Vec<FaceDetection>,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FaceEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub data_classification: String,
    pub embedding: FaceEmbeddingVector,
    pub eligibility: FaceEmbeddingEligibility,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub image: ImageGeometry,
    pub embedding: SemanticEmbeddingVector,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub query_revision: String,
    pub language: Option<String>,
    pub embedding: SemanticEmbeddingVector,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticEmbeddingVector {
    pub values: Vec<f32>,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
    pub space: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FaceEmbeddingVector {
    pub values: Vec<f32>,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
    pub space: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FaceEmbeddingEligibility {
    pub eligible: bool,
    pub landmarks_in_image: bool,
    pub inter_eye_distance_pixels: f32,
    pub alignment_rmse_pixels: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageGeometry {
    pub width: u32,
    pub height: u32,
    /// Pixel-space contract for all returned coordinates. The current value
    /// means width/height and coordinates refer to the decoded raster exactly
    /// as submitted; EXIF orientation metadata was not applied.
    pub orientation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FaceDetection {
    pub bounding_box: BoundingBox,
    pub landmarks: FivePointLandmarks,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FivePointLandmarks {
    pub right_eye: Point,
    pub left_eye: Point,
    pub nose_tip: Point,
    pub right_mouth_corner: Point,
    pub left_mouth_corner: Point,
}

impl FivePointLandmarks {
    pub fn points(self) -> [Point; 5] {
        [
            self.right_eye,
            self.left_eye,
            self.nose_tip,
            self.right_mouth_corner,
            self.left_mouth_corner,
        ]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VisionProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<VisionTokenizerProvenance>,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub execution_provider_fallback_reason: Option<String>,
    pub precision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VisionTokenizerProvenance {
    pub identity: String,
    pub artifact_sha256: String,
    pub max_length: usize,
    pub lowercase: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(metadata: BTreeMap<String, String>) -> FaceDetectionRequest {
        FaceDetectionRequest {
            model: "vision.detect_faces".into(),
            image: VisionImage {
                content_type: "image/jpeg".into(),
                bytes: vec![1],
            },
            source_revision: "photo:7".into(),
            metadata,
        }
    }

    fn local_metadata() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ])
    }

    #[test]
    fn face_detection_is_fail_closed_to_local_execution() {
        let valid = BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ]);
        request(valid).validate().unwrap();

        let anywhere = BTreeMap::from([
            ("infer.placement".into(), "anywhere".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ]);
        assert!(request(anywhere).validate().is_err());
    }

    #[test]
    fn face_embedding_rejects_non_finite_landmarks() {
        let request = FaceEmbeddingRequest {
            model: "vision.embed_face".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: vec![1],
            },
            landmarks: FivePointLandmarks {
                right_eye: Point {
                    x: f32::NAN,
                    y: 1.0,
                },
                left_eye: Point { x: 2.0, y: 1.0 },
                nose_tip: Point { x: 1.5, y: 2.0 },
                right_mouth_corner: Point { x: 1.0, y: 3.0 },
                left_mouth_corner: Point { x: 2.0, y: 3.0 },
            },
            source_revision: "photo:8".into(),
            metadata: BTreeMap::from([
                ("infer.placement".into(), "local_only".into()),
                ("infer.offline_required".into(), "true".into()),
                ("infer.fallback".into(), "none".into()),
            ]),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn vision_orientation_identity_names_the_unrotated_decoded_raster() {
        assert_eq!(
            VISION_ORIENTATION_INPUT_PIXELS_NO_EXIF_TRANSFORM,
            "input_pixels_no_exif_transform"
        );
    }

    #[test]
    fn semantic_image_embedding_requires_orientation_normalized_pixels() {
        let request = ImageEmbeddingRequest {
            model: "vision.embed_image".into(),
            image: VisionImage {
                content_type: "image/jpeg".into(),
                bytes: vec![1],
            },
            source_revision: "shadow:photo-1/artifact:abc".into(),
            image_orientation: VISION_ORIENTATION_INPUT_PIXELS_NO_EXIF_TRANSFORM.into(),
            metadata: local_metadata(),
        };
        assert!(request.validate().is_err());

        let valid = ImageEmbeddingRequest {
            image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            ..request
        };
        valid.validate().unwrap();
    }

    #[test]
    fn semantic_text_embedding_is_bounded_and_local_only() {
        let valid = TextEmbeddingRequest {
            model: "vision.embed_text".into(),
            text: "海边日落".into(),
            query_revision: "shadow:query:v1".into(),
            language: Some("zh-CN".into()),
            metadata: local_metadata(),
        };
        valid.validate().unwrap();

        let invalid_language = TextEmbeddingRequest {
            language: Some("zh_CN".into()),
            ..valid.clone()
        };
        assert!(invalid_language.validate().is_err());

        let cloud = TextEmbeddingRequest {
            metadata: BTreeMap::from([
                ("infer.placement".into(), "anywhere".into()),
                ("infer.offline_required".into(), "false".into()),
                ("infer.fallback".into(), "equivalent".into()),
            ]),
            ..valid
        };
        assert!(cloud.validate().is_err());
    }
}
