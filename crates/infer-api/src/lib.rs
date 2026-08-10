//! HTTP transport for the Responses data plane and infer control plane.

mod audio_streaming;
pub mod contract;
mod image_understanding;
mod observer;
mod vision;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod observer_tests;
#[cfg(test)]
mod real_vision_tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Multipart, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Response, StatusCode, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::StreamExt;
use infer_control::{
    AudioExecutionOutput, MaintenanceLeaseError, MaintenanceLeaseRequest,
    MaintenanceLeaseRevokeRequest, Runtime, RuntimeError,
};
use infer_core::{
    AlignmentRequest, AudioExecutionRequest, AudioFile, JobPageCursor, JobState,
    MAX_AUDIO_UPLOAD_BYTES, Priority, ResponsesRequest, SpeechFormat, SpeechRequest,
    TranscriptionFormat, TranscriptionRequest, VoiceCloneRequest,
};
use infer_payload::PayloadError;
use infer_provider::ProviderFailureKind;
use infer_resource::{
    EvictionApplyError, EvictionApplyRequest, ReloadBenchmarkRequest, ResourceError,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::contract::{ContractManifest, OPENAPI_JSON, PublicErrorEnvelope};

#[derive(Clone)]
pub struct ApiState {
    pub(crate) runtime: Arc<Runtime>,
}

pub fn router(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/infer/v1/contract", get(get_contract))
        .route("/infer/v1/openapi.json", get(get_openapi))
        .route("/infer/v1/observer/snapshot", get(observer::get_snapshot))
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/{response_id}", get(get_response))
        .route("/v1/responses/{response_id}/cancel", post(cancel_response))
        .route("/v1/audio/transcriptions", post(create_transcription))
        .route(
            "/v1/audio/transcriptions/stream",
            get(audio_streaming::open_transcription_stream),
        )
        .route("/v1/audio/alignments", post(create_alignment))
        .route("/v1/audio/speech", post(create_speech))
        .route("/v1/audio/voice-clones", post(create_voice_clone))
        .route(
            "/infer/v1/vision/face-detections",
            post(vision::create_face_detection),
        )
        .route(
            "/infer/v1/vision/face-embeddings",
            post(vision::create_face_embedding),
        )
        .route(
            "/infer/v1/vision/image-embeddings",
            post(vision::create_image_embedding),
        )
        .route(
            "/infer/v1/vision/text-embeddings",
            post(vision::create_text_embedding),
        )
        .route(
            "/infer/v1/vision/image-descriptions",
            post(image_understanding::create_image_description),
        )
        .route(
            "/infer/v1/vision/classification-reviews",
            post(image_understanding::create_classification_review),
        )
        .route("/infer/v1/jobs", get(get_jobs))
        .route("/infer/v1/jobs/{response_id}", get(get_job))
        .route("/infer/v1/jobs/{response_id}/cancel", post(cancel_job))
        .route("/infer/v1/explain/{response_id}", get(explain_job))
        .route("/infer/v1/metrics", get(get_metrics))
        .route("/infer/v1/budget", get(get_budget))
        .route("/infer/v1/providers", get(get_providers))
        .route(
            "/infer/v1/providers/{provider_id}/models",
            get(get_provider_models),
        )
        .route(
            "/infer/v1/resources",
            get(get_resources).post(refresh_resources),
        )
        .route("/infer/v1/resources/events", get(get_resource_audit_events))
        .route(
            "/infer/v1/resources/eviction/apply",
            post(apply_resource_eviction),
        )
        .route(
            "/infer/v1/resources/eviction/maintenance-lease",
            get(get_eviction_maintenance).post(grant_eviction_maintenance),
        )
        .route(
            "/infer/v1/resources/eviction/maintenance-lease/revoke",
            post(revoke_eviction_maintenance),
        )
        .route(
            "/infer/v1/resources/{provider_id}/deployments/{deployment_id}/load",
            post(load_resource_deployment),
        )
        .route(
            "/infer/v1/resources/{provider_id}/deployments/{deployment_id}/unload",
            post(unload_resource_deployment),
        )
        .route(
            "/infer/v1/resources/{provider_id}/deployments/{deployment_id}/benchmark-reload",
            post(benchmark_resource_reload),
        )
        .route(
            "/infer/v1/providers/{provider_id}/probe",
            post(probe_provider),
        )
        .layer(DefaultBodyLimit::max(MAX_AUDIO_UPLOAD_BYTES + 1024 * 1024))
        .layer(middleware::from_fn(log_request))
        .with_state(ApiState { runtime })
}

async fn log_request(request: Request, next: Next) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let latency_ms = started.elapsed().as_millis() as u64;
    if status.is_server_error() {
        tracing::error!(%method, %path, %status, latency_ms, "HTTP request failed");
    } else if status.is_client_error() {
        tracing::warn!(%method, %path, %status, latency_ms, "HTTP request rejected");
    } else if method != axum::http::Method::GET || !is_polling_endpoint(&path) {
        tracing::info!(%method, %path, %status, latency_ms, "HTTP request completed");
    } else {
        tracing::debug!(%method, %path, %status, latency_ms, "HTTP observation completed");
    }
    response
}

