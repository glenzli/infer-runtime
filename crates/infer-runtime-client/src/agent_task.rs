//! Bounded, inline file tasks through the official Consumer transport.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Client, Error, Result,
    transport::{ensure_success, read_bounded},
};

pub const AGENT_TASK_CAPABILITIES: &[&str] = &["infer.agent.task@20260925.1"];
pub const AGENT_TASK_INTENT: &str = "agent.file_task";
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TASK_BYTES: usize = 16 * 1024 * 1024;
// The daemon bounds decoded outputs to 16 MiB. Allow base64 and receipt overhead.
const MAX_RESULT_JSON_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskRequest {
    /// The stable Agent intent, never a deployment or physical model name.
    pub model: String,
    pub instruction: String,
    pub input_files: Vec<AgentTaskInputFile>,
    pub output_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskInputFile {
    /// Relative to the private input or output directory.
    pub path: String,
    pub content_base64: String,
    pub sha256: String,
}

impl AgentTaskInputFile {
    pub fn from_bytes(path: impl Into<String>, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Input("Agent file exceeds 8 MiB".into()));
        }
        let path = path.into();
        if !safe_relative_path(&path) {
            return Err(Error::Input("Agent file path is unsafe".into()));
        }
        Ok(Self {
            path,
            content_base64: STANDARD.encode(bytes),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }

    pub fn decoded_bytes(&self) -> Result<Vec<u8>> {
        if self.content_base64.len() > MAX_FILE_BYTES.div_ceil(3) * 4 + 4 {
            return Err(Error::MalformedResponse("Agent file exceeds 8 MiB".into()));
        }
        let bytes = STANDARD
            .decode(&self.content_base64)
            .map_err(|_| Error::MalformedResponse("Agent file is not base64".into()))?;
        if bytes.len() > MAX_FILE_BYTES
            || STANDARD.encode(&bytes) != self.content_base64
            || format!("{:x}", Sha256::digest(&bytes)) != self.sha256
        {
            return Err(Error::MalformedResponse(
                "Agent file bytes or sha256 are invalid".into(),
            ));
        }
        Ok(bytes)
    }
}

impl AgentTaskRequest {
    pub fn validate(&self) -> Result<()> {
        if self.model != AGENT_TASK_INTENT {
            return Err(Error::Input("model must be agent.file_task".into()));
        }
        if self.instruction.trim().is_empty() || self.instruction.len() > 16_384 {
            return Err(Error::Input(
                "instruction must contain 1 to 16384 bytes".into(),
            ));
        }
        if self.input_files.is_empty() || self.input_files.len() > 16 {
            return Err(Error::Input(
                "input_files must contain 1 to 16 files".into(),
            ));
        }
        if self.output_paths.is_empty() || self.output_paths.len() > 16 {
            return Err(Error::Input(
                "output_paths must contain 1 to 16 paths".into(),
            ));
        }
        let mut input_paths = BTreeSet::new();
        let mut total = 0usize;
        for file in &self.input_files {
            if !safe_relative_path(&file.path)
                || !input_paths.insert(file.path.to_ascii_lowercase())
            {
                return Err(Error::Input(
                    "input file path is unsafe or duplicated".into(),
                ));
            }
            let bytes = file
                .decoded_bytes()
                .map_err(|error| Error::Input(error.to_string()))?;
            total = total.saturating_add(bytes.len());
            if total > MAX_TASK_BYTES {
                return Err(Error::Input("total input exceeds 16 MiB".into()));
            }
        }
        let mut output_paths = BTreeSet::new();
        for path in &self.output_paths {
            if !safe_relative_path(path) || !output_paths.insert(path.to_ascii_lowercase()) {
                return Err(Error::Input("output path is unsafe or duplicated".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentTaskResult {
    pub job_id: String,
    pub state: String,
    pub answer: String,
    pub outputs: Vec<AgentTaskInputFile>,
    pub provenance: AgentTaskProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentTaskProvenance {
    pub capability_contract: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub attempt_number: usize,
    pub codex_thread_id: String,
    pub codex_turn_id: String,
    pub sandbox_profile: String,
    pub tool_policy: String,
}

impl AgentTaskResult {
    fn validate_for(&self, request: &AgentTaskRequest) -> Result<()> {
        if self.job_id.is_empty()
            || self.state != "completed"
            || self.provenance.capability_contract != AGENT_TASK_CAPABILITIES[0]
            || self.provenance.provider.is_empty()
            || self.provenance.deployment.is_empty()
            || self.provenance.model_build.is_empty()
            || self.provenance.attempt_number == 0
            || self.provenance.codex_thread_id.is_empty()
            || self.provenance.codex_turn_id.is_empty()
            || self.provenance.sandbox_profile.is_empty()
            || self.provenance.tool_policy.is_empty()
        {
            return Err(Error::MalformedResponse(
                "Agent receipt is incomplete".into(),
            ));
        }
        if self.outputs.len() != request.output_paths.len() {
            return Err(Error::MalformedResponse(
                "Agent output set differs from request".into(),
            ));
        }
        let declared: BTreeSet<_> = request.output_paths.iter().collect();
        let mut observed = BTreeSet::new();
        let mut total = 0usize;
        for output in &self.outputs {
            if !declared.contains(&output.path) || !observed.insert(&output.path) {
                return Err(Error::MalformedResponse(
                    "Agent output path was not declared".into(),
                ));
            }
            total = total.saturating_add(output.decoded_bytes()?.len());
            if total > MAX_TASK_BYTES {
                return Err(Error::MalformedResponse(
                    "Agent outputs exceed 16 MiB".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Client {
    pub async fn create_agent_task(&self, request: &AgentTaskRequest) -> Result<AgentTaskResult> {
        request.validate()?;
        let response = self
            .send_capability_with(AGENT_TASK_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/infer/v1/agent/tasks"))
                    .json(request)
            })
            .await?;
        let bytes = read_bounded(ensure_success(response).await?, MAX_RESULT_JSON_BYTES).await?;
        let result: AgentTaskResult = serde_json::from_slice(&bytes)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        result.validate_for(request)?;
        Ok(result)
    }
}

fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 240
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= 100
                && !component.starts_with('.')
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AgentTaskRequest {
        AgentTaskRequest {
            model: AGENT_TASK_INTENT.into(),
            instruction: "Revise the input.".into(),
            input_files: vec![AgentTaskInputFile::from_bytes("source.md", b"hello").unwrap()],
            output_paths: vec!["revision.md".into()],
        }
    }

    #[test]
    fn request_bounds_paths_and_content_before_transport() {
        assert!(request().validate().is_ok());
        let mut invalid = request();
        invalid.output_paths = vec!["../outside".into()];
        assert!(matches!(invalid.validate(), Err(Error::Input(_))));
        let mut invalid = request();
        invalid.input_files[0].sha256 = "0".repeat(64);
        assert!(matches!(invalid.validate(), Err(Error::Input(_))));
    }

    #[tokio::test]
    async fn invalid_agent_request_stops_before_discovery() {
        let client = Client::builder().build().unwrap();
        let mut invalid = request();
        invalid.model = "assistant.general".into();
        assert!(matches!(
            client.create_agent_task(&invalid).await,
            Err(Error::Input(_))
        ));
    }

    #[test]
    fn receipt_requires_exact_declared_outputs_and_digest() {
        let mut result = AgentTaskResult {
            job_id: "job-1".into(),
            state: "completed".into(),
            answer: "done".into(),
            outputs: vec![AgentTaskInputFile::from_bytes("revision.md", b"new").unwrap()],
            provenance: AgentTaskProvenance {
                capability_contract: AGENT_TASK_CAPABILITIES[0].into(),
                provider: "codex-agent".into(),
                deployment: "agent-primary".into(),
                model_build: "test".into(),
                attempt_number: 1,
                codex_thread_id: "thread-1".into(),
                codex_turn_id: "turn-1".into(),
                sandbox_profile: "test".into(),
                tool_policy: "test".into(),
            },
        };
        assert!(result.validate_for(&request()).is_ok());
        result.outputs[0].path = "other.md".into();
        assert!(result.validate_for(&request()).is_err());
        result.outputs[0].path = "revision.md".into();
        result.outputs[0].sha256 = "0".repeat(64);
        assert!(result.validate_for(&request()).is_err());
    }
}
