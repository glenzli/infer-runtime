//! One offline process owns the unified Gemma retrieval space and its residency.

use std::{
    collections::BTreeMap,
    io::{self, Write},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use infer_core::{
    EmbeddingSpaceConfig, MediaEmbeddingInput, MediaEmbeddingKind, MediaEmbeddingProcessing,
    MediaEmbeddingRequest,
};
use infer_resource::{
    NativeControlError, NativeInventory, NativeModelController, NativeRunningModel,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ProviderError;

#[derive(Debug, Clone)]
pub struct MediaBuildContract {
    pub model_path: String,
    pub model_build: String,
    pub model_revision: String,
    pub artifact_set_sha256: String,
    pub embedding_space: EmbeddingSpaceConfig,
    pub runtime: String,
    pub precision: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub tokenizer_identity: String,
    pub instruction_revision: String,
}

#[derive(Debug)]
pub struct MediaEmbeddingExecutionOutput {
    pub embedding: Vec<f32>,
    pub processing: MediaEmbeddingProcessing,
    pub provenance: MediaBuildContract,
}

#[async_trait]
pub trait MediaEmbeddingExecutor: Send + Sync {
    async fn embed_media(
        &self,
        physical_model: &str,
        request: MediaEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<MediaEmbeddingExecutionOutput, ProviderError>;
}
pub type DynMediaEmbeddingExecutor = Arc<dyn MediaEmbeddingExecutor>;

pub struct MediaEmbeddingWorkerExecutor {
    command: String,
    args: Vec<String>,
    builds: BTreeMap<String, MediaBuildContract>,
    process: Mutex<Option<WorkerProcess>>,
}

impl MediaEmbeddingWorkerExecutor {
    pub fn new(
        command: String,
        args: Vec<String>,
        builds: BTreeMap<String, MediaBuildContract>,
    ) -> Arc<Self> {
        Arc::new(Self {
            command,
            args,
            builds,
            process: Mutex::new(None),
        })
    }

    async fn transact(
        &self,
        request: WorkerRequest,
        cancellation: CancellationToken,
    ) -> Result<serde_json::Value, ProviderError> {
        let mut guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ProviderError::Protocol("media_request_cancelled".into())),
            guard = self.process.lock() => guard,
        };
        if guard
            .as_mut()
            .is_some_and(|worker| worker.child.try_wait().ok().flatten().is_some())
        {
            *guard = None;
        }
        if guard.is_none() {
            *guard = Some(spawn_worker(&self.command, &self.args)?);
        }
        // Own the process across the await. Dropping this future on a deadline
        // also drops the process group, so no late reply can reach another job.
        let mut worker = guard.take().expect("spawned worker");
        let result = {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ProviderError::Protocol("media_request_cancelled".into())),
                result = worker.exchange(&request) => result,
            }
        };
        match result {
            Ok(result) => {
                worker.model = Some(request.physical_model);
                *guard = Some(worker);
                Ok(result)
            }
            Err(error) => {
                *guard = None;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl MediaEmbeddingExecutor for MediaEmbeddingWorkerExecutor {
    async fn embed_media(
        &self,
        physical_model: &str,
        request: MediaEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<MediaEmbeddingExecutionOutput, ProviderError> {
        request
            .validate()
            .map_err(|_| ProviderError::InvalidInput("media_input_invalid".into()))?;
        let provenance = self
            .builds
            .get(physical_model)
            .ok_or_else(|| ProviderError::Protocol("media_model_not_admitted".into()))?
            .clone();
        let kind = request.kind();
        let mut temporary = None;
        let (text, file) = match request.input {
            MediaEmbeddingInput::Query(text) => (Some(text), None),
            MediaEmbeddingInput::File { bytes, .. } => {
                let spool = tempfile::Builder::new()
                    .prefix("infer-gemma2-input-")
                    .tempdir()?;
                let input_path = spool.path().join("input.bin");
                std::fs::File::create(&input_path)?.write_all(&bytes)?;
                let path = input_path.to_string_lossy().into_owned();
                temporary = Some(spool);
                (None, Some(path))
            }
        };
        let value = self
            .transact(
                WorkerRequest {
                    request_id: Uuid::new_v4().to_string(),
                    kind: match kind {
                        MediaEmbeddingKind::Text => "text",
                        MediaEmbeddingKind::Image => "image",
                        MediaEmbeddingKind::Audio => "audio",
                        MediaEmbeddingKind::Video => "video",
                    },
                    model: provenance.model_path.clone(),
                    physical_model: physical_model.into(),
                    text,
                    file,
                },
                cancellation,
            )
            .await?;
        drop(temporary);
        let result: EmbeddingResult = serde_json::from_value(value)
            .map_err(|_| ProviderError::Protocol("media_worker_result_invalid".into()))?;
        Ok(MediaEmbeddingExecutionOutput {
            embedding: result.embedding,
            processing: result.processing,
            provenance,
        })
    }
}

#[async_trait]
impl NativeModelController for MediaEmbeddingWorkerExecutor {
    async fn discover(&self) -> Result<NativeInventory, NativeControlError> {
        let mut process = self.process.lock().await;
        if process
            .as_mut()
            .is_some_and(|worker| worker.child.try_wait().ok().flatten().is_some())
        {
            *process = None;
        }
        let running = process
            .as_ref()
            .and_then(|worker| worker.model.clone())
            .map(|model| {
                BTreeMap::from([(
                    model,
                    NativeRunningModel {
                        resident_memory_bytes: None,
                        resident_accelerator_memory_bytes: None,
                    },
                )])
            })
            .unwrap_or_default();
        Ok(NativeInventory {
            installed: self.builds.keys().cloned().collect(),
            running,
        })
    }

    async fn load(&self, model: &str) -> Result<(), NativeControlError> {
        let build = self
            .builds
            .get(model)
            .ok_or_else(|| NativeControlError::Operation("media_model_not_admitted".into()))?;
        let value = self
            .transact(
                WorkerRequest {
                    request_id: Uuid::new_v4().to_string(),
                    kind: "load",
                    model: build.model_path.clone(),
                    physical_model: model.into(),
                    text: None,
                    file: None,
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|_| NativeControlError::Operation("media_load_failed".into()))?;
        if value.get("loaded").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(NativeControlError::Operation("media_load_failed".into()));
        }
        Ok(())
    }

    async fn unload(&self, model: &str) -> Result<(), NativeControlError> {
        if !self.builds.contains_key(model) {
            return Err(NativeControlError::Operation(
                "media_model_not_admitted".into(),
            ));
        }
        *self.process.lock().await = None;
        Ok(())
    }
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    group: u32,
    model: Option<String>,
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        // The worker starts its own process group. Decoders inherit it, so
        // cancellation/deadline/unload also release their files and memory.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.group as i32), libc::SIGKILL);
        }
        let _ = self.child.start_kill();
    }
}

impl WorkerProcess {
    async fn exchange(
        &mut self,
        request: &WorkerRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        let frame = read_frame(&mut self.stdout, 64 * 1024).await?;
        let reply: WorkerResponse = serde_json::from_slice(&frame)
            .map_err(|_| ProviderError::Protocol("media_worker_frame_invalid".into()))?;
        if reply.request_id != request.request_id {
            return Err(ProviderError::Protocol(
                "media_worker_request_mismatch".into(),
            ));
        }
        if !reply.ok {
            return Err(if reply.invalid_input {
                ProviderError::InvalidInput("media_input_invalid".into())
            } else {
                ProviderError::Protocol("media_execution_failed".into())
            });
        }
        reply
            .result
            .ok_or_else(|| ProviderError::Protocol("media_worker_result_missing".into()))
    }
}

fn spawn_worker(command: &str, args: &[String]) -> Result<WorkerProcess, ProviderError> {
    let mut command = Command::new(command);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn()?;
    let group = child
        .id()
        .ok_or_else(|| ProviderError::Protocol("media_worker_pid_missing".into()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::Protocol("media_worker_stdin_missing".into()))?;
    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Protocol("media_worker_stdout_missing".into()))?,
    );
    Ok(WorkerProcess {
        child,
        stdin,
        stdout,
        group,
        model: None,
    })
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "media_worker_closed",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(available.len(), |index| index + 1);
        if frame.len() + length > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media_worker_frame_exceeded",
            ));
        }
        frame.extend_from_slice(&available[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(frame);
        }
    }
}