fn is_polling_endpoint(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/infer/v1/contract"
            | "/infer/v1/observer/snapshot"
            | "/infer/v1/metrics"
            | "/infer/v1/jobs"
            | "/infer/v1/providers"
            | "/infer/v1/resources"
            | "/infer/v1/budget"
    )
}

async fn health() -> Json<Value> {
    Json(json!({"status":"ok", "service":"inferd"}))
}

async fn get_contract() -> Json<ContractManifest> {
    Json(ContractManifest::current())
}

async fn get_openapi() -> Result<Response<Body>, ApiError> {
    response(
        StatusCode::OK,
        "application/json",
        OPENAPI_JSON.as_bytes().to_vec(),
    )
}

async fn create_response(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<ResponsesRequest>, JsonRejection>,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let request = strict_json(request)?;
    if request.background {
        let submission = state
            .runtime
            .submit_background(&app_id, request)
            .await
            .map_err(ApiError::from)?;
        response(
            StatusCode::ACCEPTED,
            "application/json",
            serde_json::to_vec(&submission).expect("submission is serializable"),
        )
    } else if request.stream {
        let stream = state
            .runtime
            .execute_stream(&app_id, request)
            .await
            .map_err(ApiError::from)?;
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from_stream(stream.map(Ok::<_, Infallible>)))
            .map_err(|error| ApiError::internal(error.to_string()))
    } else {
        let response = state
            .runtime
            .execute(&app_id, request)
            .await
            .map_err(ApiError::from)?;
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&response).expect("Value is serializable"),
            ))
            .map_err(|error| ApiError::internal(error.to_string()))
    }
}

async fn get_response(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    state
        .runtime
        .background_response(&app_id, &response_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("background response was not found or has expired"))
}

async fn cancel_response(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    state
        .runtime
        .cancel_background(&app_id, &response_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("background response was not found"))
}

async fn create_transcription(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let form = AudioMultipart::parse(multipart, AudioMultipartContract::Transcription).await?;
    let response_format = form.transcription_format()?;
    let request = TranscriptionRequest {
        model: form.required_text("model")?,
        file: form.required_file()?,
        language: form.text.get("language").cloned(),
        prompt: form.text.get("prompt").cloned(),
        response_format,
        temperature: form.optional_number("temperature")?,
        metadata: form.metadata,
    };
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::Transcription(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Json(value) if response_format == TranscriptionFormat::Text => {
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            response(
                StatusCode::OK,
                "text/plain; charset=utf-8",
                text.as_bytes().to_vec(),
            )
        }
        AudioExecutionOutput::Json(value) => json_response(value),
        AudioExecutionOutput::Audio { .. } => {
            Err(ApiError::internal("transcription executor returned audio"))
        }
    }
}

