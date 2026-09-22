use infer_core::ResponsesRequest;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL: &str = "infer.node.text@20260922.1";
pub const MAX_FRAME: usize = 1024 * 1024;
pub const LEASE_MS: u64 = 5000;
pub const MAX_TASK_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeError {
    #[error("node unavailable")]
    Unavailable,
    #[error("node authentication or grant denied")]
    Forbidden,
    #[error("node protocol or contract mismatch")]
    Protocol,
    #[error("node capacity exhausted")]
    Busy,
    #[error("node task not found")]
    Missing,
    #[error("node task cancelled or expired")]
    Cancelled,
    #[error("node execution failed")]
    Execution,
    #[error("node execution outcome is unknown; automatic replay prohibited")]
    OutcomeUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct TaskKey {
    pub job_id: String,
    pub attempt: usize,
}

pub struct NodeAttempt {
    pub key: TaskKey,
    pub app_id: String,
    pub intent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub deployment_id: String,
    pub intent: String,
    pub contract_digest: String,
    pub build_id: String,
    pub resource_estimate: infer_core::DeploymentResourceEstimateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub offers: Vec<Offer>,
    /// Admission slots; physical resources remain owned by the destination Runtime.
    pub available_admissions: usize,
    pub lease_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskState {
    Reserved,
    Running,
    Succeeded { result: Value },
    Failed { error: NodeError },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Catalog,
    Reserve {
        key: TaskKey,
        app_id: String,
        deployment: String,
        contract_digest: String,
        ttl_ms: u64,
    },
    Dispatch {
        key: TaskKey,
        request: Box<ResponsesRequest>,
    },
    Status {
        key: TaskKey,
    },
    Cancel {
        key: TaskKey,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub protocol: String,
    pub generation: Option<String>,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Reply {
    Catalog(Catalog),
    Task(TaskState),
    Error(NodeError),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Response {
    pub protocol: String,
    pub node_id: String,
    pub generation: String,
    pub reply: Reply,
}

pub(crate) async fn read_frame<T: DeserializeOwned>(
    io: &mut (impl AsyncRead + Unpin),
) -> Result<T, NodeError> {
    let length = io.read_u32().await.map_err(|_| NodeError::Unavailable)? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(NodeError::Protocol);
    }
    let mut bytes = vec![0; length];
    io.read_exact(&mut bytes)
        .await
        .map_err(|_| NodeError::Unavailable)?;
    serde_json::from_slice(&bytes).map_err(|_| NodeError::Protocol)
}

pub(crate) async fn write_frame<T: Serialize>(
    io: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), NodeError> {
    let bytes = serde_json::to_vec(value).map_err(|_| NodeError::Protocol)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(NodeError::Protocol);
    }
    io.write_u32(bytes.len() as u32)
        .await
        .map_err(|_| NodeError::Unavailable)?;
    io.write_all(&bytes)
        .await
        .map_err(|_| NodeError::Unavailable)?;
    io.flush().await.map_err(|_| NodeError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn oversized_frame_length_is_rejected_before_waiting_for_a_body() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            read_frame::<serde_json::Value>(&mut reader),
        )
        .await
        .expect("the frame length alone must decide rejection");
        assert!(matches!(result, Err(NodeError::Protocol)));
    }
}
