use std::{collections::BTreeMap, path::Path};

use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Client, Error, Result, transport::decode};

pub const FACE_DETECTION_CAPABILITIES: &[&str] = &["infer.vision.face-detection@20260811.1"];
pub const FACE_EMBEDDING_CAPABILITIES: &[&str] = &["infer.vision.face-embedding@20260811.1"];
pub const SUBJECT_SEGMENTATION_CAPABILITIES: &[&str] =
    &["infer.vision.subject-segmentation@20260813.1"];
pub const FACE_PARSING_CAPABILITIES: &[&str] = &["infer.vision.face-parsing@20260813.1"];
pub const IMAGE_EMBEDDING_CAPABILITIES: &[&str] = &["infer.vision.image-embedding@20260811.1"];
pub const TEXT_EMBEDDING_CAPABILITIES: &[&str] = &["infer.vision.text-embedding@20260811.1"];
pub const IMAGE_DESCRIPTION_CAPABILITIES: &[&str] = &["infer.vision.image-description@20260811.1"];
pub const CLASSIFICATION_REVIEW_CAPABILITIES: &[&str] =
    &["infer.vision.classification-review@20260811.1"];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FivePointLandmarks {
    pub right_eye: Point,
    pub left_eye: Point,
    pub nose_tip: Point,
    pub right_mouth_corner: Point,
    pub left_mouth_corner: Point,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ImageGeometry {
    pub width: u32,
    pub height: u32,
    pub orientation: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct NormalizedBoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SegmentationPromptLabel {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct SegmentationPromptPoint {
    pub x: f32,
    pub y: f32,
    pub label: SegmentationPromptLabel,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FaceDetection {
    pub bounding_box: BoundingBox,
    pub landmarks: FivePointLandmarks,
    pub confidence: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SemanticEmbeddingVector {
    pub values: Vec<f32>,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
    pub space: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FaceEmbeddingEligibility {
    pub eligible: bool,
    pub landmarks_in_image: bool,
    pub inter_eye_distance_pixels: f32,
    pub alignment_rmse_pixels: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct VisionProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub execution_provider_fallback_reason: Option<String>,
    pub precision: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FaceEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub data_classification: String,
    pub embedding: SemanticEmbeddingVector,
    pub eligibility: FaceEmbeddingEligibility,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EncodedSegmentationMask {
    pub content_type: String,
    pub encoding: String,
    pub data_base64: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub foreground_pixels: u64,
    pub bounding_box: Option<BoundingBox>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SubjectSegmentationResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub image: ImageGeometry,
    pub mask: EncodedSegmentationMask,
    pub score: f32,
    pub prompt_count: usize,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EncodedLabelMap {
    pub content_type: String,
    pub encoding: String,
    pub data_base64: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FaceParsingOntology {
    pub id: String,
    pub revision: String,
    pub background_value: u8,
    pub class_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FaceParsingRegion {
    pub class_id: String,
    pub label: String,
    pub label_value: u8,
    pub pixel_count: u64,
    pub bounding_box: Option<BoundingBox>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FaceParsingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub data_classification: String,
    pub image: ImageGeometry,
    pub face_box: BoundingBox,
    pub label_map: EncodedLabelMap,
    pub ontology: FaceParsingOntology,
    pub regions: Vec<FaceParsingRegion>,
    pub provenance: VisionProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageDescriptionResult {
    pub description: String,
    pub keyword_suggestions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationDisposition {
    Matched,
    None,
    Uncertain,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClassificationSuggestion {
    pub disposition: ClassificationDisposition,
    pub category_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageUnderstandingProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_profile: String,
    pub model_build: String,
    pub physical_model: String,
    pub runtime: String,
    pub schema_revision: String,
    pub prompt_revision: String,
    pub total_duration_ms: Option<u64>,
    pub load_duration_ms: Option<u64>,
    pub prompt_eval_count: Option<u64>,
    pub eval_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ImageDescriptionResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub language: String,
    pub image: ImageGeometry,
    pub result: ImageDescriptionResult,
    pub provenance: ImageUnderstandingProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ClassificationReviewResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub taxonomy_revision: String,
    pub image: ImageGeometry,
    pub suggestion: ClassificationSuggestion,
    pub provenance: ImageUnderstandingProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextEmbeddingRequest {
    pub model: String,
    pub text: String,
    pub query_revision: String,
    pub language: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClassificationCategory {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Client {
    pub async fn embed_text(
        &self,
        request: &TextEmbeddingRequest,
    ) -> Result<TextEmbeddingResponse> {
        self.send_capability_json(
            TEXT_EMBEDDING_CAPABILITIES,
            reqwest::Method::POST,
            "/infer/v1/vision/text-embeddings",
            Some(request),
        )
        .await
    }

    pub async fn detect_faces(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<FaceDetectionResponse> {
        self.vision_multipart(
            FACE_DETECTION_CAPABILITIES,
            "/infer/v1/vision/face-detections",
            image,
            content_type,
            vec![
                ("model", "vision.detect_faces".into()),
                ("source_revision", source_revision.into()),
            ],
            metadata,
        )
        .await
    }

    pub async fn embed_face(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        landmarks: FivePointLandmarks,
        metadata: &BTreeMap<String, String>,
    ) -> Result<FaceEmbeddingResponse> {
        self.vision_multipart(
            FACE_EMBEDDING_CAPABILITIES,
            "/infer/v1/vision/face-embeddings",
            image,
            content_type,
            vec![
                ("model", "vision.embed_face".into()),
                ("source_revision", source_revision.into()),
                (
                    "landmarks",
                    serde_json::to_string(&landmarks)
                        .map_err(|error| Error::MalformedResponse(error.to_string()))?,
                ),
            ],
            metadata,
        )
        .await
    }

    pub async fn segment_subject(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        points: &[SegmentationPromptPoint],
        box_prompt: Option<NormalizedBoundingBox>,
        metadata: &BTreeMap<String, String>,
    ) -> Result<SubjectSegmentationResponse> {
        let mut fields = vec![
            ("model", "vision.segment_subject".into()),
            ("source_revision", source_revision.into()),
            (
                "image_orientation",
                "display_pixels_orientation_normalized".into(),
            ),
            ("prompt_coordinate_space", "normalized_0_1".into()),
            (
                "points",
                serde_json::to_string(points)
                    .map_err(|error| Error::MalformedResponse(error.to_string()))?,
            ),
        ];
        if let Some(box_prompt) = box_prompt {
            fields.push((
                "box_prompt",
                serde_json::to_string(&box_prompt)
                    .map_err(|error| Error::MalformedResponse(error.to_string()))?,
            ));
        }
        self.vision_multipart(
            SUBJECT_SEGMENTATION_CAPABILITIES,
            "/infer/v1/vision/subject-segmentations",
            image,
            content_type,
            fields,
            metadata,
        )
        .await
    }

    pub async fn parse_face(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        face_box: BoundingBox,
        metadata: &BTreeMap<String, String>,
    ) -> Result<FaceParsingResponse> {
        self.vision_multipart(
            FACE_PARSING_CAPABILITIES,
            "/infer/v1/vision/face-parsings",
            image,
            content_type,
            vec![
                ("model", "vision.parse_face".into()),
                ("source_revision", source_revision.into()),
                (
                    "image_orientation",
                    "display_pixels_orientation_normalized".into(),
                ),
                (
                    "face_box",
                    serde_json::to_string(&face_box)
                        .map_err(|error| Error::MalformedResponse(error.to_string()))?,
                ),
            ],
            metadata,
        )
        .await
    }

    pub async fn embed_image(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<ImageEmbeddingResponse> {
        self.vision_multipart(
            IMAGE_EMBEDDING_CAPABILITIES,
            "/infer/v1/vision/image-embeddings",
            image,
            content_type,
            vec![
                ("model", "vision.embed_image".into()),
                ("source_revision", source_revision.into()),
                (
                    "image_orientation",
                    "display_pixels_orientation_normalized".into(),
                ),
            ],
            metadata,
        )
        .await
    }

    pub async fn describe_image(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        language: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<ImageDescriptionResponse> {
        self.vision_multipart(
            IMAGE_DESCRIPTION_CAPABILITIES,
            "/infer/v1/vision/image-descriptions",
            image,
            content_type,
            vec![
                ("model", "vision.describe_image".into()),
                ("source_revision", source_revision.into()),
                (
                    "image_orientation",
                    "display_pixels_orientation_normalized".into(),
                ),
                ("language", language.into()),
            ],
            metadata,
        )
        .await
    }

    pub async fn review_classification(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        taxonomy_revision: &str,
        categories: &[ClassificationCategory],
        metadata: &BTreeMap<String, String>,
    ) -> Result<ClassificationReviewResponse> {
        self.vision_multipart(
            CLASSIFICATION_REVIEW_CAPABILITIES,
            "/infer/v1/vision/classification-reviews",
            image,
            content_type,
            vec![
                ("model", "vision.review_classification".into()),
                ("source_revision", source_revision.into()),
                (
                    "image_orientation",
                    "display_pixels_orientation_normalized".into(),
                ),
                ("taxonomy_revision", taxonomy_revision.into()),
                (
                    "categories",
                    serde_json::to_string(categories)
                        .map_err(|error| Error::MalformedResponse(error.to_string()))?,
                ),
            ],
            metadata,
        )
        .await
    }

    async fn vision_multipart<T: DeserializeOwned>(
        &self,
        supported_contracts: &'static [&'static str],
        route: &'static str,
        image: &Path,
        content_type: &'static str,
        fields: Vec<(&'static str, String)>,
        metadata: &BTreeMap<String, String>,
    ) -> Result<T> {
        let bytes = crate::transport::read_bounded_file(
            image,
            crate::transport::MAX_IMAGE_INPUT_BYTES,
            "image",
        )
        .await?;
        let filename = image
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image.bin")
            .to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let response = self
            .send_capability_with(supported_contracts, move |http, endpoint| {
                let file = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                let mut form = Form::new()
                    .part("image", file)
                    .text("metadata", metadata.clone());
                for (name, value) in &fields {
                    form = form.text(*name, value.clone());
                }
                http.post(format!("{endpoint}{route}")).multipart(form)
            })
            .await?;
        decode(response).await
    }
}
