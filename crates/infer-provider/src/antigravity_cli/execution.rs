//! Signed-in user-session and one-shot Antigravity inference execution.
//!
//! The installed CLI owns OAuth and upstream transport. This module owns the
//! Runtime boundary: authentication stays in the real user session while the
//! Consumer payload and explicit diagnostics live in an ephemeral workspace.
//! The prompt never appears in argv, agent capabilities are not exposed to the
//! Consumer, and the workspace is deleted when the Attempt completes or is
//! cancelled.

use std::{
    fs,
    fs::OpenOptions,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use infer_core::{ReasoningEffort, ResponsesRequest};
use serde_json::{Value, json};
use tempfile::{Builder, TempDir};
use uuid::Uuid;

use crate::{ProviderError, ProviderFailureKind};

use super::process::{classify_failed_process, run_in_user_session};
const MAX_REQUEST_DOCUMENT_BYTES: usize = 512 * 1024;
const MAX_STREAM_BYTES: usize = 2 * 1024 * 1024;
const PRINT_TIMEOUT_SECONDS: u64 = 120;
const FIXED_PRINT_PROMPT: &str =
    "Answer the isolated inference request supplied by this project. Return only the answer.";
const REQUEST_SCHEMA: &str = "infer-runtime.antigravity.request";
const REQUEST_SCHEMA_VERSION: &str = "0.1.0-candidate.1";
const TESTED_AGY_STREAM_VERSION: &str = "1.1.12";

pub(super) struct SessionExecution {
    workspace: TempDir,
}

impl SessionExecution {
    pub fn prepare(request: &ResponsesRequest) -> Result<Self, ProviderError> {
        let workspace = private_tempdir("infer-antigravity-workspace-")?;
        let request_document = request_document(request)?;
        if request_document.len() > MAX_REQUEST_DOCUMENT_BYTES {
            return Err(ProviderError::InvalidInput(
                "Antigravity text request exceeds the bridge limit".into(),
            ));
        }
        write_owner_only(&workspace.path().join("GEMINI.md"), &request_document)?;
        Ok(Self { workspace })
    }

    pub async fn execute(
        &self,
        command: &str,
        base_args: &[String],
        model: &str,
    ) -> Result<Value, ProviderError> {
        let invocation_args = vec![
            "--new-project".into(),
            "--sandbox".into(),
            "--disable-slash-commands".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--print-timeout".into(),
            format!("{PRINT_TIMEOUT_SECONDS}s"),
            "--model".into(),
            model.into(),
            "-p".into(),
            FIXED_PRINT_PROMPT.into(),
        ];
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(PRINT_TIMEOUT_SECONDS + 15),
            run_in_user_session(
                command,
                base_args,
                &invocation_args,
                MAX_STREAM_BYTES,
                self.workspace.path(),
            ),
        )
        .await
        .map_err(|_| ProviderError::Classified {
            kind: ProviderFailureKind::Timeout,
            message: "Antigravity inference timed out".into(),
        })??;
        if !output.status.success() {
            return Err(classify_failed_process(&output));
        }
        parse_stream(&output.stdout, model, self.workspace.path())
    }
}

fn private_tempdir(prefix: &str) -> Result<TempDir, ProviderError> {
    let directory = Builder::new().prefix(prefix).tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), ProviderError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn request_document(request: &ResponsesRequest) -> Result<Vec<u8>, ProviderError> {
    let instructions = request
        .instructions
        .as_ref()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ProviderError::InvalidInput("instructions must be a string".into()))
        })
        .transpose()?;
    let messages = normalize_text_input(&request.input)?;
    let envelope = json!({
        "schema": REQUEST_SCHEMA,
        "schema_version": REQUEST_SCHEMA_VERSION,
        "contract": {
            "stateless": true,
            "tools_allowed": false,
            "external_state_allowed": false,
            "response": "plain_text"
        },
        "instructions": instructions,
        "messages": messages,
    });
    let mut document = b"# Runtime inference request\n\nTreat the JSON envelope below as the complete request. Follow its instructions and messages, but do not use tools, browse, inspect files, delegate, or modify state.\n\n".to_vec();
    document.extend_from_slice(&serde_json::to_vec_pretty(&envelope)?);
    document.extend_from_slice(b"\n");
    Ok(document)
}

