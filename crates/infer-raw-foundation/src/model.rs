use std::{
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use ort::{
    session::{RunOptions, Session},
    value::{Tensor, ValueType},
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::RawFoundationError;

const GRAPH_BYTES: u64 = 31_056_425;
const GRAPH_SHA256: &str = "da27509dab6a2915da67e988acd86cf71f9d5bbc8d1aa0ed32933578a887b901";
const TILE_EDGE: usize = 512;
static ORT_LIBRARY: OnceLock<std::path::PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RawNindModelIdentity {
    pub model_id: String,
    pub exact_revision: String,
    pub graph_sha256: String,
    pub runtime: String,
    pub execution_provider: String,
    pub precision: String,
    pub implementation_revision: String,
    pub cache_identity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawNindTileReceipt {
    pub latency_ms: f64,
    pub output_values: usize,
    pub raw_output_mean: f64,
}

pub struct RawNindModel {
    session: Session,
    identity: RawNindModelIdentity,
}

impl RawNindModel {
    pub fn initialize_runtime(library: &Path) -> Result<(), RawFoundationError> {
        if let Some(existing) = ORT_LIBRARY.get() {
            return if existing == library {
                Ok(())
            } else {
                Err(RawFoundationError::ModelContract(format!(
                    "ONNX Runtime already initialized from {}",
                    existing.display()
                )))
            };
        }
        if !library.is_absolute() || !library.is_file() {
            return Err(RawFoundationError::ModelContract(
                "ONNX Runtime library must be an existing absolute file".into(),
            ));
        }
        ort::init_from(library)
            .map_err(native)?
            .with_name("infer-raw-foundation")
            .commit();
        let _ = ORT_LIBRARY.set(library.to_owned());
        Ok(())
    }

    pub fn open_legacy_cpu(
        graph: &Path,
        runtime_version: &str,
    ) -> Result<Self, RawFoundationError> {
        if runtime_version != "1.24.4" {
            return Err(RawFoundationError::ModelContract(format!(
                "legacy RawNIND Build requires ONNX Runtime 1.24.4, got {runtime_version}"
            )));
        }
        Self::open_cpu(
            graph,
            runtime_version,
            "rawnind-public-bayer-foundation-v1",
            "legacy-rawnind-1.24.4",
        )
    }

    pub fn open_experimental_cpu(
        graph: &Path,
        runtime_version: &str,
    ) -> Result<Self, RawFoundationError> {
        if runtime_version != "1.27.0" {
            return Err(RawFoundationError::ModelContract(format!(
                "experimental RawNIND Build requires ONNX Runtime 1.27.0, got {runtime_version}"
            )));
        }
        Self::open_cpu(
            graph,
            runtime_version,
            "rawnind-public-bayer-foundation-ort127-exp1",
            "rawnind-cpu-ort127-exp1",
        )
    }

    fn open_cpu(
        graph: &Path,
        runtime_version: &str,
        implementation_revision: &str,
        cache_identity: &str,
    ) -> Result<Self, RawFoundationError> {
        let metadata = graph.metadata()?;
        if !metadata.is_file() || metadata.len() != GRAPH_BYTES {
            return Err(RawFoundationError::ModelContract(
                "RawNIND graph size changed".into(),
            ));
        }
        let bytes = std::fs::read(graph)?;
        if format!("{:x}", Sha256::digest(&bytes)) != GRAPH_SHA256 {
            return Err(RawFoundationError::ModelContract(
                "RawNIND graph digest changed".into(),
            ));
        }
        let session = Session::builder()
            .map_err(native)?
            .commit_from_file(graph)
            .map_err(native)?;
        validate_outlet(session.inputs(), "input", &[1, 4, 512, 512])?;
        validate_outlet(session.outputs(), "output", &[1, 3, 1024, 1024])?;
        Ok(Self {
            session,
            identity: RawNindModelIdentity {
                model_id: "darktable-ai/rawnind-public-bayer".into(),
                exact_revision: "release-5.6.0@5454d7aa6d89a67054fd4a83343b09e69acaf76a".into(),
                graph_sha256: GRAPH_SHA256.into(),
                runtime: format!("onnxruntime-{runtime_version}"),
                execution_provider: "CPUExecutionProvider".into(),
                precision: "float32".into(),
                implementation_revision: implementation_revision.into(),
                cache_identity: cache_identity.into(),
            },
        })
    }

    pub fn identity(&self) -> &RawNindModelIdentity {
        &self.identity
    }

    pub fn run_tile(
        &mut self,
        input: Vec<f32>,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<f32>, RawNindTileReceipt), RawFoundationError> {
        if input.len() != 4 * TILE_EDGE * TILE_EDGE {
            return Err(RawFoundationError::ModelContract(
                "RawNIND tile input length changed".into(),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(RawFoundationError::Lease(
                infer_artifact_lease::ArtifactLeaseError::Cancelled,
            ));
        }
        let tensor = Tensor::from_array(([1, 4, TILE_EDGE, TILE_EDGE], input)).map_err(native)?;
        let run_options = Arc::new(RunOptions::new().map_err(native)?);
        let cancellation_for_run = cancellation.clone();
        let terminate = Arc::clone(&run_options);
        let finished = Arc::new(AtomicBool::new(false));
        let finished_for_run = Arc::clone(&finished);
        let guard = std::thread::spawn(move || {
            while !finished_for_run.load(Ordering::Acquire) && !cancellation_for_run.is_cancelled()
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if cancellation_for_run.is_cancelled() {
                let _ = terminate.terminate();
            }
        });
        let started = Instant::now();
        let outputs = self
            .session
            .run_with_options(ort::inputs!["input" => tensor], &run_options)
            .map_err(native);
        finished.store(true, Ordering::Release);
        let _ = guard.join();
        if cancellation.is_cancelled() {
            return Err(infer_artifact_lease::ArtifactLeaseError::Cancelled.into());
        }
        let outputs = outputs?;
        let (_, output) = outputs["output"]
            .try_extract_tensor::<f32>()
            .map_err(native)?;
        let output = output.to_vec();
        if output.len() != 3 * 1024 * 1024 || output.iter().any(|value| !value.is_finite()) {
            return Err(RawFoundationError::ModelContract(
                "RawNIND output tensor changed".into(),
            ));
        }
        let mean = output.iter().map(|value| f64::from(*value)).sum::<f64>() / output.len() as f64;
        Ok((
            output,
            RawNindTileReceipt {
                latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                output_values: 3 * 1024 * 1024,
                raw_output_mean: mean,
            },
        ))
    }
}

fn validate_outlet(
    outlets: &[ort::value::Outlet],
    name: &str,
    expected_shape: &[i64],
) -> Result<(), RawFoundationError> {
    if outlets.len() != 1 || outlets[0].name() != name {
        return Err(RawFoundationError::ModelContract(format!(
            "expected one tensor named {name}"
        )));
    }
    let ValueType::Tensor { shape, .. } = outlets[0].dtype() else {
        return Err(RawFoundationError::ModelContract(format!(
            "{name} is not a tensor"
        )));
    };
    if shape.as_ref() != expected_shape {
        return Err(RawFoundationError::ModelContract(format!(
            "{name} shape changed"
        )));
    }
    Ok(())
}

fn native(error: impl std::fmt::Display) -> RawFoundationError {
    RawFoundationError::NativeExecution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaccepted_runtime_version_fails_before_graph_or_session_access() {
        let error = RawNindModel::open_legacy_cpu(Path::new("/does/not/exist"), "1.27.0")
            .err()
            .expect("unaccepted runtime must fail closed");
        assert!(error.to_string().contains("requires ONNX Runtime 1.24.4"));
    }
}
