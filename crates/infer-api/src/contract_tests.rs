use std::collections::BTreeMap;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header, request},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
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
    contracted(request).header(header::AUTHORIZATION, "Bearer contract-test-token")
}

fn capability(request: request::Builder, capability: &'static str) -> request::Builder {
    request.header(contract::CAPABILITY_CONTRACT_HEADER, capability)
}

fn contracted(request: request::Builder) -> request::Builder {
    request.header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
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
async fn core_and_capability_catalog_are_public_bootstrap_with_exact_identity() {
    let manifest = contract_router()
        .oneshot(
            contracted(Request::builder())
                .uri("/infer/v1/contract")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manifest.status(), StatusCode::OK);
    let manifest = body_json(manifest).await;
    assert_eq!(manifest["schema"], "infer-runtime.consumer-core");
    assert_eq!(manifest["schema_version"], contract::CORE_VERSION);
    assert_eq!(manifest["core_contract"], contract::CORE_CONTRACT);
    assert_eq!(manifest["openapi_sha256"], contract::OPENAPI_SHA256);
    assert_eq!(
        manifest["error_codes"],
        serde_json::json!(contract::CORE_ERROR_CODES)
    );
    assert_eq!(
        manifest["supported_core_contracts"],
        serde_json::json!(contract::SUPPORTED_CORE_CONTRACTS)
    );
    assert_eq!(
        manifest["capability_catalog"]["url"],
        "/infer/v1/capabilities"
    );

    let catalog = contract_router()
        .oneshot(
            contracted(Request::builder())
                .uri("/infer/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(catalog.status(), StatusCode::OK);
    let catalog = body_json(catalog).await;
    assert_eq!(catalog["schema"], contract::CAPABILITY_CATALOG_SCHEMA);
    assert_eq!(
        catalog["schema_version"],
        contract::CAPABILITY_CATALOG_VERSION
    );
    assert_eq!(catalog["core_contract"], contract::CORE_CONTRACT);
    assert!(
        catalog["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability["id"] == "infer.audio.transcription-stream")
    );
    for capability in catalog["capabilities"].as_array().unwrap() {
        assert_eq!(capability["schema"]["format"], "openapi-3.1");
        let url = capability["schema"]["url"].as_str().unwrap();
        assert!(url.starts_with("/infer/v1/capability-schemas/"));
        assert_eq!(capability["schema"]["sha256"].as_str().unwrap().len(), 64);

        let schema = contract_router()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(schema.status(), StatusCode::OK);
        let schema = body_json(schema).await;
        assert_eq!(
            schema["x-infer-capability-contract"],
            format!(
                "{}@{}",
                capability["id"].as_str().unwrap(),
                capability["schema_version"].as_str().unwrap()
            )
        );
    }

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
        contract::CORE_VERSION
    );
}

#[test]
fn catalog_routes_have_one_exact_runtime_capability_identity() {
    let mut routes = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for capability in contract::CAPABILITIES {
        let version = capability.schema_version;
        let identity = format!("{}@{}", capability.id, capability.schema_version);
        assert!(identities.insert(identity.clone()));
        for route in capability.routes {
            assert!(routes.insert((route.method, route.path, version)));
            let concrete = route
                .path
                .replace("{response_id}", "resp_example")
                .replace("{job_id}", "raw_example");
            assert_eq!(
                contract::required_capability_id(&concrete),
                Some(capability.id),
                "{} {}",
                route.method,
                route.path
            );
        }
    }
    for route in contract::CONSUMER_ROUTES {
        let concrete = route.path.replace("{response_id}", "resp_example");
        assert_eq!(contract::required_capability_id(&concrete), None);
    }
}

#[tokio::test]
async fn explicit_contract_handshake_is_exact_and_fail_closed() {
    for requested in ["20260812.1", "0.1.0-candidate.4", "0.1.0-candidate.3"] {
        let response = contract_router()
            .oneshot(
                Request::builder()
                    .uri("/infer/v1/contract")
                    .header(contract::CONSUMER_CORE_HEADER, requested)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "consumer_core_unsupported");
    }

    let missing = contract_router()
        .oneshot(
            Request::builder()
                .uri("/infer/v1/contract")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(
        body_json(missing).await["error"]["code"],
        "consumer_core_unsupported"
    );

    let duplicate = contract_router()
        .oneshot(
            Request::builder()
                .uri("/infer/v1/contract")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(
        body_json(duplicate).await["error"]["code"],
        "consumer_core_unsupported"
    );
}

#[tokio::test]
async fn missing_or_old_contract_is_rejected_before_every_consumer_surface_class() {
    for (method, path) in [
        ("POST", "/v1/responses"),
        ("GET", "/v1/responses/resp_example"),
        ("POST", "/v1/audio/speech"),
        ("GET", "/infer/v1/jobs"),
        ("GET", "/infer/v1/explain/resp_example"),
        ("POST", "/infer/v1/vision/text-embeddings"),
        ("POST", "/infer/v1/raw/foundations/leases"),
    ] {
        for requested in [None, Some("0.1.0-candidate.4")] {
            let mut request = Request::builder().method(method).uri(path);
            if let Some(requested) = requested {
                request = request.header(contract::CONSUMER_CORE_HEADER, requested);
            }
            let response = contract_router()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED, "{path}");
            assert_eq!(
                body_json(response).await["error"]["code"],
                "consumer_core_unsupported"
            );
        }
    }
}

#[tokio::test]
async fn duplex_audio_rejects_a_request_without_a_server_upgrade_context() {
    let response = contract_router()
        .oneshot(
            capability(
                contracted(Request::builder()),
                "infer.audio.transcription-stream@20260811.1",
            )
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
async fn capability_handshake_is_exact_before_auth_or_payload_parsing() {
    for requested in [None, Some("infer.responses@20260811.1")] {
        let mut request = contracted(Request::builder())
            .method("POST")
            .uri("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(requested) = requested {
            request = request.header(contract::CAPABILITY_CONTRACT_HEADER, requested);
        }
        let response = contract_router()
            .oneshot(request.body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "capability_contract_unsupported"
        );
    }
}

#[tokio::test]
async fn unknown_response_json_fields_use_the_stable_error_envelope() {
    let response = contract_router()
        .oneshot(
            capability(
                authenticated(Request::builder()),
                "infer.responses@20260812.1",
            )
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
            capability(
                authenticated(Request::builder()),
                "infer.audio.speech@20260811.1",
            )
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
                capability(
                    authenticated(Request::builder()),
                    "infer.audio.transcription@20260814.1",
                )
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
async fn audio_event_route_rejects_any_attempt_to_widen_local_offline_execution() {
    let body = "--x\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.detect_events\r\n--x\r\nContent-Disposition: form-data; name=\"infer.placement\"\r\n\r\ncloud_only\r\n--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\naudio\r\n--x--\r\n";
    let response = contract_router()
        .oneshot(
            capability(
                authenticated(Request::builder()),
                "infer.audio.event-detection@20260813.2",
            )
            .method("POST")
            .uri("/v1/audio/event-detections")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
            .body(Body::from(body))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("infer.placement is fixed to local_only")
    );
}

#[tokio::test]
async fn authentication_failures_use_the_same_error_envelope() {
    let response = contract_router()
        .oneshot(
            capability(contracted(Request::builder()), "infer.responses@20260812.1")
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
