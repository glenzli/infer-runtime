use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{AgentTaskInputFile, AgentTaskRequest, ResponsesRequest, RuntimeConfig};
use infer_provider::{
    AgentTaskExecution, CodexAppServerProvider, Provider, ProviderByteStream, ProviderError,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::{contract, router};

fn test_router(agent_grant: bool) -> axum::Router {
    test_router_with_providers(agent_grant, BTreeMap::new())
}

fn test_router_with_providers(
    agent_grant: bool,
    providers: BTreeMap<String, infer_provider::DynProvider>,
) -> axum::Router {
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
        providers,
        credentials,
    ))
}

struct FakeAgentProvider {
    calls: AtomicUsize,
    fail_unknown: bool,
}

#[async_trait]
impl Provider for FakeAgentProvider {
    fn id(&self) -> &str {
        "codex-agent"
    }

    async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
        panic!("Agent provider must not use Responses")
    }

    async fn execute_stream(
        &self,
        _request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        panic!("Agent provider must not stream Responses")
    }

    async fn execute_agent_task(
        &self,
        request: AgentTaskRequest,
        model: &str,
    ) -> Result<AgentTaskExecution, ProviderError> {
        assert_eq!(model, "gpt-6-sol");
        assert_eq!(request.input_files[0].path, "a.txt");
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_unknown {
            return Err(ProviderError::RemoteOutcomeUnknown);
        }
        Ok(AgentTaskExecution {
            answer: "done".into(),
            outputs: vec![AgentTaskInputFile {
                path: "draft.txt".into(),
                content_base64: "b2s=".into(),
                sha256: format!("{:x}", Sha256::digest(b"ok")),
            }],
            thread_id: "thread-test".into(),
            turn_id: "turn-test".into(),
            sandbox_profile: "infer_agent_task".into(),
            tool_policy: "test-bounded".into(),
        })
    }
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
async fn admitted_contract_returns_unavailable_without_executor() {
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

#[tokio::test]
async fn admitted_agent_runs_one_attempt_and_records_job() {
    let fake = Arc::new(FakeAgentProvider {
        calls: AtomicUsize::new(0),
        fail_unknown: false,
    });
    let providers = BTreeMap::from([(
        "codex-agent".into(),
        fake.clone() as infer_provider::DynProvider,
    )]);
    let app = test_router_with_providers(true, providers);
    let response = app
        .clone()
        .oneshot(request("infer.agent.task@20260925.1", valid_body()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["state"], "completed");
    assert_eq!(result["outputs"][0]["content_base64"], "b2s=");
    assert_eq!(result["provenance"]["provider"], "codex-agent");
    assert_eq!(result["provenance"]["deployment"], "codex_agent_gpt_6_sol");
    assert_eq!(result["provenance"]["attempt_number"], 1);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

    let job_id = result["job_id"].as_str().unwrap();
    let job = app
        .oneshot(
            Request::builder()
                .uri(format!("/infer/v1/jobs/{job_id}"))
                .header(header::AUTHORIZATION, "Bearer agent-test-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::OK);
    let body = job.into_body().collect().await.unwrap().to_bytes();
    let snapshot: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(snapshot["state"], "succeeded");
    assert_eq!(snapshot["attempts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_agent_outcome_is_not_retried() {
    let fake = Arc::new(FakeAgentProvider {
        calls: AtomicUsize::new(0),
        fail_unknown: true,
    });
    let app = test_router_with_providers(
        true,
        BTreeMap::from([(
            "codex-agent".into(),
            fake.clone() as infer_provider::DynProvider,
        )]),
    );
    let response = app
        .clone()
        .oneshot(request("infer.agent.task@20260925.1", valid_body()))
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "requires a signed-in Codex subscription and performs a cloud Agent turn"]
async fn live_agent_http_job_round_trip() {
    let provider = Arc::new(CodexAppServerProvider::new(
        "codex-agent",
        "codex",
        vec!["app-server".into(), "--listen".into(), "stdio://".into()],
        std::collections::BTreeSet::from(["gpt-6-sol".into()]),
    ));
    let app = test_router_with_providers(
        true,
        BTreeMap::from([(
            "codex-agent".into(),
            provider as infer_provider::DynProvider,
        )]),
    );
    let response = app
        .clone()
        .oneshot(request("infer.agent.task@20260925.1", valid_body()))
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["state"], "completed");
    assert_eq!(result["outputs"][0]["path"], "draft.txt");
    assert_eq!(result["provenance"]["provider"], "codex-agent");
}
