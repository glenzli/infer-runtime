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
    request.header(header::AUTHORIZATION, "Bearer contract-test-token")
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
