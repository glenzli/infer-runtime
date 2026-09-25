//! HTTP transport for the Responses data plane and infer control plane.

mod agent_task;
mod audio_streaming;
pub mod contract;
mod image_understanding;
mod observer;
mod ocr;
pub mod raw_foundation;
mod retrieval;
mod vision;

#[cfg(test)]
mod agent_task_tests;
#[cfg(test)]
mod audio_contract_tests;
#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod observer_tests;
#[cfg(test)]
mod ocr_tests;
#[cfg(test)]
mod real_audio_event_tests;
#[cfg(test)]
mod real_vision_tests;
#[cfg(test)]
mod retrieval_tests;

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
    MaintenanceLeaseRevokeRequest, Runtime, RuntimeError, TelemetryRange,
};
use infer_core::{
    AlignmentRequest, AudioEmbeddingRequest, AudioExecutionRequest, AudioFile,
    AudioTextEmbeddingRequest, EventDetectionRequest, JobPageCursor, JobSnapshot, JobState,
    MAX_AUDIO_UPLOAD_BYTES, Priority, ResponsesRequest, SoundGenerationRequest, SpeechFormat,
    SpeechRequest, TranscriptionFormat, TranscriptionRequest, VoiceCloneRequest,
};
use infer_payload::PayloadError;
use infer_provider::ProviderFailureKind;
use infer_resource::{
    EvictionApplyError, EvictionApplyRequest, ReloadBenchmarkRequest, ResourceError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::contract::{
    CAPABILITY_CONTRACT_HEADER, CONSUMER_CORE_HEADER, CORE_CONTRACT, CapabilityCatalog,
    ContractManifest, OPENAPI_JSON, PublicErrorEnvelope, capability_schema_document,
    required_capability_id,
};

#[derive(Clone)]
pub struct ApiState {
    pub(crate) runtime: Arc<Runtime>,
}

pub fn router(runtime: Arc<Runtime>) -> Router {
    with_consumer_contract_admission(base_router(runtime))
}

fn base_router(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/infer/v1/contract", get(get_contract))
        .route("/infer/v1/capabilities", get(get_capabilities))
        .route("/infer/v1/openapi.json", get(get_openapi))
        .route(
            "/infer/v1/capability-schemas/{capability_id}/{version}/openapi.json",
            get(get_capability_schema),
        )
        .route("/infer/v1/observer/snapshot", get(observer::get_snapshot))
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/{response_id}", get(get_response))
        .route("/v1/responses/{response_id}/cancel", post(cancel_response))
        .route("/v1/audio/transcriptions", post(create_transcription))
        .route("/v1/audio/event-detections", post(create_event_detection))
        .route("/v1/audio/embeddings", post(create_audio_embedding))
        .route(
            "/v1/audio/text-embeddings",
            post(create_audio_text_embedding),
        )
        .route(
            "/v1/audio/transcriptions/stream",
            get(audio_streaming::open_transcription_stream),
        )
        .route("/v1/audio/alignments", post(create_alignment))
        .route("/v1/audio/speech", post(create_speech))
        .route("/v1/audio/sound-generations", post(create_sound_generation))
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
            "/infer/v1/vision/subject-segmentations",
            post(vision::create_subject_segmentation),
        )
        .route(
            "/infer/v1/vision/subject-segmentations/soft-mask",
            post(vision::create_subject_segmentation_soft_mask),
        )
        .route(
            "/infer/v1/vision/semantic-groundings",
            post(vision::create_semantic_grounding),
        )
        .route(
            "/infer/v1/vision/image-completions",
            post(vision::create_image_completion),
        )
        .route(
            "/infer/v1/vision/face-parsings",
            post(vision::create_face_parsing),
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
        .route(
            "/infer/v1/text/query-embeddings",
            post(retrieval::create_query_embeddings),
        )
        .route(
            "/infer/v1/text/document-embeddings",
            post(retrieval::create_document_embeddings),
        )
        .route("/infer/v1/text/rerank", post(retrieval::create_rerank))
        .route("/infer/v1/documents/ocr", post(ocr::create_document_ocr))
        .route("/infer/v1/agent/tasks", post(agent_task::create_agent_task))
        .route("/infer/v1/jobs", get(get_jobs))
        .route("/infer/v1/operator/jobs", get(get_operator_jobs))
        .route(
            "/infer/v1/operator/jobs/{response_id}/explain",
            get(explain_operator_job),
        )
        .route(
            "/infer/v1/operator/jobs/{response_id}/cancel",
            post(cancel_operator_job),
        )
        .route("/infer/v1/jobs/{response_id}", get(get_job))
        .route("/infer/v1/jobs/{response_id}/cancel", post(cancel_job))
        .route("/infer/v1/explain/{response_id}", get(explain_job))
        .route("/infer/v1/metrics", get(get_metrics))
        .route("/infer/v1/telemetry", get(get_telemetry))
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

/// Production composition with the typed RAW routes. Construction remains
/// fail-closed in `inferd`; callers cannot reach these routes unless the exact
/// configured graph/runtime assembly has passed startup verification.
pub fn router_with_raw(
    runtime: Arc<Runtime>,
    raw: Arc<infer_control::RawFoundationControl>,
) -> Router {
    with_consumer_contract_admission(
        base_router(Arc::clone(&runtime)).merge(raw_foundation::router(runtime, raw)),
    )
}

fn with_consumer_contract_admission(router: Router) -> Router {
    router.layer(middleware::from_fn(enforce_consumer_contract))
}

async fn enforce_consumer_contract(request: Request, next: Next) -> axum::response::Response {
    if !requires_consumer_contract(request.uri().path()) {
        return next.run(request).await;
    }
    match require_current_consumer_contract(request.headers()) {
        Ok(()) => match require_current_capability_contract(
            request.headers(),
            required_capability_id(request.uri().path()),
        ) {
            Ok(Some(contract)) => {
                infer_control::with_admitted_capability_contract(contract, next.run(request)).await
            }
            Ok(None) => next.run(request).await,
            Err(error) => error.into_response(),
        },
        Err(error) => error.into_response(),
    }
}

fn requires_consumer_contract(path: &str) -> bool {
    path == "/infer/v1/contract"
        || path == "/infer/v1/capabilities"
        || path == "/v1/responses"
        || path.starts_with("/v1/responses/")
        || path.starts_with("/v1/audio/")
        || path == "/infer/v1/jobs"
        || path.starts_with("/infer/v1/jobs/")
        || path.starts_with("/infer/v1/explain/")
        || path.starts_with("/infer/v1/vision/")
        || path.starts_with("/infer/v1/text/")
        || path.starts_with("/infer/v1/documents/")
        || path.starts_with("/infer/v1/agent/")
        || path.starts_with("/infer/v1/raw/")
}

fn require_current_consumer_contract(headers: &HeaderMap) -> Result<(), ApiError> {
    let values = headers.get_all(CONSUMER_CORE_HEADER);
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return Err(ApiError::upgrade_required(format!(
            "{CONSUMER_CORE_HEADER} must be exactly {CORE_CONTRACT}; upgrade this Consumer before retrying"
        )));
    };
    if values.next().is_some() {
        return Err(ApiError::upgrade_required(
            "consumer contract header must appear exactly once",
        ));
    }
    if value.as_bytes() != CORE_CONTRACT.as_bytes() {
        return Err(ApiError::upgrade_required(format!(
            "only {CORE_CONTRACT} is supported; upgrade this Consumer before retrying"
        )));
    }
    Ok(())
}

fn require_current_capability_contract(
    headers: &HeaderMap,
    capability_id: Option<&'static str>,
) -> Result<Option<&'static str>, ApiError> {
    let Some(capability_id) = capability_id else {
        return Ok(None);
    };
    let values = headers.get_all(CAPABILITY_CONTRACT_HEADER);
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return Err(ApiError::capability_upgrade_required(format!(
            "{CAPABILITY_CONTRACT_HEADER} must select a supported {capability_id} schema"
        )));
    };
    let value = value.to_str().unwrap_or_default();
    if values.next().is_some() {
        return Err(ApiError::capability_upgrade_required(format!(
            "no supported {capability_id} schema was selected for this route"
        )));
    }
    contract::supported_capability_contract(capability_id, value)
        .ok_or_else(|| {
            ApiError::capability_upgrade_required(format!(
                "no supported {capability_id} schema was selected for this route"
            ))
        })
        .map(Some)
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
            | "/infer/v1/capabilities"
            | "/infer/v1/observer/snapshot"
            | "/infer/v1/metrics"
            | "/infer/v1/telemetry"
            | "/infer/v1/jobs"
            | "/infer/v1/operator/jobs"
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

