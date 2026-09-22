//! Composition root for trusted, built-in Provider families.
//!
//! Provider instances and admitted Builds remain configuration data. This
//! owner only turns a validated configuration into typed runtime components;
//! it is deliberately not a dynamic plugin or external Provider SPI.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use infer_artifact::ArtifactStore;
use infer_core::{LocalInventoryKind, LocalWorkerAdapterKind, ProviderKind, RuntimeConfig};
use infer_provider::{
    AudioTextQueryNormalizer, AudioWorkerExecutor, CodexAppServerProvider, CoremlSamExecutor,
    DynAudioDuplexExecutor, DynAudioExecutor, DynAudioStreamExecutor, DynFaceDetectionExecutor,
    DynFaceEmbeddingExecutor, DynFaceParsingExecutor, DynImageCompletionExecutor,
    DynImageEmbeddingExecutor, DynImageUnderstandingExecutor, DynOcrExecutor, DynProvider,
    DynRetrievalExecutor, DynSemanticGroundingExecutor, DynSubjectSegmentationExecutor,
    DynTextEmbeddingExecutor, OcrBuildContract, OcrWorkerExecutor, OllamaVisionExecutor,
    OnnxProviderRuntime, ProviderRuntimeReadiness, ResponsesProvider, RetrievalBuildContract,
    RetrievalWorkerExecutor, SamBuildContract, provider_requires_ffmpeg, resolve_provider_process,
    verify_clap_worker, verify_coreml_sam_worker, verify_yamnet_worker,
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
    pub face_parsing_executors: BTreeMap<String, DynFaceParsingExecutor>,
    pub subject_segmentation_executors: BTreeMap<String, DynSubjectSegmentationExecutor>,
    pub semantic_grounding_executors: BTreeMap<String, DynSemanticGroundingExecutor>,
    pub image_completion_executors: BTreeMap<String, DynImageCompletionExecutor>,
    pub image_embedding_executors: BTreeMap<String, DynImageEmbeddingExecutor>,
    pub text_embedding_executors: BTreeMap<String, DynTextEmbeddingExecutor>,
    pub image_understanding_executors: BTreeMap<String, DynImageUnderstandingExecutor>,
    pub retrieval_executors: BTreeMap<String, DynRetrievalExecutor>,
    pub ocr_executors: BTreeMap<String, DynOcrExecutor>,
    pub native_controllers: NativeControllerMap,
    pub schedulers: BTreeMap<String, ProviderScheduler>,
    pub readiness: BTreeMap<String, ProviderRuntimeReadiness>,
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
                        | ProviderKind::CoremlWorker
                )
            })
            .then(|| ArtifactStore::from_config(&config.artifacts))
            .transpose()?;
        let mut assembly = Self::empty();
        'providers: for (id, provider) in &config.providers {
            assembly.schedulers.insert(
                id.clone(),
                ProviderScheduler::new(
                    provider.max_concurrency,
                    provider.max_queue,
                    Duration::from_millis(provider.priority_aging_ms),
                ),
            );
            let process =
                resolve_provider_process(id, provider, provider_requires_ffmpeg(config, id));
            assembly
                .readiness
                .insert(id.clone(), process.readiness.clone());
            if !process.readiness.is_ready() {
                continue;
            }
            match provider.kind {
                ProviderKind::TrustedNode => {
                    let peer = provider.node.clone().ok_or_else(|| {
                        RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                            "missing node configuration".into(),
                        ))
                    })?;
                    assembly.providers.insert(
                        id.clone(),
                        Arc::new(infer_provider::TrustedNodeProvider::new(id.clone(), peer)?),
                    );
                }
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
                        process.command.clone().expect("resolved command"),
                        process.args.clone(),
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
                    let mut query_normalizers = BTreeMap::new();
                    let mut admitted_worker_paths = Vec::new();
                    for deployment in config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                    {
                        let build = &config.model_builds[&deployment.build];
                        let Some(worker) = &build.local_worker else {
                            continue;
                        };
                        debug_assert!(matches!(
                            worker.adapter,
                            LocalWorkerAdapterKind::YamnetAudioEvents
                                | LocalWorkerAdapterKind::ClapAudioTextEmbedding
                        ));
                        let resolved = match store.resolve_local_worker_build_identity(
                            &deployment.build,
                            &worker.adapter.to_string(),
                            &worker.artifact_set_sha256,
                        ) {
                            Ok(resolved) => resolved,
                            Err(_) => {
                                assembly.readiness.get_mut(id).expect("readiness exists").mark_unavailable(
                                    "artifact_store",
                                    "admitted audio worker artifact is unavailable or failed integrity verification",
                                );
                                continue 'providers;
                            }
                        };
                        admitted_model_paths.insert(
                            build.model_id.clone(),
                            resolved.runtime_root.to_string_lossy().into_owned(),
                        );
                        if let Some(normalizer) = &worker.audio_text_query_normalizer {
                            let deployment = config
                                .deployments
                                .get(&normalizer.deployment)
                                .ok_or_else(|| {
                                    RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                                        format!(
                                            "CLAP query normalizer Deployment {} is absent",
                                            normalizer.deployment
                                        ),
                                    ))
                                })?;
                            let provider =
                                config.providers.get(&deployment.provider).ok_or_else(|| {
                                    RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                                        "CLAP query normalizer Provider is absent".into(),
                                    ))
                                })?;
                            let normalizer_build =
                                config.model_builds.get(&deployment.build).ok_or_else(|| {
                                    RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                                        "CLAP query normalizer Build is absent".into(),
                                    ))
                                })?;
                            if provider.kind != ProviderKind::Responses
                                || provider.placement != infer_core::Placement::Local
                                || normalizer_build.input_modalities
                                    != vec![infer_core::Modality::Text]
                                || normalizer_build.output_modalities
                                    != vec![infer_core::Modality::Text]
                            {
                                return Err(RuntimeError::Provider(infer_provider::ProviderError::Protocol("CLAP query normalizer must be a local text Responses Deployment".into())));
                            }
                            query_normalizers.insert(
                                build.model_id.clone(),
                                AudioTextQueryNormalizer {
                                    endpoint: format!(
                                        "{}/responses",
                                        provider
                                            .base_url
                                            .as_deref()
                                            .ok_or_else(|| RuntimeError::Provider(
                                                infer_provider::ProviderError::Protocol(
                                                    "CLAP query normalizer has no endpoint".into()
                                                )
                                            ))?
                                            .trim_end_matches('/')
                                    ),
                                    model: normalizer_build.model_id.clone(),
                                    deployment: normalizer.deployment.clone(),
                                    build: deployment.build.clone(),
                                    prompt_revision: normalizer.prompt_revision.clone(),
                                    source_language: normalizer.source_language.clone(),
                                    target_language: normalizer.target_language.clone(),
                                    max_query_bytes: normalizer.max_query_bytes,
                                    max_output_bytes: normalizer.max_output_bytes,
                                },
                            );
                        }
                        admitted_worker_paths.push((
                            worker.adapter,
                            resolved.runtime_root.to_string_lossy().into_owned(),
                        ));
                    }
                    if provider_requires_ffmpeg(config, id) {
                        for (adapter_kind, model_path) in admitted_worker_paths {
                            let check = match adapter_kind {
                                LocalWorkerAdapterKind::YamnetAudioEvents => verify_yamnet_worker(
                                    process.command.as_deref().expect("resolved command"),
                                    &process.args,
                                    &model_path,
                                ),
                                LocalWorkerAdapterKind::ClapAudioTextEmbedding => {
                                    verify_clap_worker(
                                        process.command.as_deref().expect("resolved command"),
                                        &process.args,
                                        &model_path,
                                    )
                                }
                                _ => continue,
                            };
                            match check {
                                Ok(check) => assembly
                                    .readiness
                                    .get_mut(id)
                                    .expect("readiness exists")
                                    .checks
                                    .push(check),
                                Err(check) => {
                                    let readiness =
                                        assembly.readiness.get_mut(id).expect("readiness exists");
                                    readiness.status =
                                        infer_provider::ProviderReadinessStatus::Unavailable;
                                    readiness.summary = check
                                        .message
                                        .clone()
                                        .unwrap_or_else(|| "audio worker is unavailable".into());
                                    readiness.checks.push(*check);
                                    continue 'providers;
                                }
                            }
                        }
                    }
                    let adapter = Arc::new(
                        AudioWorkerExecutor::with_admitted_model_paths_and_normalizers(
                            id,
                            process.command.clone().expect("resolved command"),
                            process.args.clone(),
                            admitted_model_paths,
                            query_normalizers,
                        ),
                    );
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
                        process.command.clone().expect("resolved command"),
                        process.args.clone(),
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
                        process.command.clone().expect("resolved command"),
                        process.args.clone(),
                        builds,
                    );
                    assembly.ocr_executors.insert(id.clone(), Arc::new(adapter));
                }
                ProviderKind::CoremlWorker => {
                    let store = artifact_store
                        .as_ref()
                        .expect("CoreML worker requires an artifact store");
                    let mut worker_args = process.args.clone();
                    worker_args.push("--compiled-cache-root".into());
                    worker_args.push(
                        config
                            .runtimes
                            .coreml_sam
                            .compiled_cache_root
                            .clone()
                            .expect("validated CoreML SAM cache root"),
                    );
                    worker_args.push("--compute-units".into());
                    worker_args.push(config.runtimes.coreml_sam.compute_units.as_str().into());
                    let mut builds = BTreeMap::new();
                    for deployment in config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                    {
                        let build = &config.model_builds[&deployment.build];
                        let worker = build.local_worker.as_ref().expect("validated worker Build");
                        debug_assert_eq!(worker.adapter, LocalWorkerAdapterKind::Sam21Coreml);
                        let resolved = match store.resolve_local_worker_build_identity(
                            &deployment.build,
                            &worker.adapter.to_string(),
                            &worker.artifact_set_sha256,
                        ) {
                            Ok(resolved) => resolved,
                            Err(_) => {
                                assembly
                                    .readiness
                                    .get_mut(id)
                                    .expect("readiness exists")
                                    .mark_unavailable(
                                        "artifact_store",
                                        "admitted SAM artifact is unavailable or failed integrity verification",
                                    );
                                continue 'providers;
                            }
                        };
                        match verify_coreml_sam_worker(
                            process.command.as_deref().expect("resolved command"),
                            &worker_args,
                            &resolved.runtime_root.to_string_lossy(),
                            &worker.artifact_set_sha256,
                        ) {
                            Ok(check) => assembly
                                .readiness
                                .get_mut(id)
                                .expect("readiness exists")
                                .checks
                                .push(check),
                            Err(check) => {
                                let readiness =
                                    assembly.readiness.get_mut(id).expect("readiness exists");
                                readiness.status =
                                    infer_provider::ProviderReadinessStatus::Unavailable;
                                readiness.summary = check
                                    .message
                                    .clone()
                                    .unwrap_or_else(|| "SAM worker is unavailable".into());
                                readiness.checks.push(*check);
                                continue 'providers;
                            }
                        }
                        builds.insert(
                            build.model_id.clone(),
                            SamBuildContract {
                                model_path: resolved.runtime_root.to_string_lossy().into_owned(),
                                model_build: deployment.build.clone(),
                                artifact_sha256: build
                                    .provenance
                                    .artifact_sha256
                                    .clone()
                                    .expect("validated aggregate digest"),
                                preprocessing_identity: worker
                                    .preprocessing_identity
                                    .clone()
                                    .expect("validated preprocessing"),
                                postprocessing_identity: worker.postprocessing_identity.clone(),
                                runtime: worker.runtime.clone(),
                                precision: worker.precision.clone(),
                                requested_execution_provider: worker
                                    .requested_execution_provider
                                    .clone()
                                    .expect("validated requested EP"),
                                actual_execution_provider: worker
                                    .actual_execution_provider
                                    .clone()
                                    .expect("validated actual EP"),
                            },
                        );
                    }
                    let adapter = CoremlSamExecutor::new(
                        id,
                        process.command.clone().expect("resolved command"),
                        worker_args,
                        builds,
                    );
                    assembly
                        .subject_segmentation_executors
                        .insert(id.clone(), Arc::new(adapter));
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
                    assembly
                        .face_parsing_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynFaceParsingExecutor);
                    assembly.image_embedding_executors.insert(
                        id.clone(),
                        Arc::clone(&adapter) as DynImageEmbeddingExecutor,
                    );
                    assembly.semantic_grounding_executors.insert(
                        id.clone(),
                        Arc::clone(&adapter) as DynSemanticGroundingExecutor,
                    );
                    assembly.image_completion_executors.insert(
                        id.clone(),
                        Arc::clone(&adapter) as DynImageCompletionExecutor,
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
            face_parsing_executors: BTreeMap::new(),
            subject_segmentation_executors: BTreeMap::new(),
            semantic_grounding_executors: BTreeMap::new(),
            image_completion_executors: BTreeMap::new(),
            image_embedding_executors: BTreeMap::new(),
            text_embedding_executors: BTreeMap::new(),
            image_understanding_executors: BTreeMap::new(),
            retrieval_executors: BTreeMap::new(),
            ocr_executors: BTreeMap::new(),
            native_controllers: BTreeMap::new(),
            schedulers: BTreeMap::new(),
            readiness: BTreeMap::new(),
        }
    }
}
