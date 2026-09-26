//! Single-request native worker bridge. Dropping execution kills the process and releases scratch files.
use crate::ProviderError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use infer_core::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;
const PROTOCOL: &str = "infer.apple-image-worker@20260926.2";
pub struct AppleImageExecutor {
    command: String,
    args: Vec<String>,
    digest: String,
}
pub struct AppleImageExecution {
    pub result: AppleImageResult,
    pub provenance: AppleImageProvenance,
}
impl AppleImageExecutor {
    pub async fn execute_on_node(
        peer: infer_core::NodePeerConfig,
        context: crate::ProviderAttemptContext,
        deployment: String,
        request: AppleImageRequest,
    ) -> Result<AppleImageExecution, ProviderError> {
        let source = request.parameters.source_revision.clone();
        let operation = request.parameters.options.model_id();
        let node_id = peer.node_id.clone();
        let client =
            infer_node::NodeClient::new_images(peer).map_err(crate::trusted_node::map_error)?;
        let result = client
            .execute_image(
                infer_node::NodeAttempt {
                    key: infer_node::TaskKey {
                        job_id: context.job_id,
                        attempt: context.attempt,
                    },
                    app_id: context.app_id,
                    intent: context.intent,
                },
                deployment,
                infer_node::AppleNodeRequest {
                    parameters: request.parameters,
                    bytes: request.bytes,
                },
                context.remaining,
            )
            .await
            .map_err(crate::trusted_node::map_error)?;
        let mut response: AppleImageResponse = serde_json::from_value(result)?;
        let output_operation = match &response.result {
            AppleImageResult::Segment { .. } => "segment",
            AppleImageResult::RawRender { .. } => "raw_render",
            AppleImageResult::Ocr { .. } => "ocr",
            AppleImageResult::Aesthetics { .. } => "aesthetics",
        };
        if response.source_revision != source
            || output_operation != operation
            || response.provenance.execution_location != "device"
            || response.provenance.worker_protocol != PROTOCOL
        {
            return Err(invalid("native node response identity mismatch"));
        }
        validate_remote_response(&response)?;
        response.provenance.execution_node = Some(node_id);
        Ok(AppleImageExecution {
            result: response.result,
            provenance: response.provenance,
        })
    }

