//! Apple-managed native images over the negotiated Consumer API.
//! This module has no dependency on Apple frameworks; execution belongs to Infer.
use crate::{Client, Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
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
    pub fn validate(&self) -> Result<()> {
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
            Err(Error::Input("invalid Apple image options".into()))
        }
    }
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
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty()
            || self.model.len() > 256
            || self.source_revision.trim().is_empty()
            || self.source_revision.len() > 256
        {
            return Err(Error::Input(
                "model and source_revision are required and bounded".into(),
            ));
        }
        self.options.validate()?;
        if self
            .metadata
            .get("infer.placement")
            .is_some_and(|v| v != "local_only" && v != "private")
            || self
                .metadata
                .get("infer.offline_required")
                .is_some_and(|v| v != "true")
            || self
                .metadata
                .get("infer.fallback")
                .is_some_and(|v| v != "none")
        {
            return Err(Error::Input(
                "Apple images require local_only/private, offline_required=true and fallback=none"
                    .into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleImageRaster {
    pub width: u32,
    pub height: u32,
    pub content_type: String,
    pub data_base64: String,
    pub sha256: String,
    pub semantics: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleOcrLine {
    pub text: String,
    pub confidence: f32,
    pub r#box: AppleImageBox,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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

pub const APPLE_IMAGE_CAPABILITIES: &[&str] = &[APPLE_IMAGE_CONTRACT];
pub const APPLE_IMAGE_ENDPOINT: &str = "/infer/v1/vision/apple-images";
/// Encoded preview limit of the current paired-node transport (RAW is local only).
pub const MAX_APPLE_NODE_INPUT_BYTES: usize = 192 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_APPLE_OUTPUT_BYTES.div_ceil(3) * 4 + 64 * 1024;

impl Client {
    /// Request a native operation using an encoded file. No host path is transmitted.
    pub async fn apple_image(
        &self,
        image: &Path,
        parameters: &AppleImageParameters,
    ) -> Result<AppleImageResponse> {
        parameters.validate()?;
        let bytes =
            crate::transport::read_bounded_file(image, MAX_APPLE_IMAGE_BYTES as u64, "image")
                .await?;
        self.apple_image_bytes(&bytes, parameters).await
    }

    /// Send an explicitly prepared preview or RAW. This does not resize or rotate it.
    /// Drop the future to cancel the HTTP waiter and its non-durable native job.
    pub async fn apple_image_bytes(
        &self,
        bytes: &[u8],
        parameters: &AppleImageParameters,
    ) -> Result<AppleImageResponse> {
        parameters.validate()?;
        if bytes.is_empty() || bytes.len() > MAX_APPLE_IMAGE_BYTES {
            return Err(Error::Input("Apple image exceeds byte limit".into()));
        }
        let mut wire = parameters.clone();
        for (key, value) in [
            ("infer.placement", "local_only"),
            ("infer.offline_required", "true"),
            ("infer.fallback", "none"),
        ] {
            wire.metadata
                .entry(key.into())
                .or_insert_with(|| value.into());
        }
        let request = serde_json::to_string(&wire).map_err(|e| Error::Input(e.to_string()))?;
        if request.len() > 16 * 1024 {
            return Err(Error::Input(
                "Apple image request exceeds JSON limit".into(),
            ));
        }
        let response = self
            .send_capability_with(APPLE_IMAGE_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}{APPLE_IMAGE_ENDPOINT}"))
                    .multipart(
                        Form::new()
                            .text("request", request.clone())
                            .part("image", Part::bytes(bytes.to_vec()).file_name("image.bin")),
                    )
            })
            .await?;
        let response = crate::transport::ensure_success(response).await?;
        let body = crate::transport::read_bounded(response, MAX_RESPONSE_BYTES).await?;
        let result: AppleImageResponse =
            serde_json::from_slice(&body).map_err(|_| malformed("invalid Apple image response"))?;
        result.validate_for(parameters)?;
        Ok(result)
    }
}

impl AppleImageRaster {
    /// Verify the encoded PNG's bound, digest and IHDR geometry before use.
    /// Consumers still decode PNG pixels with their normal image decoder.
    pub fn png_bytes(&self) -> Result<Vec<u8>> {
        if self.content_type != "image/png"
            || self.data_base64.len() > MAX_APPLE_OUTPUT_BYTES.div_ceil(3) * 4
        {
            return Err(malformed("invalid PNG encoding or size"));
        }
        let bytes = STANDARD
            .decode(&self.data_base64)
            .map_err(|_| malformed("invalid PNG base64"))?;
        if bytes.len() < 33
            || bytes.len() > MAX_APPLE_OUTPUT_BYTES
            || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
            || &bytes[8..16] != b"\0\0\0\rIHDR"
            || u32::from_be_bytes(bytes[16..20].try_into().unwrap()) != self.width
            || u32::from_be_bytes(bytes[20..24].try_into().unwrap()) != self.height
            || !valid_dimensions(self.width, self.height)
            || format!("{:x}", Sha256::digest(&bytes)) != self.sha256
        {
            return Err(malformed("PNG digest or geometry mismatch"));
        }
        Ok(bytes)
    }
}
impl AppleImageResponse {
    pub fn validate_for(&self, request: &AppleImageParameters) -> Result<()> {
        let p = &self.provenance;
        if self.source_revision != request.source_revision
            || [
                &self.id,
                &self.provider,
                &self.deployment,
                &self.model_build,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256)
            || p.os_version.is_empty()
            || p.os_version.len() > 256
            || p.worker_protocol != "infer.apple-image-worker@20260926.2"
            || p.execution_location != "device"
            || p.worker_sha256.len() != 64
            || !p.worker_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || p.model_ownership != "apple_os_managed_weights_not_exposed"
            || p.execution_node
                .as_ref()
                .is_some_and(|n| n.is_empty() || n.len() > 256)
            || p.request_revision.as_ref().is_some_and(|r| r.len() > 128)
            || (request
                .metadata
                .get("infer.placement")
                .is_none_or(|v| v == "local_only")
                && p.execution_node.is_some())
        {
            return Err(malformed("Apple image identity or provenance mismatch"));
        }
        match (&request.options, &self.result) {
            (
                AppleImageOperation::Segment { .. },
                AppleImageResult::Segment { raster, confidence },
            ) => {
                if !finite_unit((*confidence).into())
                    || raster.semantics != "apple_vision_iterative_mask"
                {
                    return Err(malformed("invalid native mask"));
                }
                raster.png_bytes()?;
            }
            (
                AppleImageOperation::RawRender { .. },
                AppleImageResult::RawRender {
                    raster,
                    decoder_version,
                },
            ) => {
                if !matches!(decoder_version.as_str(), "9" | "9.dng")
                    || raster.semantics != "display_referred_srgb_8bit"
                    || p.execution_node.is_some()
                {
                    return Err(malformed("invalid RAW 9 result"));
                }
                raster.png_bytes()?;
            }
            (
                AppleImageOperation::Ocr {},
                AppleImageResult::Ocr {
                    width,
                    height,
                    lines,
                },
            ) => {
                if !valid_dimensions(*width, *height)
                    || lines.len() > 512
                    || lines.iter().any(|l| {
                        let b = &l.r#box;
                        l.text.len() > 8192
                            || !finite_unit(l.confidence.into())
                            || [b.x, b.y, b.width, b.height]
                                .iter()
                                .any(|v| !finite_unit(*v))
                            || b.width <= 0.0
                            || b.height <= 0.0
                            || b.x + b.width > 1.000001
                            || b.y + b.height > 1.000001
                    })
                {
                    return Err(malformed("invalid OCR result"));
                }
            }
            (
                AppleImageOperation::Aesthetics {},
                AppleImageResult::Aesthetics { overall_score, .. },
            ) if overall_score.is_finite() && (-1.0..=1.0).contains(overall_score) => {}
            _ => return Err(malformed("Apple image operation mismatch")),
        }
        Ok(())
    }
}
fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 80_000_000
}
fn malformed(message: &str) -> Error {
    Error::MalformedResponse(message.into())
}
#[cfg(test)]
mod tests;
