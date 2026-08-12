//! Persistent process bridge for local PP-OCRv6 execution.

use std::{collections::BTreeMap, io, process::Stdio, sync::Arc};

use async_trait::async_trait;
use image::GenericImageView;
use infer_core::{DocumentOcrRequest, ImageGeometry, OcrTextLine};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ProviderError;

const MAX_OCR_WORKER_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OcrBuildContract {
    pub model_build: String,
    pub detection_model: String,
    pub detection_revision: String,
    pub detection_artifact_sha256: String,
    pub recognition_model: String,
    pub recognition_revision: String,
    pub recognition_artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub precision: String,
}

#[derive(Debug)]
pub struct OcrExecutionOutput {
    pub image: ImageGeometry,
    pub lines: Vec<OcrTextLine>,
    pub provenance: OcrBuildContract,
}

#[async_trait]
pub trait OcrExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn recognize(
        &self,
        physical_model: &str,
        request: DocumentOcrRequest,
        cancellation: CancellationToken,
    ) -> Result<OcrExecutionOutput, ProviderError>;
}

pub type DynOcrExecutor = Arc<dyn OcrExecutor>;

pub struct OcrWorkerExecutor {
    id: String,
    command: String,
    args: Vec<String>,
    builds: BTreeMap<String, OcrBuildContract>,
    process: Arc<Mutex<Option<WorkerProcess>>>,
}

impl OcrWorkerExecutor {
    pub fn new(
        id: impl Into<String>,
        command: String,
        args: Vec<String>,
        builds: BTreeMap<String, OcrBuildContract>,
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
        process.stdin.write_all(&line).await?;
        process.stdin.write_all(b"\n").await?;
        process.stdin.flush().await?;
        loop {
            let response_line = {
                let read = read_bounded_line(&mut process.stdout, MAX_OCR_WORKER_RESPONSE_BYTES);
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => Some(result),
                    _ = cancellation.cancelled() => None,
                }
            };
            let Some(response_line) = response_line else {
                process.child.kill().await?;
                *guard = None;
                return Err(ProviderError::Protocol("ocr_execution_cancelled".into()));
            };
            let response_line = match response_line {
                Ok(Some(response_line)) => response_line,
                Ok(None) => {
                    *guard = None;
                    return Err(ProviderError::Protocol(
                        "OCR worker exited without a response".into(),
                    ));
                }
                Err(error) => {
                    process.child.kill().await?;
                    *guard = None;
                    return Err(error.into());
                }
            };
            let response: WorkerResponse = match serde_json::from_slice(&response_line) {
                Ok(response) => response,
                Err(error) => {
                    process.child.kill().await?;
                    *guard = None;
                    return Err(error.into());
                }
            };
            if response.request_id != request.request_id {
                continue;
            }
            if response.ok {
                return response
                    .result
                    .ok_or_else(|| ProviderError::Protocol("OCR worker omitted result".into()));
            }
            return Err(ProviderError::Protocol(
                response
                    .error
                    .unwrap_or_else(|| "ocr_execution_failed".into()),
            ));
        }
    }
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
                "OCR worker response exceeds the frame limit",
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

#[async_trait]
impl OcrExecutor for OcrWorkerExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn recognize(
        &self,
        physical_model: &str,
        request: DocumentOcrRequest,
        cancellation: CancellationToken,
    ) -> Result<OcrExecutionOutput, ProviderError> {
        let provenance = self
            .builds
            .get(physical_model)
            .cloned()
            .ok_or_else(|| ProviderError::InvalidInput("OCR Build is not admitted".into()))?;
        let temporary_files = TempDir::new()?;
        let decoded = image::load_from_memory(&request.image.bytes)
            .map_err(|_| ProviderError::InvalidInput("OCR image failed to decode".into()))?;
        let (width, height) = decoded.dimensions();
        if u64::from(width) * u64::from(height) > infer_core::MAX_VISION_IMAGE_PIXELS {
            return Err(ProviderError::InvalidInput(
                "OCR image exceeds the decoded pixel limit".into(),
            ));
        }
        let extension = match request.image.content_type.as_str() {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            _ => return Err(ProviderError::InvalidInput("unsupported OCR image".into())),
        };
        let image_path = temporary_files.path().join(format!("input.{extension}"));
        fs::write(&image_path, request.image.bytes).await?;
        let result = self
            .round_trip(
                &WorkerRequest {
                    request_id: Uuid::new_v4().simple().to_string(),
                    operation: "recognize",
                    detection_model: provenance.detection_model.clone(),
                    recognition_model: provenance.recognition_model.clone(),
                    image_path: image_path.to_string_lossy().into_owned(),
                },
                cancellation,
            )
            .await?;
        if result.image.width != width || result.image.height != height {
            return Err(ProviderError::Protocol(
                "OCR worker image geometry does not match the submitted artifact".into(),
            ));
        }
        Ok(OcrExecutionOutput {
            image: result.image,
            lines: result.lines,
            provenance,
        })
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
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::Protocol("OCR worker stdin is unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("OCR worker stdout is unavailable".into()))?;
    Ok(WorkerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

#[derive(Serialize)]
struct WorkerRequest {
    request_id: String,
    operation: &'static str,
    detection_model: String,
    recognition_model: String,
    image_path: String,
}

#[derive(Deserialize)]
struct WorkerResponse {
    request_id: String,
    ok: bool,
    result: Option<WorkerResult>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct WorkerResult {
    image: ImageGeometry,
    lines: Vec<OcrTextLine>,
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::read_bounded_line;

    #[tokio::test]
    async fn bounded_line_accepts_one_complete_frame() {
        let mut reader = BufReader::new(&b"{\"ok\":true}\ntrailing"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 32).await.unwrap(),
            Some(b"{\"ok\":true}\n".to_vec())
        );
    }

    #[tokio::test]
    async fn bounded_line_rejects_oversized_frame() {
        let mut reader = BufReader::new(&b"123456\n"[..]);
        let error = read_bounded_line(&mut reader, 4).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
