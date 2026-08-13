//! Composition root for trusted, built-in Provider families.
//!
//! Provider instances and admitted Builds remain configuration data. This
//! owner only turns a validated configuration into typed runtime components;
//! it is deliberately not a dynamic plugin or external Provider SPI.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use infer_artifact::ArtifactStore;
use infer_core::{LocalInventoryKind, LocalWorkerAdapterKind, ProviderKind, RuntimeConfig};
use infer_provider::{
    AudioWorkerExecutor, CodexAppServerProvider, DynAudioDuplexExecutor, DynAudioExecutor,
    DynAudioStreamExecutor, DynFaceDetectionExecutor, DynFaceEmbeddingExecutor,
    DynImageEmbeddingExecutor, DynImageUnderstandingExecutor, DynOcrExecutor, DynProvider,
    DynRetrievalExecutor, DynTextEmbeddingExecutor, OcrBuildContract, OcrWorkerExecutor,
    OllamaVisionExecutor, OnnxProviderRuntime, ResponsesProvider, RetrievalBuildContract,
    RetrievalWorkerExecutor,
};
use infer_resource::{DynNativeModelController, NativeControllerMap};

use crate::{RuntimeError, scheduler::ProviderScheduler};

pub(super) struct ProviderAssembly {
    pub providers: BTreeMap<String, DynProvider>,
    pub audio_executors: BTreeMap<String, DynAudioExecutor>,
    pub audio_stream_executors: BTreeMap<String, DynAudioStreamExecutor>,
    pub audio_duplex_executors: BTreeMap<String, DynAudioDuplexExecutor>,
    pub face_detection_executors: BTreeMap<String, DynFaceDetectionExecutor>,
    pub face_embedding_executors: BTreeMap<String, DynFaceEmbeddingExecutor>,
    pub image_embedding_executors: BTreeMap<String, DynImageEmbeddingExecutor>,
    pub text_embedding_executors: BTreeMap<String, DynTextEmbeddingExecutor>,
    pub image_understanding_executors: BTreeMap<String, DynImageUnderstandingExecutor>,
    pub retrieval_executors: BTreeMap<String, DynRetrievalExecutor>,
    pub ocr_executors: BTreeMap<String, DynOcrExecutor>,
    pub native_controllers: NativeControllerMap,
    pub schedulers: BTreeMap<String, ProviderScheduler>,
}

