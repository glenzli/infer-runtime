use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
use infer_provider::{AudioWorkerExecutor, DynAudioExecutor};
use serde_json::Value;
use tower::ServiceExt;

use crate::contract;
use crate::router;

#[tokio::test]
#[ignore = "requires INFER_YAMNET_PYTHON, INFER_YAMNET_MODEL, TensorFlow 2.20, and ffmpeg"]
async fn real_yamnet_http_acceptance_records_echo_job_provenance() {
    let python = std::env::var("INFER_YAMNET_PYTHON").expect("set INFER_YAMNET_PYTHON");
    let model = std::env::var("INFER_YAMNET_MODEL").expect("set INFER_YAMNET_MODEL");
    let worker = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../workers/yamnet_audio_worker.py")
        .canonicalize()
        .unwrap();

    let mut config: RuntimeConfig =
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
    config
        .model_builds
        .get_mut("yamnet_tfhub_v1_tensorflow_2_20")
        .unwrap()
        .model_id = model;
    let mut echo = config.apps["example-local-consumer"].clone();
    echo.allowed_intents = Some(vec!["audio.detect_events".into()]);
    config.apps.insert("echo".into(), echo);
    config.validate().unwrap();

    let credentials = AppCredentials::from_pairs([("echo", "real-yamnet-token")]).unwrap();
    let executors = BTreeMap::from([(
        "yamnet-local".into(),
        Arc::new(AudioWorkerExecutor::new(
            "yamnet-local",
            python,
            vec![worker.to_string_lossy().into_owned()],
        )) as DynAudioExecutor,
    )]);
    let app = router(Runtime::with_audio_executors(
        config,
        BTreeMap::new(),
        credentials,
        executors,
    ));

    let boundary = "real-yamnet-event";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio.detect_events\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"noise.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    )
    .into_bytes();
    body.extend(white_noise_wav(2));
    body.extend(format!("\r\n--{boundary}--\r\n").into_bytes());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/audio/event-detections")
                .header(header::AUTHORIZATION, "Bearer real-yamnet-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .header(
                    contract::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.event-detection@20260813.1",
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
    let event = body_json(response).await;
    assert_eq!(event["model"], "audio.detect_events");
    assert_eq!(event["object"], "audio.event_detection");
    assert_eq!(event["coverage"]["status"], "full");
    assert_eq!(event["coverage"]["window_count"], 4);
    assert_eq!(event["speech_presence"]["status"], "absent");
    assert!(event["events"].as_array().unwrap().len() >= 2);
    assert!(event["events"].as_array().unwrap().iter().all(|event| {
        event["class_id"]
            .as_str()
            .is_some_and(|class_id| class_id.starts_with("/m/"))
            && event["start_seconds"].as_f64().is_some()
            && event["end_seconds"].as_f64().is_some()
            && event["score"].as_f64().is_some()
    }));
    assert_eq!(event["provenance"]["model"], "google/yamnet/1");
    assert_eq!(
        event["provenance"]["model_archive_sha256"],
        "b80da2a1a56926fb0767205051a200dd7b3beaf3ea1ea126c42a53943996e5e0"
    );

    let job_id = event["id"].as_str().unwrap();
    let job = app
        .oneshot(
            Request::builder()
                .uri(format!("/infer/v1/jobs/{job_id}"))
                .header(header::AUTHORIZATION, "Bearer real-yamnet-token")
                .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::OK);
    let job = body_json(job).await;
    assert_eq!(job["app_id"], "echo");
    assert_eq!(job["provider"], "yamnet-local");
    assert_eq!(job["deployment"], "yamnet_audio_events_tfhub_v1");
    assert_eq!(job["model_profile"], "yamnet_tfhub_v1");
    assert_eq!(job["model_build"], "yamnet_tfhub_v1_tensorflow_2_20");
    assert_eq!(job["attempts"][0]["outcome"], "succeeded");
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn white_noise_wav(seconds: usize) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    let samples = SAMPLE_RATE as usize * seconds;
    let data_bytes = (samples * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend(b"RIFF");
    wav.extend((36 + data_bytes).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(SAMPLE_RATE.to_le_bytes());
    wav.extend((SAMPLE_RATE * 2).to_le_bytes());
    wav.extend(2_u16.to_le_bytes());
    wav.extend(16_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(data_bytes.to_le_bytes());
    let mut state = 0x1234_5678_u32;
    for _ in 0..samples {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let sample = ((state >> 16) as i16) / 2;
        wav.extend(sample.to_le_bytes());
    }
    wav
}
