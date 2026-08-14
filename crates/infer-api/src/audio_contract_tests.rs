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

#[async_trait]
impl AudioExecutor for ArrayLanguageTranscriptionExecutor {
    fn id(&self) -> &str {
        "mlx-audio-local"
    }

    async fn execute(
        &self,
        _physical_model: &str,
        request: AudioExecutionRequest,
    ) -> Result<AudioExecutionOutput, ProviderError> {
        assert!(matches!(request, AudioExecutionRequest::Transcription(_)));
        Ok(AudioExecutionOutput::Json(serde_json::json!({
            "text": "provider output",
            "language": ["Chinese", "English"],
            "segments": []
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
