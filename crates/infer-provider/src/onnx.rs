//! In-process ONNX Runtime provider and typed vision adapters.
//!
//! Session ownership and native execution stay here. Public requests never
//! expose tensor names, shapes, filesystem paths, or execution-provider knobs.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use image::{ImageFormat, RgbImage};
use infer_artifact::ArtifactStore;
use infer_core::{
    EncodedLabelMap, FaceDetection, FaceDetectionRequest, FaceEmbeddingEligibility,
    FaceEmbeddingRequest, FaceEmbeddingVector, FaceParsingOntology, FaceParsingRegion,
    FaceParsingRequest, ImageEmbeddingRequest, ImageGeometry, MAX_VISION_IMAGE_PIXELS,
    ModelBuildConfig, OnnxAdapterKind, OnnxExecutionProvider, OnnxRuntimeConfig,
    SemanticEmbeddingVector, TextEmbeddingRequest, VisionImage,
};
use infer_resource::{
    NativeControlError, NativeInventory, NativeModelController, NativeRunningModel,
};
use ort::{
    ep,
    session::{RunOptions, Session},
    value::{Outlet, TensorElementType, ValueType},
};
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;

use crate::ProviderError;

mod bisenet;
mod sface;
mod siglip;
mod yunet;

static ORT_LIBRARY: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug)]
pub struct FaceDetectionExecutionOutput {
    pub image: ImageGeometry,
    pub detections: Vec<FaceDetection>,
    pub provenance: OnnxExecutionProvenance,
}

#[derive(Debug)]
pub struct FaceEmbeddingExecutionOutput {
    pub embedding: FaceEmbeddingVector,
    pub eligibility: FaceEmbeddingEligibility,
    pub provenance: OnnxExecutionProvenance,
}

#[derive(Debug)]
pub struct ImageEmbeddingExecutionOutput {
    pub image: ImageGeometry,
    pub embedding: SemanticEmbeddingVector,
    pub provenance: OnnxExecutionProvenance,
}

#[derive(Debug)]
pub struct TextEmbeddingExecutionOutput {
    pub embedding: SemanticEmbeddingVector,
    pub provenance: OnnxExecutionProvenance,
}

#[derive(Debug)]
pub struct FaceParsingExecutionOutput {
    pub image: ImageGeometry,
    pub face_box: infer_core::BoundingBox,
    pub label_map: EncodedLabelMap,
    pub ontology: FaceParsingOntology,
    pub regions: Vec<FaceParsingRegion>,
    pub provenance: VisionExecutionProvenance,
}

#[derive(Debug)]
pub struct VisionExecutionProvenance {
    pub model_build: String,
    pub artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub tokenizer: Option<OnnxTokenizerProvenance>,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub execution_provider_fallback_reason: Option<String>,
    pub precision: String,
}

pub type OnnxExecutionProvenance = VisionExecutionProvenance;

#[derive(Debug)]
pub struct OnnxTokenizerProvenance {
    pub identity: String,
    pub artifact_sha256: String,
    pub max_length: usize,
    pub lowercase: bool,
}

#[async_trait]
pub trait FaceDetectionExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn detect_faces(
        &self,
        physical_model: &str,
        request: FaceDetectionRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceDetectionExecutionOutput, ProviderError>;
}

pub type DynFaceDetectionExecutor = Arc<dyn FaceDetectionExecutor>;

#[async_trait]
pub trait FaceEmbeddingExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn embed_face(
        &self,
        physical_model: &str,
        request: FaceEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceEmbeddingExecutionOutput, ProviderError>;
}

pub type DynFaceEmbeddingExecutor = Arc<dyn FaceEmbeddingExecutor>;

#[async_trait]
pub trait FaceParsingExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn parse_face(
        &self,
        physical_model: &str,
        request: FaceParsingRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceParsingExecutionOutput, ProviderError>;
}

pub type DynFaceParsingExecutor = Arc<dyn FaceParsingExecutor>;

#[async_trait]
pub trait ImageEmbeddingExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn embed_image(
        &self,
        physical_model: &str,
        request: ImageEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<ImageEmbeddingExecutionOutput, ProviderError>;
}

pub type DynImageEmbeddingExecutor = Arc<dyn ImageEmbeddingExecutor>;

#[async_trait]
pub trait TextEmbeddingExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn embed_text(
        &self,
        physical_model: &str,
        request: TextEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<TextEmbeddingExecutionOutput, ProviderError>;
}

pub type DynTextEmbeddingExecutor = Arc<dyn TextEmbeddingExecutor>;

