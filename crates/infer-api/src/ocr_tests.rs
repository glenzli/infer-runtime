use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use image::{ImageFormat, RgbImage};
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::{
    DocumentOcrRequest, ImageGeometry, OcrTextLine, Point, RuntimeConfig,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use infer_provider::{
    DynOcrExecutor, OcrBuildContract, OcrExecutionOutput, OcrExecutor, ProviderError,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::{contract, router};

const TOKEN: &str = "ocr-contract-token";

struct FakeOcrExecutor;

#[async_trait]
impl OcrExecutor for FakeOcrExecutor {
    fn id(&self) -> &str {
        "pp-ocr-local"
    }

    async fn recognize(
        &self,
        _physical_model: &str,
        _request: DocumentOcrRequest,
        _cancellation: CancellationToken,
    ) -> Result<OcrExecutionOutput, ProviderError> {
        Ok(OcrExecutionOutput {
            image: ImageGeometry {
                width: 16,
                height: 8,
                orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
            },
            lines: vec![OcrTextLine {
                polygon: [
                    Point { x: 1.0, y: 1.0 },
                    Point { x: 14.0, y: 1.0 },
                    Point { x: 14.0, y: 6.0 },
                    Point { x: 1.0, y: 6.0 },
                ],
                text: "private-recognized-marker".into(),
                confidence: 0.99,
            }],
            provenance: OcrBuildContract {
                model_build: "test-pp-ocrv6-build".into(),
                detection_model: "/private/detection/path".into(),
                detection_revision: "det-revision".into(),
                detection_artifact_sha256: "det-digest".into(),
                recognition_model: "/private/recognition/path".into(),
                recognition_revision: "rec-revision".into(),
                recognition_artifact_sha256: "rec-digest".into(),
                preprocessing_identity: "test-preprocess-v1".into(),
                postprocessing_identity: "test-postprocess-v1".into(),
                runtime: "fake-onnxruntime".into(),
                requested_execution_provider: "cpu".into(),
                actual_execution_provider: "cpu".into(),
                precision: "fp32".into(),
            },
        })
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
        "pp-ocr-local".into(),
        Arc::new(FakeOcrExecutor) as DynOcrExecutor,
    )]);
    router(Runtime::with_local_worker_executors(
        config,
        BTreeMap::new(),
        credentials,
        BTreeMap::new(),
        executors,
    ))
}

fn png() -> Vec<u8> {
    let image = RgbImage::from_pixel(16, 8, image::Rgb([255, 255, 255]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image.write_to(&mut cursor, ImageFormat::Png).unwrap();
    cursor.into_inner()
}

fn multipart(image: Vec<u8>, orientation: &str, extra_field: bool) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ndocument.ocr\r\n",
    );
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"source_revision\"\r\n\r\ndocument:1\r\n",
    );
    body.extend_from_slice(
        format!("--infer-boundary\r\nContent-Disposition: form-data; name=\"image_orientation\"\r\n\r\n{orientation}\r\n").as_bytes(),
    );
    if extra_field {
        body.extend_from_slice(
            b"--infer-boundary\r\nContent-Disposition: form-data; name=\"tensor_map\"\r\n\r\nforbidden\r\n",
        );
    }
    body.extend_from_slice(
        b"--infer-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"document.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    body.extend_from_slice(&image);
    body.extend_from_slice(b"\r\n--infer-boundary--\r\n");
    body
}

fn request(body: Vec<u8>) -> Request<Body> {
    Request::post("/infer/v1/documents/ocr")
        .header(contract::CONSUMER_CORE_HEADER, contract::CORE_CONTRACT)
        .header(
            contract::CAPABILITY_CONTRACT_HEADER,
            "infer.document.ocr@20260812.1",
        )
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=infer-boundary",
        )
        .body(Body::from(body))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ocr_traverses_http_auth_job_and_typed_executor() {
    let response = service(None)
        .oneshot(request(multipart(
            png(),
            VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
            false,
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["object"], "document.ocr");
    assert_eq!(body["source_revision"], "document:1");
    assert_eq!(body["image"]["width"], 16);
    assert_eq!(body["image"]["height"], 8);
    assert_eq!(body["lines"][0]["text"], "private-recognized-marker");
    assert_eq!(body["lines"][0]["confidence"], 0.99);
    assert_eq!(body["provenance"]["model_build"], "test-pp-ocrv6-build");
    assert!(!body.to_string().contains("/private/"));
}

#[tokio::test]
async fn ocr_multipart_and_orientation_are_strict() {
    let unknown = service(None)
        .clone()
        .oneshot(request(multipart(
            png(),
            VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
            true,
        )))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    let wrong_orientation = service(None)
        .oneshot(request(multipart(
            png(),
            "input_pixels_no_exif_transform",
            false,
        )))
        .await
        .unwrap();
    assert_eq!(wrong_orientation.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ocr_intent_acl_is_independent_from_qwen_vl() {
    let denied = service(Some(&["vision.describe_image"]))
        .oneshot(request(multipart(
            png(),
            VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
            false,
        )))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(denied).await["error"]["code"], "intent_forbidden");
}
