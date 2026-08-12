use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{
    RetrievalEmbeddingRequest, RetrievalRerankRequest, RetrievalRerankResult, RuntimeConfig,
};
use infer_provider::{
    DynRetrievalExecutor, ProviderError, RetrievalBuildContract, RetrievalEmbeddingExecutionOutput,
    RetrievalEmbeddingKind, RetrievalExecutor, RetrievalRerankExecutionOutput,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::{contract, router};

const TOKEN: &str = "retrieval-contract-token";

struct FakeRetrievalExecutor;

#[async_trait]
impl RetrievalExecutor for FakeRetrievalExecutor {
    fn id(&self) -> &str {
        "qwen3-retrieval-local"
    }

    async fn embed(
        &self,
        _physical_model: &str,
        request: RetrievalEmbeddingRequest,
        kind: RetrievalEmbeddingKind,
        _cancellation: CancellationToken,
    ) -> Result<RetrievalEmbeddingExecutionOutput, ProviderError> {
        Ok(RetrievalEmbeddingExecutionOutput {
            embeddings: request.inputs.iter().map(|_| vec![1.0, 0.0, 0.0]).collect(),
            dimensions: 3,
            normalized: true,
            instruction_revision: matches!(kind, RetrievalEmbeddingKind::Query)
                .then(|| "test-query-template-v1".into()),
            provenance: build_contract(Some("test-space-v1")),
        })
    }

    async fn rerank(
        &self,
        _physical_model: &str,
        request: RetrievalRerankRequest,
        _cancellation: CancellationToken,
    ) -> Result<RetrievalRerankExecutionOutput, ProviderError> {
        let results = request
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| RetrievalRerankResult {
                candidate_id: candidate.id.clone(),
                source_revision: candidate.source_revision.clone(),
                score: 0.9 - index as f32 * 0.2,
                rank: index + 1,
            })
            .collect();
        Ok(RetrievalRerankExecutionOutput {
            results,
            instruction_revision: "test-rerank-template-v1".into(),
            score_semantics: "model_relevance_not_calibrated_probability".into(),
            provenance: build_contract(None),
        })
    }
}

fn build_contract(embedding_space: Option<&str>) -> RetrievalBuildContract {
    RetrievalBuildContract {
        model_path: "/private/model/path-never-published".into(),
        model_build: "test-qwen3-retrieval-build".into(),
        model_revision: "test-model-revision".into(),
        artifact_sha256: "test-artifact-digest".into(),
        tokenizer_identity: "test-tokenizer".into(),
        runtime: "fake-mlx-runtime".into(),
        precision: "fp32".into(),
        embedding_space: embedding_space.map(str::to_owned),
        embedding_dimensions: embedding_space.map(|_| 3),
    }
}

fn service(allowed_intents: Option<&[&str]>) -> Router {
    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    if let Some(allowed_intents) = allowed_intents {
        let operator = config.apps.get_mut("local-operator").unwrap();
        operator.allow_all_intents = false;
        operator.allowed_intents = Some(
            allowed_intents
                .iter()
                .map(|intent| (*intent).to_owned())
                .collect(),
        );
    }
    config.validate().unwrap();
    let credentials = AppCredentials::from_pairs([("local-operator", TOKEN)]).unwrap();
    let executors = BTreeMap::from([(
        "qwen3-retrieval-local".into(),
        Arc::new(FakeRetrievalExecutor) as DynRetrievalExecutor,
    )]);
    router(Runtime::with_local_worker_executors(
        config,
        BTreeMap::new(),
        credentials,
        executors,
        BTreeMap::new(),
    ))
}