#[derive(Clone)]
pub struct OnnxProviderRuntime {
    id: String,
    runtime: OnnxRuntimeConfig,
    store: ArtifactStore,
    builds: Arc<BTreeMap<String, RegisteredBuild>>,
    sessions: Arc<Mutex<BTreeMap<String, Arc<SessionEntry>>>>,
}

#[derive(Clone)]
struct RegisteredBuild {
    build_id: String,
    build: ModelBuildConfig,
}

struct SessionEntry {
    session: Mutex<Session>,
    tokenizer: Option<Tokenizer>,
    build_id: String,
    build: ModelBuildConfig,
    requested_execution_provider: OnnxExecutionProvider,
    actual_execution_provider: OnnxExecutionProvider,
    execution_provider_fallback_reason: Option<String>,
}

impl OnnxProviderRuntime {
    pub fn new(
        id: impl Into<String>,
        runtime: OnnxRuntimeConfig,
        store: ArtifactStore,
        builds: BTreeMap<String, (String, ModelBuildConfig)>,
    ) -> Result<Arc<Self>, ProviderError> {
        let library = runtime.library.as_deref().ok_or_else(|| {
            ProviderError::NativeRuntime("ONNX Runtime library is missing".into())
        })?;
        initialize_runtime(Path::new(library))?;
        let builds = builds
            .into_iter()
            .map(|(physical_model, (build_id, build))| {
                (physical_model, RegisteredBuild { build_id, build })
            })
            .collect();
        Ok(Arc::new(Self {
            id: id.into(),
            runtime,
            store,
            builds: Arc::new(builds),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }))
    }

    fn registered(&self, model: &str) -> Result<&RegisteredBuild, ProviderError> {
        self.builds.get(model).ok_or_else(|| {
            ProviderError::Protocol(format!("unregistered ONNX model identity {model}"))
        })
    }

    fn load_sync(&self, model: &str) -> Result<Arc<SessionEntry>, ProviderError> {
        // Session creation is serialized with unload and other loads. P0 uses
        // small Sessions; avoiding duplicate native allocations is more
        // important than parallel cold starts. Inference itself releases this
        // registry lock and is serialized only per Session.
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if let Some(session) = sessions.get(model) {
            return Ok(Arc::clone(session));
        }
        let registered = self.registered(model)?;
        let onnx = registered.build.onnx.as_ref().ok_or_else(|| {
            ProviderError::Protocol("registered ONNX build has no manifest".into())
        })?;
        let model_path = self
            .store
            .resolve_onnx(&registered.build_id, onnx)
            .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
        let requested = self
            .runtime
            .preferred_execution_providers
            .iter()
            .copied()
            .find(|provider| onnx.allowed_execution_providers.contains(provider))
            .ok_or_else(|| {
                ProviderError::NativeRuntime(format!(
                    "build {} and runtime have no common execution provider",
                    registered.build_id
                ))
            })?;

        let cache = self
            .store
            .root()
            .join("cache")
            .join("coreml")
            .join(&onnx.artifact.sha256);
        let loaded = build_session(&model_path, requested, &cache);
        let (session, actual, fallback_reason) = match loaded {
            Ok(session) => (session, requested, None),
            Err(_primary)
                if requested != OnnxExecutionProvider::Cpu
                    && self.runtime.allow_cpu_fallback
                    && onnx
                        .allowed_execution_providers
                        .contains(&OnnxExecutionProvider::Cpu) =>
            {
                let session = build_session(&model_path, OnnxExecutionProvider::Cpu, &cache)
                    .map_err(|fallback| {
                        ProviderError::NativeRuntime(format!(
                            "{} failed and CPU fallback failed: {fallback}",
                            execution_provider_name(requested)
                        ))
                    })?;
                (
                    session,
                    OnnxExecutionProvider::Cpu,
                    Some("requested_execution_provider_unavailable_for_build".into()),
                )
            }
            Err(error) => return Err(error),
        };
        validate_session_contract(&session, onnx)?;
        let tokenizer = siglip::load_tokenizer(&self.store, &registered.build_id, onnx)?;
        let entry = Arc::new(SessionEntry {
            session: Mutex::new(session),
            tokenizer,
            build_id: registered.build_id.clone(),
            build: registered.build.clone(),
            requested_execution_provider: requested,
            actual_execution_provider: actual,
            execution_provider_fallback_reason: fallback_reason,
        });
        sessions.insert(model.into(), Arc::clone(&entry));
        Ok(entry)
    }

    fn unload_sync(&self, model: &str) -> Result<(), ProviderError> {
        self.registered(model)?;
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .remove(model);
        Ok(())
    }

