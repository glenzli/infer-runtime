use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{AudioExecutionRequest, RuntimeConfig};
use infer_provider::{AudioExecutionOutput, AudioExecutor, DynAudioExecutor, ProviderError};
use tower::ServiceExt;

use crate::{contract, router};

struct ArrayLanguageTranscriptionExecutor;

struct ClapEmbeddingExecutor;

#[async_trait]
impl AudioExecutor for ArrayLanguageTranscriptionExecutor {
    fn id(&self) -> &str {
        "mlx-audio-local"
    }

    async fn execute(
        &self,
        _physical_model: &str,
        request: AudioExecutionRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<AudioExecutionOutput, ProviderError> {
        assert!(matches!(request, AudioExecutionRequest::Transcription(_)));
        Ok(AudioExecutionOutput::Json(serde_json::json!({
            "text": "provider output",
            "language": ["Chinese", "English"],
            "segments": []
        })))
    }
}

#[async_trait]
impl AudioExecutor for ClapEmbeddingExecutor {
    fn id(&self) -> &str {
        "clap-local"
    }

    async fn execute(
        &self,
        _physical_model: &str,
        request: AudioExecutionRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<AudioExecutionOutput, ProviderError> {
        assert!(matches!(
            request,
            AudioExecutionRequest::Embedding(_) | AudioExecutionRequest::TextEmbedding(_)
        ));
        Ok(AudioExecutionOutput::Json(serde_json::json!({
            "embedding": std::iter::once(1.0f32)
                .chain(std::iter::repeat_n(0.0f32, 511))
                .collect::<Vec<_>>(),
            "dimensions": 512,
            "normalized": true
        })))
    }
}

#[tokio::test]
async fn transcription_endpoint_normalizes_provider_language_arrays_for_official_sdk() {
    let config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    config.validate().unwrap();
    let runtime = Runtime::with_audio_executors(
        config,
        BTreeMap::new(),
        AppCredentials::from_pairs([("example-local-consumer", "transcription-contract-token")])
            .unwrap(),
        BTreeMap::from([(
            "mlx-audio-local".into(),
            Arc::new(ArrayLanguageTranscriptionExecutor) as DynAudioExecutor,
        )]),
    );
    let app = router(runtime);

    let boundary = "typed-transcription-language";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.transcribe\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.wav\"\r\nContent-Type: audio/wav\r\n\r\nnot-a-real-audio-file\r\n--{boundary}--\r\n"
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/audio/transcriptions")
                .header(header::AUTHORIZATION, "Bearer transcription-contract-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(
                    contract::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.transcription@20260814.1",
                )
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let transcription: infer_runtime_client::TranscriptionResponse =
        serde_json::from_slice(&bytes).expect("official SDK response type must deserialize");
    assert_eq!(
        transcription.extra.get("model"),
        Some(&serde_json::Value::String("audio.transcribe".into()))
    );
    assert_eq!(transcription.language, None);
    assert!(matches!(
        transcription.language_evidence,
        Some(infer_runtime_client::TranscriptionLanguageEvidence::InputSet {
            source: infer_runtime_client::TranscriptionLanguageEvidenceSource::ProviderReported,
            languages,
        }) if languages == vec!["Chinese".to_owned(), "English".to_owned()]
    ));
}

#[tokio::test]
async fn audio_embedding_endpoint_binds_build_provenance_and_sdk_shape() {
    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    let app_acl = config.apps.get_mut("example-local-consumer").unwrap();
    app_acl.allowed_intents = Some(vec!["audio.embed".into(), "audio.embed_text_query".into()]);
    config.validate().unwrap();
    let runtime = Runtime::with_audio_executors(
        config,
        BTreeMap::new(),
        AppCredentials::from_pairs([("example-local-consumer", "embedding-contract-token")])
            .unwrap(),
        BTreeMap::from([(
            "clap-local".into(),
            Arc::new(ClapEmbeddingExecutor) as DynAudioExecutor,
        )]),
    );
    let app = router(runtime);

    let text_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/audio/text-embeddings")
                .header(header::AUTHORIZATION, "Bearer embedding-contract-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(
                    contract::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.embedding@20260815.2",
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"audio.embed_text_query","text":"tone","query_revision":"query:1","language":"en"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(text_response.status(), StatusCode::OK);
    let text_bytes = text_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let text_embedding: infer_runtime_client::AudioEmbeddingResponse =
        serde_json::from_slice(&text_bytes).unwrap();
    assert_eq!(text_embedding.embedding.len(), 512);
    assert_eq!(text_embedding.query_revision.as_deref(), Some("query:1"));
    assert_eq!(
        text_embedding.provenance.build,
        "clap_htsat_unfused_8fa0f1c6d043_pytorch_mps"
    );
    assert_eq!(
        text_embedding.embedding_space.identity,
        "laion-clap-htsat-unfused@8fa0f1c6:audio-mono-48khz-10s:text-roberta-bpe:l2_512_fp32:mps:v1"
    );

    let boundary = "typed-audio-embedding";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.embed\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\nchunk:1\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.wav\"\r\nContent-Type: audio/wav\r\n\r\nbounded-fixture\r\n--{boundary}--\r\n"
    );
    let audio_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/audio/embeddings")
                .header(header::AUTHORIZATION, "Bearer embedding-contract-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(
                    contract::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.embedding@20260815.2",
                )
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(audio_response.status(), StatusCode::OK);
    let audio_embedding: infer_runtime_client::AudioEmbeddingResponse = serde_json::from_slice(
        &audio_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(audio_embedding.source_revision.as_deref(), Some("chunk:1"));
    assert!(audio_embedding.query_revision.is_none());
}