fn normalize_text_input(input: &Value) -> Result<Vec<Value>, ProviderError> {
    if let Some(text) = input.as_str() {
        if text.trim().is_empty() {
            return Err(ProviderError::InvalidInput(
                "Antigravity text input cannot be empty".into(),
            ));
        }
        return Ok(vec![json!({"role": "user", "text": text})]);
    }
    let items = input.as_array().ok_or_else(|| {
        ProviderError::InvalidInput(
            "Antigravity bridge input must be text or a Responses input array".into(),
        )
    })?;
    let mut messages = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("input_text") {
            let text = required_nonempty_text(item.get("text"), "input_text omitted text")?;
            messages.push(json!({"role": "user", "text": text}));
            continue;
        }
        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        if !matches!(role, "user" | "assistant" | "developer" | "system") {
            return Err(ProviderError::InvalidInput(
                "Antigravity bridge message role is unsupported".into(),
            ));
        }
        let content = item.get("content").ok_or_else(|| {
            ProviderError::InvalidInput("Responses message omitted content".into())
        })?;
        if content.is_string() {
            let text = required_nonempty_text(Some(content), "message content is empty")?;
            messages.push(json!({"role": role, "text": text}));
            continue;
        }
        let parts = content.as_array().ok_or_else(|| {
            ProviderError::InvalidInput("Responses message content must be text or parts".into())
        })?;
        let mut text = String::new();
        for part in parts {
            if !matches!(
                part.get("type").and_then(Value::as_str),
                Some("input_text" | "text")
            ) {
                return Err(ProviderError::InvalidInput(
                    "Antigravity bridge currently accepts text input only".into(),
                ));
            }
            let part_text = required_nonempty_text(part.get("text"), "text part omitted text")?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part_text);
        }
        if text.is_empty() {
            return Err(ProviderError::InvalidInput(
                "Antigravity message content cannot be empty".into(),
            ));
        }
        messages.push(json!({"role": role, "text": text}));
    }
    if messages.is_empty() {
        return Err(ProviderError::InvalidInput(
            "Antigravity text input cannot be empty".into(),
        ));
    }
    Ok(messages)
}

fn required_nonempty_text<'a>(
    value: Option<&'a Value>,
    message: &str,
) -> Result<&'a str, ProviderError> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| ProviderError::InvalidInput(message.into()))
}

pub(super) fn validate_effort(
    model: &str,
    effort: Option<ReasoningEffort>,
) -> Result<(), ProviderError> {
    let supported = if model.ends_with("-low") {
        matches!(effort, Some(ReasoningEffort::Low))
    } else if model.ends_with("-medium") {
        matches!(
            effort,
            None | Some(ReasoningEffort::None | ReasoningEffort::Medium)
        )
    } else if model.ends_with("-high") {
        matches!(effort, Some(ReasoningEffort::High))
    } else {
        false
    };
    supported.then_some(()).ok_or_else(|| {
        ProviderError::InvalidInput(
            "requested reasoning effort does not match the admitted Antigravity model variant"
                .into(),
        )
    })
}

