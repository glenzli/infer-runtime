//! Typed exact-OCR contracts for orientation-normalized display artifacts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, Fallback, ImageGeometry, PlacementScope, Point, RequestConstraints,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS, VisionImage,
};

pub const MAX_OCR_LINES: usize = 4_096;
pub const MAX_OCR_RESULT_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct DocumentOcrRequest {
    pub model: String,
    pub image: VisionImage,
    pub source_revision: String,
    pub image_orientation: String,
    pub metadata: BTreeMap<String, String>,
}

impl DocumentOcrRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty() {
            return Err(ContractError::MissingModel);
        }
        if self.image.bytes.is_empty() || self.image.bytes.len() > crate::MAX_VISION_IMAGE_BYTES {
            return Err(invalid(
                "image payload is empty or exceeds the vision limit",
            ));
        }
        if !matches!(self.image.content_type.as_str(), "image/jpeg" | "image/png") {
            return Err(invalid(
                "image content type must be image/jpeg or image/png",
            ));
        }
        if self.source_revision.trim().is_empty() || self.source_revision.len() > 256 {
            return Err(invalid(
                "source_revision is required and must not exceed 256 UTF-8 bytes",
            ));
        }
        if self.image_orientation != VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS {
            return Err(invalid(format!(
                "image_orientation must be {VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS}"
            )));
        }
        let constraints = RequestConstraints::from_metadata(&self.metadata)?;
        if constraints.placement != Some(PlacementScope::LocalOnly)
            || constraints.offline_required != Some(true)
            || constraints.fallback != Some(Fallback::None)
        {
            return Err(invalid(
                "OCR requires local_only, offline_required=true and fallback=none",
            ));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::InvalidVision(format!("invalid document OCR request: {}", message.into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentOcrResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub image: ImageGeometry,
    pub lines: Vec<OcrTextLine>,
    pub provenance: OcrProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcrTextLine {
    pub polygon: [Point; 4],
    pub text: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcrProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub detection_revision: String,
    pub detection_artifact_sha256: String,
    pub recognition_revision: String,
    pub recognition_artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub precision: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DocumentOcrRequest {
        DocumentOcrRequest {
            model: "document.ocr".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: vec![1],
            },
            source_revision: "artifact:1".into(),
            image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            metadata: BTreeMap::from([
                ("infer.placement".into(), "local_only".into()),
                ("infer.offline_required".into(), "true".into()),
                ("infer.fallback".into(), "none".into()),
            ]),
        }
    }

    #[test]
    fn ocr_requires_normalized_display_pixels() {
        request().validate().unwrap();
        let mut invalid = request();
        invalid.image_orientation = "input_pixels_no_exif_transform".into();
        assert!(invalid.validate().is_err());
    }
}
