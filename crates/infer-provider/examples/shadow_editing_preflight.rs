//! Verify strict CUDA session admission for the configured Shadow ONNX Builds.
//! This uses Artifact Store integrity checks and never runs image inference.
use std::{collections::BTreeMap, path::Path};

use infer_artifact::ArtifactStore;
use infer_core::RuntimeConfig;
use infer_provider::OnnxProviderRuntime;
use infer_resource::NativeModelController;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("config path required")?;
    let config = RuntimeConfig::load(Path::new(&path))?;
    let store = ArtifactStore::from_config(&config.artifacts)?;
    let mut failed = false;
    for name in [
        "yunet_2026may_onnx_cuda_v1",
        "grounding_dino_tiny_onnx_cuda_static_fp32_v1",
        "bisenet_resnet18_face_parsing_onnx_cuda_static_v1",
        "lama_inpainting_onnx_cuda_static_fp32_v1",
    ] {
        let build = config
            .model_builds
            .get(name)
            .ok_or("missing fixed Build")?
            .clone();
        let model = build.model_id.clone();
        let runtime = OnnxProviderRuntime::new(
            "editing-preflight",
            config.runtimes.onnx.clone(),
            store.clone(),
            BTreeMap::from([(model.clone(), (name.into(), build))]),
        )?;
        match NativeModelController::load(runtime.as_ref(), &model).await {
            Ok(()) => println!("{name}: native CUDA session admitted"),
            Err(error) => {
                failed = true;
                println!("{name}: {error:?}");
            }
        }
        NativeModelController::unload(runtime.as_ref(), &model).await?;
    }
    if failed {
        Err("one or more Builds failed strict CUDA admission".into())
    } else {
        Ok(())
    }
}
