//! Ollama-native adapter for bounded structured image understanding.
//!
//! The public surface remains typed and model-agnostic. Only this adapter
//! knows Ollama's multimodal chat request, revisioned prompt schema, and timing
//! response. Sensitive image and generated semantic payloads are never placed
//! in provider errors or logs.

use std::{collections::BTreeSet, io::Cursor, net::IpAddr};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use image::ImageFormat;
use infer_core::{
    CLASSIFICATION_REVIEW_PROMPT_REVISION, CLASSIFICATION_REVIEW_SCHEMA_REVISION,
    ClassificationDisposition, ClassificationReviewRequest, ClassificationSuggestion,
    IMAGE_DESCRIPTION_PROMPT_REVISION, IMAGE_DESCRIPTION_SCHEMA_REVISION, ImageDescriptionRequest,
    ImageDescriptionResult, ImageGeometry, MAX_KEYWORD_SUGGESTIONS, MAX_VISION_IMAGE_PIXELS,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS, VisionImage,
};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{ProviderError, ProviderFailureKind};

const MAX_OLLAMA_CHAT_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub struct ImageDescriptionExecutionOutput {
    pub image: ImageGeometry,
    pub result: ImageDescriptionResult,
    pub provenance: OllamaVisionProvenance,
}

#[derive(Debug)]
pub struct ClassificationReviewExecutionOutput {
    pub image: ImageGeometry,
    pub suggestion: ClassificationSuggestion,
    pub provenance: OllamaVisionProvenance,
}

#[derive(Debug)]
pub struct OllamaVisionProvenance {
    pub runtime: String,
    pub schema_revision: String,
    pub prompt_revision: String,
    pub total_duration_ms: Option<u64>,
    pub load_duration_ms: Option<u64>,
    pub prompt_eval_count: Option<u64>,
    pub eval_count: Option<u64>,
}

#[async_trait]
pub trait ImageUnderstandingExecutor: Send + Sync {
    fn id(&self) -> &str;

    async fn describe_image(
        &self,
        physical_model: &str,
        request: ImageDescriptionRequest,
        cancellation: CancellationToken,
    ) -> Result<ImageDescriptionExecutionOutput, ProviderError>;

    async fn review_classification(
        &self,
        physical_model: &str,
        request: ClassificationReviewRequest,
        cancellation: CancellationToken,
    ) -> Result<ClassificationReviewExecutionOutput, ProviderError>;
}

pub type DynImageUnderstandingExecutor = std::sync::Arc<dyn ImageUnderstandingExecutor>;

#[derive(Clone)]
pub struct OllamaVisionExecutor {
    id: String,
    endpoint: Url,
    client: Client,
}

impl OllamaVisionExecutor {
    pub fn new(id: impl Into<String>, native_base_url: &str) -> Result<Self, ProviderError> {
        let mut base = Url::parse(native_base_url).map_err(|_| {
            ProviderError::InvalidInput("Ollama native endpoint must be an absolute URL".into())
        })?;
        let loopback = base
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if base.scheme() != "http"
            || !loopback
            || base.port().is_none()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.username() != ""
            || base.password().is_some()
        {
            return Err(ProviderError::InvalidInput(
                "Ollama native endpoint must be a credential-free numeric loopback HTTP origin with an explicit port"
                    .into(),
            ));
        }
        base.set_path("/api/chat");
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .map_err(ProviderError::Transport)?;
        Ok(Self {
            id: id.into(),
            endpoint: base,
            client,
        })
    }