fn request(path: &str, capability: &'static str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
        .header(contract::CAPABILITY_CONTRACT_HEADER, capability)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn query_embedding_traverses_http_auth_job_and_typed_executor() {
    let service = service(None);
    let response = service
        .clone()
        .oneshot(request(
            "/infer/v1/text/query-embeddings",
            "infer.text.embedding@20260812.1",
            json!({
                "model": "semantic.embed_query",
                "inputs": [{
                    "id": "query",
                    "text": "private-query-marker",
                    "source_revision": "query:1"
                }]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["object"], "text.embedding");
    assert_eq!(body["data"][0]["id"], "query");
    assert_eq!(body["data"][0]["source_revision"], "query:1");
    assert_eq!(body["data"][0]["embedding"]["dimensions"], 3);
    assert_eq!(body["data"][0]["embedding"]["space"], "test-space-v1");
    assert_eq!(
        body["provenance"]["instruction_revision"],
        "test-query-template-v1"
    );
    assert_eq!(
        body["provenance"]["model_build"],
        "test-qwen3-retrieval-build"
    );
    assert!(!body.to_string().contains("/private/model/path"));

    let job_id = body["id"].as_str().unwrap();
    let job = service
        .oneshot(
            Request::get(format!("/infer/v1/jobs/{job_id}"))
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::OK);
    let job = json_body(job).await;
    assert_eq!(job["intent"], "semantic.embed_query");
    assert_eq!(
        job["capability_contract"],
        "infer.text.embedding@20260812.1"
    );
    let serialized = job.to_string();
    assert!(!serialized.contains("private-query-marker"));
    assert!(!serialized.contains("/private/model/path"));
}

#[tokio::test]
async fn rerank_preserves_candidate_identity_without_publishing_text() {
    let response = service(None)
        .oneshot(request(
            "/infer/v1/text/rerank",
            "infer.text.rerank@20260812.1",
            json!({
                "model": "semantic.rerank",
                "query": {"id":"q","text":"private-query-marker","source_revision":"q:1"},
                "candidates": [
                    {"id":"a","text":"private-document-marker-a","source_revision":"a:1"},
                    {"id":"b","text":"private-document-marker-b","source_revision":"b:1"}
                ],
                "top_n": 1
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["object"], "text.rerank");
    assert_eq!(body["query_revision"], "q:1");
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["candidate_id"], "a");
    assert_eq!(body["results"][0]["source_revision"], "a:1");
    assert!(!body.to_string().contains("private-"));
}

#[tokio::test]
async fn retrieval_contract_and_acl_fail_closed() {
    let missing_capability = service(None)
        .clone()
        .oneshot(
            Request::post("/infer/v1/text/query-embeddings")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "semantic.embed_query",
                        "inputs": [{"id":"q","text":"x","source_revision":"q:1"}]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_capability.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(
        json_body(missing_capability).await["error"]["code"],
        "capability_contract_unsupported"
    );

    let denied = service(Some(&["semantic.embed_query"]))
        .oneshot(request(
            "/infer/v1/text/rerank",
            "infer.text.rerank@20260812.1",
            json!({
                "model": "semantic.rerank",
                "query": {"id":"q","text":"q","source_revision":"q:1"},
                "candidates": [{"id":"a","text":"a","source_revision":"a:1"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(denied).await["error"]["code"], "intent_forbidden");
}

#[tokio::test]
async fn retrieval_json_is_strict_and_cannot_relax_local_execution() {
    let unknown = service(None)
        .clone()
        .oneshot(request(
            "/infer/v1/text/query-embeddings",
            "infer.text.embedding@20260812.1",
            json!({
                "model": "semantic.embed_query",
                "inputs": [{"id":"q","text":"q","source_revision":"q:1"}],
                "unknown": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    let remote = service(None)
        .oneshot(request(
            "/infer/v1/text/query-embeddings",
            "infer.text.embedding@20260812.1",
            json!({
                "model": "semantic.embed_query",
                "inputs": [{"id":"q","text":"q","source_revision":"q:1"}],
                "metadata": {"infer.placement": "cloud_only"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(remote.status(), StatusCode::BAD_REQUEST);
}
