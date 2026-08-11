use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header, request},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{AudioExecutionRequest, RuntimeConfig};
use infer_provider::{AudioExecutionOutput, AudioExecutor, DynAudioExecutor, ProviderError};
use serde_json::Value;
use tower::ServiceExt;

use crate::{contract, router};

fn contract_router() -> Router {
    let config: RuntimeConfig = toml::from_str(include_str!("../../../config/infer.example.toml"))
        .expect("checked-in config parses");
    let credentials =
        AppCredentials::from_pairs([("local-operator", "contract-test-token")]).unwrap();
    router(Runtime::with_providers_and_credentials(
        config,
        BTreeMap::new(),
        credentials,
    ))
}

fn authenticated(request: request::Builder) -> request::Builder {
    request.header(header::AUTHORIZATION, "Bearer contract-test-token")
}

#[derive(Clone)]
struct StaticEventExecutor {
    calls: Arc<AtomicUsize>,
    drift: bool,
}

#[async_trait]
impl AudioExecutor for StaticEventExecutor {
    fn id(&self) -> &str {
        "yamnet-local"
    }

    async fn execute(
        &self,
        physical_model: &str,
        request: AudioExecutionRequest,
    ) -> Result<AudioExecutionOutput, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(physical_model, "models/yamnet-tfhub-v1");
        let AudioExecutionRequest::EventDetection(request) = request else {
            panic!("event executor received another audio task");
        };
        assert_eq!(request.model, "audio.detect_events");
        assert_eq!(request.metadata["infer.placement"], "local_only");
        assert_eq!(request.metadata["infer.offline_required"], "true");
        assert_eq!(request.metadata["infer.fallback"], "none");

        let mut result: Value = serde_json::from_str(include_str!(
            "../../../contracts/v0.1/fixtures/audio-event-detection-response.json"
        ))
        .unwrap();
        let object = result.as_object_mut().unwrap();
        object.remove("id");
        object.remove("model");
        if self.drift {
            object["policy"]["revision"] = Value::String("unadmitted-policy".into());
        }
        Ok(AudioExecutionOutput::Json(result))
    }
}

fn event_router(allowed: bool) -> (Router, Arc<AtomicUsize>) {
    event_router_with_drift(allowed, false)
}

fn event_router_with_drift(allowed: bool, drift: bool) -> (Router, Arc<AtomicUsize>) {
    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    let mut echo = config.apps["example-local-consumer"].clone();
    echo.allowed_intents = Some(if allowed {
        vec!["audio.detect_events".into()]
    } else {
        Vec::new()
    });
    config.apps.insert("echo".into(), echo);
    config.validate().unwrap();
    let credentials = AppCredentials::from_pairs([("echo", "echo-contract-token")]).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let executor = StaticEventExecutor {
        calls: Arc::clone(&calls),
        drift,
    };
    let executors = BTreeMap::from([(
        "yamnet-local".into(),
        Arc::new(executor) as DynAudioExecutor,
    )]);
    (
        router(Runtime::with_audio_executors(
            config,
            BTreeMap::new(),
            credentials,
            executors,
        )),
        calls,
    )
}

fn event_request(metadata: Option<&str>) -> Request<Body> {
    let metadata = metadata
        .map(|value| {
            format!(
                "--event\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{value}\r\n"
            )
        })
        .unwrap_or_default();
    let body = format!(
        "--event\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.detect_events\r\n{metadata}--event\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sound.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFFaudio\r\n--event--\r\n"
    );
    Request::builder()
        .method("POST")
        .uri("/v1/audio/event-detections")
        .header(header::AUTHORIZATION, "Bearer echo-contract-token")
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=event")
        .body(Body::from(body))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body is readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("response is JSON")
}