    fn installed_models(&self) -> BTreeSet<String> {
        self.builds
            .iter()
            .filter_map(|(model, registered)| {
                registered
                    .build
                    .onnx
                    .as_ref()
                    .and_then(|onnx| self.store.resolve_onnx(&registered.build_id, onnx).ok())
                    .map(|_| model.clone())
            })
            .collect()
    }
}

#[async_trait]
impl NativeModelController for OnnxProviderRuntime {
    async fn discover(&self) -> Result<NativeInventory, NativeControlError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let installed = this.installed_models();
            let sessions = this.sessions.lock().expect("session registry poisoned");
            let running = sessions
                .keys()
                .map(|model| {
                    // The ONNX file size is not a defensible estimate of the
                    // prepared Session's memory. Stay unknown until a native
                    // or measured residency contract exists, so eviction
                    // cannot make decisions from a convenient false number.
                    (
                        model.clone(),
                        NativeRunningModel {
                            resident_memory_bytes: None,
                            resident_accelerator_memory_bytes: None,
                        },
                    )
                })
                .collect();
            Ok(NativeInventory { installed, running })
        })
        .await
        .map_err(|error| NativeControlError::Operation(error.to_string()))?
    }

    async fn load(&self, model: &str) -> Result<(), NativeControlError> {
        let this = self.clone();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || this.load_sync(&model).map(|_| ()))
            .await
            .map_err(|error| NativeControlError::Operation(error.to_string()))?
            .map_err(|error| NativeControlError::Operation(error.to_string()))
    }

    async fn unload(&self, model: &str) -> Result<(), NativeControlError> {
        let this = self.clone();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || this.unload_sync(&model))
            .await
            .map_err(|error| NativeControlError::Operation(error.to_string()))?
            .map_err(|error| NativeControlError::Operation(error.to_string()))
    }
}

#[async_trait]
impl FaceDetectionExecutor for OnnxProviderRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    async fn detect_faces(
        &self,
        physical_model: &str,
        request: FaceDetectionRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceDetectionExecutionOutput, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::NativeRuntime(
                "ONNX execution cancelled".into(),
            ));
        }
        let entry = {
            let this = self.clone();
            let model = physical_model.to_owned();
            tokio::task::spawn_blocking(move || this.load_sync(&model))
                .await
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))??
        };
        if entry
            .build
            .onnx
            .as_ref()
            .is_none_or(|onnx| onnx.adapter != OnnxAdapterKind::YunetFaceDetection)
        {
            return Err(ProviderError::Protocol(
                "selected ONNX build is not a face detection adapter".into(),
            ));
        }
        let run_options = Arc::new(
            RunOptions::new().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
        );
        let options_for_run = Arc::clone(&run_options);
        let entry_for_run = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            yunet::run(&entry_for_run, request, options_for_run.as_ref())
        });
        tokio::select! {
            result = &mut task => result
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
            _ = cancellation.cancelled() => {
                let _ = run_options.terminate();
                let _ = task.await;
                Err(ProviderError::NativeRuntime("ONNX execution cancelled".into()))
            }
        }
    }
}

#[async_trait]
impl FaceEmbeddingExecutor for OnnxProviderRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    async fn embed_face(
        &self,
        physical_model: &str,
        request: FaceEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceEmbeddingExecutionOutput, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::NativeRuntime(
                "ONNX execution cancelled".into(),
            ));
        }
        let entry = {
            let this = self.clone();
            let model = physical_model.to_owned();
            tokio::task::spawn_blocking(move || this.load_sync(&model))
                .await
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))??
        };
        if entry
            .build
            .onnx
            .as_ref()
            .is_none_or(|onnx| onnx.adapter != OnnxAdapterKind::SfaceEmbedding)
        {
            return Err(ProviderError::Protocol(
                "selected ONNX build is not a face embedding adapter".into(),
            ));
        }
        let run_options = Arc::new(
            RunOptions::new().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
        );
        let options_for_run = Arc::clone(&run_options);
        let entry_for_run = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            sface::run(&entry_for_run, request, options_for_run.as_ref())
        });
        tokio::select! {
            result = &mut task => result
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
            _ = cancellation.cancelled() => {
                let _ = run_options.terminate();
                let _ = task.await;
                Err(ProviderError::NativeRuntime("ONNX execution cancelled".into()))
            }
        }
    }
}

