//! Hardened Codex App Server bridge for subscription-backed inference.
//!
//! Codex App Server is an agent protocol, not a Responses endpoint. This
//! adapter deliberately exposes only stateless inference with typed text/image
//! input, append-only text output, and one separately admitted hosted image
//! generation operation. Each call runs in an empty ephemeral workspace; all
//! other tool-like items fail the Attempt closed.

mod image_generation;
mod input;

use std::{
    collections::{BTreeSet, VecDeque},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use async_stream::try_stream;
use async_trait::async_trait;
use bytes::Bytes;
use infer_core::{ReasoningEffort, ResponsesRequest};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use uuid::Uuid;

use crate::{
    Provider, ProviderByteStream, ProviderError, ProviderFailureKind, ProviderModelCatalog,
    ProviderModelInfo,
};
use image_generation::GeneratedImage;
use input::prepare_turn_input;

const TEXT_BASE_INSTRUCTIONS: &str = "You are serving one stateless inference request. Answer the user directly. Do not use tools, inspect files, run commands, access applications, browse, delegate, or modify external state. Do not mention this bridge or its execution environment.";
const IMAGE_BASE_INSTRUCTIONS: &str = "You are serving one stateless image generation request. Use the built-in image generation tool exactly once to generate one PNG from the user's text prompt. Do not use any other tool, inspect files, run commands, access applications, browse, delegate, or modify external state. Do not mention this bridge or its execution environment.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeMode {
    Text,
    ImageGeneration,
}

pub struct CodexAppServerProvider {
    id: String,
    command: String,
    args: Vec<String>,
    admitted_models: BTreeSet<String>,
}