    async fn execute<T: for<'de> Deserialize<'de>>(
        &self,
        physical_model: &str,
        image: &VisionImage,
        prompt: String,
        cancellation: CancellationToken,
    ) -> Result<(ImageGeometry, T, NativeTiming), ProviderError> {
        let image_geometry = decode_geometry(image)?;
        let body = OllamaChatRequest {
            model: physical_model,
            messages: [OllamaChatMessage {
                role: "user",
                content: &prompt,
                images: [STANDARD.encode(&image.bytes)],
            }],
            stream: false,
            think: false,
            options: OllamaOptions { temperature: 0.0 },
        };
        let send = self.client.post(self.endpoint.clone()).json(&body).send();
        tokio::pin!(send);
        let response = tokio::select! {
            result = &mut send => result.map_err(ProviderError::Transport)?,
            _ = cancellation.cancelled() => {
                return Err(ProviderError::Classified {
                    kind: ProviderFailureKind::Unavailable,
                    message: "Ollama image understanding request was cancelled".into(),
                });
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            // Do not retain or expose the upstream body: it may echo prompt
            // material, category descriptions, or generated semantics.
            return Err(ProviderError::Upstream {
                status,
                body: "<redacted Ollama image-understanding error>".into(),
            });
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => {
                    return Err(ProviderError::Classified {
                        kind: ProviderFailureKind::Unavailable,
                        message: "Ollama image understanding request was cancelled".into(),
                    });
                }
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(ProviderError::Transport)?;
            if body.len().saturating_add(chunk.len()) > MAX_OLLAMA_CHAT_RESPONSE_BYTES {
                return Err(ProviderError::Protocol(
                    "Ollama image-understanding response exceeded its byte limit".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let response = serde_json::from_slice::<OllamaChatResponse>(&body)
            .map_err(|_| ProviderError::Protocol("Ollama returned malformed chat JSON".into()))?;
        let output = serde_json::from_str::<T>(&response.message.content).map_err(|_| {
            ProviderError::Protocol(
                "Ollama image-understanding output did not match the configured schema".into(),
            )
        })?;
        Ok((image_geometry, output, response.into()))
    }
}

#[async_trait]
impl ImageUnderstandingExecutor for OllamaVisionExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn describe_image(
        &self,
        physical_model: &str,
        request: ImageDescriptionRequest,
        cancellation: CancellationToken,
    ) -> Result<ImageDescriptionExecutionOutput, ProviderError> {
        let schema = image_description_schema();
        let prompt = format!(
            "Describe the supplied image in language `{}`. Return only one JSON object with one concise factual description and up to {} short keyword suggestions. Do not include confidence scores, markdown, commentary, or facts that are not visible. Follow this JSON Schema exactly: {schema}",
            request.language, MAX_KEYWORD_SUGGESTIONS,
        );
        let (image, wire, timing) = self
            .execute::<ImageDescriptionWire>(physical_model, &request.image, prompt, cancellation)
            .await?;
        let result = normalize_description(wire)?;
        Ok(ImageDescriptionExecutionOutput {
            image,
            result,
            provenance: timing.provenance(
                IMAGE_DESCRIPTION_SCHEMA_REVISION,
                IMAGE_DESCRIPTION_PROMPT_REVISION,
            ),
        })
    }

    async fn review_classification(
        &self,
        physical_model: &str,
        request: ClassificationReviewRequest,
        cancellation: CancellationToken,
    ) -> Result<ClassificationReviewExecutionOutput, ProviderError> {
        let categories = serde_json::to_string(&request.categories).map_err(|_| {
            ProviderError::Protocol("classification set was not serializable".into())
        })?;
        let schema = classification_review_schema();
        let prompt = format!(
            "Review the supplied image against this closed category set: {categories}. Choose `matched` only when exactly one supplied category clearly applies; copy its id exactly. Choose `none` when no category applies, or `uncertain` when the image is ambiguous. For none/uncertain return an empty category_id. This is an assistant proposal, not user feedback. Do not invent categories, scores, markdown, or commentary. Return only one JSON object and follow this JSON Schema exactly: {schema}"
        );
        let (image, wire, timing) = self
            .execute::<ClassificationReviewWire>(
                physical_model,
                &request.image,
                prompt,
                cancellation,
            )
            .await?;
        let category_id = match wire.disposition {
            ClassificationDisposition::Matched => Some(wire.category_id.trim().to_owned()),
            ClassificationDisposition::None | ClassificationDisposition::Uncertain => {
                if !wire.category_id.trim().is_empty() {
                    return Err(ProviderError::Protocol(
                        "Ollama returned category_id for a none/uncertain classification".into(),
                    ));
                }
                None
            }
        };
        let suggestion = ClassificationSuggestion {
            disposition: wire.disposition,
            category_id,
        };
        suggestion
            .validate_against(&request.categories)
            .map_err(|_| {
                ProviderError::Protocol(
                    "Ollama classification result escaped the supplied closed set".into(),
                )
            })?;
        Ok(ClassificationReviewExecutionOutput {
            image,
            suggestion,
            provenance: timing.provenance(
                CLASSIFICATION_REVIEW_SCHEMA_REVISION,
                CLASSIFICATION_REVIEW_PROMPT_REVISION,
            ),
        })
    }
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: [OllamaChatMessage<'a>; 1],
    stream: bool,
    think: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaChatMessage<'a> {
    role: &'static str,
    content: &'a str,
    images: [String; 1],
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f64,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
    #[serde(default)]
    total_duration: Option<u64>,
    #[serde(default)]
    load_duration: Option<u64>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

struct NativeTiming {
    total_duration_ms: Option<u64>,
    load_duration_ms: Option<u64>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
}

impl From<OllamaChatResponse> for NativeTiming {
    fn from(response: OllamaChatResponse) -> Self {
        Self {
            total_duration_ms: response.total_duration.map(nanoseconds_to_milliseconds),
            load_duration_ms: response.load_duration.map(nanoseconds_to_milliseconds),
            prompt_eval_count: response.prompt_eval_count,
            eval_count: response.eval_count,
        }
    }
}

impl NativeTiming {
    fn provenance(self, schema_revision: &str, prompt_revision: &str) -> OllamaVisionProvenance {
        OllamaVisionProvenance {
            runtime: "ollama_native_chat".into(),
            schema_revision: schema_revision.into(),
            prompt_revision: prompt_revision.into(),
            total_duration_ms: self.total_duration_ms,
            load_duration_ms: self.load_duration_ms,
            prompt_eval_count: self.prompt_eval_count,
            eval_count: self.eval_count,
        }
    }
}

fn nanoseconds_to_milliseconds(value: u64) -> u64 {
    value / 1_000_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageDescriptionWire {
    description: String,
    keyword_suggestions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassificationReviewWire {
    disposition: ClassificationDisposition,
    category_id: String,
}

fn normalize_description(
    wire: ImageDescriptionWire,
) -> Result<ImageDescriptionResult, ProviderError> {
    let mut seen = BTreeSet::new();
    let keyword_suggestions = wire
        .keyword_suggestions
        .into_iter()
        .map(|keyword| keyword.trim().to_owned())
        .filter(|keyword| !keyword.is_empty())
        .filter(|keyword| seen.insert(keyword.to_lowercase()))
        .collect();
    let result = ImageDescriptionResult {
        description: wire.description.trim().to_owned(),
        keyword_suggestions,
    };
    result.validate().map_err(|_| {
        ProviderError::Protocol("Ollama image description exceeded public output bounds".into())
    })?;
    Ok(result)
}

fn image_description_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "description": {"type": "string", "minLength": 1, "maxLength": 1024},
            "keyword_suggestions": {
                "type": "array",
                "maxItems": MAX_KEYWORD_SUGGESTIONS,
                "items": {"type": "string", "minLength": 1, "maxLength": 128}
            }
        },
        "required": ["description", "keyword_suggestions"]
    })
}

fn classification_review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "disposition": {"type": "string", "enum": ["matched", "none", "uncertain"]},
            "category_id": {"type": "string", "maxLength": 128}
        },
        "required": ["disposition", "category_id"]
    })
}