async fn create_alignment(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let form = AudioMultipart::parse(multipart, AudioMultipartContract::Alignment).await?;
    let request = AlignmentRequest {
        model: form.required_text("model")?,
        file: form.required_file()?,
        text: form.required_text("text")?,
        language: form.text.get("language").cloned(),
        metadata: form.metadata,
    };
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::Alignment(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Json(value) => json_response(value),
        AudioExecutionOutput::Audio { .. } => {
            Err(ApiError::internal("alignment executor returned audio"))
        }
    }
}

async fn create_speech(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<SpeechRequest>, JsonRejection>,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let request = strict_json(request)?;
    if request.execution_mode == infer_core::ExecutionMode::ServerStream {
        let result = state
            .runtime
            .execute_speech_stream(&app_id, request)
            .await?;
        return speech_stream_response(result);
    }
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::Speech(request))
        .await?;
    audio_response(result)
}

fn speech_stream_response(
    result: infer_control::SpeechRuntimeStream,
) -> Result<Response<Body>, ApiError> {
    let mut response = Response::new(Body::from_stream(result.stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    insert_response_header(headers, "x-infer-job-id", &result.job_id)?;
    insert_response_header(headers, "x-infer-model", &result.logical_model)?;
    insert_response_header(headers, "x-infer-audio-format", &result.descriptor.format)?;
    insert_response_header(
        headers,
        "x-infer-sample-rate-hz",
        &result.descriptor.sample_rate_hz.to_string(),
    )?;
    insert_response_header(
        headers,
        "x-infer-channels",
        &result.descriptor.channels.to_string(),
    )?;
    Ok(response)
}

fn insert_response_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ApiError> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| ApiError::internal(format!("invalid {name} response metadata")))?;
    headers.insert(name, value);
    Ok(())
}

async fn create_voice_clone(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let form = AudioMultipart::parse(multipart, AudioMultipartContract::VoiceClone).await?;
    let request = VoiceCloneRequest {
        model: form.required_text("model")?,
        input: form.required_text("input")?,
        reference_audio: form.required_file()?,
        reference_text: form.required_text("reference_text")?,
        language: form.text.get("language").cloned(),
        response_format: form.speech_format()?,
        metadata: form.metadata,
    };
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::VoiceClone(request))
        .await?;
    audio_response(result)
}

fn audio_response(result: infer_control::AudioRuntimeResult) -> Result<Response<Body>, ApiError> {
    match result.output {
        AudioExecutionOutput::Audio {
            bytes,
            content_type,
        } => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header("x-infer-job-id", result.job_id)
            .header("x-infer-model", result.logical_model)
            .body(Body::from(bytes))
            .map_err(|error| ApiError::internal(error.to_string())),
        AudioExecutionOutput::Json(_) => Err(ApiError::internal("speech executor returned JSON")),
    }
}

fn json_response(value: Value) -> Result<Response<Body>, ApiError> {
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&value).expect("Value is serializable"),
    )
}

fn response(
    status: StatusCode,
    content_type: &'static str,
    body: Vec<u8>,
) -> Result<Response<Body>, ApiError> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .map_err(|error| ApiError::internal(error.to_string()))
}

#[derive(Default)]
struct AudioMultipart {
    text: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    file: Option<AudioFile>,
}

#[derive(Clone, Copy)]
enum AudioMultipartContract {
    Transcription,
    Alignment,
    VoiceClone,
}

impl AudioMultipartContract {
    fn file_field(self) -> &'static str {
        match self {
            Self::Transcription | Self::Alignment => "file",
            Self::VoiceClone => "reference_audio",
        }
    }

    fn allows_text(self, name: &str) -> bool {
        match self {
            Self::Transcription => matches!(
                name,
                "model" | "language" | "prompt" | "response_format" | "temperature"
            ),
            Self::Alignment => matches!(name, "model" | "text" | "language"),
            Self::VoiceClone => matches!(
                name,
                "model" | "input" | "reference_text" | "language" | "response_format"
            ),
        }
    }
}