#[async_trait]
impl FaceParsingExecutor for OnnxProviderRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    async fn parse_face(
        &self,
        physical_model: &str,
        request: FaceParsingRequest,
        cancellation: CancellationToken,
    ) -> Result<FaceParsingExecutionOutput, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::NativeRuntime(
                "ONNX execution cancelled".into(),
            ));
        }
        let entry = {
            let this = self.clone();
            let model = physical_model.to_owned();
            tokio::task::spawn_blocking(move || this.load_sync(&model))
                .await
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))??
        };
        if entry
            .build
            .onnx
            .as_ref()
            .is_none_or(|onnx| onnx.adapter != OnnxAdapterKind::BisenetFaceParsing)
        {
            return Err(ProviderError::Protocol(
                "selected ONNX build is not a face parsing adapter".into(),
            ));
        }
        let run_options = Arc::new(
            RunOptions::new().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
        );
        let options_for_run = Arc::clone(&run_options);
        let entry_for_run = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            bisenet::run(&entry_for_run, request, options_for_run.as_ref())
        });
        tokio::select! {
            result = &mut task => result
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
            _ = cancellation.cancelled() => {
                let _ = run_options.terminate();
                let _ = task.await;
                Err(ProviderError::NativeRuntime("ONNX execution cancelled".into()))
            }
        }
    }
}

#[async_trait]
impl ImageEmbeddingExecutor for OnnxProviderRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    async fn embed_image(
        &self,
        physical_model: &str,
        request: ImageEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<ImageEmbeddingExecutionOutput, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::NativeRuntime(
                "ONNX execution cancelled".into(),
            ));
        }
        let entry = {
            let this = self.clone();
            let model = physical_model.to_owned();
            tokio::task::spawn_blocking(move || this.load_sync(&model))
                .await
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))??
        };
        if entry
            .build
            .onnx
            .as_ref()
            .is_none_or(|onnx| onnx.adapter != OnnxAdapterKind::SiglipImageEmbedding)
        {
            return Err(ProviderError::Protocol(
                "selected ONNX build is not an image embedding adapter".into(),
            ));
        }
        let run_options = Arc::new(
            RunOptions::new().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
        );
        let options_for_run = Arc::clone(&run_options);
        let entry_for_run = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            siglip::run_image(&entry_for_run, request, options_for_run.as_ref())
        });
        tokio::select! {
            result = &mut task => result
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
            _ = cancellation.cancelled() => {
                let _ = run_options.terminate();
                let _ = task.await;
                Err(ProviderError::NativeRuntime("ONNX execution cancelled".into()))
            }
        }
    }
}

#[async_trait]
impl TextEmbeddingExecutor for OnnxProviderRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    async fn embed_text(
        &self,
        physical_model: &str,
        request: TextEmbeddingRequest,
        cancellation: CancellationToken,
    ) -> Result<TextEmbeddingExecutionOutput, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::NativeRuntime(
                "ONNX execution cancelled".into(),
            ));
        }
        let entry = {
            let this = self.clone();
            let model = physical_model.to_owned();
            tokio::task::spawn_blocking(move || this.load_sync(&model))
                .await
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))??
        };
        if entry
            .build
            .onnx
            .as_ref()
            .is_none_or(|onnx| onnx.adapter != OnnxAdapterKind::SiglipTextEmbedding)
        {
            return Err(ProviderError::Protocol(
                "selected ONNX build is not a text embedding adapter".into(),
            ));
        }
        let run_options = Arc::new(
            RunOptions::new().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
        );
        let options_for_run = Arc::clone(&run_options);
        let entry_for_run = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            siglip::run_text(&entry_for_run, request, options_for_run.as_ref())
        });
        tokio::select! {
            result = &mut task => result
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?,
            _ = cancellation.cancelled() => {
                let _ = run_options.terminate();
                let _ = task.await;
                Err(ProviderError::NativeRuntime("ONNX execution cancelled".into()))
            }
        }
    }
}

fn initialize_runtime(library: &Path) -> Result<(), ProviderError> {
    if let Some(existing) = ORT_LIBRARY.get() {
        if existing != library {
            return Err(ProviderError::NativeRuntime(format!(
                "ONNX Runtime already initialized from {}",
                existing.display()
            )));
        }
        return Ok(());
    }
    if !library.is_absolute() || !library.is_file() {
        return Err(ProviderError::NativeRuntime(
            "ONNX Runtime library must be an existing absolute file".into(),
        ));
    }
    ort::init_from(library)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?
        .with_name("infer-runtime")
        .commit();
    let _ = ORT_LIBRARY.set(library.to_owned());
    Ok(())
}