async fn get_capabilities() -> Json<CapabilityCatalog> {
    Json(CapabilityCatalog::current())
}

async fn get_openapi() -> Result<Response<Body>, ApiError> {
    response(
        StatusCode::OK,
        "application/json",
        OPENAPI_JSON.as_bytes().to_vec(),
    )
}

async fn get_capability_schema(
    Path((capability_id, version)): Path<(String, String)>,
) -> Result<Response<Body>, ApiError> {
    let document = capability_schema_document(&capability_id, &version)
        .ok_or_else(|| ApiError::not_found("capability schema not found"))?;
    response(
        StatusCode::OK,
        "application/json",
        document.as_bytes().to_vec(),
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

async fn create_event_detection(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let form = AudioMultipart::parse(multipart, AudioMultipartContract::EventDetection).await?;
    let request = EventDetectionRequest {
        model: form.required_text("model")?,
        file: form.required_file()?,
        metadata: fail_closed_event_metadata(form.metadata)?,
    };
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::EventDetection(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Json(value) => json_response(value),
        AudioExecutionOutput::Audio { .. } => {
            Err(ApiError::internal("sound-event executor returned audio"))
        }
    }
}

async fn create_audio_embedding(
    State(state): State<ApiState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let form = AudioMultipart::parse(multipart, AudioMultipartContract::Embedding).await?;
    let request = AudioEmbeddingRequest {
        model: form.required_text("model")?,
        file: form.required_file()?,
        source_revision: form.required_text("source_revision")?,
        metadata: fail_closed_local_audio_metadata(form.metadata, "audio-text embedding")?,
    };
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::Embedding(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Json(value) => json_response(value),
        AudioExecutionOutput::Audio { .. } => Err(ApiError::internal(
            "audio embedding executor returned audio",
        )),
    }
}

async fn create_audio_text_embedding(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<AudioTextEmbeddingRequest>, JsonRejection>,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    request.metadata = fail_closed_local_audio_metadata(request.metadata, "audio-text embedding")?;
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::TextEmbedding(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Json(value) => json_response(value),
        AudioExecutionOutput::Audio { .. } => Err(ApiError::internal(
            "audio text embedding executor returned audio",
        )),
    }
}

fn fail_closed_local_audio_metadata(
    mut metadata: BTreeMap<String, String>,
    purpose: &str,
) -> Result<BTreeMap<String, String>, ApiError> {
    for (key, required) in [
        ("infer.placement", "local_only"),
        ("infer.offline_required", "true"),
        ("infer.fallback", "none"),
    ] {
        if metadata.get(key).is_some_and(|actual| actual != required) {
            return Err(ApiError::bad_request(format!(
                "{key} is fixed to {required} for {purpose}"
            )));
        }
        metadata.insert(key.into(), required.into());
    }
    Ok(metadata)
}

fn fail_closed_event_metadata(
    mut metadata: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ApiError> {
    for (key, required) in [
        ("infer.placement", "local_only"),
        ("infer.offline_required", "true"),
        ("infer.fallback", "none"),
    ] {
        if metadata.get(key).is_some_and(|actual| actual != required) {
            return Err(ApiError::bad_request(format!(
                "{key} is fixed to {required} for sound-event detection"
            )));
        }
        metadata.insert(key.into(), required.into());
    }
    Ok(metadata)
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

async fn create_sound_generation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<SoundGenerationRequest>, JsonRejection>,
) -> Result<Response<Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    if headers
        .get(infer_core::CAPABILITY_CONTRACT_HEADER)
        .is_some_and(|value| value == "infer.audio.sound-generation@20260926.1")
        && request.model_choice.is_some()
    {
        return Err(ApiError::bad_request(
            "model_choice requires sound-generation contract 20260926.2",
        ));
    }
    request.metadata = fail_closed_local_audio_metadata(request.metadata, "sound generation")?;
    let duration_seconds = request.duration_seconds;
    let model_choice = request.selected_model_choice();
    let result = state
        .runtime
        .execute_audio(&app_id, AudioExecutionRequest::SoundGeneration(request))
        .await?;
    match result.output {
        AudioExecutionOutput::Audio {
            bytes,
            content_type: "audio/wav",
        } => {
            let digest = format!("{:x}", Sha256::digest(&bytes));
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            insert_response_header(headers, "x-infer-job-id", &result.job_id)?;
            insert_response_header(headers, "x-infer-model", &result.logical_model)?;
            insert_response_header(headers, "x-infer-model-choice", model_choice.as_str())?;
            insert_response_header(headers, "x-infer-provider", &result.provider)?;
            insert_response_header(headers, "x-infer-deployment", &result.deployment)?;
            insert_response_header(headers, "x-infer-model-build", &result.model_build)?;
            insert_response_header(headers, "x-infer-physical-model", &result.physical_model)?;
            insert_response_header(headers, "x-infer-placement", &result.placement)?;
            insert_response_header(headers, "x-infer-artifact-sha256", &digest)?;
            insert_response_header(
                headers,
                "x-infer-seed",
                &result.seed.expect("sound seed is assigned").to_string(),
            )?;
            insert_response_header(
                headers,
                "x-infer-duration-seconds",
                &duration_seconds.to_string(),
            )?;
            Ok(response)
        }
        _ => Err(ApiError::internal("sound worker did not return WAV audio")),
    }
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
    EventDetection,
    Embedding,
    VoiceClone,
}

impl AudioMultipartContract {
    fn file_field(self) -> &'static str {
        match self {
            Self::Transcription | Self::Alignment | Self::EventDetection | Self::Embedding => {
                "file"
            }
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
            Self::EventDetection => name == "model",
            Self::Embedding => matches!(name, "model" | "source_revision"),
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

/// Console-only, cross-App view. This deliberately does not reuse the
/// consumer-scoped `/infer/v1/jobs` route: a normal App can never widen its
/// own Job visibility with query parameters.
async fn get_operator_jobs(
    State(state): State<ApiState>,
    headers: HeaderMap,
    query: Result<Query<JobListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
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
        .operator_job_page(priority, job_state, cursor.as_ref(), limit)?;
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
    if state.runtime.cancel_for_app(&app_id, &response_id).await? {
        Ok(Json(json!({"id":response_id, "cancelled":true})))
    } else {
        Err(ApiError::not_found("response is terminal or was not found"))
    }
}

async fn cancel_operator_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    if state.runtime.cancel(&response_id).await? {
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
    explain_job_response(&state, job)
}

async fn explain_operator_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let job = state
        .runtime
        .snapshot(&response_id)
        .await?
        .ok_or_else(|| ApiError::not_found("response was not found"))?;
    explain_job_response(&state, job)
}

fn explain_job_response(state: &ApiState, job: JobSnapshot) -> Result<Json<Value>, ApiError> {
    let response_id = job.id.clone();
    let audit_events = state.runtime.audit_events(&response_id)?;
    Ok(Json(json!({
        "response_id": response_id,
        "intent": job.intent,
        "consumer_core_contract": job.consumer_core_contract,
        "capability_contract": job.capability_contract,
        "policy": job.policy,
        "selected_provider": job.provider,
        "selected_deployment": job.deployment,
        "model_profile": job.model_profile,
        "model_build": job.model_build,
        "physical_model": job.physical_model,
        "placement": job.placement,
        "capability_level": job.capability_level,
        "evaluation_status": job.evaluation_status,
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
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let budget = state.runtime.budget_snapshot()?;
    Ok(Json(
        serde_json::to_value(budget).expect("budget snapshot is serializable"),
    ))
}

async fn get_providers(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        json!({"providers": state.runtime.provider_snapshots()}),
    ))
}

async fn get_provider_models(
    State(state): State<ApiState>,
    Path(provider_id): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let fresh = query.get("fresh").is_some_and(|value| value == "1");
    Ok(Json(
        serde_json::to_value(
            state
                .runtime
                .provider_model_catalog(&provider_id, fresh)
                .await?,
        )
        .expect("provider model catalog is serializable"),
    ))
}

async fn get_resources(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(state.runtime.resource_snapshot().await)
            .expect("resource snapshot is serializable"),
    ))
}

async fn refresh_resources(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
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
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    Ok(Json(
        serde_json::to_value(state.runtime.metrics().await)
            .expect("metrics snapshot is serializable"),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryQuery {
    window: Option<String>,
}

async fn get_telemetry(
    State(state): State<ApiState>,
    headers: HeaderMap,
    query: Result<Query<TelemetryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let actor = authenticate(&state, &headers)?;
    state.runtime.authorize_resource_admin(&actor)?;
    let query = strict_query(query)?;
    let range = query
        .window
        .as_deref()
        .map(TelemetryRange::parse)
        .unwrap_or(Some(TelemetryRange::LastDay))
        .ok_or_else(|| ApiError::bad_request("window must be one of 1h, 24h, or 7d"))?;
    Ok(Json(
        serde_json::to_value(state.runtime.telemetry(range)?)
            .expect("telemetry snapshot is serializable"),
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
            code: contract::error_code::INVALID_API_KEY,
            message: "missing or invalid bearer credential".into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: contract::error_code::NOT_FOUND,
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: contract::error_code::INVALID_REQUEST,
            message: message.into(),
        }
    }
    fn upgrade_required(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UPGRADE_REQUIRED,
            code: contract::error_code::CONSUMER_CORE_UNSUPPORTED,
            message: message.into(),
        }
    }
    fn capability_upgrade_required(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UPGRADE_REQUIRED,
            code: contract::error_code::CAPABILITY_CONTRACT_UNSUPPORTED,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: contract::error_code::INTERNAL,
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
            RuntimeError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                contract::error_code::INVALID_API_KEY,
            ),
            RuntimeError::ObserverCredentialRestricted => (
                StatusCode::FORBIDDEN,
                contract::error_code::OBSERVER_CREDENTIAL_RESTRICTED,
            ),
            RuntimeError::ObserverAccessRequired(_) => (
                StatusCode::FORBIDDEN,
                contract::error_code::OBSERVER_ACCESS_REQUIRED,
            ),
            RuntimeError::UnknownIntent(_)
            | RuntimeError::DataPlaneMismatch { .. }
            | RuntimeError::Contract(_)
            | RuntimeError::ProviderProbeUnsupported(_) => (
                StatusCode::BAD_REQUEST,
                contract::error_code::INVALID_REQUEST,
            ),
            RuntimeError::UnknownApp(_)
            | RuntimeError::PolicyNotAllowed(_)
            | RuntimeError::OverrideNotAllowed { .. } => (
                StatusCode::FORBIDDEN,
                contract::error_code::POLICY_VIOLATION,
            ),
            RuntimeError::IntentNotAllowed { .. } => (
                StatusCode::FORBIDDEN,
                contract::error_code::INTENT_FORBIDDEN,
            ),
            RuntimeError::AgentTaskNotAllowed(_) => (
                StatusCode::FORBIDDEN,
                contract::error_code::AGENT_TASK_FORBIDDEN,
            ),
            RuntimeError::NamedRouteNotAllowed { .. } => (
                StatusCode::FORBIDDEN,
                contract::error_code::ROUTE_TARGET_FORBIDDEN,
            ),
            RuntimeError::ResourceAdminRequired(_) => (
                StatusCode::FORBIDDEN,
                contract::error_code::RESOURCE_ADMIN_REQUIRED,
            ),
            RuntimeError::NoCandidate => (StatusCode::CONFLICT, contract::error_code::NO_CANDIDATE),
            RuntimeError::NamedModelUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                contract::error_code::UPSTREAM_UNAVAILABLE,
            ),
            RuntimeError::Cancelled => (StatusCode::CONFLICT, contract::error_code::CANCELLED),
            RuntimeError::QueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                contract::error_code::QUEUE_FULL,
            ),
            RuntimeError::AppQueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                contract::error_code::APP_QUEUE_FULL,
            ),
            RuntimeError::QuotaExceeded { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                contract::error_code::QUOTA_EXCEEDED,
            ),
            RuntimeError::DeadlineExpired => (
                StatusCode::GATEWAY_TIMEOUT,
                contract::error_code::DEADLINE_EXCEEDED,
            ),
            RuntimeError::ProviderUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                contract::error_code::PROVIDER_UNAVAILABLE,
            ),
            // An enabled node budget waits rather than rejecting ordinary
            // work. This branch is therefore only reachable for an internal
            // scheduler failure or an invalid embedded configuration, neither
            // of which should disclose resource accounting details.
            RuntimeError::NodeCapacity(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                contract::error_code::PROVIDER_UNAVAILABLE,
            ),
            RuntimeError::ProviderProbeModelMissing(_) => (
                StatusCode::CONFLICT,
                contract::error_code::PROVIDER_PROBE_MODEL_MISSING,
            ),
            RuntimeError::BackgroundDisabled | RuntimeError::BackgroundLocalOnly => (
                StatusCode::CONFLICT,
                contract::error_code::BACKGROUND_UNAVAILABLE,
            ),
            RuntimeError::Payload(PayloadError::TooLarge { .. }) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                contract::error_code::BACKGROUND_PAYLOAD_TOO_LARGE,
            ),
            RuntimeError::BackgroundDisabledWithPendingJobs
            | RuntimeError::BackgroundKeyUnavailable(_)
            | RuntimeError::BackgroundPayloadFormat
            | RuntimeError::BackgroundAttemptBudgetExhausted
            | RuntimeError::BackgroundTaskFailed
            | RuntimeError::Payload(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                contract::error_code::BACKGROUND_PERSISTENCE_ERROR,
            ),
            RuntimeError::Resource(
                ResourceError::UnknownProvider(_) | ResourceError::UnknownDeployment { .. },
            ) => (StatusCode::NOT_FOUND, contract::error_code::NOT_FOUND),
            RuntimeError::Resource(ResourceError::UnknownEvictionDeployment(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                contract::error_code::INVALID_EVICTION_PLAN,
            ),
            RuntimeError::Resource(ResourceError::Lifecycle(_)) => (
                StatusCode::CONFLICT,
                contract::error_code::RESOURCE_TRANSITION_CONFLICT,
            ),
            RuntimeError::Resource(ResourceError::ReloadBenchmarkRequest(_)) => (
                StatusCode::BAD_REQUEST,
                contract::error_code::INVALID_RELOAD_BENCHMARK,
            ),
            RuntimeError::Resource(ResourceError::EvictionApply(
                EvictionApplyError::MissingExpectedDeployment
                | EvictionApplyError::MissingReason
                | EvictionApplyError::ReasonTooLong,
            )) => (
                StatusCode::BAD_REQUEST,
                contract::error_code::INVALID_EVICTION_APPROVAL,
            ),
            RuntimeError::Resource(ResourceError::EvictionApply(
                EvictionApplyError::NoActionableTarget | EvictionApplyError::TargetChanged { .. },
            )) => (
                StatusCode::CONFLICT,
                contract::error_code::EVICTION_RECOMMENDATION_CHANGED,
            ),
            RuntimeError::Resource(ResourceError::NativeControl(_)) => (
                StatusCode::BAD_GATEWAY,
                contract::error_code::NATIVE_CONTROL_UNAVAILABLE,
            ),
            RuntimeError::MaintenanceLease(
                MaintenanceLeaseError::DurationTooShort
                | MaintenanceLeaseError::DurationTooLong { .. }
                | MaintenanceLeaseError::MissingReason
                | MaintenanceLeaseError::ReasonTooLong,
            ) => (
                StatusCode::BAD_REQUEST,
                contract::error_code::INVALID_MAINTENANCE_LEASE,
            ),
            RuntimeError::MaintenanceLease(
                MaintenanceLeaseError::MonitorDisabled
                | MaintenanceLeaseError::ActiveLease
                | MaintenanceLeaseError::LeaseMismatch,
            ) => (
                StatusCode::CONFLICT,
                contract::error_code::MAINTENANCE_LEASE_CONFLICT,
            ),
            RuntimeError::Store(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                contract::error_code::PERSISTENCE_ERROR,
            ),
            RuntimeError::Artifact(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                contract::error_code::ARTIFACT_STORE_ERROR,
            ),
            RuntimeError::Credential(_) => unreachable!("handled above"),
            RuntimeError::Provider(provider) => match provider.kind() {
                ProviderFailureKind::Authentication => (
                    StatusCode::BAD_GATEWAY,
                    contract::error_code::UPSTREAM_AUTHENTICATION,
                ),
                ProviderFailureKind::RateLimited => (
                    StatusCode::TOO_MANY_REQUESTS,
                    contract::error_code::UPSTREAM_RATE_LIMITED,
                ),
                ProviderFailureKind::Timeout => (
                    StatusCode::GATEWAY_TIMEOUT,
                    contract::error_code::UPSTREAM_TIMEOUT,
                ),
                ProviderFailureKind::Unavailable => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    contract::error_code::UPSTREAM_UNAVAILABLE,
                ),
                ProviderFailureKind::InvalidRequest => (
                    StatusCode::BAD_REQUEST,
                    contract::error_code::UPSTREAM_INVALID_REQUEST,
                ),
                ProviderFailureKind::Protocol => (
                    StatusCode::BAD_GATEWAY,
                    contract::error_code::UPSTREAM_PROTOCOL,
                ),
            },
        };
        Self {
            status,
            code,
            message: error.public_message(),
        }
    }
}