#[tokio::test]
async fn contract_identity_and_openapi_are_available_without_authentication() {
    let manifest = contract_router()
        .oneshot(
            Request::builder()
                .uri("/infer/v1/contract")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manifest.status(), StatusCode::OK);
    let manifest = body_json(manifest).await;
    assert_eq!(manifest["contract_version"], contract::CONTRACT_VERSION);
    assert_eq!(manifest["operator_routes"]["stability"], "experimental");
    assert!(
        manifest["experimental_routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|route| route["path"] == "/v1/audio/transcriptions/stream")
    );

    let openapi = contract_router()
        .oneshot(
            Request::builder()
                .uri("/infer/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(openapi.status(), StatusCode::OK);
    assert_eq!(openapi.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        body_json(openapi).await["info"]["version"],
        contract::CONTRACT_VERSION
    );
}

#[tokio::test]
async fn duplex_audio_rejects_a_request_without_a_server_upgrade_context() {
    let response = contract_router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/audio/transcriptions/stream")
                .header(header::CONNECTION, "upgrade")
                .header(header::UPGRADE, "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
}

#[tokio::test]
async fn unknown_response_json_fields_use_the_stable_error_envelope() {
    let response = contract_router()
        .oneshot(
            authenticated(Request::builder())
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"text.summarize","input":"text","modle":"typo"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["code"], "invalid_request_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("modle")
    );
}

#[tokio::test]
async fn unknown_speech_json_fields_use_the_stable_error_envelope() {
    let response = contract_router()
        .oneshot(
            authenticated(Request::builder())
                .method("POST")
                .uri("/v1/audio/speech")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"speech.synthesize","input":"hi","voice":"default","formt":"wav"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "invalid_request_error"
    );
}

#[tokio::test]
async fn audio_multipart_rejects_unknown_and_wrong_file_fields() {
    for (body, expected) in [
        (
            "--x\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.transcribe\r\n--x\r\nContent-Disposition: form-data; name=\"modle\"\r\n\r\ntypo\r\n--x--\r\n",
            "unknown multipart field `modle`",
        ),
        (
            "--x\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.transcribe\r\n--x\r\nContent-Disposition: form-data; name=\"reference_audio\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\naudio\r\n--x--\r\n",
            "unexpected multipart file field `reference_audio`",
        ),
    ] {
        let response = contract_router()
            .oneshot(
                authenticated(Request::builder())
                    .method("POST")
                    .uri("/v1/audio/transcriptions")
                    .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = body_json(response).await;
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected)
        );
    }
}

#[tokio::test]
async fn authentication_failures_use_the_same_error_envelope() {
    let response = contract_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"model":"text.summarize","input":"text"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error = body_json(response).await;
    assert_eq!(error["error"]["code"], "invalid_api_key");
    assert_eq!(error["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn job_list_rejects_unknown_query_fields_with_the_stable_envelope() {
    let response = contract_router()
        .oneshot(
            authenticated(Request::builder())
                .uri("/infer/v1/jobs?stat=succeeded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert_eq!(error["error"]["code"], "invalid_request_error");
    assert!(error["error"]["message"].as_str().unwrap().contains("stat"));
}

#[tokio::test]
async fn audio_event_route_enforces_acl_and_fail_closed_metadata_before_dispatch() {
    let (denied, denied_calls) = event_router(false);
    let response = denied.oneshot(event_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "intent_forbidden"
    );
    assert_eq!(denied_calls.load(Ordering::SeqCst), 0);

    let (conflicting, conflicting_calls) = event_router(true);
    let response = conflicting
        .oneshot(event_request(Some(r#"{"infer.placement":"cloud_only"}"#)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "invalid_request_error"
    );
    assert_eq!(conflicting_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn audio_event_route_returns_typed_result_and_payload_free_job_provenance() {
    let (app, calls) = event_router(true);
    let response = app.clone().oneshot(event_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let event = body_json(response).await;
    assert_eq!(event["model"], "audio.detect_events");
    assert_eq!(event["object"], "audio.event_detection");
    assert_eq!(event["ontology"]["class_id_namespace"], "audioset_mid");
    assert_eq!(event["speech_presence"]["status"], "unknown");
    assert_eq!(event["coverage"]["status"], "full");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let job_id = event["id"].as_str().unwrap();
    let job = app
        .oneshot(
            Request::builder()
                .uri(format!("/infer/v1/jobs/{job_id}"))
                .header(header::AUTHORIZATION, "Bearer echo-contract-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::OK);
    let job = body_json(job).await;
    assert_eq!(job["app_id"], "echo");
    assert_eq!(job["intent"], "audio.detect_events");
    assert_eq!(job["provider"], "yamnet-local");
    assert_eq!(job["deployment"], "yamnet_audio_events_tfhub_v1");
    assert_eq!(job["model_profile"], "yamnet_tfhub_v1");
    assert_eq!(job["model_build"], "yamnet_tfhub_v1_tensorflow_2_20");
    assert_eq!(job["physical_model"], "models/yamnet-tfhub-v1");
    assert_eq!(job["attempts"].as_array().unwrap().len(), 1);
    assert!(job.get("file").is_none());
    assert!(job.get("audio").is_none());
}

#[tokio::test]
async fn audio_event_build_identity_drift_fails_closed() {
    let (app, calls) = event_router_with_drift(true, true);
    let response = app.oneshot(event_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "upstream_protocol"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
