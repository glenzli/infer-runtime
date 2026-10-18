use crate::{contract, router};
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
    MediaEmbeddingKind, MediaEmbeddingProcessing, MediaEmbeddingRequest, RuntimeConfig,
};
use infer_provider::{
    DynMediaEmbeddingExecutor, MediaBuildContract, MediaEmbeddingExecutionOutput,
    MediaEmbeddingExecutor, ProviderError,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const TOKEN: &str = "media-contract-token";
struct FakeMediaExecutor {
    contract: MediaBuildContract,
    invalid: bool,
}
#[async_trait]
impl MediaEmbeddingExecutor for FakeMediaExecutor {
    async fn embed_media(
        &self,
        _model: &str,
        request: MediaEmbeddingRequest,
        _cancellation: CancellationToken,
    ) -> Result<MediaEmbeddingExecutionOutput, ProviderError> {
        let kind = request.kind();
        let mut embedding = vec![0.0; 768];
        embedding[0] = if self.invalid { f32::NAN } else { 1.0 };
        Ok(MediaEmbeddingExecutionOutput {
            embedding,
            processing: MediaEmbeddingProcessing {
                input_tokens: 260,
                decoded_seconds: matches!(
                    kind,
                    MediaEmbeddingKind::Audio | MediaEmbeddingKind::Video
                )
                .then_some(5.0),
                video_frames: (kind == MediaEmbeddingKind::Video).then_some(5),
                video_audio_included: false,
            },
            provenance: self.contract.clone(),
        })
    }
}

fn service(allowed: bool, invalid: bool) -> Router {
    service_with_executor(allowed, invalid, true)
}

fn service_with_executor(allowed: bool, invalid: bool, available: bool) -> Router {
    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    if !allowed {
        let operator = config.apps.get_mut("local-operator").unwrap();
        operator.allow_all_intents = false;
        operator.allowed_intents = Some(vec!["semantic.embed_query".into()]);
    }
    config.validate().unwrap();
    let build = &config.model_builds["embeddinggemma2_bf16_mlx_1a4ffddb"];
    let worker = build.local_worker.as_ref().unwrap();
    let contract = MediaBuildContract {
        model_path: "/private/model/not-public".into(),
        model_build: "embeddinggemma2_bf16_mlx_1a4ffddb".into(),
        model_revision: build.provenance.source_revision.clone().unwrap(),
        artifact_set_sha256: worker.artifact_set_sha256.clone(),
        embedding_space: worker.embedding_space.clone().unwrap(),
        runtime: worker.runtime.clone(),
        precision: worker.precision.clone(),
        preprocessing_identity: worker.preprocessing_identity.clone().unwrap(),
        postprocessing_identity: worker.postprocessing_identity.clone(),
        tokenizer_identity: worker.tokenizer_identity.clone().unwrap(),
        instruction_revision: worker.instruction_revision.clone().unwrap(),
    };
    let credentials = AppCredentials::from_pairs([("local-operator", TOKEN)]).unwrap();
    let executor = Arc::new(FakeMediaExecutor { contract, invalid }) as DynMediaEmbeddingExecutor;
    let executors = if available {
        BTreeMap::from([("embeddinggemma2-local".into(), executor)])
    } else {
        BTreeMap::new()
    };
    router(
        Runtime::with_providers_and_credentials(config, BTreeMap::new(), credentials)
            .with_media_embedding_executors(executors),
    )
}

fn query(body: Value, capability: &str) -> Request<Body> {
    Request::post("/infer/v1/media/query-embeddings")
        .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
        .header(contract::CAPABILITY_CONTRACT_HEADER, capability)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}
async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn missing_media_executor_returns_service_unavailable() {
    let app = service_with_executor(true, false, false);
    let response = app
        .clone()
        .oneshot(query(
            json!({"model":"semantic.media_query","text":"hello","query_revision":"q1"}),
            "infer.media.embedding@20261013.1",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn media_query_negotiates_capability_checks_acl_and_keeps_payload_out_of_jobs() {
    let app = service(true, false);
    let response = app
        .clone()
        .oneshot(query(
            json!({"model":"semantic.media_query","text":"私人检索词","query_revision":"q-v1"}),
            "infer.media.embedding@20261013.1",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body(response).await;
    assert_eq!(value["embedding"].as_array().unwrap().len(), 768);
    assert_eq!(value["input_kind"], "text");
    assert_eq!(value["provenance"]["provider"], "embeddinggemma2-local");
    assert!(!value.to_string().contains("/private/model"));
    let job = Request::get(format!("/infer/v1/jobs/{}", value["id"].as_str().unwrap()))
        .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let job_response = app.oneshot(job).await.unwrap();
    assert_eq!(job_response.status(), StatusCode::OK);
    let job = body(job_response).await.to_string();
    assert!(!job.contains("私人检索词"));
    assert!(!job.contains("q-v1"));
    assert!(!job.contains("\"embedding\":["));
    let denied = service(false, false)
        .oneshot(query(
            json!({"model":"semantic.media_query","text":"hello","query_revision":"q1"}),
            "infer.media.embedding@20261013.1",
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn media_query_rejects_wrong_contract_paths_relaxed_placement_and_invalid_vectors() {
    let data = json!({"model":"semantic.media_query","text":"hello","query_revision":"q1"});
    let wrong = service(true, false)
        .oneshot(query(data.clone(), "infer.text.embedding@20260812.1"))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UPGRADE_REQUIRED);
    for extension in [
        json!({"path":"/private/file"}),
        json!({"metadata":{"infer.placement":"anywhere"}}),
    ] {
        let mut data = data.clone();
        data.as_object_mut()
            .unwrap()
            .extend(extension.as_object().unwrap().clone());
        let reply = service(true, false)
            .oneshot(query(data, "infer.media.embedding@20261013.1"))
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
    }
    let invalid = service(true, true)
        .oneshot(query(data, "infer.media.embedding@20261013.1"))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn image_audio_video_routes_share_one_space_and_reject_wrong_file_types() {
    let mut space = None;
    for (kind, mime) in [
        ("image", "image/png"),
        ("audio", "audio/wav"),
        ("video", "video/mp4"),
    ] {
        let boundary = "media-test-boundary";
        let orientation = if kind == "image" {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image_orientation\"\r\n\r\ndisplay_pixels_orientation_normalized\r\n"
            )
        } else {
            String::new()
        };
        let form = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nsemantic.media_{kind}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\nsource1\r\n{orientation}--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"private.bin\"\r\nContent-Type: {mime}\r\n\r\nbytes\r\n--{boundary}--\r\n"
        );
        let make = |form: String| {
            Request::post(format!("/infer/v1/media/{kind}-embeddings"))
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(
                    contract::CAPABILITY_CONTRACT_HEADER,
                    "infer.media.embedding@20261013.1",
                )
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(form))
                .unwrap()
        };
        let response = service(true, false)
            .oneshot(make(form.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = body(response).await;
        assert_eq!(value["input_kind"], kind);
        let current = value["embedding_space"]["identity"]
            .as_str()
            .unwrap()
            .to_owned();
        if let Some(previous) = &space {
            assert_eq!(previous, &current);
        } else {
            space = Some(current);
        }
        let wrong = service(true, false)
            .oneshot(make(form.replace(mime, "text/plain")))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    }
}