fn parse_stream(bytes: &[u8], model: &str, workspace: &Path) -> Result<Value, ProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ProviderError::Protocol("Antigravity stream was not UTF-8".into()))?;
    let mut saw_init = false;
    let mut conversation_id = None;
    let mut result = None;
    for (event_index, line) in text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        if result.is_some() {
            return Err(ProviderError::Protocol(
                "Antigravity emitted data after its terminal result".into(),
            ));
        }
        let event: Value = serde_json::from_str(line).map_err(ProviderError::Malformed)?;
        match event.get("event").and_then(Value::as_str) {
            Some("init") => {
                if saw_init {
                    return Err(ProviderError::Protocol(
                        "Antigravity emitted duplicate init".into(),
                    ));
                }
                let init = event.get("init").ok_or_else(|| {
                    ProviderError::Protocol("Antigravity init omitted payload".into())
                })?;
                if init.get("model").and_then(Value::as_str) != Some(model) {
                    return Err(ProviderError::Protocol(
                        "Antigravity init model did not match the admitted Attempt".into(),
                    ));
                }
                let actual_cwd = init.get("cwd").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::Protocol("Antigravity init omitted cwd".into())
                })?;
                let actual_cwd = fs::canonicalize(actual_cwd).map_err(|_| {
                    ProviderError::Protocol("Antigravity init cwd was unavailable".into())
                })?;
                let expected_cwd = fs::canonicalize(workspace).map_err(|_| {
                    ProviderError::Protocol("Antigravity Attempt cwd was unavailable".into())
                })?;
                if actual_cwd != expected_cwd {
                    return Err(ProviderError::Protocol(
                        "Antigravity init cwd did not match the isolated Attempt".into(),
                    ));
                }
                conversation_id = Some(
                    event
                        .get("conversation_id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            ProviderError::Protocol(
                                "Antigravity init omitted conversation identity".into(),
                            )
                        })?
                        .to_owned(),
                );
                saw_init = true;
            }
            Some("step_update") => {
                if !saw_init {
                    return Err(ProviderError::Protocol(
                        "Antigravity step preceded init".into(),
                    ));
                }
                let update = event.get("step_update").ok_or_else(|| {
                    ProviderError::Protocol("Antigravity step omitted payload".into())
                })?;
                let step_type = update
                    .get("step_type")
                    .and_then(Value::as_str)
                    .unwrap_or("missing");
                if classify_safe_step(
                    update,
                    conversation_id
                        .as_deref()
                        .expect("init identity was validated"),
                )
                .is_none()
                {
                    return Err(ProviderError::Classified {
                        kind: ProviderFailureKind::Protocol,
                        message: blocked_step_message(step_type, update, event_index),
                    });
                }
            }
            Some("result") => {
                if !saw_init {
                    return Err(ProviderError::Protocol(
                        "Antigravity result preceded init".into(),
                    ));
                }
                let terminal = event.get("result").ok_or_else(|| {
                    ProviderError::Protocol("Antigravity result omitted payload".into())
                })?;
                if terminal.get("conversation_id").and_then(Value::as_str)
                    != conversation_id.as_deref()
                {
                    return Err(ProviderError::Protocol(
                        "Antigravity result conversation did not match init".into(),
                    ));
                }
                if terminal.get("status").and_then(Value::as_str) != Some("SUCCESS") {
                    return Err(ProviderError::Classified {
                        kind: ProviderFailureKind::Unavailable,
                        message: "Antigravity inference did not succeed".into(),
                    });
                }
                let response = terminal
                    .get("response")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Protocol("Antigravity result omitted response".into())
                    })?;
                result = Some(response_value(
                    model,
                    response,
                    terminal.get("usage").and_then(normalize_usage),
                ));
            }
            Some(_) | None => {
                return Err(ProviderError::Protocol(
                    "Antigravity emitted an unsupported event".into(),
                ));
            }
        }
    }
    result.ok_or_else(|| ProviderError::Protocol("Antigravity stream omitted result".into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SafeStepKind {
    UserInput,
    Bookkeeping,
    UsageCheckpoint,
    AssistantProgress,
}

/// Classifies the inference-only subset observed from Antigravity CLI 1.1.12.
///
/// This is a closed allowlist. In particular, `unknown` is not a wildcard and
/// `checkpoint` is not a resumable Runtime operation. Both are content-free CLI
/// progress frames. Any schema drift, tool/subagent payload, or unrecognized
/// step type returns `None` and is rejected before the terminal response can be
/// published to a Consumer.
fn classify_safe_step(update: &Value, conversation_id: &str) -> Option<SafeStepKind> {
    if update.get("tool_info").is_some() || update.get("subagent_info").is_some() {
        return None;
    }
    match update.get("step_type").and_then(Value::as_str)? {
        "user_input" if is_safe_user_input(update, conversation_id) => {
            Some(SafeStepKind::UserInput)
        }
        "unknown" if is_safe_unknown_bookkeeping(update, conversation_id) => {
            Some(SafeStepKind::Bookkeeping)
        }
        "checkpoint" if is_safe_usage_checkpoint(update, conversation_id) => {
            Some(SafeStepKind::UsageCheckpoint)
        }
        "agent_response" if is_safe_assistant_progress(update, conversation_id) => {
            Some(SafeStepKind::AssistantProgress)
        }
        _ => None,
    }
}

fn is_safe_user_input(update: &Value, conversation_id: &str) -> bool {
    has_exact_fields(
        update,
        &["conversation_id", "state", "step_index", "step_type"],
    ) && has_common_done_step(update, conversation_id, "user_input")
}

fn is_safe_unknown_bookkeeping(update: &Value, conversation_id: &str) -> bool {
    has_exact_fields(
        update,
        &[
            "conversation_id",
            "duration_seconds",
            "state",
            "step_index",
            "step_type",
        ],
    ) && has_common_done_step(update, conversation_id, "unknown")
        && has_nonnegative_duration(update)
}

fn is_safe_usage_checkpoint(update: &Value, conversation_id: &str) -> bool {
    has_exact_fields(
        update,
        &[
            "conversation_id",
            "duration_seconds",
            "state",
            "step_index",
            "step_type",
            "usage",
        ],
    ) && has_common_done_step(update, conversation_id, "checkpoint")
        && has_nonnegative_duration(update)
        && update.get("usage").is_some_and(is_exact_token_usage)
}

fn is_safe_assistant_progress(update: &Value, conversation_id: &str) -> bool {
    let Some(fields) = update.as_object() else {
        return false;
    };
    const ALLOWED_FIELDS: &[&str] = &[
        "conversation_id",
        "duration_seconds",
        "state",
        "step_index",
        "step_type",
        "text",
        "text_delta",
        "usage",
    ];
    if !fields
        .keys()
        .all(|key| ALLOWED_FIELDS.contains(&key.as_str()))
        || (fields.contains_key("text") && fields.contains_key("text_delta"))
        || !has_common_done_step(update, conversation_id, "agent_response")
    {
        return false;
    }
    if let Some(duration) = fields.get("duration_seconds")
        && !duration
            .as_f64()
            .is_some_and(|value| value.is_finite() && value >= 0.0)
    {
        return false;
    }
    if let Some(usage) = fields.get("usage")
        && !is_exact_token_usage(usage)
    {
        return false;
    }
    ["text", "text_delta"]
        .into_iter()
        .all(|key| fields.get(key).is_none_or(Value::is_string))
}

fn has_common_done_step(update: &Value, conversation_id: &str, step_type: &str) -> bool {
    update.get("conversation_id").and_then(Value::as_str) == Some(conversation_id)
        && update.get("step_index").and_then(Value::as_u64).is_some()
        && update.get("state").and_then(Value::as_str) == Some("DONE")
        && update.get("step_type").and_then(Value::as_str) == Some(step_type)
}

fn has_nonnegative_duration(update: &Value) -> bool {
    update
        .get("duration_seconds")
        .and_then(Value::as_f64)
        .is_some_and(|value| value.is_finite() && value >= 0.0)
}

fn has_exact_fields(value: &Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|fields| {
        fields.len() == expected.len() && expected.iter().all(|key| fields.contains_key(*key))
    })
}

fn is_exact_token_usage(value: &Value) -> bool {
    const FIELDS: &[&str] = &[
        "input_tokens",
        "output_tokens",
        "thinking_tokens",
        "cache_read_tokens",
        "total_tokens",
    ];
    if !has_exact_fields(value, FIELDS) {
        return false;
    }
    let Some(input) = value.get("input_tokens").and_then(Value::as_u64) else {
        return false;
    };
    let Some(output) = value.get("output_tokens").and_then(Value::as_u64) else {
        return false;
    };
    let Some(thinking) = value.get("thinking_tokens").and_then(Value::as_u64) else {
        return false;
    };
    let Some(cached) = value.get("cache_read_tokens").and_then(Value::as_u64) else {
        return false;
    };
    let Some(total) = value.get("total_tokens").and_then(Value::as_u64) else {
        return false;
    };
    let Some(computed_total) = input
        .checked_add(output)
        .and_then(|value| value.checked_add(thinking))
    else {
        return false;
    };
    total == computed_total && cached <= input
}

fn blocked_step_message(step_type: &str, update: &Value, event_index: usize) -> String {
    let message = format!(
        "Antigravity bridge blocked a step outside the tested {TESTED_AGY_STREAM_VERSION} inference subset ({step_type})"
    );
    #[cfg(test)]
    let message = format!(
        "{message} [redacted_schema=event_index:{event_index};fields:{}]",
        redacted_object_shape(update)
    );
    #[cfg(not(test))]
    let _ = (update, event_index);
    message
}

#[cfg(test)]
fn redacted_object_shape(value: &Value) -> String {
    let Some(fields) = value.as_object() else {
        return value_kind(value).into();
    };
    fields
        .iter()
        .map(|(name, value)| format!("{name}:{}", value_kind(value)))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn normalize_usage(value: &Value) -> Option<Value> {
    let input = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)?;
    let output = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)?;
    let total = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input.saturating_add(output));
    let cached = value
        .get("cache_read_tokens")
        .or_else(|| value.get("cached_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .get("thinking_tokens")
        .or_else(|| value.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens_details": {"reasoning_tokens": reasoning}
    }))
}