impl ProviderAssembly {
    pub fn from_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let artifact_store = config
            .providers
            .values()
            .any(|provider| {
                matches!(
                    provider.kind,
                    ProviderKind::Onnx
                        | ProviderKind::AudioWorker
                        | ProviderKind::RetrievalWorker
                        | ProviderKind::OcrWorker
                )
            })
            .then(|| ArtifactStore::from_config(&config.artifacts))
            .transpose()?;
        let mut assembly = Self::empty();
        for (id, provider) in &config.providers {
            assembly.schedulers.insert(
                id.clone(),
                ProviderScheduler::new(
                    provider.max_concurrency,
                    provider.max_queue,
                    Duration::from_millis(provider.priority_aging_ms),
                ),
            );
            match provider.kind {
                ProviderKind::Responses => {
                    let api_key = provider
                        .api_key_env
                        .as_ref()
                        .and_then(|name| std::env::var(name).ok());
                    let adapter = ResponsesProvider::new(
                        id,
                        provider.base_url.as_deref().expect("validated base_url"),
                        api_key,
                    )?;
                    assembly
                        .providers
                        .insert(id.clone(), Arc::new(adapter) as DynProvider);
                    if let Some(inventory) = provider
                        .local_inventory
                        .as_ref()
                        .filter(|inventory| inventory.kind == LocalInventoryKind::OllamaTags)
                    {
                        let adapter = OllamaVisionExecutor::new(
                            id,
                            inventory.endpoint.as_deref().expect("validated endpoint"),
                        )?;
                        assembly.image_understanding_executors.insert(
                            id.clone(),
                            Arc::new(adapter) as DynImageUnderstandingExecutor,
                        );
                    }
                }
                ProviderKind::CodexAppServer => {
                    let admitted_models = config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                        .filter_map(|deployment| config.model_builds.get(&deployment.build))
                        .map(|build| build.model_id.clone())
                        .collect();
                    let adapter = CodexAppServerProvider::new(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                        admitted_models,
                    );
                    assembly
                        .providers
                        .insert(id.clone(), Arc::new(adapter) as DynProvider);
                }
                ProviderKind::AudioWorker => {
                    let store = artifact_store
                        .as_ref()
                        .expect("audio worker requires an artifact store");
                    let mut admitted_model_paths = BTreeMap::new();
                    for deployment in config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                    {
                        let build = &config.model_builds[&deployment.build];
                        let Some(worker) = &build.local_worker else {
                            continue;
                        };
                        debug_assert_eq!(worker.adapter, LocalWorkerAdapterKind::YamnetAudioEvents);
                        let resolved = store.resolve_local_worker_build_identity(
                            &deployment.build,
                            &worker.adapter.to_string(),
                            &worker.artifact_set_sha256,
                        )?;
                        admitted_model_paths.insert(
                            build.model_id.clone(),
                            resolved.runtime_root.to_string_lossy().into_owned(),
                        );
                    }
                    let adapter = Arc::new(AudioWorkerExecutor::with_admitted_model_paths(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                        admitted_model_paths,
                    ));
                    assembly
                        .audio_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynAudioExecutor);
                    assembly
                        .audio_stream_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynAudioStreamExecutor);
                    assembly
                        .audio_duplex_executors
                        .insert(id.clone(), adapter as DynAudioDuplexExecutor);
                }
                ProviderKind::RetrievalWorker => {
                    let store = artifact_store
                        .as_ref()
                        .expect("retrieval worker requires an artifact store");
                    let mut builds = BTreeMap::new();
                    for (deployment_id, deployment) in config
                        .deployments
                        .iter()
                        .filter(|(_, deployment)| deployment.provider == *id)
                    {
                        let build = &config.model_builds[&deployment.build];
                        let worker = build.local_worker.as_ref().expect("validated worker Build");
                        let resolved = store.resolve_local_worker_build_identity(
                            &deployment.build,
                            &worker.adapter.to_string(),
                            &worker.artifact_set_sha256,
                        )?;
                        builds.insert(
                            build.model_id.clone(),
                            RetrievalBuildContract {
                                model_path: resolved.runtime_root.to_string_lossy().into_owned(),
                                model_build: deployment.build.clone(),
                                model_revision: build
                                    .provenance
                                    .source_revision
                                    .clone()
                                    .expect("validated source revision"),
                                artifact_sha256: build
                                    .provenance
                                    .artifact_sha256
                                    .clone()
                                    .expect("validated aggregate digest"),
                                tokenizer_identity: worker
                                    .tokenizer_identity
                                    .clone()
                                    .expect("validated tokenizer identity"),
                                runtime: worker.runtime.clone(),
                                precision: worker.precision.clone(),
                                embedding_space: worker
                                    .embedding_space
                                    .as_ref()
                                    .map(|space| space.identity.clone()),
                                embedding_dimensions: worker
                                    .embedding_space
                                    .as_ref()
                                    .map(|space| space.dimensions),
                            },
                        );
                        let _ = deployment_id;
                    }
                    let adapter = RetrievalWorkerExecutor::new(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                        builds,
                    );
                    assembly
                        .retrieval_executors
                        .insert(id.clone(), Arc::new(adapter));
                }
                ProviderKind::OcrWorker => {
                    let store = artifact_store
                        .as_ref()
                        .expect("OCR worker requires an artifact store");
                    let mut builds = BTreeMap::new();
                    for deployment in config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                    {
                        let build = &config.model_builds[&deployment.build];
                        let worker = build.local_worker.as_ref().expect("validated worker Build");
                        debug_assert_eq!(worker.adapter, LocalWorkerAdapterKind::PpOcrv6);
                        let resolved = store.resolve_local_worker_build_identity(
                            &deployment.build,
                            &worker.adapter.to_string(),
                            &worker.artifact_set_sha256,
                        )?;
                        let detection = &resolved.manifest.artifacts["detection/inference.onnx"];
                        let recognition =
                            &resolved.manifest.artifacts["recognition/inference.onnx"];
                        builds.insert(
                            build.model_id.clone(),
                            OcrBuildContract {
                                model_build: deployment.build.clone(),
                                detection_model: resolved
                                    .runtime_root
                                    .join("detection")
                                    .to_string_lossy()
                                    .into_owned(),
                                detection_revision: detection.source_revision.clone(),
                                detection_artifact_sha256: detection.sha256.clone(),
                                recognition_model: resolved
                                    .runtime_root
                                    .join("recognition")
                                    .to_string_lossy()
                                    .into_owned(),
                                recognition_revision: recognition.source_revision.clone(),
                                recognition_artifact_sha256: recognition.sha256.clone(),
                                preprocessing_identity: worker
                                    .preprocessing_identity
                                    .clone()
                                    .expect("validated preprocessing"),
                                postprocessing_identity: worker.postprocessing_identity.clone(),
                                runtime: worker.runtime.clone(),
                                requested_execution_provider: worker
                                    .requested_execution_provider
                                    .clone()
                                    .expect("validated requested EP"),
                                actual_execution_provider: worker
                                    .actual_execution_provider
                                    .clone()
                                    .expect("validated actual EP"),
                                precision: worker.precision.clone(),
                            },
                        );
                    }
                    let adapter = OcrWorkerExecutor::new(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                        builds,
                    );
                    assembly.ocr_executors.insert(id.clone(), Arc::new(adapter));
                }
                ProviderKind::Onnx => {
                    let builds = config
                        .deployments
                        .iter()
                        .filter(|(_, deployment)| deployment.provider == *id)
                        .filter_map(|(_, deployment)| {
                            config.model_builds.get(&deployment.build).map(|build| {
                                (
                                    build.model_id.clone(),
                                    (deployment.build.clone(), build.clone()),
                                )
                            })
                        })
                        .collect();
                    let adapter = OnnxProviderRuntime::new(
                        id,
                        config.runtimes.onnx.clone(),
                        artifact_store
                            .as_ref()
                            .expect("ONNX provider requires an artifact store")
                            .clone(),
                        builds,
                    )?;
                    assembly
                        .face_detection_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynFaceDetectionExecutor);
                    assembly
                        .face_embedding_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynFaceEmbeddingExecutor);
                    assembly.image_embedding_executors.insert(
                        id.clone(),
                        Arc::clone(&adapter) as DynImageEmbeddingExecutor,
                    );
                    assembly
                        .text_embedding_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynTextEmbeddingExecutor);
                    assembly
                        .native_controllers
                        .insert(id.clone(), adapter as DynNativeModelController);
                }
                // RawFoundationControl owns its native graph and execution
                // lifecycle. This entry contributes common scheduling only.
                ProviderKind::RawFoundation => {}
            }
        }
        Ok(assembly)
    }

    fn empty() -> Self {
        Self {
            providers: BTreeMap::new(),
            audio_executors: BTreeMap::new(),
            audio_stream_executors: BTreeMap::new(),
            audio_duplex_executors: BTreeMap::new(),
            face_detection_executors: BTreeMap::new(),
            face_embedding_executors: BTreeMap::new(),
            image_embedding_executors: BTreeMap::new(),
            text_embedding_executors: BTreeMap::new(),
            image_understanding_executors: BTreeMap::new(),
            retrieval_executors: BTreeMap::new(),
            ocr_executors: BTreeMap::new(),
            native_controllers: BTreeMap::new(),
            schedulers: BTreeMap::new(),
        }
    }
}
