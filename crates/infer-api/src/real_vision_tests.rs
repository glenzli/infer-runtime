//! Explicit real-provider contract tests. They stay ignored in the portable
//! suite because they require this host's pinned ONNX Runtime and artifacts.

use std::{io::Cursor, path::Path};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use image::{ImageFormat, RgbImage};
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{OnnxExecutionProvider, RuntimeConfig};
use tower::ServiceExt;

use crate::router;

#[tokio::test]
#[ignore = "requires the pinned local ONNX Runtime and YuNet artifact"]
async fn face_detection_traverses_auth_http_job_attempt_and_cpu_provider() {
    let (service, token, _temporary) = real_service(&["vision.detect_faces"]).await;

    let image = RgbImage::from_pixel(320, 240, image::Rgb([127, 127, 127]));
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, ImageFormat::Png).unwrap();
    let body = multipart(encoded.into_inner());
    let response = service
        .clone()
        .oneshot(
            Request::post("/infer/v1/vision/face-detections")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=infer-boundary",
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["object"], "vision.face_detection");
    assert_eq!(body["source_revision"], "photo:test:1");
    assert_eq!(body["provenance"]["actual_execution_provider"], "cpu");
    assert_eq!(body["provenance"]["model_build"], "yunet_2026may_onnx");
    assert_eq!(body["image"]["width"], 320);
    assert_eq!(body["image"]["height"], 240);
}

