//! Opt-in real-hardware parity check; run from the Runtime working directory.
//! Usage: shadow_cuda_parity <config.toml> <public-face.jpg>
//! CPU reference Builds live in an isolated temporary ArtifactStore.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use infer_artifact::ArtifactStore;
use infer_core::{
    FaceDetectionRequest, FaceEmbeddingRequest, ImageEmbeddingRequest, OnnxExecutionProvider,
    RuntimeConfig, TextEmbeddingRequest, VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS, VisionImage,
};
use infer_provider::{
    FaceDetectionExecutor, FaceEmbeddingExecutor, ImageEmbeddingExecutor, OnnxProviderRuntime,
    TextEmbeddingExecutor,
};
use infer_resource::NativeModelController;
use serde_json::json;
use tokio_util::sync::CancellationToken;

type CheckResult<T> = Result<T, Box<dyn std::error::Error>>;

fn runtimes(
    config: &RuntimeConfig,
    store: &ArtifactStore,
    reference: &ArtifactStore,
    build_id: &str,
) -> CheckResult<(Arc<OnnxProviderRuntime>, Arc<OnnxProviderRuntime>, String)> {
    let build = config.model_builds[build_id].clone();
    let model = build.model_id.clone();
    let onnx = build.onnx.as_ref().ok_or("missing ONNX manifest")?;
    let source = store.resolve_onnx(build_id, onnx)?;
    let reference_id = format!("parity_cpu_{build_id}");
    let mut cpu_build = build.clone();
    let cpu_onnx = cpu_build.onnx.as_mut().unwrap();
    cpu_onnx.allowed_execution_providers = vec![OnnxExecutionProvider::Cpu];
    reference.publish_onnx(&reference_id, &source, cpu_onnx)?;
    for name in onnx.auxiliary_artifacts.keys() {
        let auxiliary = store.resolve_onnx_auxiliary(build_id, name, onnx)?;
        reference.publish_onnx_auxiliary(&reference_id, name, auxiliary, cpu_onnx)?;
    }
    let mut cpu_config = config.runtimes.onnx.clone();
    cpu_config.preferred_execution_providers = vec![OnnxExecutionProvider::Cpu];
    cpu_config.allow_cpu_fallback = false;
    let cpu = OnnxProviderRuntime::new(
        "parity-cpu",
        cpu_config,
        reference.clone(),
        BTreeMap::from([(model.clone(), (reference_id, cpu_build))]),
    )?;
    let mut cuda_config = config.runtimes.onnx.clone();
    cuda_config.preferred_execution_providers = vec![OnnxExecutionProvider::Cuda];
    cuda_config.allow_cpu_fallback = false;
    let cuda = OnnxProviderRuntime::new(
        "parity-cuda",
        cuda_config,
        store.clone(),
        BTreeMap::from([(model.clone(), (build_id.to_owned(), build))]),
    )?;
    Ok((cpu, cuda, model))
}

fn cosine(reference: &[f32], actual: &[f32], minimum: f64) -> CheckResult<f64> {
    if reference.len() != actual.len() || reference.is_empty() {
        return Err("embedding dimensions differ".into());
    }
    let dot: f64 = reference
        .iter()
        .zip(actual)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    let norm = |values: &[f32]| {
        values
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum::<f64>()
            .sqrt()
    };
    let similarity = dot / (norm(reference) * norm(actual));
    if !similarity.is_finite() || similarity < minimum {
        return Err(format!("CPU/CUDA cosine {similarity} is below {minimum}").into());
    }
    Ok(similarity)
}