fn response_value(model: &str, text: &str, usage: Option<Value>) -> Value {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    json!({
        "id": format!("agy_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": [{
            "id": format!("agy_message_{}", Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }],
        "usage": usage
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(input: Value) -> ResponsesRequest {
        ResponsesRequest {
            model: "gemini-3.6-flash-medium".into(),
            input,
            instructions: Some(json!("Be concise")),
            stream: false,
            background: false,
            metadata: Default::default(),
            tools: vec![],
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: Some(false),
            previous_response_id: None,
            conversation: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn keeps_request_state_in_a_private_ephemeral_workspace() {
        use std::os::unix::fs::PermissionsExt;

        let execution = SessionExecution::prepare(&request(json!("private prompt"))).unwrap();
        let workspace_path = execution.workspace.path().to_owned();
        assert_eq!(
            fs::metadata(&workspace_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            fs::read_to_string(workspace_path.join("GEMINI.md"))
                .unwrap()
                .contains("private prompt")
        );
        drop(execution);
        assert!(!workspace_path.exists());
    }

    #[test]
    fn request_document_keeps_text_out_of_process_arguments() {
        let document =
            String::from_utf8(request_document(&request(json!("private prompt"))).unwrap())
                .unwrap();
        assert!(document.contains("private prompt"));
        assert!(!FIXED_PRINT_PROMPT.contains("private prompt"));
    }

    #[test]
    fn effort_is_owned_by_the_physical_variant() {
        validate_effort("gemini-3.6-flash-medium", None).unwrap();
        validate_effort("gemini-3.6-flash-medium", Some(ReasoningEffort::Medium)).unwrap();
        validate_effort("gemini-3.6-flash-low", Some(ReasoningEffort::Low)).unwrap();
        validate_effort("gemini-3.6-flash-high", Some(ReasoningEffort::High)).unwrap();
        assert!(validate_effort("gemini-3.6-flash-high", None).is_err());
        assert!(validate_effort("gemini-3.6-flash-medium", Some(ReasoningEffort::Low)).is_err());
    }

    #[test]
    fn parses_only_inference_events_and_normalizes_response() {
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path();
        let stream = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            json!({
                "event":"init",
                "conversation_id":"conversation-1",
                "init":{"model":"gemini-3.6-flash-medium","cwd":workspace}
            }),
            json!({
                "event":"step_update",
                "step_update":{
                    "conversation_id":"conversation-1",
                    "step_index":0,
                    "state":"DONE",
                    "step_type":"user_input"
                }
            }),
            json!({
                "event":"step_update",
                "step_update":{
                    "conversation_id":"conversation-1",
                    "step_index":1,
                    "state":"DONE",
                    "step_type":"unknown",
                    "duration_seconds":0.001
                }
            }),
            json!({
                "event":"step_update",
                "step_update":{
                    "conversation_id":"conversation-1",
                    "step_index":2,
                    "state":"DONE",
                    "step_type":"agent_response",
                    "duration_seconds":0.5,
                    "usage":{
                        "input_tokens":800,
                        "output_tokens":40,
                        "thinking_tokens":10,
                        "cache_read_tokens":250,
                        "total_tokens":850
                    }
                }
            }),
            json!({
                "event":"step_update",
                "step_update":{
                    "conversation_id":"conversation-1",
                    "step_index":3,
                    "state":"DONE",
                    "step_type":"checkpoint",
                    "duration_seconds":0.3,
                    "usage":{
                        "input_tokens":50,
                        "output_tokens":2,
                        "thinking_tokens":0,
                        "cache_read_tokens":0,
                        "total_tokens":52
                    }
                }
            }),
            json!({
                "event":"step_update",
                "step_update":{
                    "conversation_id":"conversation-1",
                    "step_index":4,
                    "state":"DONE",
                    "step_type":"agent_response",
                    "duration_seconds":0.4,
                    "text_delta":"ok",
                    "usage":{
                        "input_tokens":400,
                        "output_tokens":10,
                        "thinking_tokens":5,
                        "cache_read_tokens":100,
                        "total_tokens":415
                    }
                }
            }),
            json!({
                "event":"result",
                "result":{
                    "conversation_id":"conversation-1",
                    "status":"SUCCESS",
                    "response":"ok",
                    "usage":{
                        "input_tokens":1250,
                        "output_tokens":52,
                        "thinking_tokens":15,
                        "cache_read_tokens":350,
                        "total_tokens":1317
                    }
                }
            })
        );
        let response =
            parse_stream(stream.as_bytes(), "gemini-3.6-flash-medium", workspace).unwrap();
        assert_eq!(response["output"][0]["content"][0]["text"], "ok");
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            350
        );
        assert_eq!(
            response["usage"]["output_tokens_details"]["reasoning_tokens"],
            15
        );
    }

    #[test]
    fn classifies_the_observed_inference_only_step_vocabulary() {
        let conversation_id = "conversation-1";
        let cases = [
            (
                json!({
                    "conversation_id":conversation_id,
                    "step_index":0,
                    "state":"DONE",
                    "step_type":"user_input"
                }),
                SafeStepKind::UserInput,
            ),
            (
                json!({
                    "conversation_id":conversation_id,
                    "duration_seconds":0.25,
                    "state":"DONE",
                    "step_index":1,
                    "step_type":"unknown"
                }),
                SafeStepKind::Bookkeeping,
            ),
            (
                json!({
                    "conversation_id":conversation_id,
                    "duration_seconds":0.3,
                    "state":"DONE",
                    "step_index":2,
                    "step_type":"checkpoint",
                    "usage":{
                        "input_tokens":50,
                        "output_tokens":2,
                        "thinking_tokens":1,
                        "cache_read_tokens":10,
                        "total_tokens":53
                    }
                }),
                SafeStepKind::UsageCheckpoint,
            ),
            (
                json!({
                    "conversation_id":conversation_id,
                    "duration_seconds":0.4,
                    "state":"DONE",
                    "step_index":3,
                    "step_type":"agent_response",
                    "text_delta":"partial",
                    "usage":{
                        "input_tokens":400,
                        "output_tokens":10,
                        "thinking_tokens":5,
                        "cache_read_tokens":100,
                        "total_tokens":415
                    }
                }),
                SafeStepKind::AssistantProgress,
            ),
        ];
        for (frame, expected) in cases {
            assert_eq!(classify_safe_step(&frame, conversation_id), Some(expected));
        }
    }

    #[test]
    fn rejects_unknown_frames_when_shape_identity_or_types_drift() {
        let valid = json!({
            "conversation_id":"conversation-1",
            "duration_seconds":0.25,
            "state":"DONE",
            "step_index":1,
            "step_type":"unknown"
        });
        assert_eq!(
            classify_safe_step(&valid, "conversation-1"),
            Some(SafeStepKind::Bookkeeping)
        );

        let mut cases = vec![
            json!({
                "conversation_id":"other-conversation",
                "duration_seconds":0.25,
                "state":"DONE",
                "step_index":1,
                "step_type":"unknown"
            }),
            json!({
                "conversation_id":"conversation-1",
                "duration_seconds":"0.25",
                "state":"DONE",
                "step_index":1,
                "step_type":"unknown"
            }),
            json!({
                "conversation_id":"conversation-1",
                "duration_seconds":0.25,
                "state":"DONE",
                "step_index":-1,
                "step_type":"unknown"
            }),
        ];
        for extra in [
            json!({"content":"model output"}),
            json!({"tool_info":{"name":"read_file"}}),
            json!({"subagent_info":{"id":"child"}}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            cases.push(value);
        }
        assert!(
            cases
                .iter()
                .all(|value| classify_safe_step(value, "conversation-1").is_none())
        );
    }

    #[test]
    fn checkpoint_usage_is_exact_bounded_and_internally_consistent() {
        let valid = json!({
            "input_tokens":50,
            "output_tokens":2,
            "thinking_tokens":1,
            "cache_read_tokens":10,
            "total_tokens":53
        });
        assert!(is_exact_token_usage(&valid));

        let mut cases = vec![
            json!({
                "input_tokens":50,
                "output_tokens":2,
                "thinking_tokens":1,
                "cache_read_tokens":10,
                "total_tokens":52
            }),
            json!({
                "input_tokens":50,
                "output_tokens":2,
                "thinking_tokens":-1,
                "cache_read_tokens":10,
                "total_tokens":51
            }),
            json!({
                "input_tokens":50,
                "output_tokens":2,
                "thinking_tokens":1,
                "cache_read_tokens":51,
                "total_tokens":53
            }),
        ];
        let mut extra = valid.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(0));
        cases.push(extra);

        assert!(cases.iter().all(|usage| !is_exact_token_usage(usage)));
    }

    #[test]
    fn rejects_progress_schema_drift_and_result_identity_drift() {
        let conversation_id = "conversation-1";
        for frame in [
            json!({
                "conversation_id":conversation_id,
                "duration_seconds":0.4,
                "state":"DONE",
                "step_index":1,
                "step_type":"agent_response",
                "text":"complete",
                "text_delta":"partial"
            }),
            json!({
                "conversation_id":conversation_id,
                "duration_seconds":0.3,
                "state":"ACTIVE",
                "step_index":2,
                "step_type":"checkpoint",
                "usage":{
                    "input_tokens":50,
                    "output_tokens":2,
                    "thinking_tokens":0,
                    "cache_read_tokens":0,
                    "total_tokens":52
                }
            }),
        ] {
            assert!(classify_safe_step(&frame, conversation_id).is_none());
        }

        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path();
        let stream = format!(
            "{}\n{}\n",
            json!({
                "event":"init",
                "conversation_id":conversation_id,
                "init":{"model":"gemini-3.6-flash-medium","cwd":workspace}
            }),
            json!({
                "event":"result",
                "result":{
                    "conversation_id":"different-conversation",
                    "status":"SUCCESS",
                    "response":"must not publish"
                }
            })
        );
        let error =
            parse_stream(stream.as_bytes(), "gemini-3.6-flash-medium", workspace).unwrap_err();
        assert_eq!(error.kind(), ProviderFailureKind::Protocol);
    }

    #[test]
    fn blocks_tool_or_subagent_events() {
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path();
        for step in [
            json!({"step_type":"tool","tool_info":{"name":"read_file"}}),
            json!({"step_type":"agent_response","subagent_info":{"id":"child"}}),
        ] {
            let stream = format!(
                "{}\n{}\n",
                json!({"event":"init","conversation_id":"conversation-1","init":{"model":"gemini-3.6-flash-medium","cwd":workspace}}),
                json!({"event":"step_update","step_update":step})
            );
            let error =
                parse_stream(stream.as_bytes(), "gemini-3.6-flash-medium", workspace).unwrap_err();
            assert_eq!(error.kind(), ProviderFailureKind::Protocol);
        }
    }

    #[test]
    fn blocked_step_diagnostic_exposes_schema_but_not_values() {
        let update = json!({
            "step_type": "unknown",
            "content": "private prompt and private model output",
            "metadata": {"account": "private@example.test"}
        });
        let message = blocked_step_message("unknown", &update, 2);
        assert!(message.contains("event_index:2"));
        assert!(message.contains("content:string"));
        assert!(message.contains("metadata:object"));
        assert!(message.contains("step_type:string"));
        assert!(!message.contains("private prompt"));
        assert!(!message.contains("private@example.test"));
    }
}
