//! Typed contracts for bounded, local image understanding.
//!
//! These requests deliberately keep image bytes out of serializable Job
//! metadata. The provider adapter owns its native multimodal prompt/wire and
//! returns only schema-validated semantic proposals.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, ImageGeometry, RequestConstraints, VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
    VisionImage,
    vision::{validate_local_vision_request, validate_revision},
};

pub const MAX_CLASSIFICATION_CANDIDATES: usize = 64;
pub const MAX_CLASSIFICATION_CANDIDATES_JSON_BYTES: usize = 64 * 1024;
pub const MAX_CATEGORY_ID_BYTES: usize = 128;
pub const MAX_CATEGORY_NAME_BYTES: usize = 256;
pub const MAX_CATEGORY_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_IMAGE_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_KEYWORD_SUGGESTIONS: usize = 16;
pub const MAX_KEYWORD_BYTES: usize = 128;

pub const IMAGE_DESCRIPTION_SCHEMA_REVISION: &str = "infer.vision.image-description@20260811.1";
pub const IMAGE_DESCRIPTION_PROMPT_REVISION: &str =
    "infer.vision.image-description-prompt@20260811.1";
pub const CLASSIFICATION_REVIEW_SCHEMA_REVISION: &str =
    "infer.vision.classification-review@20260811.1";
pub const CLASSIFICATION_REVIEW_PROMPT_REVISION: &str =
    "infer.vision.classification-review-prompt@20260811.1";

#[derive(Clone)]
pub struct ImageDescriptionRequest {
    pub model: String,
    pub image: VisionImage,
    /// Stable identity of the exact orientation-normalized encoded artifact.
    pub source_revision: String,
    pub image_orientation: String,
    /// BCP-47-shaped language requested for the generated description and keywords.
    pub language: String,
    pub metadata: BTreeMap<String, String>,
}

impl ImageDescriptionRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_understanding_image(
            &self.model,
            &self.image,
            &self.source_revision,
            &self.image_orientation,
            &self.metadata,
            "image description",
        )?;
        validate_language(&self.language)
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Clone)]
pub struct ClassificationReviewRequest {
    pub model: String,
    pub image: VisionImage,
    /// Stable identity of the exact orientation-normalized encoded artifact.
    pub source_revision: String,
    pub image_orientation: String,
    /// Revision of the Consumer-owned closed set supplied with this request.
    pub taxonomy_revision: String,
    pub categories: Vec<ClassificationCategory>,
    pub metadata: BTreeMap<String, String>,
}