impl CodexAppServerProvider {
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        admitted_models: BTreeSet<String>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
            admitted_models,
        }
    }

    async fn session(&self) -> Result<CodexSession, ProviderError> {
        CodexSession::spawn(&self.command, &self.args).await
    }

    async fn catalog_with_session(
        &self,
        session: &mut CodexSession,
    ) -> Result<ProviderModelCatalog, ProviderError> {
        let mut cursor = None;
        let mut models = Vec::new();
        loop {
            let result = session
                .request(
                    "model/list",
                    json!({
                        "cursor": cursor,
                        "includeHidden": false,
                        "limit": 100,
                    }),
                )
                .await?;
            let page = result
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| ProviderError::Protocol("model/list omitted data".into()))?;
            models.extend(
                page.iter()
                    .map(|model| self.parse_model(model))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(ProviderModelCatalog {
            provider: self.id.clone(),
            models,
        })
    }

    fn parse_model(&self, value: &Value) -> Result<ProviderModelInfo, ProviderError> {
        let string = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| ProviderError::Protocol(format!("model/list omitted {field}")))
        };
        let model = string("model")?;
        let supported_reasoning_efforts = value
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("reasoningEffort").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let input_modalities = value
            .get("inputModalities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok(ProviderModelInfo {
            id: string("id")?,
            admitted: self.admitted_models.contains(&model),
            model,
            display_name: string("displayName")?,
            description: string("description")?,
            input_modalities,
            supported_reasoning_efforts,
            default_reasoning_effort: string("defaultReasoningEffort")?,
            is_default: value
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hidden: value
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            upgrade: value
                .get("upgrade")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    pub async fn discover_model_catalog(&self) -> Result<ProviderModelCatalog, ProviderError> {
        let mut session = self.session().await?;
        self.catalog_with_session(&mut session).await
    }

    async fn execute_inner(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
        let turn = self.prepare_turn(&request).await?;
        collect_turn(turn).await
    }

    async fn prepare_turn(
        &self,
        request: &ResponsesRequest,
    ) -> Result<PreparedTurn, ProviderError> {
        let mode = validate_request(request)?;
        let developer_instructions = optional_text("instructions", request.instructions.as_ref())?;
        let effort = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort)
            .and_then(|effort| (effort != ReasoningEffort::None).then(|| effort_string(effort)));

        let mut session = self.session().await?;
        let catalog = self.catalog_with_session(&mut session).await?;
        let model = catalog
            .models
            .iter()
            .find(|candidate| candidate.model == request.model)
            .ok_or_else(|| ProviderError::Classified {
                kind: ProviderFailureKind::Unavailable,
                message: "configured Codex model is absent from current model/list".into(),
            })?;
        if !model.admitted {
            return Err(ProviderError::InvalidInput(
                "Codex model was discovered but is not admitted by a Deployment".into(),
            ));
        }
        if mode == BridgeMode::ImageGeneration {
            let capabilities = session
                .request("modelProvider/capabilities/read", json!({}))
                .await?;
            if capabilities.get("imageGeneration").and_then(Value::as_bool) != Some(true) {
                return Err(ProviderError::Classified {
                    kind: ProviderFailureKind::Unavailable,
                    message: "Codex provider does not currently advertise image generation".into(),
                });
            }
        }
        if let Some(effort) = effort.as_deref()
            && !model
                .supported_reasoning_efforts
                .iter()
                .any(|supported| supported == effort)
        {
            return Err(ProviderError::InvalidInput(format!(
                "Codex model does not advertise reasoning effort {effort}"
            )));
        }
        let input = prepare_turn_input(&request.input, session.workspace_path()).await?;
        if input.has_images
            && !model
                .input_modalities
                .iter()
                .any(|modality| modality == "image")
        {
            return Err(ProviderError::InvalidInput(
                "selected Codex model does not advertise image input".into(),
            ));
        }

        let thread = session
            .request(
                "thread/start",
                json!({
                    "approvalPolicy": "never",
                    "baseInstructions": match mode {
                        BridgeMode::Text => TEXT_BASE_INSTRUCTIONS,
                        BridgeMode::ImageGeneration => IMAGE_BASE_INSTRUCTIONS,
                    },
                    "cwd": session.workspace_path().display().to_string(),
                    "developerInstructions": developer_instructions,
                    "ephemeral": true,
                    "model": request.model,
                    "personality": "none",
                    "sandbox": "read-only",
                    "serviceName": "infer-runtime",
                }),
            )
            .await?;
        let thread_id = thread
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Protocol("thread/start omitted thread.id".into()))?
            .to_owned();
        let turn = session
            .request(
                "turn/start",
                json!({
                    "approvalPolicy": "never",
                    "effort": effort,
                    "input": input.items,
                    "sandboxPolicy": {"type": "readOnly", "networkAccess": false},
                    "summary": "none",
                    "threadId": thread_id,
                }),
            )
            .await?;
        let turn_id = turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Protocol("turn/start omitted turn.id".into()))?
            .to_owned();
        Ok(PreparedTurn {
            session,
            model: request.model.clone(),
            thread_id,
            turn_id,
            mode,
        })
    }
}

#[async_trait]
impl Provider for CodexAppServerProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
        self.execute_inner(request).await
    }

    async fn execute_stream(
        &self,
        request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        if validate_request(&request)? != BridgeMode::Text {
            return Err(ProviderError::InvalidInput(
                "image generation does not support streaming".into(),
            ));
        }
        let turn = self.prepare_turn(&request).await?;
        Ok(stream_turn(turn))
    }

    async fn model_catalog(&self) -> Result<Option<ProviderModelCatalog>, ProviderError> {
        self.discover_model_catalog().await.map(Some)
    }
}

fn validate_request(request: &ResponsesRequest) -> Result<BridgeMode, ProviderError> {
    let mode = if request.tools.is_empty() {
        BridgeMode::Text
    } else if request.requests_image_generation() {
        BridgeMode::ImageGeneration
    } else {
        return Err(ProviderError::InvalidInput(
            "only the exact image_generation tool is exposed by the Codex bridge".into(),
        ));
    };
    if mode == BridgeMode::ImageGeneration && (request.stream || request.background) {
        return Err(ProviderError::InvalidInput(
            "image generation supports unary execution only".into(),
        ));
    }
    if request.temperature.is_some()
        || request.top_p.is_some()
        || request.max_output_tokens.is_some()
        || request.truncation.is_some()
        || !request.metadata.is_empty()
    {
        return Err(ProviderError::InvalidInput(
            "request uses a field outside the Codex bridge subset".into(),
        ));
    }
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| !reasoning.extra.is_empty())
    {
        return Err(ProviderError::InvalidInput(
            "Codex bridge supports reasoning.effort only".into(),
        ));
    }
    Ok(mode)
}

