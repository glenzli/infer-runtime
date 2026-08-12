//! Persistent process bridge for local text embedding and reranking.

use std::{collections::BTreeMap, io, process::Stdio, sync::Arc};

use async_trait::async_trait;
use infer_core::{
    RetrievalEmbeddingRequest, RetrievalRerankRequest, RetrievalRerankResult, RetrievalTextInput,
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

const MAX_RETRIEVAL_WORKER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RetrievalBuildContract {
    /// Private resolved artifact directory. It is used only in the worker
    /// request and never copied into public provenance or logs.
    pub model_path: String,
    pub model_build: String,
    pub model_revision: String,
    pub artifact_sha256: String,
    pub tokenizer_identity: String,
    pub runtime: String,
    pub precision: String,
    pub embedding_space: Option<String>,
    pub embedding_dimensions: Option<usize>,
}

#[derive(Debug)]
pub struct RetrievalEmbeddingExecutionOutput {
    pub embeddings: Vec<Vec<f32>>,
    pub dimensions: usize,
    pub normalized: bool,
    pub instruction_revision: Option<String>,
    pub provenance: RetrievalBuildContract,
}

#[derive(Debug)]
pub struct RetrievalRerankExecutionOutput {
    pub results: Vec<RetrievalRerankResult>,
    pub instruction_revision: String,
    pub score_semantics: String,
    pub provenance: RetrievalBuildContract,
}

#[derive(Debug, Clone, Copy)]
pub enum RetrievalEmbeddingKind {
    Query,
    Documents,
}

#[async_trait]
pub trait RetrievalExecutor: Send + Sync {
    fn id(&self) -> &str;

    async fn embed(
        &self,
        physical_model: &str,
        request: RetrievalEmbeddingRequest,
        kind: RetrievalEmbeddingKind,
        cancellation: CancellationToken,
    ) -> Result<RetrievalEmbeddingExecutionOutput, ProviderError>;

    async fn rerank(
        &self,
        physical_model: &str,
        request: RetrievalRerankRequest,
        cancellation: CancellationToken,
    ) -> Result<RetrievalRerankExecutionOutput, ProviderError>;
}

pub type DynRetrievalExecutor = Arc<dyn RetrievalExecutor>;

pub struct RetrievalWorkerExecutor {
    id: String,
    command: String,
    args: Vec<String>,
    builds: BTreeMap<String, RetrievalBuildContract>,
    process: Arc<Mutex<Option<WorkerProcess>>>,
}

impl RetrievalWorkerExecutor {
    pub fn new(
        id: impl Into<String>,
        command: String,
        args: Vec<String>,
        builds: BTreeMap<String, RetrievalBuildContract>,
    ) -> Self {
        Self {
            id: id.into(),
            command,
            args,
            builds,
            process: Arc::new(Mutex::new(None)),
        }
    }

    fn build(&self, physical_model: &str) -> Result<RetrievalBuildContract, ProviderError> {
        self.builds
            .get(physical_model)
            .cloned()
            .ok_or_else(|| ProviderError::InvalidInput("retrieval Build is not admitted".into()))
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
                let read =
                    read_bounded_line(&mut process.stdout, MAX_RETRIEVAL_WORKER_RESPONSE_BYTES);
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => Some(result),
                    _ = cancellation.cancelled() => None,
                }
            };
            let Some(response_line) = response_line else {
                process.child.kill().await?;
                *guard = None;
                return Err(ProviderError::Protocol(
                    "retrieval_execution_cancelled".into(),
                ));
            };
            let response_line = match response_line {
                Ok(Some(response_line)) => response_line,
                Ok(None) => {
                    *guard = None;
                    return Err(ProviderError::Protocol(
                        "retrieval worker exited without a response".into(),
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
                return response.result.ok_or_else(|| {
                    ProviderError::Protocol("retrieval worker omitted result".into())
                });
            }
            return Err(ProviderError::Protocol(
                response
                    .error
                    .unwrap_or_else(|| "retrieval_execution_failed".into()),
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
                "retrieval worker response exceeds the frame limit",
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
impl RetrievalExecutor for RetrievalWorkerExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn embed(
        &self,
        physical_model: &str,
        request: RetrievalEmbeddingRequest,
        kind: RetrievalEmbeddingKind,
        cancellation: CancellationToken,
    ) -> Result<RetrievalEmbeddingExecutionOutput, ProviderError> {
        let provenance = self.build(physical_model)?;
        let operation = match kind {
            RetrievalEmbeddingKind::Query => "embed_query",
            RetrievalEmbeddingKind::Documents => "embed_documents",
        };
        let result = self
            .round_trip(
                &WorkerRequest {
                    request_id: Uuid::new_v4().simple().to_string(),
                    operation,
                    model: provenance.model_path.clone(),
                    texts: Some(request.inputs.into_iter().map(|item| item.text).collect()),
                    query: None,
                    candidates: None,
                },
                cancellation,
            )
            .await?;
        let WorkerResult::Embedding {
            embeddings,
            dimensions,
            normalized,
            instruction_revision,
        } = result
        else {
            return Err(ProviderError::Protocol(
                "retrieval worker returned the wrong result kind".into(),
            ));
        };
        Ok(RetrievalEmbeddingExecutionOutput {
            embeddings,
            dimensions,
            normalized,
            instruction_revision,
            provenance,
        })
    }

    async fn rerank(
        &self,
        physical_model: &str,
        request: RetrievalRerankRequest,
        cancellation: CancellationToken,
    ) -> Result<RetrievalRerankExecutionOutput, ProviderError> {
        let provenance = self.build(physical_model)?;
        let candidate_revisions = request
            .candidates
            .iter()
            .map(|candidate| (candidate.id.clone(), candidate.source_revision.clone()))
            .collect::<BTreeMap<_, _>>();
        let top_n = request.top_n.unwrap_or(request.candidates.len());
        let result = self
            .round_trip(
                &WorkerRequest {
                    request_id: Uuid::new_v4().simple().to_string(),
                    operation: "rerank",
                    model: provenance.model_path.clone(),
                    texts: None,
                    query: Some(request.query.text),
                    candidates: Some(request.candidates),
                },
                cancellation,
            )
            .await?;
        let WorkerResult::Rerank {
            results,
            instruction_revision,
            score_semantics,
        } = result
        else {
            return Err(ProviderError::Protocol(
                "retrieval worker returned the wrong result kind".into(),
            ));
        };
        let results = results
            .into_iter()
            .take(top_n)
            .map(|result| {
                let source_revision = candidate_revisions
                    .get(&result.candidate_id)
                    .cloned()
                    .ok_or_else(|| {
                        ProviderError::Protocol(
                            "retrieval worker returned an unknown candidate id".into(),
                        )
                    })?;
                Ok(infer_core::RetrievalRerankResult {
                    candidate_id: result.candidate_id,
                    source_revision,
                    score: result.score,
                    rank: result.rank,
                })
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        Ok(RetrievalRerankExecutionOutput {
            results,
            instruction_revision,
            score_semantics,
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
        .ok_or_else(|| ProviderError::Protocol("retrieval worker stdin is unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("retrieval worker stdout is unavailable".into()))?;
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
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    texts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<Vec<RetrievalTextInput>>,
}

#[derive(Deserialize)]
struct WorkerResponse {
    request_id: String,
    ok: bool,
    result: Option<WorkerResult>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkerResult {
    Embedding {
        embeddings: Vec<Vec<f32>>,
        dimensions: usize,
        normalized: bool,
        instruction_revision: Option<String>,
    },
    Rerank {
        results: Vec<WorkerRankResult>,
        instruction_revision: String,
        score_semantics: String,
    },
}

#[derive(Deserialize)]
struct WorkerRankResult {
    candidate_id: String,
    score: f32,
    rank: usize,
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
