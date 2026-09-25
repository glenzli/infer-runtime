//! Contract for an explicitly admitted, file-bearing Agent task.
//!
//! The request itself grants no filesystem access. The executor stages only
//! validated inline inputs and constructs a separate permission profile.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const AGENT_TASK_CAPABILITY_CONTRACT: &str = "infer.agent.task@20260925.1";
pub const AGENT_TASK_INTENT: &str = "agent.file_task";
pub const MAX_AGENT_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_AGENT_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskRequest {
    /// Stable Agent data-plane Intent, never a physical Codex model.
    pub model: String,
    pub instruction: String,
    /// Paths are relative to the task's private `input/` directory.
    pub input_files: Vec<AgentTaskInputFile>,
    /// Paths are relative to the task's private `output/` directory.
    pub output_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskInputFile {
    pub path: String,
    pub content_base64: String,
    pub sha256: String,
}

impl AgentTaskRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model != AGENT_TASK_INTENT {
            return Err("model must be agent.file_task");
        }
        if self.instruction.trim().is_empty() || self.instruction.len() > 16_384 {
            return Err("instruction must contain 1 to 16384 bytes");
        }
        if self.input_files.is_empty() || self.input_files.len() > 16 {
            return Err("input_files must contain 1 to 16 files");
        }
        if self.output_paths.is_empty() || self.output_paths.len() > 16 {
            return Err("output_paths must contain 1 to 16 paths");
        }
        let mut input_paths = BTreeSet::new();
        let mut total_bytes = 0usize;
        for file in &self.input_files {
            if !safe_relative_path(&file.path)
                || !input_paths.insert(file.path.to_ascii_lowercase())
            {
                return Err("input file path is unsafe or duplicated");
            }
            if file.content_base64.len() > MAX_AGENT_FILE_BYTES.div_ceil(3) * 4 + 4 {
                return Err("input file exceeds byte limit");
            }
            let bytes = STANDARD
                .decode(&file.content_base64)
                .map_err(|_| "input file is not canonical base64")?;
            if STANDARD.encode(&bytes) != file.content_base64 {
                return Err("input file is not canonical base64");
            }
            if bytes.len() > MAX_AGENT_FILE_BYTES {
                return Err("input file exceeds byte limit");
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_AGENT_INPUT_BYTES {
                return Err("total input exceeds byte limit");
            }
            if format!("{:x}", Sha256::digest(&bytes)) != file.sha256 {
                return Err("input file sha256 does not match bytes");
            }
        }
        let mut output_paths = BTreeSet::new();
        for path in &self.output_paths {
            if !safe_relative_path(path) || !output_paths.insert(path.to_ascii_lowercase()) {
                return Err("output path is unsafe or duplicated");
            }
        }
        Ok(())
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

/// Successful bounded Agent execution receipt.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskResult {
    pub job_id: String,
    pub state: String,
    pub answer: String,
    pub outputs: Vec<AgentTaskInputFile>,
    pub provenance: AgentTaskProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str) -> AgentTaskRequest {
        AgentTaskRequest {
            model: AGENT_TASK_INTENT.into(),
            instruction: "Read the input and write a revision.".into(),
            input_files: vec![AgentTaskInputFile {
                path: path.into(),
                content_base64: "aGk=".into(),
                sha256: format!("{:x}", Sha256::digest(b"hi")),
            }],
            output_paths: vec!["draft.txt".into()],
        }
    }

    #[test]
    fn inline_files_are_bounded_and_content_verified() {
        assert!(request("chapter.txt").validate().is_ok());
        for path in [
            "/etc/passwd",
            "../secret",
            "a/../secret",
            "a//b",
            "C:secret",
            "a\\b",
            ".hidden",
            "中文.txt",
        ] {
            assert!(request(path).validate().is_err(), "accepted {path}");
        }
        let mut invalid = request("chapter.txt");
        invalid.input_files[0].sha256 = "0".repeat(64);
        assert_eq!(
            invalid.validate(),
            Err("input file sha256 does not match bytes")
        );
        let mut invalid = request("chapter.txt");
        invalid.output_paths = vec!["../../outside".into()];
        assert!(invalid.validate().is_err());
        let mut duplicate = request("A.txt");
        duplicate.input_files.push(AgentTaskInputFile {
            path: "a.txt".into(),
            content_base64: "aGk=".into(),
            sha256: format!("{:x}", Sha256::digest(b"hi")),
        });
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn unknown_request_fields_are_rejected() {
        let mut value = serde_json::to_value(request("a.txt")).unwrap();
        value["host_path"] = serde_json::json!("/Users/private");
        assert!(serde_json::from_value::<AgentTaskRequest>(value).is_err());
    }
}