fn optional_text(field: &str, value: Option<&Value>) -> Result<Option<String>, ProviderError> {
    value
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ProviderError::InvalidInput(format!("{field} must be a string")))
        })
        .transpose()
}

fn effort_string(effort: ReasoningEffort) -> String {
    serde_json::to_value(effort)
        .expect("reasoning effort serializes")
        .as_str()
        .expect("reasoning effort is a string")
        .to_owned()
}

struct PreparedTurn {
    session: CodexSession,
    model: String,
    thread_id: String,
    turn_id: String,
    mode: BridgeMode,
}

#[derive(Default)]
struct TurnAccumulator {
    usage: Option<Value>,
    messages: Vec<Value>,
    images: Vec<GeneratedImage>,
}

enum TurnProgress {
    Continue,
    TextDelta(String),
    Completed(Value),
}

async fn collect_turn(mut turn: PreparedTurn) -> Result<Value, ProviderError> {
    let mut accumulator = TurnAccumulator::default();
    loop {
        let message = turn.session.next_message().await?;
        if let TurnProgress::Completed(response) = observe_turn_message(
            &message,
            &turn.model,
            &turn.thread_id,
            &turn.turn_id,
            turn.mode,
            &mut accumulator,
        )? {
            return Ok(response);
        }
    }
}

fn observe_turn_message(
    message: &Value,
    model: &str,
    thread_id: &str,
    turn_id: &str,
    mode: BridgeMode,
    accumulator: &mut TurnAccumulator,
) -> Result<TurnProgress, ProviderError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(TurnProgress::Continue);
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    if params
        .get("threadId")
        .and_then(Value::as_str)
        .is_some_and(|candidate| candidate != thread_id)
        || params
            .get("turnId")
            .and_then(Value::as_str)
            .is_some_and(|candidate| candidate != turn_id)
    {
        return Ok(TurnProgress::Continue);
    }
    match method {
        "item/started" | "item/completed" => {
            let item = params
                .get("item")
                .ok_or_else(|| ProviderError::Protocol(format!("{method} omitted item")))?;
            enforce_inference_item(item, mode)?;
            if method == "item/completed" {
                record_completed_item(item, accumulator)?;
            }
            Ok(TurnProgress::Continue)
        }
        "item/agentMessage/delta" => {
            let delta = params
                .get("delta")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::Protocol("agentMessage delta omitted text".into()))?;
            Ok(TurnProgress::TextDelta(delta.to_owned()))
        }
        "thread/tokenUsage/updated" => {
            accumulator.usage = params
                .pointer("/tokenUsage/last")
                .map(normalize_usage)
                .transpose()?;
            Ok(TurnProgress::Continue)
        }
        "turn/completed" => {
            let turn = params
                .get("turn")
                .ok_or_else(|| ProviderError::Protocol("turn/completed omitted turn".into()))?;
            match turn.get("status").and_then(Value::as_str) {
                Some("completed") => {}
                Some("interrupted") => {
                    return Err(ProviderError::Classified {
                        kind: ProviderFailureKind::Unavailable,
                        message: "Codex turn was interrupted".into(),
                    });
                }
                Some("failed") => return Err(classify_turn_failure(turn)),
                _ => {
                    return Err(ProviderError::Protocol(
                        "turn/completed carried an unknown status".into(),
                    ));
                }
            }
            if let Some(items) = turn.get("items").and_then(Value::as_array) {
                for item in items {
                    enforce_inference_item(item, mode)?;
                    record_completed_item(item, accumulator)?;
                }
            }
            Ok(TurnProgress::Completed(response_value(
                model,
                mode,
                std::mem::take(&mut accumulator.messages),
                std::mem::take(&mut accumulator.images),
                accumulator.usage.take(),
            )?))
        }
        "error" => Err(ProviderError::Protocol(
            "Codex App Server emitted an error notification".into(),
        )),
        _ => Ok(TurnProgress::Continue),
    }
}