async fn release(
    cpu: Arc<OnnxProviderRuntime>,
    cuda: Arc<OnnxProviderRuntime>,
    model: &str,
) -> CheckResult<()> {
    NativeModelController::unload(cpu.as_ref(), model).await?;
    NativeModelController::unload(cuda.as_ref(), model).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> CheckResult<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: shadow_cuda_parity <config.toml> <public-face.jpg>".into());
    }
    let config = RuntimeConfig::load(Path::new(&args[0]))?;
    let store = ArtifactStore::from_config(&config.artifacts)?;
    let directory = tempfile::tempdir()?;
    let reference = ArtifactStore::at(directory.path())?;
    let image = VisionImage {
        content_type: "image/jpeg".into(),
        bytes: std::fs::read(&args[1])?,
    };
    let metadata = BTreeMap::new();
    let cancellation = CancellationToken::new;

    let (cpu, cuda, model) = runtimes(&config, &store, &reference, "yunet_2026may_onnx_cuda_v1")?;
    let request = FaceDetectionRequest {
        model: "vision.detect_faces".into(),
        image: image.clone(),
        source_revision: "public-fixture:parity:v1".into(),
        metadata: metadata.clone(),
    };
    let expected = cpu
        .detect_faces(&model, request.clone(), cancellation())
        .await?;
    let actual = cuda.detect_faces(&model, request, cancellation()).await?;
    assert_eq!(actual.provenance.actual_execution_provider, "cuda");
    assert_eq!(expected.detections.len(), actual.detections.len());
    assert!(
        !expected.detections.is_empty(),
        "use a public fixture containing a face"
    );
    for (a, b) in expected.detections.iter().zip(&actual.detections) {
        for (left, right) in [
            (a.bounding_box.x, b.bounding_box.x),
            (a.bounding_box.y, b.bounding_box.y),
            (a.bounding_box.width, b.bounding_box.width),
            (a.bounding_box.height, b.bounding_box.height),
        ] {
            assert!(
                (left - right).abs() < 0.5,
                "face coordinates drift by at least half a pixel"
            );
        }
        assert!((a.confidence - b.confidence).abs() < 0.001);
    }
    let landmarks = expected.detections[0].landmarks;
    println!(
        "{}",
        json!({"yunet":"CPU/CUDA parity passed","detections":actual.detections.len()})
    );
    release(cpu, cuda, &model).await?;

    let (cpu, cuda, model) = runtimes(&config, &store, &reference, "sface_2021dec_onnx_cuda_v1")?;
    let request = FaceEmbeddingRequest {
        model: "vision.embed_face".into(),
        image: image.clone(),
        landmarks,
        source_revision: "public-fixture:parity:v1".into(),
        metadata: metadata.clone(),
    };
    let expected = cpu
        .embed_face(&model, request.clone(), cancellation())
        .await?;
    let actual = cuda.embed_face(&model, request, cancellation()).await?;
    assert_eq!(actual.provenance.actual_execution_provider, "cuda");
    assert!(expected.eligibility.eligible && actual.eligibility.eligible);
    let similarity = cosine(&expected.embedding.values, &actual.embedding.values, 0.999)?;
    println!("{}", json!({"sface_cosine":similarity}));
    release(cpu, cuda, &model).await?;

    let (cpu, cuda, model) = runtimes(
        &config,
        &store,
        &reference,
        "siglip2_base_patch16_224_image_onnx_cuda_v1",
    )?;
    let request = ImageEmbeddingRequest {
        model: "semantic.embed_image".into(),
        image,
        source_revision: "public-fixture:parity:v1".into(),
        image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
        metadata: metadata.clone(),
    };
    let expected = cpu
        .embed_image(&model, request.clone(), cancellation())
        .await?;
    let actual = cuda.embed_image(&model, request, cancellation()).await?;
    assert_eq!(actual.provenance.actual_execution_provider, "cuda");
    assert_eq!(expected.embedding.space, actual.embedding.space);
    let similarity = cosine(&expected.embedding.values, &actual.embedding.values, 0.9999)?;
    println!("{}", json!({"siglip_image_cosine":similarity}));
    let space = actual.embedding.space;
    release(cpu, cuda, &model).await?;

    let (cpu, cuda, model) = runtimes(
        &config,
        &store,
        &reference,
        "siglip2_base_patch16_224_text_onnx_cuda_v1",
    )?;
    let request = TextEmbeddingRequest {
        model: "semantic.embed_text".into(),
        text: "a portrait of a person".into(),
        query_revision: "public-fixture:query:v1".into(),
        language: Some("en".into()),
        metadata,
    };
    let expected = cpu
        .embed_text(&model, request.clone(), cancellation())
        .await?;
    let actual = cuda.embed_text(&model, request, cancellation()).await?;
    assert_eq!(actual.provenance.actual_execution_provider, "cuda");
    assert_eq!(expected.embedding.space, actual.embedding.space);
    assert_eq!(space, actual.embedding.space);
    let similarity = cosine(&expected.embedding.values, &actual.embedding.values, 0.9999)?;
    println!("{}", json!({"siglip_text_cosine":similarity}));
    release(cpu, cuda, &model).await?;
    Ok(())
}