impl AudioMultipart {
    async fn parse(
        mut multipart: Multipart,
        contract: AudioMultipartContract,
    ) -> Result<Self, ApiError> {
        let mut form = Self::default();
        let mut seen = BTreeSet::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|error| ApiError::bad_request(error.to_string()))?
        {
            let name = field
                .name()
                .ok_or_else(|| ApiError::bad_request("multipart field needs a name"))?
                .to_owned();
            if !seen.insert(name.clone()) {
                return Err(ApiError::bad_request(format!(
                    "duplicate multipart field `{name}`"
                )));
            }
            if name == contract.file_field() {
                if form.file.is_some() {
                    return Err(ApiError::bad_request("only one audio file is allowed"));
                }
                let filename = field.file_name().unwrap_or("audio.wav").to_owned();
                let content_type = field.content_type().map(str::to_owned);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::bad_request(error.to_string()))?;
                form.file = Some(AudioFile {
                    filename,
                    content_type,
                    bytes: bytes.to_vec(),
                });
            } else if name == "file" || name == "reference_audio" {
                return Err(ApiError::bad_request(format!(
                    "unexpected multipart file field `{name}`; expected `{}`",
                    contract.file_field()
                )));
            } else if name == "metadata"
                || name.starts_with("infer.")
                || contract.allows_text(&name)
            {
                let value = field
                    .text()
                    .await
                    .map_err(|error| ApiError::bad_request(error.to_string()))?;
                if name == "metadata" {
                    let metadata: BTreeMap<String, String> = serde_json::from_str(&value)
                        .map_err(|error| ApiError::bad_request(error.to_string()))?;
                    for (key, value) in metadata {
                        if form.metadata.insert(key.clone(), value).is_some() {
                            return Err(ApiError::bad_request(format!(
                                "duplicate metadata key `{key}`"
                            )));
                        }
                    }
                } else if name.starts_with("infer.") {
                    if form.metadata.insert(name.clone(), value).is_some() {
                        return Err(ApiError::bad_request(format!(
                            "duplicate metadata key `{name}`"
                        )));
                    }
                } else {
                    form.text.insert(name, value);
                }
            } else {
                return Err(ApiError::bad_request(format!(
                    "unknown multipart field `{name}`"
                )));
            }
        }
        Ok(form)
    }

    fn required_text(&self, name: &'static str) -> Result<String, ApiError> {
        self.text
            .get(name)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| ApiError::bad_request(format!("missing multipart field `{name}`")))
    }

    fn required_file(&self) -> Result<AudioFile, ApiError> {
        self.file
            .clone()
            .ok_or_else(|| ApiError::bad_request("missing audio file"))
    }

    fn optional_number(&self, name: &'static str) -> Result<Option<f64>, ApiError> {
        self.text
            .get(name)
            .map(|value| {
                value.parse().map_err(|_| {
                    ApiError::bad_request(format!("multipart field `{name}` must be a number"))
                })
            })
            .transpose()
    }

    fn transcription_format(&self) -> Result<TranscriptionFormat, ApiError> {
        parse_format(self.text.get("response_format"), TranscriptionFormat::Json)
    }

    fn speech_format(&self) -> Result<SpeechFormat, ApiError> {
        parse_format(self.text.get("response_format"), SpeechFormat::Wav)
    }
}

fn strict_json<T>(request: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    request.map(|Json(value)| value).map_err(|rejection| {
        ApiError::bad_request(format!(
            "invalid JSON request body: {}",
            rejection.body_text()
        ))
    })
}

fn strict_query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(value)| value).map_err(|rejection| {
        ApiError::bad_request(format!("invalid query string: {}", rejection.body_text()))
    })
}

