//! Hardened Codex App Server bridge for subscription-backed inference.
//!
//! Codex App Server is an agent protocol, not a Responses endpoint. This
//! adapter deliberately exposes only stateless inference with typed text/image
//! input, append-only text output, and separately admitted hosted Web Search
//! and image generation operations. Each call runs in an empty ephemeral
//! workspace; all other tool-like items fail the Attempt closed.

mod image_generation;
mod input;
mod web_search;

use std::{
    collections::{BTreeSet, VecDeque},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use async_stream::try_stream;
use async_trait::async_trait;
use bytes::Bytes;
use infer_core::{ReasoningEffort, ResponsesRequest, ToolChoice};
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
use web_search::CompletedWebSearch;

const TEXT_BASE_INSTRUCTIONS: &str = "You are serving one stateless inference request. Answer the user directly. Do not use tools, inspect files, run commands, access applications, browse, delegate, or modify external state. Do not mention this bridge or its execution environment.";
const IMAGE_BASE_INSTRUCTIONS: &str = "You are serving one stateless image generation request. Use the built-in image generation tool exactly once to generate one PNG from the user's text prompt. Do not use any other tool, inspect files, run commands, access applications, browse, delegate, or modify external state. Do not mention this bridge or its execution environment.";
const WEB_SEARCH_AUTO_INSTRUCTIONS: &str = "You are serving one stateless inference request. Answer the user directly. You may use only the built-in web search tool when current or web-grounded information is needed. Do not use commands, files, applications, MCP, delegation, or any other tool, and do not modify external state. Do not mention this bridge or its execution environment.";
const WEB_SEARCH_REQUIRED_INSTRUCTIONS: &str = "You are serving one stateless web-grounded inference request. Use the built-in web search tool at least once, then answer the user directly. Do not use commands, files, applications, MCP, delegation, or any other tool, and do not modify external state. Do not mention this bridge or its execution environment.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSearchMode {
    Cached,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeMode {
    Text,
    WebSearch {
        search_mode: WebSearchMode,
        required: bool,
    },
    ImageGeneration,
}

impl BridgeMode {
    fn web_search_enabled(self) -> bool {
        matches!(self, Self::WebSearch { .. })
    }

    fn requires_web_search(self) -> bool {
        matches!(self, Self::WebSearch { required: true, .. })
    }

    fn codex_web_search_setting(self) -> &'static str {
        match self {
            Self::Text | Self::ImageGeneration => "disabled",
            Self::WebSearch {
                search_mode: WebSearchMode::Cached,
                ..
            } => "cached",
            Self::WebSearch {
                search_mode: WebSearchMode::Live,
                ..
            } => "live",
        }
    }
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

    fn execution_args(&self, mode: BridgeMode) -> Vec<String> {
        let mut args = Vec::with_capacity(self.args.len() + 2);
        let mut index = 0;
        while index < self.args.len() {
            if self.args[index] == "-c"
                && self
                    .args
                    .get(index + 1)
                    .is_some_and(|value| value.trim_start().starts_with("web_search="))
            {
                index += 2;
                continue;
            }
            args.push(self.args[index].clone());
            index += 1;
        }
        args.push("-c".into());
        args.push(format!(
            "web_search=\"{}\"",
            mode.codex_web_search_setting()
        ));
        args
    }

    async fn session(&self, mode: BridgeMode) -> Result<CodexSession, ProviderError> {
        CodexSession::spawn(&self.command, &self.execution_args(mode)).await
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
        let mut session = self.session(BridgeMode::Text).await?;
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

        let mut session = self.session(mode).await?;
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
                        BridgeMode::WebSearch { required: false, .. } => WEB_SEARCH_AUTO_INSTRUCTIONS,
                        BridgeMode::WebSearch { required: true, .. } => WEB_SEARCH_REQUIRED_INSTRUCTIONS,
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
        if validate_request(&request)? == BridgeMode::ImageGeneration {
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
    let mode = if request.tools.is_empty() || request.effective_tool_choice() == ToolChoice::None {
        BridgeMode::Text
    } else if request.requests_image_generation() {
        BridgeMode::ImageGeneration
    } else if request.requests_web_search() {
        match request.effective_tool_choice() {
            ToolChoice::None => BridgeMode::Text,
            choice => BridgeMode::WebSearch {
                search_mode: if request.web_search_external_access() == Some(false) {
                    WebSearchMode::Cached
                } else {
                    WebSearchMode::Live
                },
                required: choice == ToolChoice::Required,
            },
        }
    } else {
        return Err(ProviderError::InvalidInput(
            "only the bounded web_search and exact image_generation hosted tools are exposed by the Codex bridge".into(),
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
    searches: Vec<CompletedWebSearch>,
}

#[derive(Debug)]
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
    if message.get("id").is_some() {
        return Err(ProviderError::Classified {
            kind: ProviderFailureKind::Protocol,
            message: "Codex bridge refused an interactive server request".into(),
        });
    }
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
            if mode.requires_web_search() && accumulator.searches.is_empty() {
                return Err(ProviderError::Protocol(
                    "Codex completed a required web search turn without a webSearch item".into(),
                ));
            }
            Ok(TurnProgress::Completed(response_value(
                model,
                mode,
                std::mem::take(&mut accumulator.messages),
                std::mem::take(&mut accumulator.images),
                std::mem::take(&mut accumulator.searches),
                accumulator.usage.take(),
            )?))
        }
        // App Server owns retries inside this turn. Keep the same session alive;
        // the caller's existing deadline still bounds the complete attempt.
        "error" if params.get("willRetry").and_then(Value::as_bool) == Some(true) => {
            Ok(TurnProgress::Continue)
        }
        "error" => Err(classify_turn_failure(params)),
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
        Some("webSearch") if mode.web_search_enabled() => Ok(()),
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
        Some("webSearch") => {
            let search = CompletedWebSearch::from_completed_item(item)?;
            if let Some(existing) = accumulator
                .searches
                .iter()
                .find(|existing| existing.id() == search.id())
            {
                if existing != &search {
                    return Err(ProviderError::Protocol(
                        "Codex repeated a web search result with conflicting content".into(),
                    ));
                }
            } else {
                accumulator.searches.push(search);
            }
        }
        _ => {}
    }
    Ok(())
}

fn classify_turn_failure(turn: &Value) -> ProviderError {
    use ProviderFailureKind::*;
    let info = turn.pointer("/error/codexErrorInfo");
    let name = info
        .and_then(Value::as_str)
        .or_else(|| {
            let object = info?.as_object()?;
            (object.len() == 1).then(|| object.keys().next().unwrap().as_str())
        })
        .unwrap_or("other");
    // Only known, payload-free codes enter persisted errors. Never use the
    // upstream message, details, or an unknown object key as a diagnostic.
    let (kind, code) = match name {
        "unauthorized" => (Authentication, "Codex turn failed: unauthorized"),
        "usageLimitExceeded" => (RateLimited, "Codex turn failed: usageLimitExceeded"),
        "sessionBudgetExceeded" => (RateLimited, "Codex turn failed: sessionBudgetExceeded"),
        "rateLimitExceeded" => (RateLimited, "Codex turn failed: rateLimitExceeded"),
        "serverOverloaded" => (Unavailable, "Codex turn failed: serverOverloaded"),
        "internalServerError" => (Unavailable, "Codex turn failed: internalServerError"),
        "badRequest" => (InvalidRequest, "Codex turn failed: badRequest"),
        "contextWindowExceeded" => (InvalidRequest, "Codex turn failed: contextWindowExceeded"),
        "cyberPolicy" | "misalignmentPolicyViolation" => {
            (InvalidRequest, "Codex turn failed: policy rejection")
        }
        "httpConnectionFailed"
        | "responseStreamConnectionFailed"
        | "responseStreamDisconnected"
        | "responseTooManyFailedAttempts" => {
            let status = info
                .and_then(|i| i.get(name))
                .and_then(|i| i.get("httpStatusCode"))
                .and_then(Value::as_u64);
            let kind = match status {
                Some(401 | 403) => Authentication,
                Some(408 | 504) => Timeout,
                Some(429) => RateLimited,
                Some(400..=499) => InvalidRequest,
                _ => Unavailable,
            };
            let code = match name {
                "httpConnectionFailed" => "Codex turn failed: httpConnectionFailed",
                "responseStreamConnectionFailed" => {
                    "Codex turn failed: responseStreamConnectionFailed"
                }
                "responseStreamDisconnected" => "Codex turn failed: responseStreamDisconnected",
                _ => "Codex turn failed: responseTooManyFailedAttempts",
            };
            (kind, code)
        }
        _ => (Protocol, "Codex turn failed: unrecognized error"),
    };
    ProviderError::CodexTurn { kind, code }
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
    searches: Vec<CompletedWebSearch>,
    usage: Option<Value>,
) -> Result<Value, ProviderError> {
    match mode {
        BridgeMode::Text | BridgeMode::WebSearch { .. } => {
            text_response_value(model, messages, searches, usage)
        }
        BridgeMode::ImageGeneration => image_response_value(model, images, usage),
    }
}

fn text_response_value(
    model: &str,
    messages: Vec<Value>,
    searches: Vec<CompletedWebSearch>,
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
    let mut output = searches
        .iter()
        .map(CompletedWebSearch::response_item)
        .collect::<Vec<_>>();
    output.push(json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }));
    Ok(json!({
        "id": format!("codex_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": output,
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
            tool_choice: None,
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
        let web_mode = BridgeMode::WebSearch {
            search_mode: WebSearchMode::Live,
            required: false,
        };
        assert!(enforce_inference_item(&json!({"type":"webSearch"}), web_mode).is_ok());
        assert!(enforce_inference_item(&json!({"type":"webSearch"}), BridgeMode::Text).is_err());
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

    #[test]
    fn web_search_request_controls_per_process_mode_and_required_postcondition() {
        let provider = CodexAppServerProvider::new(
            "codex-test",
            "codex",
            vec![
                "app-server".into(),
                "-c".into(),
                "web_search=\"disabled\"".into(),
            ],
            BTreeSet::new(),
        );
        let mut search = request(json!("current status"));
        search.tools = vec![json!({"type": "web_search", "external_web_access": false})];
        search.tool_choice = Some(ToolChoice::Required);
        let mode = validate_request(&search).unwrap();
        assert_eq!(
            mode,
            BridgeMode::WebSearch {
                search_mode: WebSearchMode::Cached,
                required: true,
            }
        );
        assert_eq!(
            provider.execution_args(mode).last().map(String::as_str),
            Some("web_search=\"cached\"")
        );
        assert_eq!(
            provider
                .execution_args(mode)
                .iter()
                .filter(|argument| argument.starts_with("web_search="))
                .count(),
            1
        );

        search.tool_choice = Some(ToolChoice::None);
        assert_eq!(validate_request(&search).unwrap(), BridgeMode::Text);
        assert_eq!(
            provider
                .execution_args(BridgeMode::Text)
                .last()
                .map(String::as_str),
            Some("web_search=\"disabled\"")
        );

        let mut declared_function = request(json!("answer without tools"));
        declared_function.tools = vec![json!({"type": "function", "name": "ignored"})];
        declared_function.tool_choice = Some(ToolChoice::None);
        assert_eq!(
            validate_request(&declared_function).unwrap(),
            BridgeMode::Text
        );
    }

    #[test]
    fn required_search_and_interactive_server_requests_fail_closed() {
        let mode = BridgeMode::WebSearch {
            search_mode: WebSearchMode::Live,
            required: true,
        };
        let mut accumulator = TurnAccumulator::default();
        let missing = observe_turn_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "status": "completed", "items": []}
                }
            }),
            "gpt-5.6-terra",
            "thread-1",
            "turn-1",
            mode,
            &mut accumulator,
        )
        .unwrap_err();
        assert_eq!(missing.kind(), ProviderFailureKind::Protocol);

        let interactive = observe_turn_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "item/commandExecution/requestApproval",
                "params": {"threadId": "thread-1", "turnId": "turn-1"}
            }),
            "gpt-5.6-terra",
            "thread-1",
            "turn-1",
            mode,
            &mut accumulator,
        )
        .unwrap_err();
        assert_eq!(interactive.kind(), ProviderFailureKind::Protocol);
    }

    #[test]
    fn terminal_codex_errors_keep_safe_codes_and_retry_semantics() {
        use ProviderFailureKind::*;
        for (info, expected) in [
            (json!("rateLimitExceeded"), RateLimited),
            (json!("unauthorized"), Authentication),
            (json!("contextWindowExceeded"), InvalidRequest),
            (
                json!({"httpConnectionFailed": {"httpStatusCode": 401}}),
                Authentication,
            ),
            (
                json!({"responseStreamConnectionFailed": {"httpStatusCode": 429}}),
                RateLimited,
            ),
            (
                json!({"responseStreamDisconnected": {"httpStatusCode": 504}}),
                Timeout,
            ),
            (
                json!({"responseStreamDisconnected": {"httpStatusCode": 502}}),
                Unavailable,
            ),
            (
                json!({"responseTooManyFailedAttempts": {"httpStatusCode": null}}),
                Unavailable,
            ),
            (
                json!({"responseTooManyFailedAttempts": {"httpStatusCode": 400}}),
                InvalidRequest,
            ),
            (json!({"PRIVATE_UNKNOWN": {}}), Protocol),
        ] {
            let notification = json!({"method":"error", "params":{
                "threadId":"thread-1", "turnId":"turn-1", "willRetry":false,
                "error":{"codexErrorInfo":info,"message":"PRIVATE_PROMPT", "additionalDetails":"PRIVATE_TOKEN"}
            }});
            let error = observe_turn_message(
                &notification,
                "model",
                "thread-1",
                "turn-1",
                BridgeMode::Text,
                &mut TurnAccumulator::default(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(error.public_message().starts_with("Codex turn failed:"));
            assert!(!error.public_message().contains("PRIVATE"));
            assert_eq!(
                classify_turn_failure(&notification["params"]).kind(),
                expected
            );
        }
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
echo '{"jsonrpc":"2.0","method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":true,"error":{"codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502}},"message":"upstream reconnecting"}}}'
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
    async fn fake_app_server_normalizes_required_web_search_without_other_tools() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-codex-web");
        std::fs::write(
            &script,
            r##"#!/bin/sh
read initialize
echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake","platformFamily":"unix","platformOs":"test","codexHome":"/tmp"}}'
read initialized
read models
echo '{"jsonrpc":"2.0","id":2,"result":{"data":[{"id":"gpt-5.6-terra","model":"gpt-5.6-terra","displayName":"Terra","description":"test model","supportedReasoningEfforts":[{"reasoningEffort":"low","description":""}],"defaultReasoningEffort":"low","inputModalities":["text"],"isDefault":true,"hidden":false,"upgrade":null}],"nextCursor":null}}'
read thread
echo '{"jsonrpc":"2.0","id":3,"result":{"thread":{"id":"thread-1"}}}'
read turn
echo '{"jsonrpc":"2.0","id":4,"result":{"turn":{"id":"turn-1"}}}'
echo '{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"id":"ws-1","type":"webSearch","query":"runtime release","action":{"type":"openPage","url":"https://example.com/release"}}}}'
echo '{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"id":"msg-1","type":"agentMessage","text":"grounded answer","phase":"final_answer"}}}'
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
        let mut search = request(json!("current release"));
        search.tools = vec![json!({"type": "web_search"})];
        search.tool_choice = Some(ToolChoice::Required);
        let response = provider.execute(search).await.unwrap();
        assert_eq!(response["output"][0]["type"], "web_search_call");
        assert_eq!(response["output"][0]["action"]["type"], "open_page");
        assert_eq!(
            response["output"][1]["content"][0]["text"],
            "grounded answer"
        );
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
