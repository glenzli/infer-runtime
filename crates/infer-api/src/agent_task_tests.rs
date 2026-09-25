use std::collections::BTreeMap;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::{contract, router};

fn test_router(agent_grant: bool) -> axum::Router {
    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    config
        .apps
        .get_mut("local-operator")
        .unwrap()
        .allow_agent_file_tasks = agent_grant;
    let credentials = AppCredentials::from_pairs([("local-operator", "agent-test-token")]).unwrap();
    router(Runtime::with_providers_and_credentials(
        config,
        BTreeMap::new(),
        credentials,
    ))
}

fn valid_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "agent.file_task",
        "instruction": "Read input/a.txt and write output/draft.txt",
        "input_files": [{
            "path": "a.txt",
            "content_base64": "aGk=",
            "sha256": format!("{:x}", Sha256::digest(b"hi")),
        }],
        "output_paths": ["draft.txt"]
    }))
    .unwrap()
}

fn request(capability: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/infer/v1/agent/tasks")
        .header(header::AUTHORIZATION, "Bearer agent-test-token")
        .header(header::CONTENT_TYPE, "application/json")
        .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
        .header(contract::CAPABILITY_CONTRACT_HEADER, capability)
        .body(Body::from(body))
        .unwrap()
}

async fn code(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    value["error"]["code"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn exact_contract_and_separate_app_acl_fail_before_agent_admission() {
    let unsupported = test_router(true)
        .oneshot(request("infer.responses@20260812.1", valid_body()))
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(code(unsupported).await, "capability_contract_unsupported");

    let forbidden = test_router(false)
        .oneshot(request("infer.agent.task@20260925.1", valid_body()))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert_eq!(code(forbidden).await, "agent_task_forbidden");
}

#[tokio::test]
async fn admitted_contract_remains_closed_without_read_isolation() {
    let response = test_router(true)
        .oneshot(request("infer.agent.task@20260925.1", valid_body()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(response).await, "agent_task_unavailable");

    let mut unsafe_body: Value = serde_json::from_slice(&valid_body()).unwrap();
    unsafe_body["input_files"][0]["path"] = json!("../../private.txt");
    let invalid = test_router(true)
        .oneshot(request(
            "infer.agent.task@20260925.1",
            serde_json::to_vec(&unsafe_body).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(code(invalid).await, "invalid_request_error");
}