    pub fn new(command: String, args: Vec<String>) -> Result<Self, ProviderError> {
        if !cfg!(target_os = "macos") {
            return Err(ProviderError::InvalidInput(
                "Apple images require macOS 27".into(),
            ));
        }
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&command)?));
        Ok(Self {
            command,
            args,
            digest,
        })
    }
    pub async fn execute(
        &self,
        physical_model: &str,
        request: AppleImageRequest,
        cancellation: CancellationToken,
    ) -> Result<AppleImageExecution, ProviderError> {
        request
            .validate()
            .map_err(|_| invalid("invalid Apple image request"))?;
        if physical_model != request.parameters.options.model_id() {
            return Err(invalid("Apple deployment operation mismatch"));
        }
        if cancellation.is_cancelled() {
            return Err(invalid("apple_execution_cancelled"));
        }
        if format!(
            "{:x}",
            Sha256::digest(tokio::fs::read(&self.command).await?)
        ) != self.digest
        {
            return Err(invalid("Apple worker identity changed; restart required"));
        }
        let scratch = tempfile::TempDir::new()?;
        let input = scratch.path().join("input");
        let output = scratch.path().join("output.png");
        tokio::fs::write(&input, &request.bytes).await?;
        let mut payload = serde_json::to_value(&request.parameters.options)?;
        payload["input_path"] = json!(input);
        payload["output_path"] = json!(output);
        if let Some(b) = payload.as_object_mut().and_then(|o| o.remove("box_prompt")) {
            payload["box"] = b;
        }
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("missing worker stdin"))?;
        stdin.write_all(&serde_json::to_vec(&payload)?).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing worker stdout"))?
            .take(2 * 1024 * 1024 + 1);
        let mut bytes = Vec::new();
        tokio::select! {
            biased;
            _=cancellation.cancelled()=>{let _=child.kill().await;return Err(invalid("apple_execution_cancelled"));},
            read=stdout.read_to_end(&mut bytes)=>{read?;}
        }
        if bytes.len() > 2 * 1024 * 1024 {
            let _ = child.kill().await;
            return Err(invalid("Apple worker response exceeds limit"));
        }
        let status = tokio::select! {
            biased;
            _=cancellation.cancelled()=>{let _=child.kill().await;return Err(invalid("apple_execution_cancelled"));},
            status=child.wait()=>status?,
        };
        if !status.success() {
            return Err(invalid("Apple worker exited unsuccessfully"));
        }
        let envelope: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid Apple worker response"))?;
        if envelope["protocol"] != PROTOCOL || envelope["execution_location"] != "device" {
            return Err(invalid("Apple worker contract mismatch"));
        }
        if envelope["ok"] != true {
            if matches!(
                envelope["error"].as_str(),
                Some("unavailable" | "assets_not_ready")
            ) {
                // Unavailable OS assets are operation-scoped. Reuse the
                // model-unavailable path to preserve other operations' health.
                return Err(ProviderError::ModelMissing {
                    model: physical_model.into(),
                });
            }

            // Input/camera failures must not degrade the shared provider health.
            match envelope["error"].as_str() {
                Some("unsupported_raw9") => {
                    return Err(ProviderError::InvalidInput(
                        "apple_raw9_unsupported_input".into(),
                    ));
                }
                Some("pixel_limit" | "invalid_image") => {
                    return Err(ProviderError::InvalidInput(
                        "apple_image_invalid_input".into(),
                    ));
                }
                _ => return Err(invalid("apple_image_execution_failed")),
            }
        }
        let value = &envelope["result"];
        let result = match &request.parameters.options {
            AppleImageOperation::Segment { .. } => AppleImageResult::Segment {
                raster: read_raster(&output, value, "apple_vision_iterative_mask").await?,
                confidence: bounded_float(value, "confidence", 0.0, 1.0)?,
            },
            AppleImageOperation::RawRender { .. } => {
                let decoder_version = bounded_string(value, "decoder_version", 128)?;
                if !matches!(decoder_version.as_str(), "9" | "9.dng") {
                    return Err(invalid("native RAW decoder version mismatch"));
                }
                AppleImageResult::RawRender {
                    raster: read_raster(&output, value, "display_referred_srgb_8bit").await?,
                    decoder_version,
                }
            }
            AppleImageOperation::Ocr {} => {
                let lines: Vec<AppleOcrLine> = serde_json::from_value(value["lines"].clone())?;
                if lines.len() > 512
                    || lines.iter().any(|l| {
                        l.text.len() > 8192
                            || !l.confidence.is_finite()
                            || !(0.0..=1.0).contains(&l.confidence)
                            || [l.r#box.x, l.r#box.y, l.r#box.width, l.r#box.height]
                                .iter()
                                .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
                    })
                {
                    return Err(invalid("invalid Apple OCR result"));
                }
                AppleImageResult::Ocr {
                    width: dimension(value, "width")?,
                    height: dimension(value, "height")?,
                    lines,
                }
            }
            AppleImageOperation::Aesthetics {} => AppleImageResult::Aesthetics {
                overall_score: bounded_float(value, "overall_score", -1.0, 1.0)?,
                is_utility: value["is_utility"]
                    .as_bool()
                    .ok_or_else(|| invalid("invalid aesthetics result"))?,
            },
        };
        Ok(AppleImageExecution {
            result,
            provenance: AppleImageProvenance {
                os_version: bounded_string(&envelope, "os_version", 256)?,
                worker_protocol: PROTOCOL.into(),
                worker_sha256: self.digest.clone(),
                execution_location: "device".into(),
                execution_node: None,
                elapsed_ms: envelope["elapsed_ms"]
                    .as_u64()
                    .ok_or_else(|| invalid("invalid execution timing"))?,
                model_ownership: "apple_os_managed_weights_not_exposed".into(),
                request_revision: value.get("request_revision").map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                }),
            },
        })
    }
}
// A paired device is an authenticated executor; still validate its result contract.
fn validate_remote_response(response: &AppleImageResponse) -> Result<(), ProviderError> {
    let p = &response.provenance;
    if p.os_version.is_empty()
        || p.os_version.len() > 256
        || !valid_node_digest(&p.worker_sha256)
        || p.model_ownership != "apple_os_managed_weights_not_exposed"
        || p.request_revision.as_ref().is_some_and(|s| s.len() > 128)
        || p.execution_node.is_some()
    {
        return Err(invalid("invalid native node provenance"));
    }
    match &response.result {
        AppleImageResult::Segment { raster, confidence } => {
            if !confidence.is_finite()
                || !(0.0..=1.0).contains(confidence)
                || raster.content_type != "image/png"
                || raster.semantics != "apple_vision_iterative_mask"
            {
                return Err(invalid("invalid remote mask"));
            }
            let bytes = STANDARD
                .decode(&raster.data_base64)
                .map_err(|_| invalid("invalid remote mask encoding"))?;
            if format!("{:x}", Sha256::digest(&bytes)) != raster.sha256 {
                return Err(invalid("remote mask digest mismatch"));
            }
            let dims = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            )
            .into_dimensions()
            .map_err(|_| invalid("invalid remote PNG"))?;
            if dims != (raster.width, raster.height)
                || dims.0 == 0
                || dims.1 == 0
                || u64::from(dims.0) * u64::from(dims.1) > 80_000_000
            {
                return Err(invalid("invalid remote mask geometry"));
            }
        }
        AppleImageResult::Ocr {
            width,
            height,
            lines,
        } => {
            if *width == 0
                || *height == 0
                || u64::from(*width) * u64::from(*height) > 80_000_000
                || lines.len() > 512
                || lines.iter().any(|l| {
                    l.text.len() > 8192
                        || !l.confidence.is_finite()
                        || !(0.0..=1.0).contains(&l.confidence)
                        || [l.r#box.x, l.r#box.y, l.r#box.width, l.r#box.height]
                            .iter()
                            .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
                        || l.r#box.width <= 0.0
                        || l.r#box.height <= 0.0
                        || l.r#box.x + l.r#box.width > 1.000001
                        || l.r#box.y + l.r#box.height > 1.000001
                })
            {
                return Err(invalid("invalid remote OCR result"));
            }
        }
        AppleImageResult::Aesthetics { overall_score, .. }
            if overall_score.is_finite() && (-1.0..=1.0).contains(overall_score) => {}
        _ => return Err(invalid("invalid or unsupported remote image result")),
    }
    Ok(())
}
fn invalid(message: &str) -> ProviderError {
    ProviderError::Protocol(message.into())
}
fn dimension(value: &Value, key: &str) -> Result<u32, ProviderError> {
    let n = value[key]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 80_000_000)
        .ok_or_else(|| invalid("invalid native raster dimensions"))?;
    Ok(n as u32)
}
fn bounded_string(value: &Value, key: &str, max: usize) -> Result<String, ProviderError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max)
        .map(str::to_owned)
        .ok_or_else(|| invalid("invalid native result string"))
}
fn bounded_float(value: &Value, key: &str, min: f64, max: f64) -> Result<f32, ProviderError> {
    value[key]
        .as_f64()
        .filter(|n| n.is_finite() && (min..=max).contains(n))
        .map(|v| v as f32)
        .ok_or_else(|| invalid("invalid native result score"))
}
async fn read_raster(
    path: &PathBuf,
    value: &Value,
    semantics: &str,
) -> Result<AppleImageRaster, ProviderError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_APPLE_OUTPUT_BYTES as u64
    {
        return Err(invalid("native raster exceeds limit"));
    }
    let bytes = tokio::fs::read(path).await?;
    let dimensions =
        image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|_| invalid("invalid native PNG"))?;
    if dimensions != (dimension(value, "width")?, dimension(value, "height")?)
        || u64::from(dimensions.0) * u64::from(dimensions.1) > 80_000_000
    {
        return Err(invalid("native raster geometry mismatch"));
    }
    Ok(AppleImageRaster {
        width: dimensions.0,
        height: dimensions.1,
        content_type: "image/png".into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        data_base64: STANDARD.encode(bytes),
        semantics: semantics.into(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nonfinite_or_out_of_range_scores() {
        assert!(bounded_float(&json!({"score":1.1}), "score", 0.0, 1.0).is_err());
    }
    #[tokio::test]
    async fn rejects_mismatched_native_raster() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("out.png");
        image::GrayImage::new(2, 3).save(&path).unwrap();
        assert!(
            read_raster(&path, &json!({"width":3,"height":2}), "mask")
                .await
                .is_err()
        );
        assert!(
            read_raster(&path, &json!({"width":2,"height":3}), "mask")
                .await
                .is_ok()
        );
    }
}