#[cfg(test)]
fn mapped_runtime_error_codes() -> std::collections::BTreeSet<&'static str> {
    contract::CORE_ERROR_CODES
        .iter()
        .chain(contract::CAPABILITY_ERROR_CODES)
        .chain(contract::OPERATOR_ERROR_CODES)
        .copied()
        .collect()
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
    fn public_error_registry_is_unique_and_contains_every_direct_mapping() {
        let codes = mapped_runtime_error_codes();
        assert_eq!(
            codes.len(),
            contract::CORE_ERROR_CODES.len()
                + contract::CAPABILITY_ERROR_CODES.len()
                + contract::OPERATOR_ERROR_CODES.len()
        );
        for required in [
            contract::error_code::CONSUMER_CORE_UNSUPPORTED,
            contract::error_code::CAPABILITY_CONTRACT_UNSUPPORTED,
            contract::error_code::INVALID_REQUEST,
            contract::error_code::INVALID_API_KEY,
            contract::error_code::NO_CANDIDATE,
            contract::error_code::PROVIDER_UNAVAILABLE,
            contract::error_code::RAW_EXECUTION_FAILED,
        ] {
            assert!(
                codes.contains(required),
                "missing public error code {required}"
            );
        }
    }

    #[test]
    fn upstream_rate_limit_keeps_its_public_error_semantics() {
        let error: ApiError = RuntimeError::Provider(ProviderError::Upstream {
            status: 429,
            body: String::new(),
            retry_after: None,
        })
        .into();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code, "upstream_rate_limited");
    }

    #[test]
    fn provider_native_diagnostics_are_not_exposed_publicly() {
        let error: ApiError = RuntimeError::Provider(ProviderError::NativeRuntime(
            "/Users/example/private/model.onnx failed with secret payload".into(),
        ))
        .into();
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.code, "upstream_protocol");
        assert_eq!(error.message, "provider protocol failed");
        assert!(!error.message.contains("/Users"));
        assert!(!error.message.contains("secret"));
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
            intent: "reasoning.solve".into(),
        }
        .into();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "intent_forbidden");
    }

    #[test]
    fn named_route_acl_has_a_distinct_non_enumerating_forbidden_code() {
        let error: ApiError = RuntimeError::NamedRouteNotAllowed {
            app_id: "reader".into(),
            intent: "text.edit".into(),
        }
        .into();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "route_target_forbidden");
    }

    #[test]
    fn missing_consumer_contract_header_is_upgrade_required() {
        let error = require_current_consumer_contract(&HeaderMap::new()).unwrap_err();
        assert_eq!(error.status, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(error.code, "consumer_core_unsupported");
    }

    #[test]
    fn capability_contract_is_exact_and_route_scoped() {
        let mut headers = HeaderMap::new();
        let error =
            require_current_capability_contract(&headers, Some("infer.audio.transcription"))
                .unwrap_err();
        assert_eq!(error.status, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(error.code, "capability_contract_unsupported");

        headers.insert(
            infer_core::CAPABILITY_CONTRACT_HEADER,
            HeaderValue::from_static("infer.audio.transcription@20260814.1"),
        );
        assert!(
            require_current_capability_contract(&headers, Some("infer.audio.transcription"))
                .is_ok()
        );
        assert!(require_current_capability_contract(&headers, None).is_ok());
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
        assert!(is_polling_endpoint("/infer/v1/telemetry"));
        assert!(is_polling_endpoint("/infer/v1/jobs"));
        assert!(!is_polling_endpoint("/v1/responses"));
        assert!(!is_polling_endpoint("/infer/v1/jobs/resp_123"));
    }
}