impl ClassificationReviewRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_understanding_image(
            &self.model,
            &self.image,
            &self.source_revision,
            &self.image_orientation,
            &self.metadata,
            "classification review",
        )?;
        validate_revision(&self.taxonomy_revision, "taxonomy_revision")?;
        if self.categories.is_empty() || self.categories.len() > MAX_CLASSIFICATION_CANDIDATES {
            return Err(ContractError::InvalidVision(format!(
                "classification review requires between 1 and {MAX_CLASSIFICATION_CANDIDATES} categories"
            )));
        }
        let mut ids = BTreeSet::new();
        for category in &self.categories {
            category.validate()?;
            if !ids.insert(category.id.as_str()) {
                return Err(ContractError::InvalidVision(format!(
                    "classification category id `{}` is duplicated",
                    category.id
                )));
            }
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

fn validate_understanding_image(
    model: &str,
    image: &VisionImage,
    source_revision: &str,
    image_orientation: &str,
    metadata: &BTreeMap<String, String>,
    operation: &str,
) -> Result<(), ContractError> {
    validate_local_vision_request(model, image, source_revision, metadata, operation)?;
    if image_orientation != VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS {
        return Err(ContractError::InvalidVision(format!(
            "{operation} requires image_orientation={VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClassificationCategory {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ClassificationCategory {
    fn validate(&self) -> Result<(), ContractError> {
        validate_bounded_text(
            &self.id,
            "classification category id",
            MAX_CATEGORY_ID_BYTES,
        )?;
        validate_bounded_text(
            &self.name,
            "classification category name",
            MAX_CATEGORY_NAME_BYTES,
        )?;
        if let Some(description) = &self.description {
            validate_bounded_text(
                description,
                "classification category description",
                MAX_CATEGORY_DESCRIPTION_BYTES,
            )?;
        }
        Ok(())
    }
}

fn validate_bounded_text(
    value: &str,
    field: &str,
    maximum_bytes: usize,
) -> Result<(), ContractError> {
    if value.trim().is_empty() || value.len() > maximum_bytes || value.contains('\0') {
        return Err(ContractError::InvalidVision(format!(
            "{field} is required, must not contain NUL, and must not exceed {maximum_bytes} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_language(language: &str) -> Result<(), ContractError> {
    if language.is_empty()
        || language.len() > 35
        || !language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ContractError::InvalidVision(
            "language must be a BCP-47-shaped tag no longer than 35 ASCII bytes".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageDescriptionResult {
    pub description: String,
    pub keyword_suggestions: Vec<String>,
}

impl ImageDescriptionResult {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_bounded_text(
            &self.description,
            "image description",
            MAX_IMAGE_DESCRIPTION_BYTES,
        )?;
        if self.keyword_suggestions.len() > MAX_KEYWORD_SUGGESTIONS {
            return Err(ContractError::InvalidVision(format!(
                "image description returned more than {MAX_KEYWORD_SUGGESTIONS} keyword suggestions"
            )));
        }
        let mut normalized = BTreeSet::new();
        for keyword in &self.keyword_suggestions {
            validate_bounded_text(keyword, "keyword suggestion", MAX_KEYWORD_BYTES)?;
            if !normalized.insert(keyword.trim().to_lowercase()) {
                return Err(ContractError::InvalidVision(
                    "keyword suggestions must be unique after case folding".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationDisposition {
    Matched,
    None,
    Uncertain,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClassificationSuggestion {
    pub disposition: ClassificationDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
}

impl ClassificationSuggestion {
    pub fn validate_against(
        &self,
        categories: &[ClassificationCategory],
    ) -> Result<(), ContractError> {
        match self.disposition {
            ClassificationDisposition::Matched => {
                let category_id = self.category_id.as_deref().ok_or_else(|| {
                    ContractError::InvalidVision(
                        "matched classification suggestion omitted category_id".into(),
                    )
                })?;
                if !categories
                    .iter()
                    .any(|candidate| candidate.id == category_id)
                {
                    return Err(ContractError::InvalidVision(
                        "matched classification suggestion returned an id outside the supplied closed set"
                            .into(),
                    ));
                }
            }
            ClassificationDisposition::None | ClassificationDisposition::Uncertain => {
                if self.category_id.is_some() {
                    return Err(ContractError::InvalidVision(
                        "none/uncertain classification suggestion must omit category_id".into(),
                    ));
                }
            }
        }
        Ok(())
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_eval_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn local_metadata() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ])
    }

    #[test]
    fn classification_closed_set_rejects_duplicates_and_foreign_results() {
        let category = ClassificationCategory {
            id: "travel".into(),
            name: "Travel".into(),
            description: None,
        };
        let request = ClassificationReviewRequest {
            model: "vision.review_classification".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: vec![1],
            },
            source_revision: "photo:1".into(),
            image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            taxonomy_revision: "taxonomy:1".into(),
            categories: vec![category.clone(), category.clone()],
            metadata: local_metadata(),
        };
        assert!(request.validate().is_err());

        let suggestion = ClassificationSuggestion {
            disposition: ClassificationDisposition::Matched,
            category_id: Some("other".into()),
        };
        assert!(suggestion.validate_against(&[category]).is_err());
    }

    #[test]
    fn image_understanding_is_fixed_to_normalized_local_pixels() {
        let request = ImageDescriptionRequest {
            model: "vision.describe_image".into(),
            image: VisionImage {
                content_type: "image/jpeg".into(),
                bytes: vec![1],
            },
            source_revision: "photo:2".into(),
            image_orientation: "input_pixels_no_exif_transform".into(),
            language: "zh-CN".into(),
            metadata: local_metadata(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn semantic_output_is_bounded_and_deduplicated() {
        let result = ImageDescriptionResult {
            description: "A street at night".into(),
            keyword_suggestions: vec!["Night".into(), "night".into()],
        };
        assert!(result.validate().is_err());
    }
}