fn decode_geometry(image: &VisionImage) -> Result<ImageGeometry, ProviderError> {
    let format = match image.content_type.as_str() {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        _ => {
            return Err(ProviderError::InvalidInput(
                "unsupported image content type".into(),
            ));
        }
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(&image.bytes), format)
        .into_dimensions()
        .map_err(|_| ProviderError::InvalidInput("image bytes could not be decoded".into()))?;
    if u64::from(width) * u64::from(height) > MAX_VISION_IMAGE_PIXELS {
        return Err(ProviderError::InvalidInput(format!(
            "decoded image exceeds {MAX_VISION_IMAGE_PIXELS} pixels"
        )));
    }
    Ok(ImageGeometry {
        width,
        height,
        orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    fn local_metadata() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ])
    }

    fn png() -> VisionImage {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        VisionImage {
            content_type: "image/png".into(),
            bytes: bytes.into_inner(),
        }
    }

    async fn serve_once(response: Value) -> (String, tokio::task::JoinHandle<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /api/chat HTTP/1.1"));
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            while request.len() - header_end < content_length {
                let mut chunk = [0_u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            }
            let request_json =
                serde_json::from_slice::<Value>(&request[header_end..header_end + content_length])
                    .unwrap();
            let body = response.to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(), body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            request_json
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn native_endpoint_is_strictly_numeric_loopback() {
        assert!(OllamaVisionExecutor::new("local", "http://127.0.0.1:11434").is_ok());
        for endpoint in [
            "https://127.0.0.1:11434",
            "http://localhost:11434",
            "http://example.test:11434",
            "http://127.0.0.1",
            "http://user@127.0.0.1:11434",
        ] {
            assert!(OllamaVisionExecutor::new("local", endpoint).is_err());
        }
    }

    #[test]
    fn classification_schema_has_no_free_form_category() {
        let schema = classification_review_schema();
        assert_eq!(
            schema["properties"]["disposition"]["enum"],
            json!(["matched", "none", "uncertain"])
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn description_normalization_deduplicates_case_insensitively() {
        let result = normalize_description(ImageDescriptionWire {
            description: "  A street at night. ".into(),
            keyword_suggestions: vec!["Night".into(), " night ".into(), "Street".into()],
        })
        .unwrap();
        assert_eq!(result.description, "A street at night.");
        assert_eq!(result.keyword_suggestions, ["Night", "Street"]);
    }

    #[tokio::test]
    async fn description_uses_native_multimodal_schema_without_leaking_it_publicly() {
        let content = json!({
            "description": "一只猫坐在窗边。",
            "keyword_suggestions": ["猫", "窗户"]
        })
        .to_string();
        let (endpoint, captured) = serve_once(json!({
            "message": {"role": "assistant", "content": content},
            "total_duration": 12_000_000,
            "load_duration": 3_000_000,
            "prompt_eval_count": 9,
            "eval_count": 7
        }))
        .await;
        let executor = OllamaVisionExecutor::new("ollama-local", &endpoint).unwrap();
        let output = executor
            .describe_image(
                "qwen3-vl:4b",
                ImageDescriptionRequest {
                    model: "vision.describe_image".into(),
                    image: png(),
                    source_revision: "photo:1".into(),
                    image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                    language: "zh-CN".into(),
                    metadata: local_metadata(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(output.result.description, "一只猫坐在窗边。");
        assert_eq!(output.provenance.total_duration_ms, Some(12));
        assert_eq!(output.image.width, 2);
        let request = captured.await.unwrap();
        assert_eq!(request["model"], "qwen3-vl:4b");
        assert_eq!(request["stream"], false);
        assert_eq!(request["think"], false);
        // Current QwenVL/Ollama thinking builds can place schema-formatted
        // output in the private thinking channel and leave content empty. The
        // adapter therefore grounds exact fields in its revisioned prompt and
        // enforces the same schema after generation instead of forwarding a
        // provider-native `format` field.
        assert!(request.get("format").is_none());
        assert!(
            request["messages"][0]["images"][0]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[tokio::test]
    async fn classification_rejects_a_model_id_outside_the_closed_set() {
        let content = json!({"disposition": "matched", "category_id": "invented"}).to_string();
        let (endpoint, captured) = serve_once(json!({
            "message": {"role": "assistant", "content": content}
        }))
        .await;
        let executor = OllamaVisionExecutor::new("ollama-local", &endpoint).unwrap();
        let error = executor
            .review_classification(
                "qwen3-vl:8b",
                ClassificationReviewRequest {
                    model: "vision.review_classification".into(),
                    image: png(),
                    source_revision: "photo:2".into(),
                    image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                    taxonomy_revision: "taxonomy:1".into(),
                    categories: vec![infer_core::ClassificationCategory {
                        id: "travel".into(),
                        name: "旅行".into(),
                        description: None,
                    }],
                    metadata: local_metadata(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ProviderFailureKind::Protocol);
        let _ = captured.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_waiting_for_a_native_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_started, request_observed) = oneshot::channel();
        let stalled_server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            request_started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let executor =
            OllamaVisionExecutor::new("ollama-local", &format!("http://{address}")).unwrap();
        let cancellation = CancellationToken::new();
        let execution_cancellation = cancellation.clone();
        let execution = tokio::spawn(async move {
            executor
                .describe_image(
                    "qwen3-vl:4b",
                    ImageDescriptionRequest {
                        model: "vision.describe_image".into(),
                        image: png(),
                        source_revision: "photo:cancel".into(),
                        image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                        language: "zh-CN".into(),
                        metadata: local_metadata(),
                    },
                    execution_cancellation,
                )
                .await
        });
        request_observed.await.unwrap();
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .expect("cancelled native request stops promptly")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), ProviderFailureKind::Unavailable);
        stalled_server.abort();
    }
}