fn stream_turn(mut turn: PreparedTurn) -> ProviderByteStream {
    let stream = try_stream! {
        let response_id = format!("codex_{}", Uuid::new_v4().simple());
        yield sse_frame("response.created", json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
                "model": turn.model,
                "output": [],
            }
        }))?;
        let mut accumulator = TurnAccumulator::default();
        let mut sequence = 0_u64;
        loop {
            let message = turn.session.next_message().await?;
            match observe_turn_message(
                &message,
                &turn.model,
                &turn.thread_id,
                &turn.turn_id,
                turn.mode,
                &mut accumulator,
            )? {
                TurnProgress::Continue => {}
                TurnProgress::TextDelta(delta) => {
                    sequence += 1;
                    yield sse_frame("response.output_text.delta", json!({
                        "type": "response.output_text.delta",
                        "sequence_number": sequence,
                        "delta": delta,
                    }))?;
                }
                TurnProgress::Completed(mut response) => {
                    response["id"] = Value::String(response_id.clone());
                    yield sse_frame("response.completed", json!({
                        "type": "response.completed",
                        "sequence_number": sequence + 1,
                        "response": response,
                    }))?;
                    return;
                }
            }
        }
    };
    Box::pin(stream)
}

fn sse_frame(event: &str, data: Value) -> Result<Bytes, ProviderError> {
    let data = serde_json::to_string(&data)?;
    Ok(Bytes::from(format!("event: {event}\ndata: {data}\n\n")))
}

fn enforce_inference_item(item: &Value, mode: BridgeMode) -> Result<(), ProviderError> {
    match item.get("type").and_then(Value::as_str) {
        Some("userMessage" | "agentMessage" | "reasoning") => Ok(()),
        Some("imageGeneration") if mode == BridgeMode::ImageGeneration => Ok(()),
        Some(_) => Err(ProviderError::Classified {
            kind: ProviderFailureKind::Protocol,
            message: "Codex bridge blocked non-inference item/tool use".into(),
        }),
        None => Err(ProviderError::Protocol(
            "Codex item omitted its type".into(),
        )),
    }
}

