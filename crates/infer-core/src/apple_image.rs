//! Explicit Apple native image operations. OS-managed models are not pinned model artifacts.
use crate::{ContractError, Fallback, PlacementScope, RequestConstraints};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub const APPLE_IMAGE_CONTRACT: &str = "infer.vision.apple-native@20260926.2";
pub const MAX_APPLE_IMAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_APPLE_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImagePoint {
    pub x: f64,
    pub y: f64,
    pub include: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppleImageOperation {
    Segment {
        points: Vec<AppleImagePoint>,
        #[serde(default)]
        box_prompt: Option<AppleImageBox>,
    },
    RawRender {
        exposure: f32,
        noise_reduction: f32,
    },
    Ocr {},
    Aesthetics {},
}
impl AppleImageOperation {
    pub fn model_id(&self) -> &'static str {
        match self {
            Self::Segment { .. } => "segment",
            Self::RawRender { .. } => "raw_render",
            Self::Ocr {} => "ocr",
            Self::Aesthetics {} => "aesthetics",
        }
    }
    pub fn data_plane(&self) -> &'static str {
        match self {
            Self::Segment { .. } => "vision.apple_segmentation",
            Self::RawRender { .. } => "image.apple_raw_render",
            Self::Ocr {} => "vision.apple_ocr",
            Self::Aesthetics {} => "vision.apple_aesthetics",
        }
    }
    pub fn validate(&self) -> Result<(), ContractError> {
        let valid = match self {
            Self::Segment { points, box_prompt } => {
                points.len() <= 16
                    && (box_prompt.is_some() || points.iter().any(|p| p.include))
                    && points.iter().all(|p| finite_unit(p.x) && finite_unit(p.y))
                    && box_prompt.as_ref().is_none_or(|b| {
                        finite_unit(b.x)
                            && finite_unit(b.y)
                            && finite_unit(b.width)
                            && b.width > 0.0
                            && finite_unit(b.height)
                            && b.height > 0.0
                            && b.x + b.width <= 1.0
                            && b.y + b.height <= 1.0
                    })
            }
            Self::RawRender {
                exposure,
                noise_reduction,
            } => {
                exposure.is_finite()
                    && (-5.0..=5.0).contains(exposure)
                    && noise_reduction.is_finite()
                    && (0.0..=1.0).contains(noise_reduction)
            }
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(ContractError::InvalidVision(
                "invalid Apple image options".into(),
            ))
        }
    }
}
pub fn is_apple_image_plane(plane: &str) -> bool {
    matches!(
        plane,
        "vision.apple_segmentation" | "vision.apple_ocr" | "vision.apple_aesthetics"
    )
}
fn finite_unit(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageParameters {
    pub model: String,
    pub source_revision: String,
    pub options: AppleImageOperation,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}
impl AppleImageParameters {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty()
            || self.model.len() > 256
            || self.source_revision.trim().is_empty()
            || self.source_revision.len() > 256
        {
            return Err(ContractError::InvalidVision(
                "model and source_revision are required and bounded".into(),
            ));
        }
        self.options.validate()?;
        let constraints = RequestConstraints::from_metadata(&self.metadata)?;
        if !matches!(
            constraints.placement,
            Some(PlacementScope::LocalOnly | PlacementScope::Private)
        ) || constraints.offline_required != Some(true)
            || constraints.fallback != Some(Fallback::None)
        {
            return Err(ContractError::InvalidVision("Apple native images require local_only/private, offline_required and fallback=none".into()));
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct AppleImageRequest {
    pub parameters: AppleImageParameters,
    pub bytes: Vec<u8>,
}
impl AppleImageRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.parameters.validate()?;
        if self.bytes.is_empty() || self.bytes.len() > MAX_APPLE_IMAGE_BYTES {
            return Err(ContractError::InvalidVision(
                "Apple image input exceeds byte limit".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageRaster {
    pub width: u32,
    pub height: u32,
    pub content_type: String,
    pub data_base64: String,
    pub sha256: String,
    pub semantics: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleOcrLine {
    pub text: String,
    pub confidence: f32,
    pub r#box: AppleImageBox,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppleImageResult {
    Segment {
        raster: AppleImageRaster,
        confidence: f32,
    },
    RawRender {
        raster: AppleImageRaster,
        decoder_version: String,
    },
    Ocr {
        width: u32,
        height: u32,
        lines: Vec<AppleOcrLine>,
    },
    Aesthetics {
        overall_score: f32,
        is_utility: bool,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageProvenance {
    pub os_version: String,
    pub worker_protocol: String,
    pub worker_sha256: String,
    pub execution_location: String,
    pub execution_node: Option<String>,
    pub elapsed_ms: u64,
    /// Apple owns system model assets. Their weight digest is not exposed.
    pub model_ownership: String,
    pub request_revision: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageResponse {
    pub id: String,
    pub source_revision: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub result: AppleImageResult,
    pub provenance: AppleImageProvenance,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_removed_description_wire_types() {
        assert!(
            serde_json::from_str::<AppleImageOperation>(
                r#"{"operation":"describe","prompt":"What is here?"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AppleImageResult>(
                r#"{"operation":"describe","text":"Unsupported"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn rejects_removed_description_registration() {
        assert!(!is_apple_image_plane("vision.apple_description"));
        let mut config: crate::RuntimeConfig =
            toml::from_str(include_str!("../../../config/apple-image.example.toml")).unwrap();
        config.model_builds.get_mut("apple_ocr").unwrap().model_id = "describe".into();
        assert!(config.validate().is_err());
        config.model_builds.get_mut("apple_ocr").unwrap().model_id = "ocr".into();
        config.intents.get_mut("apple.ocr").unwrap().data_plane = "vision.apple_description".into();
        assert!(config.validate().is_err());
    }
    #[test]
    fn apple_registry_binds_operation_and_provider() {
        let mut config: crate::RuntimeConfig =
            toml::from_str(include_str!("../../../config/apple-image.example.toml")).unwrap();
        config.validate().unwrap();
        config.model_builds.get_mut("apple_ocr").unwrap().model_id = "segment".into();
        assert!(config.validate().is_err());
        config.model_builds.get_mut("apple_ocr").unwrap().model_id = "ocr".into();
        config
            .model_builds
            .get_mut("apple_ocr")
            .unwrap()
            .input_modalities = vec![crate::Modality::Text];
        assert!(config.validate().is_err());
    }
    #[test]
    fn rejects_invalid_geometry_and_operation_options() {
        assert!(
            AppleImageOperation::Segment {
                points: vec![],
                box_prompt: None
            }
            .validate()
            .is_err()
        );
        assert!(
            AppleImageOperation::Segment {
                points: vec![AppleImagePoint {
                    x: 0.5,
                    y: f64::NAN,
                    include: true
                }],
                box_prompt: None
            }
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_str::<AppleImageOperation>(r#"{"operation":"ocr","prompt":"bad"}"#)
                .is_err()
        );
        assert!(
            AppleImageOperation::RawRender {
                exposure: 0.0,
                noise_reduction: 1.0
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn requires_explicit_offline_local_execution() {
        let mut p = AppleImageParameters {
            model: "photo.ocr".into(),
            source_revision: "r1".into(),
            options: AppleImageOperation::Ocr {},
            metadata: BTreeMap::new(),
        };
        assert!(p.validate().is_err());
        for (k, v) in [
            ("infer.placement", "local_only"),
            ("infer.offline_required", "true"),
            ("infer.fallback", "none"),
        ] {
            p.metadata.insert(k.into(), v.into());
        }
        assert!(p.validate().is_ok());
        p.metadata
            .insert("infer.placement".into(), "anywhere".into());
        assert!(p.validate().is_err());
    }
}