#[tokio::test]
#[ignore = "requires the pinned local ONNX Runtime and SFace artifact"]
async fn face_embedding_traverses_auth_acl_job_attempt_and_cpu_provider() {
    let (service, token, _temporary) = real_service(&["vision.embed_face"]).await;
    let image = RgbImage::from_fn(112, 112, |x, y| {
        image::Rgb([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8])
    });
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, ImageFormat::Png).unwrap();
    let encoded = encoded.into_inner();
    let response = service
        .clone()
        .oneshot(
            Request::post("/infer/v1/vision/face-embeddings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=infer-boundary",
                )
                .body(Body::from(embedding_multipart(encoded.clone())))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["object"], "vision.face_embedding");
    assert_eq!(body["data_classification"], "sensitive_biometric");
    assert_eq!(body["embedding"]["dimensions"], 128);
    assert_eq!(body["embedding"]["values"].as_array().unwrap().len(), 128);
    assert_eq!(body["embedding"]["normalized"], true);
    assert_eq!(body["embedding"]["distance_metric"], "cosine");
    assert_eq!(
        body["embedding"]["space"],
        "sface_2021dec_onnx:0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79:l2_normalized_embedding_cosine_space_v1"
    );
    assert_eq!(body["provenance"]["actual_execution_provider"], "cpu");
    assert_eq!(body["provenance"]["model_build"], "sface_2021dec_onnx");
    assert_eq!(body["eligibility"]["eligible"], true);

    let job_id = body["id"].as_str().unwrap();
    let job = service
        .clone()
        .oneshot(
            Request::get(format!("/infer/v1/jobs/{job_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::OK);
    let job = job.into_body().collect().await.unwrap().to_bytes();
    let job: serde_json::Value = serde_json::from_slice(&job).unwrap();
    assert!(job.get("embedding").is_none());
    assert!(job.get("image").is_none());

    let denied = service
        .oneshot(
            Request::post("/infer/v1/vision/face-detections")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=infer-boundary",
                )
                .body(Body::from(multipart(encoded)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let denied = denied.into_body().collect().await.unwrap().to_bytes();
    let denied: serde_json::Value = serde_json::from_slice(&denied).unwrap();
    assert_eq!(denied["error"]["code"], "intent_forbidden");
}

#[tokio::test]
#[ignore = "requires pinned SigLIP 2 image/text ONNX Builds and tokenizer"]
async fn siglip_image_and_text_share_one_normalized_space_through_http() {
    let (service, token, _temporary) =
        real_service(&["vision.embed_image", "vision.embed_text"]).await;
    let image = RgbImage::from_fn(320, 240, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, ImageFormat::Png).unwrap();
    let image_response = service
        .clone()
        .oneshot(
            Request::post("/infer/v1/vision/image-embeddings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=infer-boundary",
                )
                .body(Body::from(image_embedding_multipart(encoded.into_inner())))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(image_response.status(), StatusCode::OK);
    let image_body = image_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let image_body: serde_json::Value = serde_json::from_slice(&image_body).unwrap();
    assert_eq!(image_body["object"], "vision.image_embedding");
    assert_eq!(image_body["embedding"]["dimensions"], 768);
    assert_eq!(image_body["embedding"]["normalized"], true);
    assert_eq!(
        image_body["embedding"]["values"].as_array().unwrap().len(),
        768
    );
    assert_eq!(image_body["image"]["width"], 320);
    assert_eq!(
        image_body["image"]["orientation"],
        "display_pixels_orientation_normalized"
    );
    assert_eq!(
        image_body["provenance"]["model_build"],
        "siglip2_base_patch16_224_image_onnx_cpu_v1"
    );
    assert!(image_body["provenance"].get("tokenizer").is_none());

    let text_response = service
        .clone()
        .oneshot(
            Request::post("/infer/v1/vision/text-embeddings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "model": "vision.embed_text",
                        "text": "北京的日落",
                        "query_revision": "query:test:zh:1",
                        "language": "zh-CN"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(text_response.status(), StatusCode::OK);
    let text_body = text_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let text_body: serde_json::Value = serde_json::from_slice(&text_body).unwrap();
    assert_eq!(text_body["object"], "vision.text_embedding");
    assert_eq!(text_body["query_revision"], "query:test:zh:1");
    assert_eq!(text_body["language"], "zh-CN");
    assert_eq!(text_body["embedding"]["dimensions"], 768);
    assert_eq!(text_body["embedding"]["normalized"], true);
    assert_eq!(
        text_body["embedding"]["space"],
        image_body["embedding"]["space"]
    );
    assert_eq!(
        text_body["provenance"]["model_build"],
        "siglip2_base_patch16_224_text_onnx_cpu_v1"
    );
    assert_eq!(
        text_body["provenance"]["tokenizer"]["artifact_sha256"],
        "cb9140fae3ac5122c972d37adf83e1248471a38147ad76f8215c8872c6fd8322"
    );

    for job_id in [image_body["id"].as_str(), text_body["id"].as_str()] {
        let job = service
            .clone()
            .oneshot(
                Request::get(format!("/infer/v1/jobs/{}", job_id.unwrap()))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let job = job.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&job).unwrap();
        assert!(job.get("embedding").is_none());
        assert!(job.get("image").is_none());
        assert!(job.get("text").is_none());
    }
}

#[tokio::test]
#[ignore = "requires pinned SigLIP 2 Build and macOS Core ML execution provider"]
async fn siglip_coreml_route_is_used_or_discloses_cpu_fallback() {
    let (service, token, _temporary) = real_service_with_execution_providers(
        &["vision.embed_image"],
        vec![OnnxExecutionProvider::Coreml, OnnxExecutionProvider::Cpu],
        true,
    )
    .await;
    let image = RgbImage::from_pixel(224, 224, image::Rgb([80, 120, 160]));
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, ImageFormat::Png).unwrap();
    let response = service
        .oneshot(
            Request::post("/infer/v1/vision/image-embeddings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=infer-boundary",
                )
                .body(Body::from(image_embedding_multipart(encoded.into_inner())))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["provenance"]["requested_execution_provider"], "coreml");
    match body["provenance"]["actual_execution_provider"].as_str() {
        Some("coreml") => {
            assert!(body["provenance"]["execution_provider_fallback_reason"].is_null())
        }
        Some("cpu") => assert_eq!(
            body["provenance"]["execution_provider_fallback_reason"],
            "requested_execution_provider_unavailable_for_build"
        ),
        other => panic!("unexpected actual execution provider: {other:?}"),
    }
}

async fn real_service(allowed_intents: &[&str]) -> (axum::Router, String, tempfile::TempDir) {
    real_service_with_execution_providers(allowed_intents, vec![OnnxExecutionProvider::Cpu], false)
        .await
}

async fn real_service_with_execution_providers(
    allowed_intents: &[&str],
    execution_providers: Vec<OnnxExecutionProvider>,
    allow_cpu_fallback: bool,
) -> (axum::Router, String, tempfile::TempDir) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut config = RuntimeConfig::load(repository.join("config/infer.toml")).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    config.persistence.path = temporary
        .path()
        .join("runtime.sqlite3")
        .to_string_lossy()
        .into_owned();
    config.auth.managed_credentials_directory = temporary
        .path()
        .join("credentials")
        .to_string_lossy()
        .into_owned();
    config.background.payload_directory = temporary
        .path()
        .join("payloads")
        .to_string_lossy()
        .into_owned();
    config.runtimes.onnx.preferred_execution_providers = execution_providers;
    config.runtimes.onnx.allow_cpu_fallback = allow_cpu_fallback;
    if config.runtimes.onnx.preferred_execution_providers.first()
        == Some(&OnnxExecutionProvider::Coreml)
    {
        config
            .model_builds
            .get_mut("siglip2_base_patch16_224_image_onnx_cpu_v1")
            .unwrap()
            .onnx
            .as_mut()
            .unwrap()
            .allowed_execution_providers =
            vec![OnnxExecutionProvider::Coreml, OnnxExecutionProvider::Cpu];
    }
    config
        .apps
        .get_mut("local-operator")
        .unwrap()
        .allowed_intents = Some(
        allowed_intents
            .iter()
            .map(|intent| (*intent).into())
            .collect(),
    );
    let credentials = AppCredentials::load_or_create(&config).unwrap();
    let token = credentials.token_for("local-operator").unwrap().to_owned();
    let runtime = Runtime::from_config(config).await.unwrap();
    (router(runtime), token, temporary)
}

fn image_embedding_multipart(image: Vec<u8>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nvision.embed_image\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\nphoto:test:semantic:1\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"image_orientation\"\r\n\r\ndisplay_pixels_orientation_normalized\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"semantic.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    body.extend_from_slice(&image);
    body.extend_from_slice(b"\r\n--infer-boundary--\r\n");
    body
}

fn multipart(image: Vec<u8>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nvision.detect_faces\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\nphoto:test:1\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"blank.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    body.extend_from_slice(&image);
    body.extend_from_slice(b"\r\n--infer-boundary--\r\n");
    body
}

fn embedding_multipart(image: Vec<u8>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nvision.embed_face\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\nphoto:test:embedding:1\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"landmarks\"\r\n\r\n{\"right_eye\":{\"x\":38.2946,\"y\":51.6963},\"left_eye\":{\"x\":73.5318,\"y\":51.5014},\"nose_tip\":{\"x\":56.0252,\"y\":71.7366},\"right_mouth_corner\":{\"x\":41.5493,\"y\":92.3655},\"left_mouth_corner\":{\"x\":70.7299,\"y\":92.2041}}\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"synthetic.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    body.extend_from_slice(&image);
    body.extend_from_slice(b"\r\n--infer-boundary--\r\n");
    body
}