fn record_completed_item(
    item: &Value,
    accumulator: &mut TurnAccumulator,
) -> Result<(), ProviderError> {
    match item.get("type").and_then(Value::as_str) {
        Some("agentMessage") => accumulator.messages.push(item.clone()),
        Some("imageGeneration") => {
            let image = GeneratedImage::from_completed_item(item)?;
            if let Some(existing) = accumulator
                .images
                .iter()
                .find(|existing| existing.id() == image.id())
            {
                if existing != &image {
                    return Err(ProviderError::Protocol(
                        "Codex repeated an image result with conflicting content".into(),
                    ));
                }
            } else if accumulator.images.is_empty() {
                accumulator.images.push(image);
            } else {
                return Err(ProviderError::Protocol(
                    "Codex generated more than one image".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn classify_turn_failure(turn: &Value) -> ProviderError {
    let info = turn.pointer("/error/codexErrorInfo");
    let name = info.and_then(Value::as_str).unwrap_or("other");
    let kind = match name {
        "unauthorized" => ProviderFailureKind::Authentication,
        "usageLimitExceeded" | "sessionBudgetExceeded" => ProviderFailureKind::RateLimited,
        "serverOverloaded" | "internalServerError" => ProviderFailureKind::Unavailable,
        "badRequest" | "contextWindowExceeded" => ProviderFailureKind::InvalidRequest,
        _ => ProviderFailureKind::Protocol,
    };
    ProviderError::Classified {
        kind,
        message: format!("Codex turn failed with {name}"),
    }
}

fn normalize_usage(value: &Value) -> Result<Value, ProviderError> {
    let number = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_i64)
            .ok_or_else(|| ProviderError::Protocol(format!("token usage omitted {field}")))
    };
    Ok(json!({
        "input_tokens": number("inputTokens")?,
        "output_tokens": number("outputTokens")?,
        "total_tokens": number("totalTokens")?,
        "input_tokens_details": {
            "cached_tokens": number("cachedInputTokens")?,
        },
        "output_tokens_details": {
            "reasoning_tokens": number("reasoningOutputTokens")?,
        },
    }))
}

fn response_value(
    model: &str,
    mode: BridgeMode,
    messages: Vec<Value>,
    images: Vec<GeneratedImage>,
    usage: Option<Value>,
) -> Result<Value, ProviderError> {
    match mode {
        BridgeMode::Text => text_response_value(model, messages, usage),
        BridgeMode::ImageGeneration => image_response_value(model, images, usage),
    }
}

fn text_response_value(
    model: &str,
    messages: Vec<Value>,
    usage: Option<Value>,
) -> Result<Value, ProviderError> {
    let selected = messages
        .iter()
        .rev()
        .find(|item| item.get("phase").and_then(Value::as_str) == Some("final_answer"))
        .or_else(|| messages.last())
        .ok_or_else(|| ProviderError::Protocol("Codex turn completed without an answer".into()))?;
    let text = selected
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Protocol("agentMessage omitted text".into()))?;
    let message_id = selected
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("codex_message");
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(json!({
        "id": format!("codex_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": [{
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }],
        "usage": usage,
    }))
}

fn image_response_value(
    model: &str,
    images: Vec<GeneratedImage>,
    usage: Option<Value>,
) -> Result<Value, ProviderError> {
    let [image] = images.as_slice() else {
        return Err(ProviderError::Protocol(
            "Codex turn completed without exactly one generated image".into(),
        ));
    };
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(json!({
        "id": format!("codex_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": [image.response_item()],
        "usage": usage,
    }))
}

struct CodexSession {
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    pending: VecDeque<Value>,
    next_id: u64,
    workspace: TempDir,
}

impl CodexSession {
    async fn spawn(command: &str, args: &[String]) -> Result<Self, ProviderError> {
        let workspace = tempfile::tempdir()?;
        let mut child = Command::new(command)
            .args(args)
            .current_dir(workspace.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Protocol("Codex stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Protocol("Codex stdout was not piped".into()))?;
        let mut session = Self {
            _child: child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout).lines(),
            pending: VecDeque::new(),
            next_id: 1,
            workspace,
        };
        session
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "infer-runtime",
                        "title": "infer-runtime subscription bridge",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {"experimentalApi": true},
                }),
            )
            .await?;
        session.notify("initialized", json!({})).await?;
        Ok(session)
    }

    fn workspace_path(&self) -> &std::path::Path {
        self.workspace.path()
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ProviderError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let message = self.read().await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if message.get("error").is_some() {
                    return Err(ProviderError::Protocol(format!(
                        "Codex App Server rejected {method}"
                    )));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| ProviderError::Protocol(format!("{method} omitted result")));
            }
            if message.get("method").is_some() && message.get("id").is_some() {
                return Err(ProviderError::Classified {
                    kind: ProviderFailureKind::Protocol,
                    message: "Codex bridge refused an interactive server request".into(),
                });
            }
            self.pending.push_back(message);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), ProviderError> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn write(&mut self, value: Value) -> Result<(), ProviderError> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value, ProviderError> {
        let line = self
            .stdout
            .next_line()
            .await?
            .ok_or_else(|| ProviderError::Protocol("Codex App Server closed stdout".into()))?;
        serde_json::from_str(&line).map_err(ProviderError::Malformed)
    }

    async fn next_message(&mut self) -> Result<Value, ProviderError> {
        match self.pending.pop_front() {
            Some(message) => Ok(message),
            None => self.read().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn request(input: Value) -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-5.6-terra".into(),
            input,
            instructions: None,
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

    fn real_codex_provider() -> CodexAppServerProvider {
        let args = vec![
            "app-server",
            "--strict-config",
            "--disable",
            "shell_tool",
            "--disable",
            "unified_exec",
            "--disable",
            "plugins",
            "--disable",
            "apps",
            "--disable",
            "multi_agent",
            "--disable",
            "computer_use",
            "-c",
            "agents.enabled=false",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "history.persistence=\"none\"",
            "-c",
            "memories.generate_memories=false",
            "-c",
            "feedback.enabled=false",
            "--listen",
            "stdio://",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let bundled = "/Applications/ChatGPT.app/Contents/Resources/codex";
        let command = if std::path::Path::new(bundled).is_file() {
            bundled
        } else {
            "codex"
        };
        CodexAppServerProvider::new(
            "codex-real",
            command,
            args,
            BTreeSet::from([
                "gpt-5.6-sol".into(),
                "gpt-5.6-terra".into(),
                "gpt-5.6-luna".into(),
            ]),
        )
    }

    #[test]
    fn blocks_any_agent_protocol_item_outside_inference_text() {
        assert!(enforce_inference_item(&json!({"type":"agentMessage"}), BridgeMode::Text).is_ok());
        let error = enforce_inference_item(&json!({"type":"imageGeneration"}), BridgeMode::Text)
            .unwrap_err();
        assert_eq!(error.kind(), ProviderFailureKind::Protocol);
        assert!(
            enforce_inference_item(
                &json!({"type":"imageGeneration"}),
                BridgeMode::ImageGeneration
            )
            .is_ok()
        );
        let error = enforce_inference_item(
            &json!({"type":"commandExecution"}),
            BridgeMode::ImageGeneration,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProviderFailureKind::Protocol);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_proves_catalog_unary_and_streaming_bridge() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-codex");
        std::fs::write(
            &script,
            r##"#!/bin/sh
read initialize
echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake","platformFamily":"unix","platformOs":"test","codexHome":"/tmp"}}'
read initialized
read models
echo '{"jsonrpc":"2.0","id":2,"result":{"data":[{"id":"gpt-5.6-terra","model":"gpt-5.6-terra","displayName":"Terra","description":"test model","supportedReasoningEfforts":[{"reasoningEffort":"low","description":""}],"defaultReasoningEffort":"low","inputModalities":["text","image"],"isDefault":true,"hidden":false,"upgrade":null}],"nextCursor":null}}'
read thread
echo '{"jsonrpc":"2.0","id":3,"result":{"thread":{"id":"thread-1"}}}'
read turn
echo '{"jsonrpc":"2.0","id":4,"result":{"turn":{"id":"turn-1"}}}'
echo '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"msg-1","delta":"hello "}}'
echo '{"jsonrpc":"2.0","method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":3,"cachedInputTokens":1,"cacheWriteInputTokens":0,"outputTokens":2,"reasoningOutputTokens":0,"totalTokens":5},"total":{"inputTokens":3,"cachedInputTokens":1,"cacheWriteInputTokens":0,"outputTokens":2,"reasoningOutputTokens":0,"totalTokens":5},"modelContextWindow":100}}}'
echo '{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","completedAtMs":1,"item":{"id":"msg-1","type":"agentMessage","text":"hello back","phase":"final_answer"}}}'
echo '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}}'
"##,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let provider = CodexAppServerProvider::new(
            "codex-test",
            script.display().to_string(),
            vec![],
            BTreeSet::from(["gpt-5.6-terra".into()]),
        );
        let response = provider.execute(request(json!("hello"))).await.unwrap();
        assert_eq!(
            response.pointer("/output/0/content/0/text"),
            Some(&json!("hello back"))
        );
        assert_eq!(response.pointer("/usage/total_tokens"), Some(&json!(5)));

        let mut streaming = request(json!("hello"));
        streaming.stream = true;
        let stream = provider.execute_stream(streaming).await.unwrap();
        let output = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .concat();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("response.output_text.delta"));
        assert!(output.contains("hello "));
        assert!(output.contains("response.completed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_proves_bounded_image_generation_bridge() {
        use std::{io::Cursor, os::unix::fs::PermissionsExt};

        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let image = image::DynamicImage::new_rgb8(1, 1);
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let encoded = STANDARD.encode(png.into_inner());
        let script_body = r##"#!/bin/sh
read initialize
echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake","platformFamily":"unix","platformOs":"test","codexHome":"/tmp"}}'
read initialized
read models
echo '{"jsonrpc":"2.0","id":2,"result":{"data":[{"id":"gpt-5.6-terra","model":"gpt-5.6-terra","displayName":"Terra","description":"test model","supportedReasoningEfforts":[{"reasoningEffort":"low","description":""}],"defaultReasoningEffort":"low","inputModalities":["text","image"],"isDefault":true,"hidden":false,"upgrade":null}],"nextCursor":null}}'
read capabilities
echo '{"jsonrpc":"2.0","id":3,"result":{"imageGeneration":true,"namespaceTools":true,"webSearch":true}}'
read thread
echo '{"jsonrpc":"2.0","id":4,"result":{"thread":{"id":"thread-1"}}}'
read turn
echo '{"jsonrpc":"2.0","id":5,"result":{"turn":{"id":"turn-1"}}}'
echo '{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","completedAtMs":1,"item":{"id":"image-1","type":"imageGeneration","status":"completed","result":"__IMAGE__","revisedPrompt":"one pixel","savedPath":"/tmp/untrusted.png"}}}'
echo '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[{"id":"image-1","type":"imageGeneration","status":"completed","result":"__IMAGE__","revisedPrompt":"one pixel","savedPath":"/tmp/untrusted.png"}]}}}'
"##
        .replace("__IMAGE__", &encoded);
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-codex-image");
        std::fs::write(&script, script_body).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let provider = CodexAppServerProvider::new(
            "codex-test",
            script.display().to_string(),
            vec![],
            BTreeSet::from(["gpt-5.6-terra".into()]),
        );
        let mut request = request(json!("Generate one pixel"));
        request.tools = vec![json!({"type": "image_generation"})];
        let response = provider.execute(request).await.unwrap();
        assert_eq!(
            response.pointer("/output/0/type"),
            Some(&json!("image_generation_call"))
        );
        assert_eq!(response.pointer("/output/0/result"), Some(&json!(encoded)));
    }

    #[tokio::test]
    #[ignore = "uses the signed-in local Codex subscription and consumes quota"]
    async fn real_codex_app_server_discovers_group_and_completes_text() {
        let provider = real_codex_provider();
        let catalog = provider.discover_model_catalog().await.unwrap();
        assert!(catalog.models.iter().filter(|model| model.admitted).count() >= 3);
        let mut request = request(json!("Reply with exactly: bridge-ok"));
        request.reasoning = Some(infer_core::ReasoningConfig {
            effort: Some(ReasoningEffort::Low),
            extra: Default::default(),
        });
        let response = provider.execute(request).await.unwrap();
        assert!(
            response
                .pointer("/output/0/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("bridge-ok"))
        );
    }

    #[tokio::test]
    #[ignore = "uses the signed-in local Codex subscription image tool and consumes quota"]
    async fn real_codex_app_server_generates_one_bounded_png() {
        let provider = real_codex_provider();
        let mut request = request(json!(
            "Generate one simple solid blue circle on a white background, with no text."
        ));
        request.model = "gpt-5.6-luna".into();
        request.tools = vec![json!({"type": "image_generation"})];
        let response = provider.execute(request).await.unwrap();
        assert_eq!(
            response.pointer("/output/0/type"),
            Some(&json!("image_generation_call"))
        );
        assert!(
            response
                .pointer("/output/0/result")
                .and_then(Value::as_str)
                .is_some_and(|result| !result.is_empty())
        );
    }
}