fn parse_format<T: FromStr>(value: Option<&String>, default: T) -> Result<T, ApiError> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| ApiError::bad_request("unsupported response_format"))
        })
        .unwrap_or(Ok(default))
}

async fn get_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let snapshot = state
        .runtime
        .snapshot_for_app(&app_id, &response_id)
        .await?
        .ok_or_else(|| ApiError::not_found("response was not found"))?;
    Ok(Json(
        serde_json::to_value(snapshot).expect("snapshot is serializable"),
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    priority: Option<String>,
    state: Option<String>,
}

async fn get_jobs(
    State(state): State<ApiState>,
    headers: HeaderMap,
    query: Result<Query<JobListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let query = strict_query(query)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=1_000).contains(&limit) {
        return Err(ApiError::bad_request("limit must be between 1 and 1000"));
    }
    let cursor = query
        .cursor
        .map(|cursor| {
            cursor
                .parse::<JobPageCursor>()
                .map_err(ApiError::bad_request)
        })
        .transpose()?;
    let priority = parse_job_filter::<Priority>(query.priority, "priority")?;
    let job_state = parse_job_filter::<JobState>(query.state, "state")?;
    let page = state
        .runtime
        .job_page(&app_id, priority, job_state, cursor.as_ref(), limit)?;
    Ok(Json(
        serde_json::to_value(page).expect("Job list page is serializable"),
    ))
}

fn parse_job_filter<T: FromStr>(
    value: Option<String>,
    name: &'static str,
) -> Result<Option<T>, ApiError> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| ApiError::bad_request(format!("unsupported Job {name}")))
        })
        .transpose()
}

async fn cancel_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    if state.runtime.cancel_for_app(&app_id, &response_id).await {
        Ok(Json(json!({"id":response_id, "cancelled":true})))
    } else {
        Err(ApiError::not_found("response is terminal or was not found"))
    }
}

async fn explain_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let job = state
        .runtime
        .snapshot_for_app(&app_id, &response_id)
        .await?
        .ok_or_else(|| ApiError::not_found("response was not found"))?;
    let audit_events = state.runtime.audit_events(&response_id)?;
    Ok(Json(json!({
        "response_id": job.id,
        "intent": job.intent,
        "policy": job.policy,
        "selected_provider": job.provider,
        "selected_deployment": job.deployment,
        "model_profile": job.model_profile,
        "model_build": job.model_build,
        "physical_model": job.physical_model,
        "placement": job.placement,
        "quality_grade": job.quality_grade,
        "rating_status": job.rating_status,
        "resource_class": job.resource_class,
        "constraints": job.constraints,
        "routing": job.routing,
        "audit_events": audit_events,
        "attempts": job.attempts,
        "state": job.state,
        "error": job.error,
    })))
}

async fn get_budget(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers)?;
    let budget = state.runtime.budget_snapshot()?;
    Ok(Json(
        serde_json::to_value(budget).expect("budget snapshot is serializable"),
    ))
}

async fn get_providers(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers)?;
    Ok(Json(
        json!({"providers": state.runtime.provider_snapshots()}),
    ))
}

async fn get_provider_models(
    State(state): State<ApiState>,
    Path(provider_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(state.runtime.provider_model_catalog(&provider_id).await?)
            .expect("provider model catalog is serializable"),
    ))
}

async fn get_resources(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers)?;
    Ok(Json(
        serde_json::to_value(state.runtime.resource_snapshot().await)
            .expect("resource snapshot is serializable"),
    ))
}

async fn refresh_resources(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers)?;
    Ok(Json(
        serde_json::to_value(state.runtime.refresh_resources().await)
            .expect("resource snapshot is serializable"),
    ))
}

#[derive(Debug, Deserialize)]
struct ResourceEventQuery {
    #[serde(default = "default_resource_event_limit")]
    limit: usize,
}

fn default_resource_event_limit() -> usize {
    100
}