fn build_session(
    model: &Path,
    provider: OnnxExecutionProvider,
    coreml_cache: &Path,
) -> Result<Session, ProviderError> {
    let mut builder =
        Session::builder().map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    match provider {
        OnnxExecutionProvider::Coreml => {
            fs::create_dir_all(coreml_cache)?;
            let execution_provider = ep::CoreML::default()
                .with_model_format(ep::coreml::ModelFormat::MLProgram)
                .with_compute_units(ep::coreml::ComputeUnits::All)
                .with_model_cache_dir(coreml_cache.to_string_lossy())
                .build();
            builder = builder
                .with_execution_providers([execution_provider])
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?
                .with_disable_cpu_fallback()
                .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
        }
        OnnxExecutionProvider::Cpu => {}
    }
    builder
        .commit_from_file(model)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

fn validate_session_contract(
    session: &Session,
    build: &infer_core::OnnxModelBuildConfig,
) -> Result<(), ProviderError> {
    validate_outlets("input", session.inputs(), &build.inputs)?;
    validate_outlets("output", session.outputs(), &build.outputs)?;
    Ok(())
}

fn validate_outlets(
    kind: &str,
    actual: &[Outlet],
    expected: &[infer_core::TensorContractConfig],
) -> Result<(), ProviderError> {
    if actual.len() != expected.len() {
        return Err(ProviderError::Protocol(format!(
            "ONNX graph {kind} count does not match the Build manifest"
        )));
    }
    for contract in expected {
        let Some(outlet) = actual.iter().find(|outlet| outlet.name() == contract.name) else {
            return Err(ProviderError::Protocol(format!(
                "ONNX graph is missing configured {kind} tensor {}",
                contract.name
            )));
        };
        let ValueType::Tensor { ty, shape, .. } = outlet.dtype() else {
            return Err(ProviderError::Protocol(format!(
                "ONNX {kind} tensor {} is not a tensor",
                contract.name
            )));
        };
        if !tensor_dtype_matches(&contract.dtype, *ty)
            || shape.len() != contract.shape.len()
            || shape.iter().zip(&contract.shape).any(|(actual, expected)| {
                match expected.parse::<i64>() {
                    Ok(expected) => *actual != expected,
                    Err(_) => *actual >= 0,
                }
            })
        {
            return Err(ProviderError::Protocol(format!(
                "ONNX {kind} tensor {} dtype/shape does not match the Build manifest",
                contract.name
            )));
        }
    }
    Ok(())
}

fn tensor_dtype_matches(expected: &str, actual: TensorElementType) -> bool {
    match expected.to_ascii_lowercase().as_str() {
        "float32" | "f32" => actual == TensorElementType::Float32,
        "float16" | "f16" => actual == TensorElementType::Float16,
        "float64" | "f64" => actual == TensorElementType::Float64,
        "int8" | "i8" => actual == TensorElementType::Int8,
        "int16" | "i16" => actual == TensorElementType::Int16,
        "int32" | "i32" => actual == TensorElementType::Int32,
        "int64" | "i64" => actual == TensorElementType::Int64,
        "uint8" | "u8" => actual == TensorElementType::Uint8,
        "uint16" | "u16" => actual == TensorElementType::Uint16,
        "uint32" | "u32" => actual == TensorElementType::Uint32,
        "uint64" | "u64" => actual == TensorElementType::Uint64,
        "bool" => actual == TensorElementType::Bool,
        _ => false,
    }
}

fn decode_image(image: &VisionImage) -> Result<RgbImage, ProviderError> {
    let format = match image.content_type.as_str() {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        _ => {
            return Err(ProviderError::InvalidInput(
                "unsupported image content type".into(),
            ));
        }
    };
    let dimensions = image::ImageReader::with_format(std::io::Cursor::new(&image.bytes), format)
        .into_dimensions()
        .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_VISION_IMAGE_PIXELS {
        return Err(ProviderError::InvalidInput(format!(
            "decoded image exceeds {MAX_VISION_IMAGE_PIXELS} pixels"
        )));
    }
    Ok(image::load_from_memory_with_format(&image.bytes, format)
        .map_err(|error| ProviderError::InvalidInput(error.to_string()))?
        .to_rgb8())
}

fn tensor_data<'a>(
    outputs: &'a ort::session::SessionOutputs<'_>,
    name: &str,
) -> Result<&'a [f32], ProviderError> {
    outputs
        .get(name)
        .ok_or_else(|| ProviderError::Protocol(format!("missing ONNX output {name}")))?
        .try_extract_tensor::<f32>()
        .map(|(_, data)| data)
        .map_err(|error| ProviderError::Protocol(error.to_string()))
}

fn parameter(
    build: &infer_core::OnnxModelBuildConfig,
    name: &str,
    default: f64,
) -> Result<f64, ProviderError> {
    let value = build
        .postprocessing_parameters
        .get(name)
        .copied()
        .unwrap_or(default);
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(ProviderError::Protocol(format!(
            "invalid ONNX postprocessing parameter {name}"
        )))
    }
}