#[derive(Serialize)]
struct WorkerRequest {
    request_id: String,
    kind: &'static str,
    model: String,
    #[serde(skip)]
    physical_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}
#[derive(Deserialize)]
struct WorkerResponse {
    request_id: String,
    ok: bool,
    result: Option<serde_json::Value>,
    #[serde(default)]
    invalid_input: bool,
}
#[derive(Deserialize)]
struct EmbeddingResult {
    embedding: Vec<f32>,
    processing: MediaEmbeddingProcessing,
}

pub fn verify_media_worker(
    command: &str,
    args: &[String],
    model: &str,
) -> Result<(), ProviderError> {
    use std::io::Read;
    let mut child = std::process::Command::new(command)
        .args(args)
        .args(["--verify-model", model])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(90) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProviderError::Protocol("media_readiness_timeout".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut frame = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .take(4097)
        .read_to_end(&mut frame)?;
    let value: serde_json::Value = serde_json::from_slice(&frame)
        .map_err(|_| ProviderError::Protocol("media_readiness_invalid".into()))?;
    if !status.success()
        || frame.len() > 4096
        || value.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
        || value["device"] != "metal"
        || value["precision"] != "bf16"
        || value["dimensions"] != 768
    {
        return Err(ProviderError::Protocol("media_runtime_unavailable".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_inflight_future_terminates_the_worker_and_decoder_group() {
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("pids");
        let program = "import os,sys,subprocess,time; sys.stdin.readline(); child=subprocess.Popen(['/bin/sleep','60']); open(sys.argv[1],'w').write(str(os.getpid())+' '+str(child.pid)); time.sleep(60)";
        let executor = MediaEmbeddingWorkerExecutor::new(
            "/usr/bin/python3".into(),
            vec![
                "-c".into(),
                program.into(),
                receipt.to_string_lossy().into_owned(),
            ],
            BTreeMap::new(),
        );
        let task = tokio::spawn(async move {
            executor
                .transact(
                    WorkerRequest {
                        request_id: "cancel-test".into(),
                        kind: "load",
                        model: "test".into(),
                        physical_model: "test".into(),
                        text: None,
                        file: None,
                    },
                    CancellationToken::new(),
                )
                .await
        });
        let pids = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = tokio::fs::read_to_string(&receipt).await {
                    let pids: Vec<i32> = value
                        .split_whitespace()
                        .map(|v| v.parse().unwrap())
                        .collect();
                    if pids.len() == 2 {
                        break pids;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(unsafe { libc::kill(-pids[0], 0) }, 0);
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while unsafe { libc::kill(-pids[0], 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker and decoder group cleaned after future drop");
    }
    #[tokio::test]
    async fn framing_rejects_oversized_unterminated_and_malformed_output() {
        assert_eq!(
            read_frame(&mut BufReader::new(&b"{}\n"[..]), 3)
                .await
                .unwrap(),
            b"{}\n"
        );
        assert!(
            read_frame(&mut BufReader::new(&b"1234\n"[..]), 3)
                .await
                .is_err()
        );
        assert!(
            read_frame(&mut BufReader::new(&b"{}"[..]), 3)
                .await
                .is_err()
        );
    }
}
