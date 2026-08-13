//! Persistent, cancellation-safe bridge for Apple CoreML SAM 2.1 models.
//!
//! The worker owns CoreML specialization and a single bounded image-embedding
//! cache. This Rust owner keeps image and mask files ephemeral, validates the
//! returned binary mask, and never exposes worker paths or diagnostics.

use std::{collections::BTreeMap, io, process::Stdio, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::ImageFormat;
use infer_core::{
    BoundingBox, EncodedSegmentationMask, EncodedSoftSegmentationMask, ImageGeometry,
    MAX_SEGMENTATION_MASK_BYTES, MAX_VISION_IMAGE_PIXELS, NormalizedBoundingBox,
    SUBJECT_SEGMENTATION_SOFT_MASK_HEIGHT, SUBJECT_SEGMENTATION_SOFT_MASK_WIDTH,
    SegmentationPromptPoint, SubjectSegmentationRequest,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{ProviderError, onnx::VisionExecutionProvenance};

const MAX_WORKER_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct SamBuildContract {
    /// Private resolved artifact directory. Never copied into public output.
    pub model_path: String,
    pub model_build: String,
    pub artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub runtime: String,
    pub precision: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
}

#[derive(Debug)]
pub struct SubjectSegmentationExecutionOutput {
    pub image: ImageGeometry,
    pub mask: EncodedSegmentationMask,
    pub score: f32,
    pub provenance: VisionExecutionProvenance,
}

#[derive(Debug)]
pub struct SubjectSegmentationSoftMaskExecutionOutput {
    pub input_coordinate_extent: ImageGeometry,
    pub mask: EncodedSoftSegmentationMask,
    pub score: f32,
    pub provenance: VisionExecutionProvenance,
}

#[async_trait]
pub trait SubjectSegmentationExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn segment_subject(
        &self,
        physical_model: &str,
        request: SubjectSegmentationRequest,
        cancellation: CancellationToken,
    ) -> Result<SubjectSegmentationExecutionOutput, ProviderError>;

    async fn segment_subject_soft_mask(
        &self,
        physical_model: &str,
        request: SubjectSegmentationRequest,
        cancellation: CancellationToken,
    ) -> Result<SubjectSegmentationSoftMaskExecutionOutput, ProviderError>;
}

pub type DynSubjectSegmentationExecutor = Arc<dyn SubjectSegmentationExecutor>;

pub struct CoremlSamExecutor {
    id: String,
    command: String,
    args: Vec<String>,
    builds: BTreeMap<String, SamBuildContract>,
    process: Arc<Mutex<Option<WorkerProcess>>>,
}

impl CoremlSamExecutor {
    pub fn new(
        id: impl Into<String>,
        command: String,
        args: Vec<String>,
        builds: BTreeMap<String, SamBuildContract>,
    ) -> Self {
        Self {
            id: id.into(),
            command,
            args,
            builds,
            process: Arc::new(Mutex::new(None)),
        }
    }

    async fn round_trip(
        &self,
        request: &WorkerRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkerResult, ProviderError> {
        let mut guard = self.process.lock().await;
        if guard
            .as_mut()
            .is_some_and(|process| process.child.try_wait().ok().flatten().is_some())
        {
            *guard = None;
        }
        if guard.is_none() {
            *guard = Some(spawn_worker(&self.command, &self.args).await?);
        }
        let process = guard.as_mut().expect("worker was initialized");
        let line = serde_json::to_vec(request)?;
        if line.len() > MAX_WORKER_RESPONSE_BYTES {
            return Err(ProviderError::Protocol(
                "SAM worker request exceeds the frame limit".into(),
            ));
        }
        process.stdin.write_all(&line).await?;
        process.stdin.write_all(b"\n").await?;
        process.stdin.flush().await?;

        let response_line = {
            let read = read_bounded_line(&mut process.stdout, MAX_WORKER_RESPONSE_BYTES);
            tokio::pin!(read);
            tokio::select! {
                result = &mut read => Some(result),
                _ = cancellation.cancelled() => None,
            }
        };
        let Some(response_line) = response_line else {
            terminate(process).await;
            *guard = None;
            return Err(ProviderError::Protocol("sam_execution_cancelled".into()));
        };
        let response_line = match response_line {
            Ok(Some(line)) => line,
            Ok(None) => {
                *guard = None;
                return Err(ProviderError::Protocol(
                    "SAM worker exited without a response".into(),
                ));
            }
            Err(_) => {
                terminate(process).await;
                *guard = None;
                return Err(ProviderError::Protocol(
                    "SAM worker response exceeded the frame limit".into(),
                ));
            }
        };
        let response: WorkerResponse = match serde_json::from_slice(&response_line) {
            Ok(response) => response,
            Err(_) => {
                terminate(process).await;
                *guard = None;
                return Err(ProviderError::Protocol(
                    "SAM worker returned malformed JSON".into(),
                ));
            }
        };
        if response.request_id != request.request_id {
            terminate(process).await;
            *guard = None;
            return Err(ProviderError::Protocol(
                "SAM worker response identity mismatch".into(),
            ));
        }
        if response.ok {
            response
                .result
                .ok_or_else(|| ProviderError::Protocol("SAM worker omitted result".into()))
        } else {
            Err(ProviderError::Protocol(
                response
                    .error
                    .unwrap_or_else(|| "sam_execution_failed".into()),
            ))
        }
    }
}

#[async_trait]
impl SubjectSegmentationExecutor for CoremlSamExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn segment_subject(
        &self,
        physical_model: &str,
        request: SubjectSegmentationRequest,
        cancellation: CancellationToken,
    ) -> Result<SubjectSegmentationExecutionOutput, ProviderError> {
        let (provenance, dimensions, result, mask_bytes) = self
            .run_segmentation(physical_model, request, cancellation, "segment_subject")
            .await?;
        let mask_image = image::load_from_memory_with_format(&mask_bytes, ImageFormat::Png)
            .map_err(|_| ProviderError::Protocol("SAM worker returned an invalid PNG mask".into()))?
            .to_luma8();
        if mask_image.dimensions() != dimensions {
            return Err(ProviderError::Protocol(
                "SAM mask geometry does not match the submitted image".into(),
            ));
        }
        let (foreground_pixels, bounding_box) = validate_binary_mask(&mask_image)?;
        let mask_sha256 = format!("{:x}", Sha256::digest(&mask_bytes));
        Ok(SubjectSegmentationExecutionOutput {
            image: input_geometry(dimensions),
            mask: EncodedSegmentationMask {
                content_type: "image/png".into(),
                encoding: "binary_u8_png".into(),
                data_base64: STANDARD.encode(mask_bytes),
                sha256: mask_sha256,
                width: dimensions.0,
                height: dimensions.1,
                foreground_pixels,
                bounding_box,
            },
            score: result.score,
            provenance,
        })
    }

    async fn segment_subject_soft_mask(
        &self,
        physical_model: &str,
        request: SubjectSegmentationRequest,
        cancellation: CancellationToken,
    ) -> Result<SubjectSegmentationSoftMaskExecutionOutput, ProviderError> {
        let (provenance, dimensions, result, mask_bytes) = self
            .run_segmentation(
                physical_model,
                request,
                cancellation,
                "segment_subject_soft_mask",
            )
            .await?;
        let mask_image = image::load_from_memory_with_format(&mask_bytes, ImageFormat::Png)
            .map_err(|_| {
                ProviderError::Protocol("SAM worker returned an invalid PNG soft mask".into())
            })?
            .to_luma8();
        if mask_image.dimensions()
            != (
                SUBJECT_SEGMENTATION_SOFT_MASK_WIDTH,
                SUBJECT_SEGMENTATION_SOFT_MASK_HEIGHT,
            )
        {
            return Err(ProviderError::Protocol(
                "SAM soft mask geometry must be the frozen 256x256 raster".into(),
            ));
        }
        Ok(SubjectSegmentationSoftMaskExecutionOutput {
            input_coordinate_extent: input_geometry(dimensions),
            mask: EncodedSoftSegmentationMask {
                content_type: "image/png".into(),
                encoding: "gray8_sigmoid_probability_png".into(),
                data_base64: STANDARD.encode(&mask_bytes),
                sha256: format!("{:x}", Sha256::digest(&mask_bytes)),
                width: SUBJECT_SEGMENTATION_SOFT_MASK_WIDTH,
                height: SUBJECT_SEGMENTATION_SOFT_MASK_HEIGHT,
            },
            score: result.score,
            provenance,
        })
    }
}

impl CoremlSamExecutor {
    async fn run_segmentation(
        &self,
        physical_model: &str,
        request: SubjectSegmentationRequest,
        cancellation: CancellationToken,
        operation: &'static str,
    ) -> Result<(VisionExecutionProvenance, (u32, u32), WorkerResult, Vec<u8>), ProviderError> {
        let provenance = self
            .builds
            .get(physical_model)
            .cloned()
            .ok_or_else(|| ProviderError::InvalidInput("SAM Build is not admitted".into()))?;
        let format = match request.image.content_type.as_str() {
            "image/jpeg" => ("jpg", ImageFormat::Jpeg),
            "image/png" => ("png", ImageFormat::Png),
            _ => return Err(ProviderError::InvalidInput("unsupported SAM image".into())),
        };
        let dimensions =
            image::ImageReader::with_format(std::io::Cursor::new(&request.image.bytes), format.1)
                .into_dimensions()
                .map_err(|_| ProviderError::InvalidInput("SAM image failed to decode".into()))?;
        if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_VISION_IMAGE_PIXELS {
            return Err(ProviderError::InvalidInput(
                "SAM image exceeds the decoded pixel limit".into(),
            ));
        }

        let temporary_files = TempDir::new()?;
        let image_path = temporary_files.path().join(format!("input.{}", format.0));
        let mask_path = temporary_files.path().join("mask.png");
        fs::write(&image_path, &request.image.bytes).await?;
        let image_sha256 = format!("{:x}", Sha256::digest(&request.image.bytes));
        let result = self
            .round_trip(
                &WorkerRequest {
                    request_id: Uuid::new_v4().simple().to_string(),
                    operation,
                    model: provenance.model_path.clone(),
                    artifact_sha256: provenance.artifact_sha256.clone(),
                    image_path: image_path.to_string_lossy().into_owned(),
                    image_sha256,
                    output_path: mask_path.to_string_lossy().into_owned(),
                    points: request.points,
                    box_prompt: request.box_prompt,
                },
                cancellation,
            )
            .await?;
        if !result.score.is_finite() || !(0.0..=1.0).contains(&result.score) {
            return Err(ProviderError::Protocol(
                "SAM worker returned an invalid mask score".into(),
            ));
        }
        let metadata = fs::metadata(&mask_path)
            .await
            .map_err(|_| ProviderError::Protocol("SAM worker omitted mask output".into()))?;
        if metadata.len() == 0 || metadata.len() > MAX_SEGMENTATION_MASK_BYTES as u64 {
            return Err(ProviderError::Protocol(
                "SAM mask output violates the response limit".into(),
            ));
        }
        let mask_bytes = fs::read(&mask_path).await?;
        Ok((
            VisionExecutionProvenance {
                model_build: provenance.model_build,
                artifact_sha256: provenance.artifact_sha256,
                preprocessing_identity: provenance.preprocessing_identity,
                postprocessing_identity: provenance.postprocessing_identity,
                tokenizer: None,
                runtime: provenance.runtime,
                requested_execution_provider: provenance.requested_execution_provider,
                actual_execution_provider: provenance.actual_execution_provider,
                execution_provider_fallback_reason: None,
                precision: provenance.precision,
            },
            dimensions,
            result,
            mask_bytes,
        ))
    }
}

fn input_geometry(dimensions: (u32, u32)) -> ImageGeometry {
    ImageGeometry {
        width: dimensions.0,
        height: dimensions.1,
        orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
    }
}

fn validate_binary_mask(
    mask: &image::GrayImage,
) -> Result<(u64, Option<BoundingBox>), ProviderError> {
    let mut count = 0_u64;
    let mut bounds = None::<(u32, u32, u32, u32)>;
    for (x, y, pixel) in mask.enumerate_pixels() {
        match pixel[0] {
            0 => {}
            255 => {
                count += 1;
                bounds = Some(match bounds {
                    Some((min_x, min_y, max_x, max_y)) => {
                        (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                    }
                    None => (x, y, x, y),
                });
            }
            _ => {
                return Err(ProviderError::Protocol(
                    "SAM mask must contain only binary pixels".into(),
                ));
            }
        }
    }
    Ok((
        count,
        bounds.map(|(min_x, min_y, max_x, max_y)| BoundingBox {
            x: min_x as f32,
            y: min_y as f32,
            width: (max_x - min_x + 1) as f32,
            height: (max_y - min_y + 1) as f32,
        }),
    ))
}

async fn read_bounded_line<R>(reader: &mut R, limit: usize) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(end) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM worker response exceeds the frame limit",
            ));
        }
        line.extend_from_slice(&available[..end]);
        let complete = available[end - 1] == b'\n';
        reader.consume(end);
        if complete {
            return Ok(Some(line));
        }
    }
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