fn execution_provenance(entry: &SessionEntry) -> OnnxExecutionProvenance {
    let onnx = entry.build.onnx.as_ref().expect("validated ONNX build");
    let tokenizer = onnx.text_preprocessing.as_ref().map(|contract| {
        let artifact = &onnx.auxiliary_artifacts[&contract.tokenizer_artifact];
        OnnxTokenizerProvenance {
            identity: contract.identity.clone(),
            artifact_sha256: artifact.sha256.clone(),
            max_length: contract.max_length,
            lowercase: contract.lowercase,
        }
    });
    OnnxExecutionProvenance {
        model_build: entry.build_id.clone(),
        artifact_sha256: onnx.artifact.sha256.clone(),
        preprocessing_identity: onnx
            .preprocessing
            .as_ref()
            .map(|preprocess| preprocess.identity.clone())
            .or_else(|| {
                onnx.text_preprocessing
                    .as_ref()
                    .map(|preprocess| preprocess.identity.clone())
            })
            .expect("validated ONNX preprocessing identity"),
        postprocessing_identity: onnx.postprocessing_identity.clone(),
        tokenizer,
        runtime: ort::info().into(),
        requested_execution_provider: execution_provider_name(entry.requested_execution_provider)
            .into(),
        actual_execution_provider: execution_provider_name(entry.actual_execution_provider).into(),
        execution_provider_fallback_reason: entry.execution_provider_fallback_reason.clone(),
        precision: onnx.precision.clone(),
    }
}