async fn get_resource_audit_events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<ResourceEventQuery>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let events = state.runtime.resource_audit_events(query.limit)?;
    Ok(Json(json!({"events": events})))
}

async fn apply_resource_eviction(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<EvictionApplyRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let result = state
        .runtime
        .apply_resource_eviction(&actor, request)
        .await?;
    Ok(Json(
        serde_json::to_value(result).expect("eviction action result is serializable"),
    ))
}

async fn get_eviction_maintenance(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(state.runtime.eviction_monitor_snapshot().await)
            .expect("eviction monitor snapshot is serializable"),
    ))
}

async fn grant_eviction_maintenance(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<MaintenanceLeaseRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(
            state
                .runtime
                .grant_eviction_maintenance(&actor, request)
                .await?,
        )
        .expect("maintenance lease is serializable"),
    ))
}

async fn revoke_eviction_maintenance(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<MaintenanceLeaseRevokeRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(
            state
                .runtime
                .revoke_eviction_maintenance(&actor, request)
                .await?,
        )
        .expect("maintenance lease is serializable"),
    ))
}

async fn load_resource_deployment(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((provider_id, deployment_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let result = state
        .runtime
        .load_resource_deployment(&provider_id, &deployment_id)
        .await?;
    Ok(Json(
        serde_json::to_value(result).expect("lifecycle action result is serializable"),
    ))
}

async fn unload_resource_deployment(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((provider_id, deployment_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let result = state
        .runtime
        .unload_resource_deployment(&provider_id, &deployment_id)
        .await?;
    Ok(Json(
        serde_json::to_value(result).expect("lifecycle action result is serializable"),
    ))
}

async fn benchmark_resource_reload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((provider_id, deployment_id)): Path<(String, String)>,
    Json(request): Json<ReloadBenchmarkRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let result = state
        .runtime
        .benchmark_resource_reload(&provider_id, &deployment_id, request)
        .await?;
    Ok(Json(
        serde_json::to_value(result).expect("reload benchmark result is serializable"),
    ))
}

async fn get_metrics(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers)?;
    Ok(Json(
        serde_json::to_value(state.runtime.metrics().await)
            .expect("metrics snapshot is serializable"),
    ))
}

async fn probe_provider(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let report = state.runtime.probe_provider(&provider_id).await?;
    Ok(Json(
        serde_json::to_value(report).expect("probe report is serializable"),
    ))
}

pub(crate) fn authenticate(state: &ApiState, headers: &HeaderMap) -> Result<String, ApiError> {
    let app_id = authenticate_any(state, headers)?;
    state.runtime.authorize_non_observer(&app_id)?;
    Ok(app_id)
}

pub(crate) fn authenticate_observer(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<String, ApiError> {
    let app_id = authenticate_any(state, headers)?;
    state.runtime.authorize_observer_summary(&app_id)?;
    Ok(app_id)
}

fn authenticate_any(state: &ApiState, headers: &HeaderMap) -> Result<String, ApiError> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?;
    let token = authorization
        .strip_prefix("Bearer ")
        .ok_or_else(ApiError::unauthorized)?;
    state.runtime.authenticate(token).map_err(ApiError::from)
}

pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_api_key",
            message: "missing or invalid bearer credential".into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request_error",
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }
}