async fn spawn_worker(command: &str, args: &[String]) -> Result<WorkerProcess, ProviderError> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::Protocol("SAM worker stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("SAM worker stdout unavailable".into()))?;
    Ok(WorkerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

async fn terminate(process: &mut WorkerProcess) {
    let _ = process.child.kill().await;
    let _ = process.child.wait().await;
}

#[derive(Debug, Serialize)]
struct WorkerRequest {
    request_id: String,
    operation: &'static str,
    model: String,
    artifact_sha256: String,
    image_path: String,
    image_sha256: String,
    output_path: String,
    points: Vec<SegmentationPromptPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    box_prompt: Option<NormalizedBoundingBox>,
}

#[derive(Debug, Deserialize)]
struct WorkerResponse {
    request_id: String,
    ok: bool,
    result: Option<WorkerResult>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkerResult {
    score: f32,
}

#[cfg(test)]
mod tests {
    use image::Luma;

    use super::*;

    #[test]
    fn binary_mask_validation_rejects_soft_pixels() {
        let mut mask = image::GrayImage::new(2, 1);
        mask.put_pixel(0, 0, Luma([255]));
        mask.put_pixel(1, 0, Luma([127]));
        assert!(validate_binary_mask(&mask).is_err());
    }

    #[test]
    fn soft_mask_contract_keeps_the_native_sam_extent() {
        assert_eq!(SUBJECT_SEGMENTATION_SOFT_MASK_WIDTH, 256);
        assert_eq!(SUBJECT_SEGMENTATION_SOFT_MASK_HEIGHT, 256);
    }

    #[tokio::test]
    async fn worker_lines_are_bounded() {
        let mut reader = BufReader::new(&b"123456\n"[..]);
        assert!(read_bounded_line(&mut reader, 4).await.is_err());
    }
}