fn execution_provider_name(provider: OnnxExecutionProvider) -> &'static str {
    match provider {
        OnnxExecutionProvider::Coreml => "coreml",
        OnnxExecutionProvider::Cpu => "cpu",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Barrier;

    use infer_core::{
        BoundingBox, FACE_PARSING_ONTOLOGY_ID, FaceEmbeddingRequest, FaceParsingRequest,
        FivePointLandmarks, Point, RuntimeConfig, VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
        VisionImage,
    };

    use super::*;

    fn real_yunet_runtime(
        preferred: Vec<OnnxExecutionProvider>,
        allow_cpu_fallback: bool,
    ) -> Arc<OnnxProviderRuntime> {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.toml");
        let config = RuntimeConfig::load(config_path).unwrap();
        let build_id = "yunet_2026may_onnx_cpu_v1";
        let build = config.model_builds[build_id].clone();
        let model = build.model_id.clone();
        let mut runtime = config.runtimes.onnx.clone();
        runtime.preferred_execution_providers = preferred;
        runtime.allow_cpu_fallback = allow_cpu_fallback;
        OnnxProviderRuntime::new(
            "onnx-real-test",
            runtime,
            ArtifactStore::from_config(&config.artifacts).unwrap(),
            BTreeMap::from([(model, (build_id.into(), build))]),
        )
        .unwrap()
    }

    fn real_sface_runtime() -> Arc<OnnxProviderRuntime> {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.toml");
        let config = RuntimeConfig::load(config_path).unwrap();
        let build_id = "sface_2021dec_onnx_cpu_v1";
        let build = config.model_builds[build_id].clone();
        let model = build.model_id.clone();
        let mut runtime = config.runtimes.onnx.clone();
        runtime.preferred_execution_providers = vec![OnnxExecutionProvider::Cpu];
        runtime.allow_cpu_fallback = false;
        OnnxProviderRuntime::new(
            "onnx-sface-test",
            runtime,
            ArtifactStore::from_config(&config.artifacts).unwrap(),
            BTreeMap::from([(model, (build_id.into(), build))]),
        )
        .unwrap()
    }

    fn real_bisenet_runtime() -> Arc<OnnxProviderRuntime> {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.toml");
        let config = RuntimeConfig::load(config_path).unwrap();
        let build_id = "bisenet_resnet18_face_parsing_onnx_cpu_v1";
        let build = config.model_builds[build_id].clone();
        let model = build.model_id.clone();
        let mut runtime = config.runtimes.onnx.clone();
        runtime.preferred_execution_providers = vec![OnnxExecutionProvider::Cpu];
        runtime.allow_cpu_fallback = false;
        OnnxProviderRuntime::new(
            "onnx-bisenet-test",
            runtime,
            ArtifactStore::from_config(&config.artifacts).unwrap(),
            BTreeMap::from([(model, (build_id.into(), build))]),
        )
        .unwrap()
    }

    fn blank_request() -> FaceDetectionRequest {
        let image = RgbImage::from_pixel(320, 240, image::Rgb([127, 127, 127]));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("PNG fixture encodes");
        FaceDetectionRequest {
            model: "vision.detect_faces".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: bytes.into_inner(),
            },
            source_revision: "test:blank".into(),
            metadata: BTreeMap::from([
                ("infer.placement".into(), "local_only".into()),
                ("infer.offline_required".into(), "true".into()),
                ("infer.fallback".into(), "none".into()),
            ]),
        }
    }

    fn synthetic_embedding_request() -> FaceEmbeddingRequest {
        let image = RgbImage::from_fn(112, 112, |x, y| {
            image::Rgb([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8])
        });
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("PNG fixture encodes");
        FaceEmbeddingRequest {
            model: "vision.embed_face".into(),
            image: VisionImage {
                content_type: "image/png".into(),
                bytes: bytes.into_inner(),
            },
            landmarks: FivePointLandmarks {
                right_eye: Point {
                    x: 38.2946,
                    y: 51.6963,
                },
                left_eye: Point {
                    x: 73.5318,
                    y: 51.5014,
                },
                nose_tip: Point {
                    x: 56.0252,
                    y: 71.7366,
                },
                right_mouth_corner: Point {
                    x: 41.5493,
                    y: 92.3655,
                },
                left_mouth_corner: Point {
                    x: 70.7299,
                    y: 92.2041,
                },
            },
            source_revision: "test:synthetic".into(),
            metadata: BTreeMap::from([
                ("infer.placement".into(), "local_only".into()),
                ("infer.offline_required".into(), "true".into()),
                ("infer.fallback".into(), "none".into()),
            ]),
        }
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime and YuNet artifact"]
    async fn real_yunet_cpu_session_and_inference() {
        let runtime = real_yunet_runtime(vec![OnnxExecutionProvider::Cpu], false);
        let model = runtime.builds.keys().next().unwrap().clone();
        let result = runtime
            .detect_faces(&model, blank_request(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.provenance.actual_execution_provider, "cpu");
        assert_eq!(result.image.width, 320);
        assert_eq!(result.image.height, 240);
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime and YuNet artifact"]
    async fn concurrent_cold_loads_publish_exactly_one_session() {
        let runtime = real_yunet_runtime(vec![OnnxExecutionProvider::Cpu], false);
        let model = runtime.builds.keys().next().unwrap().clone();
        let barrier = Arc::new(Barrier::new(3));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let runtime = Arc::clone(&runtime);
            let model = model.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                runtime.load_sync(&model).unwrap()
            }));
        }
        barrier.wait();
        let first = tasks.remove(0).await.unwrap();
        let second = tasks.remove(0).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(runtime.sessions.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires INFER_TEST_FACE_IMAGE plus pinned local ONNX Runtime and YuNet"]
    async fn real_yunet_detects_a_face_with_five_points() {
        let path = std::env::var("INFER_TEST_FACE_IMAGE")
            .expect("INFER_TEST_FACE_IMAGE must point to a JPEG fixture");
        let bytes = fs::read(path).unwrap();
        let runtime = real_yunet_runtime(vec![OnnxExecutionProvider::Cpu], false);
        let model = runtime.builds.keys().next().unwrap().clone();
        let mut request = blank_request();
        request.image = VisionImage {
            content_type: "image/jpeg".into(),
            bytes,
        };
        request.source_revision = "test:face".into();
        let result = runtime
            .detect_faces(&model, request, CancellationToken::new())
            .await
            .unwrap();
        assert!(!result.detections.is_empty());
        let face = &result.detections[0];
        assert!(face.confidence >= 0.6);
        assert!(face.bounding_box.width > 0.0);
        assert!(face.bounding_box.height > 0.0);
        assert!(face.landmarks.left_eye.x.is_finite());
        assert!(face.landmarks.right_eye.x.is_finite());
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime and SFace artifact"]
    async fn real_sface_cpu_session_load_inventory_and_unload() {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.toml");
        let config = RuntimeConfig::load(config_path).unwrap();
        let build_id = "sface_2021dec_onnx_cpu_v1";
        let build = config.model_builds[build_id].clone();
        let model = build.model_id.clone();
        let mut runtime_config = config.runtimes.onnx.clone();
        runtime_config.preferred_execution_providers = vec![OnnxExecutionProvider::Cpu];
        runtime_config.allow_cpu_fallback = false;
        let runtime = OnnxProviderRuntime::new(
            "onnx-sface-test",
            runtime_config,
            ArtifactStore::from_config(&config.artifacts).unwrap(),
            BTreeMap::from([(model.clone(), (build_id.into(), build))]),
        )
        .unwrap();
        NativeModelController::load(runtime.as_ref(), &model)
            .await
            .unwrap();
        let inventory = NativeModelController::discover(runtime.as_ref())
            .await
            .unwrap();
        assert!(inventory.installed.contains(&model));
        assert!(inventory.running.contains_key(&model));
        NativeModelController::unload(runtime.as_ref(), &model)
            .await
            .unwrap();
        assert!(
            !NativeModelController::discover(runtime.as_ref())
                .await
                .unwrap()
                .running
                .contains_key(&model)
        );
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime and BiSeNet artifact"]
    async fn real_bisenet_cpu_face_parsing_is_bounded_and_typed() {
        let runtime = real_bisenet_runtime();
        let model = runtime.builds.keys().next().unwrap().clone();
        let image = RgbImage::from_fn(512, 512, |x, y| {
            image::Rgb([(x / 2) as u8, (y / 2) as u8, ((x + y) / 4) as u8])
        });
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let response = runtime
            .parse_face(
                &model,
                FaceParsingRequest {
                    model: "vision.parse_face".into(),
                    image: VisionImage {
                        content_type: "image/png".into(),
                        bytes: bytes.into_inner(),
                    },
                    source_revision: "test:synthetic-gradient".into(),
                    image_orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
                    face_box: BoundingBox {
                        x: 128.0,
                        y: 96.0,
                        width: 256.0,
                        height: 320.0,
                    },
                    metadata: BTreeMap::from([
                        ("infer.placement".into(), "local_only".into()),
                        ("infer.offline_required".into(), "true".into()),
                        ("infer.fallback".into(), "none".into()),
                    ]),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!((response.image.width, response.image.height), (512, 512));
        assert_eq!(response.ontology.id, FACE_PARSING_ONTOLOGY_ID);
        assert_eq!(response.ontology.class_count, 19);
        assert_eq!(response.regions.len(), 19);
        assert_eq!(
            (response.label_map.width, response.label_map.height),
            (512, 512)
        );
        assert_eq!(response.label_map.encoding, "indexed_u8_png");
        assert_eq!(response.provenance.actual_execution_provider, "cpu");
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime and SFace artifact"]
    async fn real_sface_cpu_embedding_is_normalized_and_deterministic() {
        let runtime = real_sface_runtime();
        let model = runtime.builds.keys().next().unwrap().clone();
        let first = runtime
            .embed_face(
                &model,
                synthetic_embedding_request(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let second = runtime
            .embed_face(
                &model,
                synthetic_embedding_request(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let norm = first
            .embedding
            .values
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(first.embedding.dimensions, 128);
        assert_eq!(first.embedding.values, second.embedding.values);
        assert_eq!(first.provenance.actual_execution_provider, "cpu");
        assert!(first.eligibility.eligible);
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime, Core ML, and SFace artifact"]
    async fn real_sface_cpu_build_never_admits_coreml() {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.toml");
        let config = RuntimeConfig::load(config_path).unwrap();
        let build_id = "sface_2021dec_onnx_cpu_v1";
        let build = config.model_builds[build_id].clone();
        let model = build.model_id.clone();
        let mut runtime_config = config.runtimes.onnx.clone();
        runtime_config.preferred_execution_providers = vec![OnnxExecutionProvider::Coreml];
        runtime_config.allow_cpu_fallback = false;
        let runtime = OnnxProviderRuntime::new(
            "onnx-sface-coreml-test",
            runtime_config,
            ArtifactStore::from_config(&config.artifacts).unwrap(),
            BTreeMap::from([(model.clone(), (build_id.into(), build))]),
        )
        .unwrap();
        let error = NativeModelController::load(runtime.as_ref(), &model)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no common execution provider"));
        assert!(!runtime.sessions.lock().unwrap().contains_key(&model));
    }

    #[tokio::test]
    async fn pre_cancelled_vision_requests_never_enter_native_execution() {
        // The early cancellation gate is independent from native installation,
        // so exercise it with a deliberately uninitialized shell runtime.
        let runtime = OnnxProviderRuntime {
            id: "cancel-test".into(),
            runtime: OnnxRuntimeConfig::default(),
            store: ArtifactStore::at(tempfile::tempdir().unwrap().path()).unwrap(),
            builds: Arc::new(BTreeMap::new()),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = runtime
            .detect_faces("missing", blank_request(), cancellation)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = runtime
            .embed_face("missing", synthetic_embedding_request(), cancellation)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[tokio::test]
    #[ignore = "requires the pinned local ONNX Runtime, Core ML, and YuNet artifact"]
    async fn real_yunet_cpu_build_never_attempts_coreml() {
        let runtime = real_yunet_runtime(
            vec![OnnxExecutionProvider::Coreml, OnnxExecutionProvider::Cpu],
            true,
        );
        let model = runtime.builds.keys().next().unwrap().clone();
        let result = runtime
            .detect_faces(&model, blank_request(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.provenance.requested_execution_provider, "cpu");
        assert_eq!(result.provenance.actual_execution_provider, "cpu");
        assert!(
            result
                .provenance
                .execution_provider_fallback_reason
                .is_none()
        );
    }
}