impl From<RuntimeError> for ApiError {
    fn from(error: RuntimeError) -> Self {
        if matches!(error, RuntimeError::Credential(_)) {
            return Self::internal("credential subsystem unavailable");
        }
        let (status, code) = match &error {
            RuntimeError::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid_api_key"),
            RuntimeError::ObserverCredentialRestricted => {
                (StatusCode::FORBIDDEN, "observer_credential_restricted")
            }
            RuntimeError::ObserverAccessRequired(_) => {
                (StatusCode::FORBIDDEN, "observer_access_required")
            }
            RuntimeError::UnknownIntent(_)
            | RuntimeError::DataPlaneMismatch { .. }
            | RuntimeError::Contract(_)
            | RuntimeError::ProviderProbeUnsupported(_) => {
                (StatusCode::BAD_REQUEST, "invalid_request_error")
            }
            RuntimeError::UnknownApp(_)
            | RuntimeError::PolicyNotAllowed(_)
            | RuntimeError::OverrideNotAllowed { .. } => {
                (StatusCode::FORBIDDEN, "policy_violation")
            }
            RuntimeError::IntentNotAllowed { .. } => (StatusCode::FORBIDDEN, "intent_forbidden"),
            RuntimeError::ResourceAdminRequired(_) => {
                (StatusCode::FORBIDDEN, "resource_admin_required")
            }
            RuntimeError::NoCandidate => (StatusCode::CONFLICT, "no_candidate"),
            RuntimeError::Cancelled => (StatusCode::CONFLICT, "cancelled"),
            RuntimeError::QueueFull => (StatusCode::TOO_MANY_REQUESTS, "queue_full"),
            RuntimeError::AppQueueFull => (StatusCode::TOO_MANY_REQUESTS, "app_queue_full"),
            RuntimeError::QuotaExceeded { .. } => (StatusCode::TOO_MANY_REQUESTS, "quota_exceeded"),
            RuntimeError::DeadlineExpired => (StatusCode::GATEWAY_TIMEOUT, "deadline_exceeded"),
            RuntimeError::ProviderUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "provider_unavailable")
            }
            RuntimeError::ProviderProbeModelMissing(_) => {
                (StatusCode::CONFLICT, "provider_probe_model_missing")
            }
            RuntimeError::BackgroundDisabled | RuntimeError::BackgroundLocalOnly => {
                (StatusCode::CONFLICT, "background_unavailable")
            }
            RuntimeError::Payload(PayloadError::TooLarge { .. }) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "background_payload_too_large",
            ),
            RuntimeError::BackgroundDisabledWithPendingJobs
            | RuntimeError::BackgroundKeyUnavailable(_)
            | RuntimeError::BackgroundPayloadFormat
            | RuntimeError::BackgroundAttemptBudgetExhausted
            | RuntimeError::BackgroundTaskFailed
            | RuntimeError::Payload(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "background_persistence_error",
            ),
            RuntimeError::Resource(
                ResourceError::UnknownProvider(_) | ResourceError::UnknownDeployment { .. },
            ) => (StatusCode::NOT_FOUND, "not_found"),
            RuntimeError::Resource(ResourceError::UnknownEvictionDeployment(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "invalid_eviction_plan")
            }
            RuntimeError::Resource(ResourceError::Lifecycle(_)) => {
                (StatusCode::CONFLICT, "resource_transition_conflict")
            }
            RuntimeError::Resource(ResourceError::ReloadBenchmarkRequest(_)) => {
                (StatusCode::BAD_REQUEST, "invalid_reload_benchmark")
            }
            RuntimeError::Resource(ResourceError::EvictionApply(
                EvictionApplyError::MissingExpectedDeployment
                | EvictionApplyError::MissingReason
                | EvictionApplyError::ReasonTooLong,
            )) => (StatusCode::BAD_REQUEST, "invalid_eviction_approval"),
            RuntimeError::Resource(ResourceError::EvictionApply(
                EvictionApplyError::NoActionableTarget | EvictionApplyError::TargetChanged { .. },
            )) => (StatusCode::CONFLICT, "eviction_recommendation_changed"),
            RuntimeError::Resource(ResourceError::NativeControl(_)) => {
                (StatusCode::BAD_GATEWAY, "native_control_unavailable")
            }
            RuntimeError::MaintenanceLease(
                MaintenanceLeaseError::DurationTooShort
                | MaintenanceLeaseError::DurationTooLong { .. }
                | MaintenanceLeaseError::MissingReason
                | MaintenanceLeaseError::ReasonTooLong,
            ) => (StatusCode::BAD_REQUEST, "invalid_maintenance_lease"),
            RuntimeError::MaintenanceLease(
                MaintenanceLeaseError::MonitorDisabled
                | MaintenanceLeaseError::ActiveLease
                | MaintenanceLeaseError::LeaseMismatch,
            ) => (StatusCode::CONFLICT, "maintenance_lease_conflict"),
            RuntimeError::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "persistence_error"),
            RuntimeError::Artifact(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "artifact_store_error")
            }
            RuntimeError::Credential(_) => unreachable!("handled above"),
            RuntimeError::Provider(provider) => match provider.kind() {
                ProviderFailureKind::Authentication => {
                    (StatusCode::BAD_GATEWAY, "upstream_authentication")
                }
                ProviderFailureKind::RateLimited => {
                    (StatusCode::TOO_MANY_REQUESTS, "upstream_rate_limited")
                }
                ProviderFailureKind::Timeout => (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
                ProviderFailureKind::Unavailable => {
                    (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable")
                }
                ProviderFailureKind::InvalidRequest => {
                    (StatusCode::BAD_REQUEST, "upstream_invalid_request")
                }
                ProviderFailureKind::Protocol => (StatusCode::BAD_GATEWAY, "upstream_protocol"),
            },
        };
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(PublicErrorEnvelope::new(self.code, self.message)),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use infer_provider::ProviderError;
    use infer_resource::{
        LifecycleAction, LifecycleOperationError, ReloadBenchmarkRequestError, ResourceError,
    };

    use super::*;

    #[test]
    fn upstream_rate_limit_keeps_its_public_error_semantics() {
        let error: ApiError = RuntimeError::Provider(ProviderError::Upstream {
            status: 429,
            body: String::new(),
        })
        .into();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code, "upstream_rate_limited");
    }

    #[test]
    fn lifecycle_conflict_keeps_native_control_semantics() {
        let error: ApiError = RuntimeError::Resource(ResourceError::Lifecycle(
            LifecycleOperationError::InvalidState {
                deployment: "small".into(),
                action: LifecycleAction::Unload,
                state: infer_resource::ModelLifecycleState::Absent,
            },
        ))
        .into();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "resource_transition_conflict");
    }

    #[test]
    fn invalid_reload_benchmark_is_a_client_error() {
        let error: ApiError = RuntimeError::Resource(ResourceError::ReloadBenchmarkRequest(
            ReloadBenchmarkRequestError::InvalidSamples,
        ))
        .into();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "invalid_reload_benchmark");
    }

    #[test]
    fn stale_eviction_approval_is_a_conflict() {
        let error: ApiError = RuntimeError::Resource(ResourceError::EvictionApply(
            EvictionApplyError::TargetChanged {
                expected: "old".into(),
                actual: "new".into(),
            },
        ))
        .into();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "eviction_recommendation_changed");
    }

    #[test]
    fn resource_admin_requirement_has_a_distinct_forbidden_code() {
        let error: ApiError = RuntimeError::ResourceAdminRequired("reader".into()).into();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "resource_admin_required");
    }

    #[test]
    fn intent_acl_has_a_distinct_forbidden_code() {
        let error: ApiError = RuntimeError::IntentNotAllowed {
            app_id: "reader".into(),
            intent: "reasoning.deep".into(),
        }
        .into();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "intent_forbidden");
    }

    #[test]
    fn disabled_monitor_rejects_maintenance_lease_as_a_conflict() {
        let error: ApiError =
            RuntimeError::MaintenanceLease(MaintenanceLeaseError::MonitorDisabled).into();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "maintenance_lease_conflict");
    }

    #[test]
    fn high_frequency_console_reads_are_debug_only() {
        assert!(is_polling_endpoint("/infer/v1/metrics"));
        assert!(is_polling_endpoint("/infer/v1/jobs"));
        assert!(!is_polling_endpoint("/v1/responses"));
        assert!(!is_polling_endpoint("/infer/v1/jobs/resp_123"));
    }
}
