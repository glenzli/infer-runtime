//! The control plane: admission, policy selection, dispatch, job state, and cancellation.

mod app_admission;
mod attempt_policy;
mod audio_streaming;
mod background_jobs;
mod image_understanding;
mod metrics;
mod observer;
mod pressure_observation;
mod provider_health;
mod raw_foundation;
mod registry;
mod resource_control;
mod resource_monitor;
mod scheduler;
mod vision_execution;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use infer_artifact::{ArtifactError, ArtifactStore};
use infer_auth::{AppCredentials, CredentialError};
use infer_core::{
    AppConfig, AttemptOutcome, AttemptSnapshot, AttemptTrigger, AudioExecutionRequest, BuiltinTool,
    ContractError, DurablePayloadRef, ExecutionMode, ExecutionRequirements, Fallback,
    IntentProfile, JobListPage, JobPageCursor, JobSnapshot, JobState, LocalInventoryKind, Modality,
    Priority, ProviderCapability, ProviderConfig, ProviderProtocol, QuotaConfig,
    RequestConstraints, ResponsesRequest, RuntimeConfig,
};
use infer_payload::PayloadError;
use infer_provider::{
    AudioWorkerExecutor, CodexAppServerProvider, DynAudioDuplexExecutor, DynAudioExecutor,
    DynAudioStreamExecutor, DynFaceDetectionExecutor, DynFaceEmbeddingExecutor,
    DynImageEmbeddingExecutor, DynImageUnderstandingExecutor, DynProvider,
    DynTextEmbeddingExecutor, OllamaVisionExecutor, OnnxProviderRuntime, ProviderError,
    ProviderModelCatalog, ResponsesProvider, probe_responses_provider,
    probe_responses_provider_with_effort,
};
use infer_resource::{
    DynNativeModelController, ModelReservation, NativeControllerMap, ResourceError, ResourceManager,
};
use infer_store::{
    ActiveReservation, AttemptReservation, AuditEvent, AuditEventInput, ConfigSnapshot,
    QuotaLimits, QuotaResource, Store, StoreError, UsageLedgerEntry,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use app_admission::{AppAdmission, AppAdmissionPermit};
use metrics::{MetricsSnapshot, RuntimeMetrics};
use provider_health::ProviderHealth;
use registry::{Candidate, CandidatePlanningContext, plan_candidates};
use resource_monitor::EvictionMonitor;
use scheduler::{ProviderScheduler, ScheduledPermit, SchedulerError};

pub use audio_streaming::{
    RuntimeAudioByteStream, RuntimeTranscriptionSession, SpeechRuntimeStream,
};
pub use background_jobs::BackgroundSubmission;
pub use infer_observer::{ObserverIdentity, ObserverSnapshot};
pub use infer_provider::AudioExecutionOutput;
pub use infer_provider::ProviderProbeReport;
pub use metrics::{MetricsSnapshot as ControlMetricsSnapshot, ProviderQueueMetrics};
pub use raw_foundation::{
    RawFoundationCancellation, RawFoundationControl, RawFoundationControlError,
    RawFoundationLeaseGrant, RawFoundationProvenance, RawFoundationResponse,
};
pub use resource_control::AuditedEvictionActionResult;
pub use resource_monitor::{
    EvictionMonitorOutcome, EvictionMonitorSnapshot, MaintenanceLease, MaintenanceLeaseError,
    MaintenanceLeaseRequest, MaintenanceLeaseRevokeRequest,
};

pub type RuntimeByteStream = Pin<Box<dyn Stream<Item = Bytes> + Send>>;

#[derive(Debug)]
pub struct AudioRuntimeResult {
    pub job_id: String,
    pub logical_model: String,
    pub output: AudioExecutionOutput,
}

#[derive(Debug, serde::Serialize)]
pub struct BudgetSnapshot {
    pub quota: QuotaConfig,
    pub usage_ledger: Vec<UsageLedgerEntry>,
    pub active_reservations: Vec<ActiveReservation>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProviderSnapshot {
    pub id: String,
    pub kind: String,
    pub access_class: infer_core::ProviderAccessClass,
    pub placement: infer_core::Placement,
    pub configured: bool,
    pub max_concurrency: usize,
    pub max_queue: usize,
    pub capability_profile: infer_core::ProviderCapabilityProfile,
    /// Aggregate execution shapes currently routable through this Provider.
    /// Exact admission remains deployment-specific.
    pub execution_modes: BTreeSet<ExecutionMode>,
    /// Static, version-controlled admission inventory for this Provider. This
    /// is distinct from provider-native discovery and local residency state.
    pub deployments: Vec<ProviderDeploymentSnapshot>,
    pub circuit_open: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct ProviderDeploymentSnapshot {
    pub id: String,
    pub build: String,
    pub model_profile: String,
    pub model_family: String,
    /// Provider-facing model identity when it is already portable. Absolute
    /// local paths are projected to the semantic model family.
    pub model: String,
    pub resource_class: infer_core::ResourceClass,
    pub supported_efforts: Vec<infer_core::ReasoningEffort>,
    pub execution_modes: BTreeSet<ExecutionMode>,
    pub ratings: BTreeMap<String, infer_core::CapabilityRating>,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error("invalid API credential")]
    Unauthorized,
    #[error("observer credentials are restricted to the observer snapshot")]
    ObserverCredentialRestricted,
    #[error("application `{0}` does not have summary observer access")]
    ObserverAccessRequired(String),
    #[error("unknown intent profile `{0}`")]
    UnknownIntent(String),
    #[error("intent `{intent}` belongs to `{actual}`, not `{expected}`")]
    DataPlaneMismatch {
        intent: String,
        expected: String,
        actual: String,
    },
    #[error("unknown application `{0}`")]
    UnknownApp(String),
    #[error("application `{app_id}` is not permitted to submit intent `{intent}`")]
    IntentNotAllowed { app_id: String, intent: String },
    #[error("policy profile `{0}` is not permitted for this application")]
    PolicyNotAllowed(String),
    #[error("request override `{field}` is not permitted for this application")]
    OverrideNotAllowed { field: &'static str },
    #[error("application `{0}` is not permitted to administer local resources")]
    ResourceAdminRequired(String),
    #[error("no deployment satisfies the intent, capability floor, and hard constraints")]
    NoCandidate,
    #[error("provider `{0}` is unavailable")]
    ProviderUnavailable(String),
    #[error(transparent)]
    Resource(#[from] ResourceError),
    #[error(transparent)]
    MaintenanceLease(#[from] MaintenanceLeaseError),
    #[error("response was cancelled before execution started")]
    Cancelled,
    #[error("provider queue is full")]
    QueueFull,
    #[error("application pending Job limit has been reached")]
    AppQueueFull,
    #[error("provider `{0}` does not expose a probeable Responses data plane")]
    ProviderProbeUnsupported(String),
    #[error("provider `{0}` has no Responses deployment to use as its probe model")]
    ProviderProbeModelMissing(String),
    #[error("durable background execution is disabled")]
    BackgroundDisabled,
    #[error("durable background execution cannot be disabled while pending Jobs exist")]
    BackgroundDisabledWithPendingJobs,
    #[error("durable background execution currently requires local-only placement")]
    BackgroundLocalOnly,
    #[error("durable background key environment variable `{0}` is unavailable")]
    BackgroundKeyUnavailable(String),
    #[error("durable background payload has an invalid Responses representation")]
    BackgroundPayloadFormat,
    #[error("durable background Job exhausted its Attempt budget")]
    BackgroundAttemptBudgetExhausted,
    #[error("durable background blocking task failed")]
    BackgroundTaskFailed,
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error("request deadline expired")]
    DeadlineExpired,
    #[error("quota exceeded for {scope} {resource}")]
    QuotaExceeded {
        scope: String,
        resource: QuotaResource,
    },
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl From<StoreError> for RuntimeError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::QuotaExceeded { scope, resource } => {
                Self::QuotaExceeded { scope, resource }
            }
            error => Self::Store(error),
        }
    }
}

struct JobEntry {
    snapshot: JobSnapshot,
    cancellation: CancellationToken,
    admission_permit: Option<AppAdmissionPermit>,
}

struct PreparedRun {
    job_id: String,
    app_id: String,
    logical_model: String,
    provider_id: String,
    deployment_id: String,
    physical_model: String,
    estimated_cost_usd: f64,
    estimated_tokens: u64,
    cancellation: CancellationToken,
    priority: Priority,
    submitted_at: Instant,
    deadline: Option<Instant>,
    targets: Vec<Candidate>,
    existing_attempts: usize,
    recovered: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionMode {
    Immediate,
    DeferredBackground,
}

#[derive(Clone, Copy)]
struct UsageTokens {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

/// All request-specific input needed to admit one Job. Grouping this preserves
/// Runtime as the lifecycle coordinator while keeping admission inputs
/// explicit and extensible.
struct JobPreparation<'a> {
    logical_model: &'a str,
    constraints: RequestConstraints,
    execution_requirements: ExecutionRequirements,
    reasoning_effort: Option<infer_core::ReasoningEffort>,
    estimated_tokens: u64,
    id_prefix: &'static str,
    expected_data_plane: &'static str,
    durable_payload: Option<&'a DurablePayloadRef>,
}

/// Owns control-plane state. HTTP and CLI layers only call this facade.
pub struct Runtime {
    config: Arc<RuntimeConfig>,
    credentials: AppCredentials,
    providers: BTreeMap<String, DynProvider>,
    audio_executors: BTreeMap<String, DynAudioExecutor>,
    audio_stream_executors: BTreeMap<String, DynAudioStreamExecutor>,
    audio_duplex_executors: BTreeMap<String, DynAudioDuplexExecutor>,
    face_detection_executors: BTreeMap<String, DynFaceDetectionExecutor>,
    face_embedding_executors: BTreeMap<String, DynFaceEmbeddingExecutor>,
    image_embedding_executors: BTreeMap<String, DynImageEmbeddingExecutor>,
    text_embedding_executors: BTreeMap<String, DynTextEmbeddingExecutor>,
    image_understanding_executors: BTreeMap<String, DynImageUnderstandingExecutor>,
    schedulers: BTreeMap<String, ProviderScheduler>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    metrics: RuntimeMetrics,
    health: ProviderHealth,
    resources: Arc<ResourceManager>,
    _pressure_observation: Option<pressure_observation::PressureObservation>,
    resource_monitor: EvictionMonitor,
    app_admission: AppAdmission,
    /// Test/in-process callers may omit persistence, but the daemon
    /// constructor always initializes this before it accepts work.
    store: Option<Arc<Store>>,
    background: background_jobs::BackgroundJobs,
    observer: observer::ObserverRuntimeState,
}

fn scheduler_for(provider: &ProviderConfig) -> ProviderScheduler {
    ProviderScheduler::new(
        provider.max_concurrency,
        provider.max_queue,
        Duration::from_millis(provider.priority_aging_ms),
    )
}

impl Runtime {
    pub async fn from_config(config: RuntimeConfig) -> Result<Arc<Self>, RuntimeError> {
        let observer = observer::ObserverRuntimeState::new(&config.observer);
        let credentials = AppCredentials::load_or_create(&config)?;
        let background = background_jobs::BackgroundJobs::from_config(&config.background)?;
        let config_snapshot = ConfigSnapshot::from_serializable(&config)?;
        let store = Arc::new(Store::open_with_quota(
            &config.persistence.path,
            config_snapshot,
            QuotaLimits::from(&config.quota),
        )?);
        if !background.is_enabled() && store.pending_background_jobs()? > 0 {
            return Err(RuntimeError::BackgroundDisabledWithPendingJobs);
        }
        if background.is_enabled() {
            background
                .validate_pending(store.pending_background_payloads()?)
                .await?;
        }
        // Interactive work is never replayed. Durable local background work
        // is recovered separately with authenticated payloads.
        store.recover_interrupted()?;
        let mut background_recovery = if background.is_enabled() {
            store.recover_background_jobs(background.recovery_limit())?
        } else {
            infer_store::BackgroundRecovery::default()
        };
        if background.is_enabled() {
            background_recovery
                .discard
                .extend(store.expire_background_payloads(unix_time_ms())?);
            background
                .remove_orphans(store.background_payload_refs()?)
                .await?;
        }
        let mut providers = BTreeMap::new();
        let mut audio_executors = BTreeMap::new();
        let mut audio_stream_executors = BTreeMap::new();
        let mut audio_duplex_executors = BTreeMap::new();
        let mut face_detection_executors = BTreeMap::new();
        let mut face_embedding_executors = BTreeMap::new();
        let mut image_embedding_executors = BTreeMap::new();
        let mut text_embedding_executors = BTreeMap::new();
        let mut image_understanding_executors = BTreeMap::new();
        let mut native_controllers = NativeControllerMap::new();
        let mut schedulers = BTreeMap::new();
        let app_admission = AppAdmission::new(&config.apps);
        let resource_monitor = EvictionMonitor::new(config.resources.eviction.monitor.clone());
        let artifact_store = config
            .providers
            .values()
            .any(|provider| provider.kind == "onnx")
            .then(|| ArtifactStore::from_config(&config.artifacts))
            .transpose()?;
        for (id, provider) in &config.providers {
            match provider.kind.as_str() {
                "responses" => {
                    let api_key = provider
                        .api_key_env
                        .as_ref()
                        .and_then(|name| std::env::var(name).ok());
                    let adapter = ResponsesProvider::new(
                        id,
                        provider.base_url.as_deref().expect("validated base_url"),
                        api_key,
                    )?;
                    providers.insert(id.clone(), Arc::new(adapter) as DynProvider);
                    if let Some(inventory) = provider
                        .local_inventory
                        .as_ref()
                        .filter(|inventory| inventory.kind == LocalInventoryKind::OllamaTags)
                    {
                        let adapter = OllamaVisionExecutor::new(
                            id,
                            inventory.endpoint.as_deref().expect("validated endpoint"),
                        )?;
                        image_understanding_executors.insert(
                            id.clone(),
                            Arc::new(adapter) as DynImageUnderstandingExecutor,
                        );
                    }
                }
                "codex_app_server" => {
                    let admitted_models = config
                        .deployments
                        .values()
                        .filter(|deployment| deployment.provider == *id)
                        .filter_map(|deployment| config.model_builds.get(&deployment.build))
                        .map(|build| build.model_id.clone())
                        .collect::<BTreeSet<_>>();
                    let adapter = CodexAppServerProvider::new(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                        admitted_models,
                    );
                    providers.insert(id.clone(), Arc::new(adapter) as DynProvider);
                }
                "audio_worker" => {
                    let adapter = Arc::new(AudioWorkerExecutor::new(
                        id,
                        provider.command.clone().expect("validated command"),
                        provider.args.clone(),
                    ));
                    audio_executors.insert(id.clone(), Arc::clone(&adapter) as DynAudioExecutor);
                    audio_stream_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynAudioStreamExecutor);
                    audio_duplex_executors.insert(id.clone(), adapter as DynAudioDuplexExecutor);
                }
                "onnx" => {
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
                    face_detection_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynFaceDetectionExecutor);
                    face_embedding_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynFaceEmbeddingExecutor);
                    image_embedding_executors.insert(
                        id.clone(),
                        Arc::clone(&adapter) as DynImageEmbeddingExecutor,
                    );
                    text_embedding_executors
                        .insert(id.clone(), Arc::clone(&adapter) as DynTextEmbeddingExecutor);
                    native_controllers.insert(id.clone(), adapter as DynNativeModelController);
                }
                // RawFoundationControl owns its native graph and execution
                // lifecycle. This provider exists only to reuse common Job,
                // admission, scheduling, reservation, and provenance state.
                "raw_foundation" => {}
                _ => unreachable!("provider kind was validated"),
            }
            schedulers.insert(id.clone(), scheduler_for(provider));
        }
        let pressure_refresh_interval =
            Duration::from_millis(config.resources.pressure.refresh_interval_ms);
        let resources = Arc::new(ResourceManager::with_native_controllers(
            &config,
            native_controllers,
        ));
        let pressure_observation = pressure_observation::PressureObservation::start(
            Arc::clone(&resources),
            pressure_refresh_interval,
        )
        .await;
        let runtime = Arc::new(Self {
            config: Arc::new(config),
            credentials,
            providers,
            audio_executors,
            audio_stream_executors,
            audio_duplex_executors,
            face_detection_executors,
            face_embedding_executors,
            image_embedding_executors,
            text_embedding_executors,
            image_understanding_executors,
            schedulers,
            jobs: Mutex::new(HashMap::new()),
            metrics: RuntimeMetrics::default(),
            health: ProviderHealth::default(),
            resources,
            _pressure_observation: Some(pressure_observation),
            resource_monitor,
            app_admission,
            store: Some(store),
            background,
            observer,
        });
        runtime.restore_background(background_recovery).await;
        resource_monitor::spawn(&runtime);
        Ok(runtime)
    }

    /// Constructor used by tests and future in-process provider extensions.
    pub fn with_providers(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
    ) -> Arc<Self> {
        Self::with_providers_and_credentials(config, providers, AppCredentials::empty())
    }

    /// Constructor for embedded/API tests whose caller owns credential
    /// provisioning separately from the runtime-managed local store.
    pub fn with_providers_and_credentials(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        credentials: AppCredentials,
    ) -> Arc<Self> {
        Self::with_providers_and_store(config, providers, None, credentials)
    }

    fn with_providers_and_store(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        store: Option<Arc<Store>>,
        credentials: AppCredentials,
    ) -> Arc<Self> {
        Self::with_components(
            config,
            providers,
            store,
            background_jobs::BackgroundJobs::disabled(),
            credentials,
        )
    }

    fn with_components(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        store: Option<Arc<Store>>,
        background: background_jobs::BackgroundJobs,
        credentials: AppCredentials,
    ) -> Arc<Self> {
        let observer = observer::ObserverRuntimeState::new(&config.observer);
        let app_admission = AppAdmission::new(&config.apps);
        let resources = Arc::new(ResourceManager::from_config(&config));
        let resource_monitor = EvictionMonitor::new(config.resources.eviction.monitor.clone());
        let schedulers = config
            .providers
            .iter()
            .map(|(id, provider)| (id.clone(), scheduler_for(provider)))
            .collect();
        Arc::new(Self {
            config: Arc::new(config),
            credentials,
            providers,
            audio_executors: BTreeMap::new(),
            audio_stream_executors: BTreeMap::new(),
            audio_duplex_executors: BTreeMap::new(),
            face_detection_executors: BTreeMap::new(),
            face_embedding_executors: BTreeMap::new(),
            image_embedding_executors: BTreeMap::new(),
            text_embedding_executors: BTreeMap::new(),
            image_understanding_executors: BTreeMap::new(),
            schedulers,
            jobs: Mutex::new(HashMap::new()),
            metrics: RuntimeMetrics::default(),
            health: ProviderHealth::default(),
            resources,
            _pressure_observation: None,
            resource_monitor,
            app_admission,
            store,
            background,
            observer,
        })
    }

    /// Constructor for typed-provider contract tests and future embedded
    /// integrations. The caller remains responsible for matching executor ids
    /// to configured Provider ids.
    pub fn with_image_understanding_executors(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        credentials: AppCredentials,
        executors: BTreeMap<String, DynImageUnderstandingExecutor>,
    ) -> Arc<Self> {
        let mut runtime = Self::with_providers_and_credentials(config, providers, credentials);
        Arc::get_mut(&mut runtime)
            .expect("newly constructed Runtime has one owner")
            .image_understanding_executors = executors;
        runtime
    }

    pub fn authenticate(&self, bearer_token: &str) -> Result<String, RuntimeError> {
        self.credentials
            .authenticate(bearer_token)
            .map(str::to_owned)
            .ok_or(RuntimeError::Unauthorized)
    }

    pub fn authorize_resource_admin(&self, app_id: &str) -> Result<(), RuntimeError> {
        match self.config.apps.get(app_id) {
            Some(app) if app.resource_admin => Ok(()),
            Some(_) => Err(RuntimeError::ResourceAdminRequired(app_id.into())),
            None => Err(RuntimeError::UnknownApp(app_id.into())),
        }
    }

    pub async fn execute(
        self: &Arc<Self>,
        app_id: &str,
        request: ResponsesRequest,
    ) -> Result<Value, RuntimeError> {
        if request.background {
            return Err(RuntimeError::BackgroundDisabled);
        }
        let request = self.prepare_responses_request(request)?;
        let constraints = request.constraints()?;
        let execution_requirements = request.execution_requirements();
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &request.model,
                    constraints,
                    execution_requirements,
                    reasoning_effort: request.reasoning_effort(),
                    estimated_tokens: estimate_response_tokens(&request),
                    id_prefix: "resp",
                    expected_data_plane: "responses",
                    durable_payload: None,
                },
            )
            .await?;
        self.execute_prepared(request, prepared, CompletionMode::Immediate)
            .await
    }

    async fn execute_prepared(
        self: &Arc<Self>,
        request: ResponsesRequest,
        mut prepared: PreparedRun,
        completion: CompletionMode,
    ) -> Result<Value, RuntimeError> {
        let targets = prepared.targets.clone();
        let mut attempts = prepared.existing_attempts;
        let mut last_error = None;
        'targets: for (target_index, target) in targets.iter().enumerate() {
            self.set_attempt_target(&mut prepared, target).await?;
            for retry_index in 0..=attempt_policy::MAX_RETRIES_PER_CANDIDATE {
                if attempts >= attempt_policy::MAX_ATTEMPTS {
                    break 'targets;
                }
                let _resource_reservation = self.reserve_resource(&prepared).await?;
                let _permit = self.acquire(&prepared).await?;
                self.mark(&prepared.job_id, JobState::Running, None).await?;
                let trigger = if attempts == 0 {
                    AttemptTrigger::Initial
                } else if prepared.recovered && attempts == prepared.existing_attempts {
                    AttemptTrigger::Recovery
                } else if retry_index > 0 {
                    AttemptTrigger::Retry
                } else {
                    AttemptTrigger::Fallback
                };
                let attempt_number = self.begin_attempt(&prepared, trigger).await?;
                attempts += 1;
                if prepared.cancellation.is_cancelled() {
                    self.finish_attempt(
                        &prepared,
                        attempt_number,
                        AttemptOutcome::Failed,
                        Some("cancelled".into()),
                        Some("response was cancelled".into()),
                        None,
                    )
                    .await?;
                    self.mark(&prepared.job_id, JobState::Cancelled, None)
                        .await?;
                    self.metrics.cancelled();
                    return Err(RuntimeError::Cancelled);
                }
                let provider = self.provider(&prepared.provider_id)?;
                let upstream =
                    provider.execute(request.for_provider(prepared.physical_model.clone()));
                let result = match prepared.deadline {
                    Some(deadline) => tokio::select! {
                        _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                        result = timeout_at(deadline, upstream) => result.map_err(|_| RuntimeError::DeadlineExpired),
                    },
                    None => tokio::select! {
                        _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                        result = upstream => Ok(result),
                    },
                };
                match result {
                    Ok(Ok(value)) => {
                        self.health.record_success(&prepared.provider_id);
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Succeeded,
                            None,
                            None,
                            response_usage(&value),
                        )
                        .await?;
                        let response =
                            normalize_response(value, &prepared.job_id, &prepared.logical_model);
                        if completion == CompletionMode::Immediate {
                            self.mark(&prepared.job_id, JobState::Succeeded, None)
                                .await?;
                            self.metrics.succeeded();
                        }
                        return Ok(response);
                    }
                    Ok(Err(error)) => {
                        let kind = error.kind();
                        self.health.record_failure(&prepared.provider_id, &error);
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some(attempt_policy::kind_code(kind).into()),
                            Some(error.to_string()),
                            None,
                        )
                        .await?;
                        let can_retry = attempt_policy::retryable(kind)
                            && retry_index < attempt_policy::MAX_RETRIES_PER_CANDIDATE
                            && attempts < attempt_policy::MAX_ATTEMPTS;
                        let can_fallback = attempt_policy::fallback_eligible(kind)
                            && target_index + 1 < targets.len()
                            && attempts < attempt_policy::MAX_ATTEMPTS;
                        last_error = Some(error);
                        if can_retry {
                            continue;
                        }
                        if can_fallback {
                            continue 'targets;
                        }
                        break 'targets;
                    }
                    Err(RuntimeError::Cancelled) => {
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some("cancelled".into()),
                            Some("response was cancelled".into()),
                            None,
                        )
                        .await?;
                        self.mark(&prepared.job_id, JobState::Cancelled, None)
                            .await?;
                        self.metrics.cancelled();
                        return Err(RuntimeError::Cancelled);
                    }
                    Err(RuntimeError::DeadlineExpired) => {
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some("deadline_exceeded".into()),
                            Some("request deadline expired".into()),
                            None,
                        )
                        .await?;
                        self.mark(
                            &prepared.job_id,
                            JobState::Expired,
                            Some("request deadline expired".into()),
                        )
                        .await?;
                        self.metrics.expired();
                        return Err(RuntimeError::DeadlineExpired);
                    }
                    Err(error) => unreachable!("provider attempt returned {error}"),
                }
            }
        }
        let error = last_error.expect("an exhausted attempt plan has a provider error");
        self.mark(&prepared.job_id, JobState::Failed, Some(error.to_string()))
            .await?;
        self.metrics.failed();
        Err(RuntimeError::Provider(error))
    }

    pub async fn execute_stream(
        self: &Arc<Self>,
        app_id: &str,
        request: ResponsesRequest,
    ) -> Result<RuntimeByteStream, RuntimeError> {
        let request = self.prepare_responses_request(request)?;
        let constraints = request.constraints()?;
        let execution_requirements = request.execution_requirements();
        let mut prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &request.model,
                    constraints,
                    execution_requirements,
                    reasoning_effort: request.reasoning_effort(),
                    estimated_tokens: estimate_response_tokens(&request),
                    id_prefix: "resp",
                    expected_data_plane: "responses",
                    durable_payload: None,
                },
            )
            .await?;
        let targets = prepared.targets.clone();
        let mut attempts = 0;
        let mut last_error = None;
        'targets: for (target_index, target) in targets.iter().enumerate() {
            self.set_attempt_target(&mut prepared, target).await?;
            for retry_index in 0..=attempt_policy::MAX_RETRIES_PER_CANDIDATE {
                if attempts >= attempt_policy::MAX_ATTEMPTS {
                    break 'targets;
                }
                let resource_reservation = self.reserve_resource(&prepared).await?;
                let permit = self.acquire(&prepared).await?;
                self.mark(&prepared.job_id, JobState::Running, None).await?;
                let trigger = if attempts == 0 {
                    AttemptTrigger::Initial
                } else if retry_index > 0 {
                    AttemptTrigger::Retry
                } else {
                    AttemptTrigger::Fallback
                };
                let attempt_number = self.begin_attempt(&prepared, trigger).await?;
                attempts += 1;
                let provider = self.provider(&prepared.provider_id)?;
                let upstream_call =
                    provider.execute_stream(request.for_provider(prepared.physical_model.clone()));
                let upstream = match prepared.deadline {
                    Some(deadline) => tokio::select! {
                        _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                        result = timeout_at(deadline, upstream_call) => {
                            result.map_err(|_| RuntimeError::DeadlineExpired)
                        }
                    },
                    None => tokio::select! {
                        _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                        result = upstream_call => Ok(result),
                    },
                };
                match upstream {
                    Ok(Ok(stream)) => {
                        return Ok(self.normalize_stream(
                            prepared,
                            attempt_number,
                            permit,
                            resource_reservation,
                            stream,
                        ));
                    }
                    Ok(Err(error)) => {
                        let kind = error.kind();
                        self.health.record_failure(&prepared.provider_id, &error);
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some(attempt_policy::kind_code(kind).into()),
                            Some(error.to_string()),
                            None,
                        )
                        .await?;
                        let can_retry = attempt_policy::retryable(kind)
                            && retry_index < attempt_policy::MAX_RETRIES_PER_CANDIDATE
                            && attempts < attempt_policy::MAX_ATTEMPTS;
                        let can_fallback = attempt_policy::fallback_eligible(kind)
                            && target_index + 1 < targets.len()
                            && attempts < attempt_policy::MAX_ATTEMPTS;
                        last_error = Some(error);
                        if can_retry {
                            continue;
                        }
                        if can_fallback {
                            continue 'targets;
                        }
                        break 'targets;
                    }
                    Err(RuntimeError::Cancelled) => {
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some("cancelled".into()),
                            Some("response was cancelled".into()),
                            None,
                        )
                        .await?;
                        self.mark(&prepared.job_id, JobState::Cancelled, None)
                            .await?;
                        self.metrics.cancelled();
                        return Err(RuntimeError::Cancelled);
                    }
                    Err(RuntimeError::DeadlineExpired) => {
                        self.finish_attempt(
                            &prepared,
                            attempt_number,
                            AttemptOutcome::Failed,
                            Some("deadline_exceeded".into()),
                            Some("request deadline expired".into()),
                            None,
                        )
                        .await?;
                        self.mark(
                            &prepared.job_id,
                            JobState::Expired,
                            Some("request deadline expired".into()),
                        )
                        .await?;
                        self.metrics.expired();
                        return Err(RuntimeError::DeadlineExpired);
                    }
                    Err(error) => unreachable!("stream setup attempt returned {error}"),
                }
            }
        }
        let error = last_error.expect("an exhausted attempt plan has a provider error");
        self.mark(&prepared.job_id, JobState::Failed, Some(error.to_string()))
            .await?;
        self.metrics.failed();
        Err(RuntimeError::Provider(error))
    }

    pub async fn snapshot(&self, response_id: &str) -> Result<Option<JobSnapshot>, RuntimeError> {
        let in_memory = self
            .jobs
            .lock()
            .await
            .get(response_id)
            .map(|entry| entry.snapshot.clone());
        if in_memory.is_some() {
            return Ok(in_memory);
        }
        self.store
            .as_ref()
            .map(|store| store.load_job(response_id))
            .transpose()
            .map(Option::flatten)
            .map_err(RuntimeError::from)
    }

    pub async fn snapshot_for_app(
        &self,
        app_id: &str,
        response_id: &str,
    ) -> Result<Option<JobSnapshot>, RuntimeError> {
        Ok(self
            .snapshot(response_id)
            .await?
            .filter(|snapshot| snapshot.app_id == app_id))
    }

    pub fn job_page(
        &self,
        app_id: &str,
        priority: Option<Priority>,
        state: Option<JobState>,
        cursor: Option<&JobPageCursor>,
        limit: usize,
    ) -> Result<JobListPage, RuntimeError> {
        match &self.store {
            Some(store) => Ok(store.job_page(app_id, priority, state, cursor, limit)?),
            None => Ok(JobListPage {
                jobs: Vec::new(),
                next_cursor: None,
            }),
        }
    }

    fn prepare_responses_request(
        &self,
        mut request: ResponsesRequest,
    ) -> Result<ResponsesRequest, RuntimeError> {
        request.validate()?;
        let intent = self
            .config
            .intent(&request.model)
            .ok_or_else(|| RuntimeError::UnknownIntent(request.model.clone()))?;
        request.apply_intent_defaults(intent);
        Ok(request)
    }

    pub fn audit_events(&self, response_id: &str) -> Result<Vec<AuditEvent>, RuntimeError> {
        self.store
            .as_ref()
            .map(|store| store.audit_events(response_id))
            .transpose()
            .map(|events| events.unwrap_or_default())
            .map_err(RuntimeError::from)
    }

    pub async fn cancel(&self, response_id: &str) -> bool {
        self.cancel_if_owned(None, response_id).await
    }

    pub async fn cancel_for_app(&self, app_id: &str, response_id: &str) -> bool {
        self.cancel_if_owned(Some(app_id), response_id).await
    }

    async fn cancel_if_owned(&self, app_id: Option<&str>, response_id: &str) -> bool {
        let jobs = self.jobs.lock().await;
        match jobs.get(response_id) {
            Some(entry)
                if app_id.is_none_or(|app_id| entry.snapshot.app_id == app_id)
                    && !matches!(
                        entry.snapshot.state,
                        JobState::Succeeded
                            | JobState::Failed
                            | JobState::Cancelled
                            | JobState::Expired
                    ) =>
            {
                entry.cancellation.cancel();
                true
            }
            _ => false,
        }
    }

    pub async fn metrics(&self) -> MetricsSnapshot {
        let mut queues = BTreeMap::new();
        for (id, scheduler) in &self.schedulers {
            if let Ok(snapshot) = scheduler.snapshot().await {
                queues.insert(id.clone(), snapshot);
            }
        }
        self.metrics.snapshot(queues)
    }

    /// Operator-facing accounting view. It intentionally exposes metadata and
    /// normalized usage only—never request bodies or provider credentials.
    pub fn budget_snapshot(&self) -> Result<BudgetSnapshot, RuntimeError> {
        let (usage_ledger, active_reservations) = match &self.store {
            Some(store) => (store.usage_entries()?, store.active_reservations()?),
            None => (Vec::new(), Vec::new()),
        };
        Ok(BudgetSnapshot {
            quota: self.config.quota.clone(),
            usage_ledger,
            active_reservations,
        })
    }

    /// Safe, operator-visible provider inventory. Credentials are represented
    /// only by `configured`; environment variable names and values remain out
    /// of this control response.
    pub fn provider_snapshots(&self) -> Vec<ProviderSnapshot> {
        let unavailable = self.health.unavailable_providers();
        self.config
            .providers
            .iter()
            .map(|(id, provider)| {
                let deployments =
                    self.config
                        .deployments
                        .iter()
                        .filter(|(_, deployment)| deployment.provider == *id)
                        .map(|(deployment_id, deployment)| {
                            let build = &self.config.model_builds[&deployment.build];
                            let profile = &self.config.model_profiles[&build.profile];
                            let ratings = profile
                                .ratings
                                .iter()
                                .filter(|(intent_id, _)| {
                                    self.config.intents.get(*intent_id).is_some_and(|intent| {
                                        intent.input_modalities.iter().all(|modality| {
                                            build.input_modalities.contains(modality)
                                        }) && intent.output_modalities.iter().all(|modality| {
                                            build.output_modalities.contains(modality)
                                        }) && intent
                                            .required_features
                                            .iter()
                                            .all(|feature| build.features.contains(feature))
                                    })
                                })
                                .map(|(intent_id, rating)| (intent_id.clone(), rating.clone()))
                                .collect();
                            ProviderDeploymentSnapshot {
                                id: deployment_id.clone(),
                                build: deployment.build.clone(),
                                model_profile: build.profile.clone(),
                                model_family: profile.family.clone(),
                                model: if std::path::Path::new(&build.model_id).is_absolute() {
                                    profile.family.clone()
                                } else {
                                    build.model_id.clone()
                                },
                                resource_class: deployment.resource_class,
                                supported_efforts: deployment.supported_efforts.clone(),
                                execution_modes: deployment.supported_execution_modes.clone(),
                                ratings,
                            }
                        })
                        .collect::<Vec<_>>();
                let mut execution_modes = self
                    .config
                    .deployments
                    .values()
                    .filter(|deployment| deployment.provider == *id)
                    .flat_map(|deployment| deployment.supported_execution_modes.iter().copied())
                    .collect::<BTreeSet<_>>();
                if matches!(
                    provider.capability_profile.protocol,
                    ProviderProtocol::Responses | ProviderProtocol::CodexAppServer
                ) && provider
                    .capability_profile
                    .supports(infer_core::ProviderCapability::Streaming)
                {
                    execution_modes.insert(ExecutionMode::ServerStream);
                }
                ProviderSnapshot {
                    id: id.clone(),
                    kind: provider.kind.clone(),
                    access_class: provider.access_class,
                    placement: provider.placement,
                    configured: provider.is_configured(),
                    max_concurrency: provider.max_concurrency,
                    max_queue: provider.max_queue,
                    capability_profile: provider.capability_profile.clone(),
                    execution_modes,
                    deployments,
                    circuit_open: unavailable.contains(id),
                }
            })
            .collect()
    }

    /// Runs an explicit, real request compatibility probe against one configured
    /// Responses provider. This is intentionally operator-triggered because it
    /// can consume provider quota.
    pub async fn probe_provider(
        &self,
        provider_id: &str,
    ) -> Result<ProviderProbeReport, RuntimeError> {
        let config = self
            .config
            .providers
            .get(provider_id)
            .ok_or_else(|| RuntimeError::ProviderUnavailable(provider_id.into()))?;
        if !matches!(
            config.capability_profile.protocol,
            ProviderProtocol::Responses | ProviderProtocol::CodexAppServer
        ) {
            return Err(RuntimeError::ProviderProbeUnsupported(provider_id.into()));
        }
        let deployment = self
            .config
            .deployments
            .values()
            .find(|deployment| deployment.provider == provider_id)
            .ok_or_else(|| RuntimeError::ProviderProbeModelMissing(provider_id.into()))?;
        let model = self
            .config
            .model_builds
            .get(&deployment.build)
            .map(|build| build.model_id.clone())
            .ok_or_else(|| RuntimeError::ProviderProbeModelMissing(provider_id.into()))?;
        let provider = self.provider(provider_id)?;
        if config.capability_profile.protocol == ProviderProtocol::CodexAppServer {
            Ok(probe_responses_provider_with_effort(
                provider.as_ref(),
                &model,
                &config.capability_profile,
                deployment.supported_efforts.first().copied(),
            )
            .await)
        } else {
            Ok(
                probe_responses_provider(provider.as_ref(), &model, &config.capability_profile)
                    .await,
            )
        }
    }

    /// Returns a provider-native dynamic model group without widening routing
    /// admission. This is an operator observation, not a registry mutation.
    pub async fn provider_model_catalog(
        &self,
        provider_id: &str,
    ) -> Result<ProviderModelCatalog, RuntimeError> {
        let provider = self.provider(provider_id)?;
        provider
            .model_catalog()
            .await?
            .ok_or_else(|| RuntimeError::ProviderProbeUnsupported(provider_id.into()))
    }

    pub async fn execute_audio(
        self: &Arc<Self>,
        app_id: &str,
        request: AudioExecutionRequest,
    ) -> Result<AudioRuntimeResult, RuntimeError> {
        request.validate()?;
        if let AudioExecutionRequest::Speech(speech) = &request {
            self.authorize_speech_voice(app_id, speech)?;
        }
        let logical_model = request.model().to_owned();
        let constraints = request.constraints()?;
        let (expected_data_plane, input_modalities) = match &request {
            AudioExecutionRequest::Transcription(_) => {
                ("audio.transcription", BTreeSet::from([Modality::Audio]))
            }
            AudioExecutionRequest::Alignment(_) => (
                "audio.alignment",
                BTreeSet::from([Modality::Audio, Modality::Text]),
            ),
            AudioExecutionRequest::Speech(_) => ("audio.speech", BTreeSet::from([Modality::Text])),
            AudioExecutionRequest::VoiceClone(_) => (
                "audio.voice_clone",
                BTreeSet::from([Modality::Audio, Modality::Text]),
            ),
        };
        let prepared = self
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &logical_model,
                    constraints,
                    execution_requirements: ExecutionRequirements {
                        input_modalities,
                        execution_mode: ExecutionMode::Unary,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "audio",
                    expected_data_plane,
                    durable_payload: None,
                },
            )
            .await?;
        let _resource_reservation = self.reserve_resource(&prepared).await?;
        let _permit = self.acquire(&prepared).await?;
        self.mark(&prepared.job_id, JobState::Running, None).await?;
        let attempt_number = self
            .begin_attempt(&prepared, AttemptTrigger::Initial)
            .await?;
        let executor = self.audio_executor(&prepared.provider_id)?;
        let upstream = executor.execute(&prepared.physical_model, request);
        let result = match prepared.deadline {
            Some(deadline) => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = timeout_at(deadline, upstream) => {
                    result.map_err(|_| RuntimeError::DeadlineExpired)?.map_err(RuntimeError::Provider)
                }
            },
            None => tokio::select! {
                _ = prepared.cancellation.cancelled() => Err(RuntimeError::Cancelled),
                result = upstream => result.map_err(RuntimeError::Provider),
            },
        };
        match result {
            Ok(mut output) => {
                self.health.record_success(&prepared.provider_id);
                let usage = match &output {
                    AudioExecutionOutput::Json(value) => response_usage(value),
                    AudioExecutionOutput::Audio { .. } => None,
                };
                self.finish_attempt(
                    &prepared,
                    attempt_number,
                    AttemptOutcome::Succeeded,
                    None,
                    None,
                    usage,
                )
                .await?;
                if let AudioExecutionOutput::Json(value) = &mut output {
                    normalize_audio_json(value, &prepared.job_id, &logical_model);
                }
                self.mark(&prepared.job_id, JobState::Succeeded, None)
                    .await?;
                self.metrics.succeeded();
                Ok(AudioRuntimeResult {
                    job_id: prepared.job_id,
                    logical_model,
                    output,
                })
            }
            Err(RuntimeError::Cancelled) => {
                self.finish_attempt(
                    &prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("cancelled".into()),
                    Some("audio execution was cancelled".into()),
                    None,
                )
                .await?;
                self.mark(&prepared.job_id, JobState::Cancelled, None)
                    .await?;
                self.metrics.cancelled();
                Err(RuntimeError::Cancelled)
            }
            Err(RuntimeError::DeadlineExpired) => {
                self.finish_attempt(
                    &prepared,
                    attempt_number,
                    AttemptOutcome::Failed,
                    Some("deadline_exceeded".into()),
                    Some("request deadline expired during audio execution".into()),
                    None,
                )
                .await?;
                self.mark(
                    &prepared.job_id,
                    JobState::Expired,
                    Some("request deadline expired during audio execution".into()),
                )
                .await?;
                self.metrics.expired();
                Err(RuntimeError::DeadlineExpired)
            }
            Err(error) => {
                if let RuntimeError::Provider(provider) = &error {
                    self.health.record_failure(&prepared.provider_id, provider);
                    self.finish_attempt(
                        &prepared,
                        attempt_number,
                        AttemptOutcome::Failed,
                        Some(attempt_policy::kind_code(provider.kind()).into()),
                        Some(provider.to_string()),
                        None,
                    )
                    .await?;
                }
                self.mark(&prepared.job_id, JobState::Failed, Some(error.to_string()))
                    .await?;
                self.metrics.failed();
                Err(error)
            }
        }
    }

    pub(crate) fn authorize_speech_voice(
        &self,
        app_id: &str,
        request: &infer_core::SpeechRequest,
    ) -> Result<(), RuntimeError> {
        if request.model != "speech.synthesize" {
            return Ok(());
        }
        let app = self
            .config
            .apps
            .get(app_id)
            .ok_or_else(|| RuntimeError::UnknownApp(app_id.to_owned()))?;
        let voice = request.voice.as_deref().unwrap_or_default();
        if app.allows_speech_voice(voice) {
            Ok(())
        } else {
            Err(RuntimeError::OverrideNotAllowed { field: "voice" })
        }
    }

    async fn prepare_job(
        &self,
        app_id: &str,
        preparation: JobPreparation<'_>,
    ) -> Result<PreparedRun, RuntimeError> {
        let JobPreparation {
            logical_model,
            constraints,
            execution_requirements,
            reasoning_effort,
            estimated_tokens,
            id_prefix,
            expected_data_plane,
            durable_payload,
        } = preparation;
        let app = self
            .config
            .apps
            .get(app_id)
            .ok_or_else(|| RuntimeError::UnknownApp(app_id.to_owned()))?;
        let intent = self
            .config
            .intent(logical_model)
            .ok_or_else(|| RuntimeError::UnknownIntent(logical_model.to_owned()))?;
        if !app.allows_intent(logical_model) {
            return Err(RuntimeError::IntentNotAllowed {
                app_id: app_id.to_owned(),
                intent: logical_model.to_owned(),
            });
        }
        if execution_requirements
            .provider_capabilities
            .contains(&ProviderCapability::WebSearch)
            && !app.allows_builtin_tool(BuiltinTool::WebSearch)
        {
            return Err(RuntimeError::OverrideNotAllowed {
                field: "tools.web_search",
            });
        }
        if intent.data_plane != expected_data_plane {
            return Err(RuntimeError::DataPlaneMismatch {
                intent: logical_model.to_owned(),
                expected: expected_data_plane.to_owned(),
                actual: intent.data_plane.clone(),
            });
        }
        validate_overrides(app, &constraints)?;
        let policy_name = effective_policy(&self.config, app, intent, &constraints)?;
        let profile = self
            .config
            .profiles
            .get(&policy_name)
            .expect("validated profile");
        let unavailable_providers = self.health.unavailable_providers();
        let unavailable_deployments = self.resources.unavailable_deployments().await;
        let plan = plan_candidates(
            &self.config,
            logical_model,
            intent,
            profile,
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &execution_requirements,
                reasoning_effort,
                allowed_provider_access_classes: &app.allowed_provider_access_classes,
                allowed_cloud_input_modalities: &app.allowed_cloud_input_modalities,
                unavailable_providers: &unavailable_providers,
                unavailable_deployments: &unavailable_deployments,
            },
        );
        let fallback = constraints.fallback.unwrap_or(Fallback::None);
        let mut targets = plan.candidates.clone();
        if fallback == Fallback::AllowLowerCapability {
            targets.extend(plan.lower_capability_candidates.iter().cloned());
        }
        let candidate = targets.first().cloned().ok_or(RuntimeError::NoCandidate)?;
        if fallback == Fallback::None {
            targets.truncate(1);
        }
        let admission_permit = self
            .app_admission
            .try_admit(app_id)
            .map_err(|_| RuntimeError::AppQueueFull)?;
        let job_id = format!("{id_prefix}_{}", Uuid::new_v4().simple());
        let cancellation = CancellationToken::new();
        let priority = constraints.priority.unwrap_or(Priority::Normal);
        let submitted_at = Instant::now();
        let deadline = constraints
            .deadline_ms
            .map(|milliseconds| submitted_at + Duration::from_millis(milliseconds));
        let snapshot = JobSnapshot {
            id: job_id.clone(),
            app_id: app_id.to_owned(),
            intent: logical_model.to_owned(),
            provider: candidate.provider_id.clone(),
            deployment: candidate.deployment_id.clone(),
            model_profile: candidate.model_profile_id.clone(),
            model_build: candidate.build_id.clone(),
            physical_model: candidate.physical_model.clone(),
            placement: candidate.placement,
            capability_level: candidate.capability_level,
            evaluation_status: candidate.evaluation_status,
            resource_class: candidate.resource_class,
            state: JobState::Queued,
            policy: policy_name,
            priority,
            constraints,
            routing: plan.decision,
            attempts: vec![],
            error: None,
        };
        let admitted_event = audit_event(
            "job.admitted",
            json!({
                "intent": snapshot.intent,
                "policy": snapshot.policy,
                "provider": snapshot.provider,
                "deployment": snapshot.deployment,
                "durable_background": durable_payload.is_some(),
            }),
        );
        match (durable_payload, &self.store) {
            (Some(payload), Some(store)) => {
                store.persist_background_job_with_event(&snapshot, payload, admitted_event)?;
            }
            (Some(_), None) => return Err(RuntimeError::BackgroundDisabled),
            (None, _) => self.persist_snapshot(&snapshot, admitted_event)?,
        }
        self.jobs.lock().await.insert(
            job_id.clone(),
            JobEntry {
                snapshot,
                cancellation: cancellation.clone(),
                admission_permit: Some(admission_permit),
            },
        );
        self.metrics.submitted();
        Ok(PreparedRun {
            job_id,
            app_id: app_id.to_owned(),
            logical_model: logical_model.to_owned(),
            provider_id: candidate.provider_id,
            deployment_id: candidate.deployment_id,
            physical_model: candidate.physical_model,
            estimated_cost_usd: candidate.estimated_cost_usd,
            estimated_tokens,
            cancellation,
            priority,
            submitted_at,
            deadline,
            targets,
            existing_attempts: 0,
            recovered: false,
        })
    }

    fn provider(&self, id: &str) -> Result<DynProvider, RuntimeError> {
        self.providers
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }

    async fn reserve_resource(
        &self,
        prepared: &PreparedRun,
    ) -> Result<Option<ModelReservation>, RuntimeError> {
        match self.resources.reserve_model(&prepared.deployment_id) {
            Ok(reservation) => Ok(reservation),
            Err(error) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some("local model lifecycle is transitioning".into()),
                )
                .await?;
                self.metrics.failed();
                Err(RuntimeError::Resource(error))
            }
        }
    }
    fn audio_executor(&self, id: &str) -> Result<DynAudioExecutor, RuntimeError> {
        self.audio_executors
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProviderUnavailable(id.to_owned()))
    }
    async fn acquire(&self, prepared: &PreparedRun) -> Result<ScheduledPermit, RuntimeError> {
        let scheduler = self
            .schedulers
            .get(&prepared.provider_id)
            .ok_or_else(|| RuntimeError::ProviderUnavailable(prepared.provider_id.clone()))?;
        match scheduler
            .acquire(
                prepared.job_id.clone(),
                prepared.priority,
                prepared.deadline,
                prepared.cancellation.clone(),
            )
            .await
        {
            Ok(permit) => {
                self.metrics
                    .dispatched(prepared.submitted_at.elapsed().as_millis() as u64);
                Ok(permit)
            }
            Err(SchedulerError::QueueFull) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some("provider queue is full".into()),
                )
                .await?;
                self.metrics.queue_rejected();
                Err(RuntimeError::QueueFull)
            }
            Err(SchedulerError::DeadlineExpired) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Expired,
                    Some("request deadline expired before execution started".into()),
                )
                .await?;
                self.metrics.expired();
                Err(RuntimeError::DeadlineExpired)
            }
            Err(SchedulerError::Cancelled) => {
                self.mark(&prepared.job_id, JobState::Cancelled, None)
                    .await?;
                self.metrics.cancelled();
                Err(RuntimeError::Cancelled)
            }
            Err(SchedulerError::Unavailable) => Err(RuntimeError::ProviderUnavailable(
                prepared.provider_id.clone(),
            )),
        }
    }
    fn persist_snapshot(
        &self,
        snapshot: &JobSnapshot,
        event: AuditEventInput,
    ) -> Result<(), RuntimeError> {
        if let Some(store) = &self.store {
            store.persist_job_with_event(snapshot, event)?;
        }
        Ok(())
    }

    async fn mark(
        &self,
        response_id: &str,
        state: JobState,
        error: Option<String>,
    ) -> Result<(), RuntimeError> {
        let snapshot = {
            let mut jobs = self.jobs.lock().await;
            let Some(entry) = jobs.get_mut(response_id) else {
                return Ok(());
            };
            entry.snapshot.state = state;
            entry.snapshot.error = error;
            if matches!(
                state,
                JobState::Succeeded | JobState::Failed | JobState::Cancelled | JobState::Expired
            ) {
                entry.admission_permit.take();
            }
            entry.snapshot.clone()
        };
        self.persist_snapshot(
            &snapshot,
            audit_event(
                "job.state_changed",
                json!({ "state": job_state_code(state) }),
            ),
        )
    }

    async fn start_attempt(
        &self,
        prepared: &PreparedRun,
        trigger: AttemptTrigger,
    ) -> Result<usize, RuntimeError> {
        let (number, snapshot) = {
            let mut jobs = self.jobs.lock().await;
            let entry = jobs
                .get_mut(&prepared.job_id)
                .expect("prepared Job must remain registered");
            let number = entry.snapshot.attempts.len() + 1;
            entry.snapshot.attempts.push(AttemptSnapshot {
                number,
                provider: prepared.provider_id.clone(),
                deployment: entry.snapshot.deployment.clone(),
                outcome: AttemptOutcome::Running,
                trigger,
                error_kind: None,
                error: None,
            });
            (number, entry.snapshot.clone())
        };
        self.persist_snapshot(
            &snapshot,
            audit_event(
                "attempt.opened",
                json!({
                    "number": number,
                    "trigger": attempt_trigger_code(trigger),
                    "provider": prepared.provider_id,
                    "deployment": prepared.deployment_id,
                }),
            ),
        )?;
        Ok(number)
    }

    fn reserve_attempt(
        &self,
        prepared: &PreparedRun,
        attempt_number: usize,
    ) -> Result<(), RuntimeError> {
        if let Some(store) = &self.store {
            store.reserve_attempt(&AttemptReservation {
                job_id: prepared.job_id.clone(),
                attempt_number,
                app_id: prepared.app_id.clone(),
                provider: prepared.provider_id.clone(),
                deployment: prepared.deployment_id.clone(),
                amount_usd: prepared.estimated_cost_usd,
                estimated_tokens: prepared.estimated_tokens,
            })?;
        }
        Ok(())
    }

    async fn begin_attempt(
        &self,
        prepared: &PreparedRun,
        trigger: AttemptTrigger,
    ) -> Result<usize, RuntimeError> {
        let attempt_number = self.start_attempt(prepared, trigger).await?;
        if let Err(error) = self.reserve_attempt(prepared, attempt_number) {
            // The Attempt never reached a provider. Persist a terminal audit
            // result, but do not create a ledger entry because no reservation
            // could have been acquired.
            self.finish_attempt(
                prepared,
                attempt_number,
                AttemptOutcome::Failed,
                Some("quota_rejected".into()),
                Some(error.to_string()),
                None,
            )
            .await?;
            self.mark(
                &prepared.job_id,
                JobState::Failed,
                Some("quota reservation was rejected".into()),
            )
            .await?;
            self.metrics.failed();
            return Err(error);
        }
        Ok(attempt_number)
    }

    async fn finish_attempt(
        &self,
        prepared: &PreparedRun,
        attempt_number: usize,
        outcome: AttemptOutcome,
        error_kind: Option<String>,
        error: Option<String>,
        usage: Option<UsageTokens>,
    ) -> Result<(), RuntimeError> {
        let snapshot = {
            let mut jobs = self.jobs.lock().await;
            let entry = jobs
                .get_mut(&prepared.job_id)
                .expect("prepared Job must remain registered");
            let attempt = entry
                .snapshot
                .attempts
                .last_mut()
                .expect("started Attempt must remain registered");
            debug_assert_eq!(attempt.number, attempt_number);
            attempt.outcome = outcome;
            attempt.error_kind = error_kind;
            attempt.error = error;
            entry.snapshot.clone()
        };
        if let Some(store) = &self.store {
            store.persist_job_and_settle_with_event(
                &snapshot,
                UsageLedgerEntry {
                    job_id: prepared.job_id.clone(),
                    attempt_number,
                    app_id: prepared.app_id.clone(),
                    provider: prepared.provider_id.clone(),
                    deployment: prepared.deployment_id.clone(),
                    outcome: attempt_outcome_code(outcome).into(),
                    amount_usd: prepared.estimated_cost_usd,
                    // A deployment currently supplies a whole-request cost
                    // estimate, not token prices. Even with provider token
                    // counts the USD amount remains explicitly estimated.
                    estimated: true,
                    input_tokens: usage.map(|value| value.input_tokens),
                    output_tokens: usage.map(|value| value.output_tokens),
                    total_tokens: usage.map(|value| value.total_tokens),
                },
                audit_event(
                    "attempt.finished",
                    json!({
                        "number": attempt_number,
                        "outcome": attempt_outcome_code(outcome),
                        "error_kind": snapshot.attempts.last().and_then(|attempt| attempt.error_kind.as_deref()),
                    }),
                ),
            )?;
        }
        Ok(())
    }

    async fn set_attempt_target(
        &self,
        prepared: &mut PreparedRun,
        target: &Candidate,
    ) -> Result<(), RuntimeError> {
        prepared.provider_id = target.provider_id.clone();
        prepared.deployment_id = target.deployment_id.clone();
        prepared.physical_model = target.physical_model.clone();
        prepared.estimated_cost_usd = target.estimated_cost_usd;
        let snapshot = {
            let mut jobs = self.jobs.lock().await;
            let entry = jobs
                .get_mut(&prepared.job_id)
                .expect("prepared Job must remain registered");
            entry.snapshot.provider = target.provider_id.clone();
            entry.snapshot.deployment = target.deployment_id.clone();
            entry.snapshot.model_profile = target.model_profile_id.clone();
            entry.snapshot.model_build = target.build_id.clone();
            entry.snapshot.physical_model = target.physical_model.clone();
            entry.snapshot.placement = target.placement;
            entry.snapshot.capability_level = target.capability_level;
            entry.snapshot.evaluation_status = target.evaluation_status;
            entry.snapshot.resource_class = target.resource_class;
            entry.snapshot.clone()
        };
        self.persist_snapshot(
            &snapshot,
            audit_event(
                "candidate.selected",
                json!({
                    "provider": target.provider_id,
                    "deployment": target.deployment_id,
                    "model_profile": target.model_profile_id,
                    "model_build": target.build_id,
                }),
            ),
        )
    }

    fn normalize_stream(
        self: &Arc<Self>,
        prepared: PreparedRun,
        attempt_number: usize,
        permit: ScheduledPermit,
        resource_reservation: Option<ModelReservation>,
        mut upstream: infer_provider::ProviderByteStream,
    ) -> RuntimeByteStream {
        let runtime = Arc::clone(self);
        let output = stream! {
            let _permit = permit;
            let _resource_reservation = resource_reservation;
            let mut buffer = Vec::new();
            let mut failed = false;
            let mut attempt_finished = false;
            let mut usage = None;
            loop {
                let next = tokio::select! {
                    _ = prepared.cancellation.cancelled() => {
                        let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Failed, Some("cancelled".into()), Some("response was cancelled".into()), None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Cancelled, None).await;
                        runtime.metrics.cancelled();
                        yield sse_error("cancelled", "response was cancelled");
                        return;
                    }
                    _ = async { if let Some(deadline) = prepared.deadline { tokio::time::sleep_until(deadline).await; } }, if prepared.deadline.is_some() => {
                        let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Failed, Some("deadline_exceeded".into()), Some("request deadline expired".into()), None).await;
                        let _ = runtime.mark(&prepared.job_id, JobState::Expired, Some("request deadline expired while streaming".into())).await;
                        runtime.metrics.expired();
                        yield sse_error("deadline_exceeded", "request deadline expired");
                        return;
                    }
                    item = upstream.next() => item,
                };
                match next {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        while let Some(end) = find_frame_end(&buffer) {
                            let frame: Vec<u8> = buffer.drain(..end).collect();
                            let normalized = normalize_frame(frame, &prepared.job_id, &prepared.logical_model);
                            if normalized.failed { failed = true; }
                            if normalized.usage.is_some() { usage = normalized.usage; }
                            yield Bytes::from(normalized.bytes);
                        }
                    }
                    Some(Err(error)) => {
                        runtime.health.record_failure(&prepared.provider_id, &error);
                        let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Failed, Some(attempt_policy::kind_code(error.kind()).into()), Some(error.to_string()), None).await;
                        attempt_finished = true;
                        failed = true;
                        yield sse_error("upstream_error", &error.to_string());
                        break;
                    }
                    None => break,
                }
            }
            if !buffer.is_empty() {
                let normalized = normalize_frame(buffer, &prepared.job_id, &prepared.logical_model);
                if normalized.usage.is_some() { usage = normalized.usage; }
                yield Bytes::from(normalized.bytes);
            }
            if failed {
                if !attempt_finished { let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Failed, Some("protocol".into()), Some("provider returned failed stream".into()), None).await; }
                let _ = runtime.mark(&prepared.job_id, JobState::Failed, Some("provider returned failed stream".into())).await;
                runtime.metrics.failed();
            } else {
                runtime.health.record_success(&prepared.provider_id);
                let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Succeeded, None, None, usage).await;
                let _ = runtime.mark(&prepared.job_id, JobState::Succeeded, None).await;
                runtime.metrics.succeeded();
            }
        };
        Box::pin(output)
    }
}

fn effective_policy(
    config: &RuntimeConfig,
    app: &AppConfig,
    intent: &IntentProfile,
    constraints: &RequestConstraints,
) -> Result<String, RuntimeError> {
    let selected = constraints
        .policy
        .as_ref()
        .or(intent.default_policy.as_ref())
        .or(app.default_policy.as_ref())
        .unwrap_or(&config.defaults.policy);
    if constraints.policy.is_some()
        && !app
            .allowed_policies
            .iter()
            .any(|profile| profile == selected)
    {
        return Err(RuntimeError::PolicyNotAllowed(selected.clone()));
    }
    Ok(selected.clone())
}

fn validate_overrides(
    app: &AppConfig,
    constraints: &RequestConstraints,
) -> Result<(), RuntimeError> {
    let allowed = &app.request_overrides;
    if constraints
        .provider_access_class
        .is_some_and(|value| !app.allows_provider_access(value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.provider_access_class",
        });
    }
    if constraints
        .priority
        .is_some_and(|value| !allowed.priority.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.priority",
        });
    }
    if constraints
        .placement
        .is_some_and(|value| !allowed.placement.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.placement",
        });
    }
    if constraints
        .prefer
        .is_some_and(|value| !allowed.prefer.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.prefer",
        });
    }
    if constraints.offline_required == Some(true) && !allowed.offline_required {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.offline_required",
        });
    }
    if constraints
        .capability_floor
        .is_some_and(|value| !allowed.capability_floor.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.capability_floor",
        });
    }
    if constraints
        .latency
        .is_some_and(|value| !allowed.latency.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.latency",
        });
    }
    if constraints
        .fallback
        .is_some_and(|value| !allowed.fallback.contains(&value))
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.fallback",
        });
    }
    if let (Some(value), Some(range)) = (constraints.max_cost_usd, &allowed.max_cost_usd)
        && (value < range.min || value > range.max)
    {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.max_cost_usd",
        });
    }
    if constraints.max_cost_usd.is_some() && allowed.max_cost_usd.is_none() {
        return Err(RuntimeError::OverrideNotAllowed {
            field: "infer.max_cost_usd",
        });
    }
    Ok(())
}

fn normalize_response(mut value: Value, response_id: &str, logical_model: &str) -> Value {
    if !value.is_object() {
        value = json!({ "output": value });
    }
    let object = value
        .as_object_mut()
        .expect("value was normalized to an object");
    object.insert("id".into(), Value::String(response_id.into()));
    object.insert("object".into(), Value::String("response".into()));
    object.insert("model".into(), Value::String(logical_model.into()));
    object.insert("created_at".into(), json!(unix_time_ms() / 1_000));
    object
        .entry("parallel_tool_calls")
        .or_insert(Value::Bool(true));
    object
        .entry("tool_choice")
        .or_insert_with(|| Value::String("auto".into()));
    object.entry("tools").or_insert_with(|| json!([]));
    value
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Conservative token estimate used only for admission reservations. Provider
/// usage replaces the token counts when it is available; USD stays estimated
/// until deployments declare token pricing.
fn estimate_response_tokens(request: &ResponsesRequest) -> u64 {
    let input_bytes = serde_json::to_vec(&request.input)
        .map(|value| value.len() as u64)
        .unwrap_or_default();
    let instruction_bytes = request
        .instructions
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok())
        .map(|value| value.len() as u64)
        .unwrap_or_default();
    let input_estimate = (input_bytes + instruction_bytes).div_ceil(4);
    input_estimate.saturating_add(request.max_output_tokens.unwrap_or(512).into())
}

fn response_usage(value: &Value) -> Option<UsageTokens> {
    let usage = value.get("usage")?.as_object()?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))?
        .as_u64()?;
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))?
        .as_u64()?;
    Some(UsageTokens {
        input_tokens,
        output_tokens,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
    })
}

fn stream_event_usage(event: &Value) -> Option<UsageTokens> {
    response_usage(event).or_else(|| event.get("response").and_then(response_usage))
}

fn attempt_outcome_code(outcome: AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Running => "running",
        AttemptOutcome::Succeeded => "succeeded",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Interrupted => "interrupted",
    }
}

fn attempt_trigger_code(trigger: AttemptTrigger) -> &'static str {
    match trigger {
        AttemptTrigger::Initial => "initial",
        AttemptTrigger::Retry => "retry",
        AttemptTrigger::Fallback => "fallback",
        AttemptTrigger::Recovery => "recovery",
    }
}

fn job_state_code(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Expired => "expired",
    }
}

fn audit_event(kind: &str, details: Value) -> AuditEventInput {
    AuditEventInput {
        kind: kind.into(),
        details,
    }
}

fn normalize_audio_json(value: &mut Value, job_id: &str, logical_model: &str) {
    if !value.is_object() {
        *value = json!({ "output": value.take() });
    }
    let object = value
        .as_object_mut()
        .expect("audio value was normalized to an object");
    object.insert("id".into(), Value::String(job_id.into()));
    object.insert("model".into(), Value::String(logical_model.into()));
}

struct NormalizedFrame {
    bytes: Vec<u8>,
    failed: bool,
    usage: Option<UsageTokens>,
}

fn normalize_frame(frame: Vec<u8>, response_id: &str, logical_model: &str) -> NormalizedFrame {
    let Ok(text) = String::from_utf8(frame.clone()) else {
        return NormalizedFrame {
            bytes: frame,
            failed: false,
            usage: None,
        };
    };
    let mut failed = false;
    let mut usage = None;
    let mut output = String::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            match serde_json::from_str::<Value>(data) {
                Ok(mut event) => {
                    if let Some(tokens) = stream_event_usage(&event) {
                        usage = Some(tokens);
                    }
                    let is_failed = event
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "response.failed");
                    failed |= is_failed;
                    if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
                    {
                        response.insert("id".into(), Value::String(response_id.into()));
                        response.insert("model".into(), Value::String(logical_model.into()));
                    }
                    output.push_str("data: ");
                    output.push_str(&event.to_string());
                    output.push('\n');
                }
                Err(_) => {
                    output.push_str(line);
                    output.push('\n');
                }
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push('\n');
    NormalizedFrame {
        bytes: output.into_bytes(),
        failed,
        usage,
    }
}

fn find_frame_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

fn sse_error(code: &str, message: &str) -> Bytes {
    Bytes::from(format!(
        "event: error\ndata: {}\n\n",
        json!({"type":"error","error":{"code":code,"message":message}})
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use infer_core::{
        CapabilityLevel, LocalInventoryConfig, LocalInventoryKind, QuotaLimitConfig, RuntimeConfig,
    };
    use infer_payload::EncryptedPayloadSpool;
    use infer_provider::Provider;
    use infer_resource::EvictionApplyRequest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeProvider {
        id: String,
        seen_model: Mutex<Option<String>>,
    }

    struct UnavailableProvider {
        id: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for UnavailableProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Upstream {
                status: 503,
                body: "unavailable".into(),
            })
        }
        async fn execute_stream(
            &self,
            _request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Upstream {
                status: 503,
                body: "unavailable".into(),
            })
        }
    }
    #[async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
            *self.seen_model.lock().await = Some(request.model);
            Ok(
                json!({"id":"upstream", "output":[{"type":"message","content":[{"type":"output_text","text":"summary"}]}]}),
            )
        }
        async fn execute_stream(
            &self,
            request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            *self.seen_model.lock().await = Some(request.model);
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from(
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"upstream\",\"model\":\"qwen\"}}\n\n",
            ))])))
        }
    }

    struct BreakingStreamProvider {
        id: String,
    }

    struct HangingProvider {
        id: String,
        started: Arc<tokio::sync::Notify>,
    }

    struct BackgroundProvider {
        id: String,
        seen_background: Mutex<Option<bool>>,
    }

    #[async_trait]
    impl Provider for BackgroundProvider {
        fn id(&self) -> &str {
            &self.id
        }

        async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
            *self.seen_background.lock().await = Some(request.background);
            Ok(json!({
                "output": [{
                    "type":"message",
                    "content":[{"type":"output_text","text":"durable summary"}]
                }]
            }))
        }

        async fn execute_stream(
            &self,
            _request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            unreachable!("background execution is non-streaming")
        }
    }

    struct GateProvider {
        id: String,
        calls: AtomicUsize,
        first_started: Arc<tokio::sync::Notify>,
        release_first: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Provider for BreakingStreamProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
            unreachable!("streaming test")
        }
        async fn execute_stream(
            &self,
            _request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(Bytes::from(
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                )),
                Err(ProviderError::Protocol("broken stream".into())),
            ])))
        }
    }

    #[async_trait]
    impl Provider for HangingProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
            self.started.notify_waiters();
            std::future::pending().await
        }
        async fn execute_stream(
            &self,
            _request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            unreachable!("non-streaming cancellation test")
        }
    }

    #[async_trait]
    impl Provider for GateProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.first_started.notify_waiters();
                self.release_first.notified().await;
            }
            Ok(json!({
                "output": [{"type":"message","content":[{"type":"output_text","text":"ok"}]}]
            }))
        }
        async fn execute_stream(
            &self,
            _request: ResponsesRequest,
        ) -> Result<infer_provider::ProviderByteStream, ProviderError> {
            unreachable!("quota integration test is non-streaming")
        }
    }

    fn config() -> RuntimeConfig {
        toml::from_str(
            r#"
            [server]
            bind = "127.0.0.1:8787"
            [defaults]
            policy = "balanced"
            [providers.local]
            kind = "responses"
            base_url = "http://localhost:11434/v1"
            placement = "local"
            [providers.local.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses", "instructions", "streaming", "function_tools", "reasoning_effort", "temperature", "top_p", "max_output_tokens", "truncation", "metadata"]
            [providers.cloud]
            kind = "responses"
            base_url = "https://cloud.example/v1"
            placement = "cloud"
            [providers.cloud.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses", "instructions", "streaming", "function_tools", "reasoning_effort", "temperature", "top_p", "max_output_tokens", "truncation", "metadata"]
            [profiles.balanced]
            order = ["cost"]
            [intents."text.summarize"]
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_capability_floor = "foundational"
            [model_profiles.qwen]
            family = "qwen"
            [model_profiles.qwen.ratings."text.summarize"]
            level = "foundational"
            status = "benchmarked"
            eval_profile = "summary-v1"
            score = 0.8
            [model_builds.qwen_local]
            profile = "qwen"
            model_id = "qwen"
            input_modalities = ["text"]
            output_modalities = ["text"]
            [model_builds.qwen_cloud]
            profile = "qwen"
            model_id = "qwen-cloud"
            input_modalities = ["text"]
            output_modalities = ["text"]
            [deployments.qwen_local]
            provider = "local"
            build = "qwen_local"
            estimated_cost_usd = 0.0
            [deployments.qwen_cloud]
            provider = "cloud"
            build = "qwen_cloud"
            estimated_cost_usd = 1.0
            [apps.test-app]
            credential = { source = "environment", variable = "INFER_TEST_TOKEN" }
            max_pending_jobs = 2
            allowed_policies = ["balanced"]
            [apps.test-app.request_overrides]
            priority = ["background"]
            placement = ["local_only", "anywhere"]
            capability_floor = ["capable"]
            fallback = ["equivalent", "allow_lower_capability"]
        "#,
        )
        .unwrap()
    }

    fn summary_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        }
    }

    #[test]
    fn symbiont_capability_override_shape_allows_through_expert() {
        let mut config = config();
        let app = config.apps.get_mut("test-app").unwrap();
        app.request_overrides.capability_floor = vec![
            CapabilityLevel::Foundational,
            CapabilityLevel::Capable,
            CapabilityLevel::Advanced,
            CapabilityLevel::Expert,
        ];

        for capability in [
            CapabilityLevel::Foundational,
            CapabilityLevel::Capable,
            CapabilityLevel::Advanced,
            CapabilityLevel::Expert,
        ] {
            validate_overrides(
                app,
                &RequestConstraints {
                    capability_floor: Some(capability),
                    ..RequestConstraints::default()
                },
            )
            .unwrap();
        }

        assert!(matches!(
            validate_overrides(
                app,
                &RequestConstraints {
                    capability_floor: Some(CapabilityLevel::Exceptional),
                    ..RequestConstraints::default()
                },
            ),
            Err(RuntimeError::OverrideNotAllowed {
                field: "infer.capability_floor"
            })
        ));
    }

    fn resource_config() -> RuntimeConfig {
        let mut config = config();
        config.providers.get_mut("local").unwrap().local_inventory = Some(LocalInventoryConfig {
            kind: LocalInventoryKind::OllamaTags,
            endpoint: Some("http://127.0.0.1:11434".into()),
        });
        config
    }

    #[test]
    fn response_identity_fields_are_owned_by_the_runtime() {
        let response = normalize_response(
            json!({
                "id": "upstream",
                "object": "wrong",
                "created_at": "not-a-timestamp",
                "model": "physical",
                "output": []
            }),
            "resp_public",
            "text.summarize",
        );
        assert_eq!(response["id"], "resp_public");
        assert_eq!(response["object"], "response");
        assert_eq!(response["model"], "text.summarize");
        assert!(response["created_at"].is_i64());
    }

    #[tokio::test]
    async fn intent_profile_never_reaches_provider_as_model() {
        let config = config();
        config.validate().unwrap();
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), fake.clone() as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let response = runtime.execute("test-app", request).await.unwrap();
        assert!(response["id"].as_str().unwrap().starts_with("resp_"));
        assert_eq!(response["model"], "text.summarize");
        assert_eq!(fake.seen_model.lock().await.as_deref(), Some("qwen"));
    }

    #[tokio::test]
    async fn durable_runtime_settles_an_attempt_and_exposes_its_ledger() {
        let config = config();
        let store = Arc::new(
            Store::open_in_memory_with_quota(
                ConfigSnapshot::from_serializable(&config).unwrap(),
                QuotaLimits::from(&config.quota),
            )
            .unwrap(),
        );
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::from([("local".into(), fake as DynProvider)]),
            Some(store.clone()),
            AppCredentials::empty(),
        );
        let response = runtime
            .execute(
                "test-app",
                ResponsesRequest {
                    model: "text.summarize".into(),
                    input: Value::String("transcript".into()),
                    instructions: None,
                    stream: false,
                    background: false,
                    metadata: BTreeMap::new(),
                    tools: vec![],
                    tool_choice: None,
                    reasoning: None,
                    temperature: None,
                    top_p: None,
                    max_output_tokens: Some(24),
                    truncation: None,
                    store: None,
                    previous_response_id: None,
                    conversation: None,
                },
            )
            .await
            .unwrap();
        let job_id = response["id"].as_str().unwrap();
        let persisted = store.load_job(job_id).unwrap().unwrap();
        assert_eq!(persisted.state, JobState::Succeeded);
        assert_eq!(persisted.attempts[0].outcome, AttemptOutcome::Succeeded);
        let accounting = runtime.budget_snapshot().unwrap();
        assert_eq!(accounting.usage_ledger.len(), 1);
        assert!(accounting.usage_ledger[0].estimated);
        assert!(accounting.active_reservations.is_empty());
        let audit = runtime.audit_events(job_id).unwrap();
        assert!(audit.iter().any(|event| event.kind == "job.admitted"));
        assert!(audit.iter().any(|event| event.kind == "attempt.opened"));
        assert!(audit.iter().any(|event| event.kind == "attempt.finished"));
    }

    #[tokio::test]
    async fn quota_rejection_is_recorded_without_calling_the_provider() {
        let mut config = config();
        config
            .deployments
            .get_mut("qwen_local")
            .unwrap()
            .estimated_cost_usd = 0.10;
        config.quota.apps.insert(
            "test-app".into(),
            QuotaLimitConfig {
                max_usd: Some(0.05),
                ..QuotaLimitConfig::default()
            },
        );
        config.validate().unwrap();
        let store = Arc::new(
            Store::open_in_memory_with_quota(
                ConfigSnapshot::from_serializable(&config).unwrap(),
                QuotaLimits::from(&config.quota),
            )
            .unwrap(),
        );
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::from([("local".into(), fake.clone() as DynProvider)]),
            Some(store),
            AppCredentials::empty(),
        );
        let error = runtime
            .execute(
                "test-app",
                ResponsesRequest {
                    model: "text.summarize".into(),
                    input: Value::String("transcript".into()),
                    instructions: None,
                    stream: false,
                    background: false,
                    metadata: BTreeMap::new(),
                    tools: vec![],
                    tool_choice: None,
                    reasoning: None,
                    temperature: None,
                    top_p: None,
                    max_output_tokens: None,
                    truncation: None,
                    store: None,
                    previous_response_id: None,
                    conversation: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::QuotaExceeded { .. }));
        assert!(fake.seen_model.lock().await.is_none());
        let job_id = runtime.jobs.lock().await.keys().next().unwrap().clone();
        let snapshot = runtime.snapshot(&job_id).await.unwrap().unwrap();
        assert_eq!(snapshot.state, JobState::Failed);
        assert_eq!(
            snapshot.attempts[0].error_kind.as_deref(),
            Some("quota_rejected")
        );
        assert!(runtime.budget_snapshot().unwrap().usage_ledger.is_empty());
    }

    #[tokio::test]
    async fn concurrent_runtime_quota_rejection_never_reaches_the_dummy_provider() {
        let mut config = config();
        config.providers.get_mut("local").unwrap().max_concurrency = 2;
        config.quota.apps.insert(
            "test-app".into(),
            QuotaLimitConfig {
                max_concurrent_attempts: Some(1),
                ..QuotaLimitConfig::default()
            },
        );
        config.validate().unwrap();
        let store = Arc::new(
            Store::open_in_memory_with_quota(
                ConfigSnapshot::from_serializable(&config).unwrap(),
                QuotaLimits::from(&config.quota),
            )
            .unwrap(),
        );
        let provider = Arc::new(GateProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
            first_started: Arc::new(tokio::sync::Notify::new()),
            release_first: Arc::new(tokio::sync::Notify::new()),
        });
        let first_started = provider.first_started.clone().notified_owned();
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
            Some(store),
            AppCredentials::empty(),
        );
        let first = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.execute("test-app", summary_request()).await }
        });
        first_started.await;

        let rejected = runtime
            .execute("test-app", summary_request())
            .await
            .unwrap_err();
        assert!(matches!(rejected, RuntimeError::QuotaExceeded { .. }));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        provider.release_first.notify_one();
        first.await.unwrap().unwrap();
        assert!(
            runtime
                .budget_snapshot()
                .unwrap()
                .active_reservations
                .is_empty()
        );

        runtime
            .execute("test-app", summary_request())
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn streaming_response_exposes_the_public_intent() {
        let config = config();
        config.validate().unwrap();
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), fake as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: true,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let bytes: Vec<_> = runtime
            .execute_stream("test-app", request)
            .await
            .unwrap()
            .collect()
            .await;
        let response = String::from_utf8(bytes.concat().to_vec()).unwrap();
        assert!(response.contains("\"model\":\"text.summarize\""));
        assert!(!response.contains("\"model\":\"qwen\""));
        assert!(response.contains("\"id\":\"resp_"));
    }

    #[tokio::test]
    async fn application_specific_alias_is_not_a_runtime_intent() {
        let config = config();
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), fake.clone() as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "test-app.summary".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::UnknownIntent(intent)) if intent == "test-app.summary"
        ));
        assert!(fake.seen_model.lock().await.is_none());
    }

    #[tokio::test]
    async fn retries_then_falls_back_within_the_admission_plan() {
        let config = config();
        config.validate().unwrap();
        let unavailable = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let cloud = Arc::new(FakeProvider {
            id: "cloud".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([
                ("local".into(), unavailable.clone() as DynProvider),
                ("cloud".into(), cloud.clone() as DynProvider),
            ]),
        );
        let mut metadata = BTreeMap::new();
        metadata.insert("infer.fallback".into(), "equivalent".into());
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata,
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let response = runtime.execute("test-app", request).await.unwrap();
        assert_eq!(response["model"], "text.summarize");
        assert_eq!(unavailable.calls.load(Ordering::SeqCst), 2);
        assert_eq!(cloud.seen_model.lock().await.as_deref(), Some("qwen-cloud"));
        let snapshot = runtime
            .jobs
            .lock()
            .await
            .values()
            .next()
            .unwrap()
            .snapshot
            .clone();
        assert_eq!(snapshot.attempts.len(), 3);
        assert_eq!(snapshot.attempts[0].trigger, AttemptTrigger::Initial);
        assert_eq!(snapshot.attempts[1].trigger, AttemptTrigger::Retry);
        assert_eq!(snapshot.attempts[2].trigger, AttemptTrigger::Fallback);
        assert_eq!(snapshot.attempts[2].outcome, AttemptOutcome::Succeeded);
    }

    #[tokio::test]
    async fn local_only_failure_never_calls_the_cloud_fallback() {
        let config = config();
        let unavailable = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let cloud = Arc::new(FakeProvider {
            id: "cloud".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([
                ("local".into(), unavailable as DynProvider),
                ("cloud".into(), cloud.clone() as DynProvider),
            ]),
        );
        let metadata = BTreeMap::from([
            ("infer.fallback".into(), "equivalent".into()),
            ("infer.placement".into(), "local_only".into()),
        ]);
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata,
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::Provider(_))
        ));
        assert!(cloud.seen_model.lock().await.is_none());
    }

    #[tokio::test]
    async fn visible_stream_output_is_never_spliced_with_a_fallback() {
        let config = config();
        let local = Arc::new(BreakingStreamProvider { id: "local".into() });
        let cloud = Arc::new(FakeProvider {
            id: "cloud".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([
                ("local".into(), local as DynProvider),
                ("cloud".into(), cloud.clone() as DynProvider),
            ]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: true,
            background: false,
            metadata: BTreeMap::from([("infer.fallback".into(), "equivalent".into())]),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let chunks: Vec<_> = runtime
            .execute_stream("test-app", request)
            .await
            .unwrap()
            .collect()
            .await;
        let output = String::from_utf8(chunks.concat().to_vec()).unwrap();
        assert!(output.contains("partial"));
        assert!(output.contains("upstream_error"));
        assert!(cloud.seen_model.lock().await.is_none());
        let snapshot = runtime
            .jobs
            .lock()
            .await
            .values()
            .next()
            .unwrap()
            .snapshot
            .clone();
        assert_eq!(snapshot.attempts.len(), 1);
        assert_eq!(snapshot.attempts[0].outcome, AttemptOutcome::Failed);
    }

    #[tokio::test]
    async fn stream_setup_can_retry_and_fallback_before_any_output() {
        let config = config();
        let unavailable = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let cloud = Arc::new(FakeProvider {
            id: "cloud".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([
                ("local".into(), unavailable.clone() as DynProvider),
                ("cloud".into(), cloud.clone() as DynProvider),
            ]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: true,
            background: false,
            metadata: BTreeMap::from([("infer.fallback".into(), "equivalent".into())]),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let chunks: Vec<_> = runtime
            .execute_stream("test-app", request)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(!chunks.is_empty());
        assert_eq!(unavailable.calls.load(Ordering::SeqCst), 2);
        assert_eq!(cloud.seen_model.lock().await.as_deref(), Some("qwen-cloud"));
        let snapshot = runtime
            .jobs
            .lock()
            .await
            .values()
            .next()
            .unwrap()
            .snapshot
            .clone();
        assert_eq!(snapshot.attempts.len(), 3);
        assert_eq!(snapshot.attempts[2].trigger, AttemptTrigger::Fallback);
        assert_eq!(snapshot.attempts[2].outcome, AttemptOutcome::Succeeded);
    }

    #[tokio::test]
    async fn explicit_capability_fallback_can_remove_only_the_requested_raise() {
        let config = config();
        let local = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), local.clone() as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::from([
                ("infer.capability_floor".into(), "capable".into()),
                ("infer.fallback".into(), "allow_lower_capability".into()),
            ]),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        runtime.execute("test-app", request).await.unwrap();
        assert_eq!(local.seen_model.lock().await.as_deref(), Some("qwen"));
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_in_flight_non_streaming_attempt() {
        let config = config();
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone().notified_owned();
        let provider = Arc::new(HangingProvider {
            id: "local".into(),
            started,
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.execute("test-app", request).await }
        });
        ready.await;
        let job_id = runtime.jobs.lock().await.keys().next().unwrap().clone();
        assert!(
            runtime
                .snapshot_for_app("another_app", &job_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!runtime.cancel_for_app("another_app", &job_id).await);
        assert!(runtime.cancel_for_app("test-app", &job_id).await);
        assert!(matches!(task.await.unwrap(), Err(RuntimeError::Cancelled)));
        let snapshot = runtime.snapshot(&job_id).await.unwrap().unwrap();
        assert_eq!(snapshot.state, JobState::Cancelled);
        assert_eq!(snapshot.attempts.len(), 1);
        assert_eq!(
            snapshot.attempts[0].error_kind.as_deref(),
            Some("cancelled")
        );
    }

    #[tokio::test]
    async fn active_local_attempt_holds_a_resource_reservation_until_cancelled() {
        let config = resource_config();
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone().notified_owned();
        let provider = Arc::new(HangingProvider {
            id: "local".into(),
            started,
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
        );
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.execute("test-app", summary_request()).await }
        });
        ready.await;
        let snapshot = runtime.resource_snapshot().await;
        let local = snapshot
            .providers
            .iter()
            .find(|provider| provider.provider == "local")
            .unwrap();
        assert_eq!(local.model_lifecycle[0].active_reservations, 1);
        let job_id = runtime.jobs.lock().await.keys().next().unwrap().clone();
        assert!(runtime.cancel(&job_id).await);
        assert!(matches!(task.await.unwrap(), Err(RuntimeError::Cancelled)));
        let snapshot = runtime.resource_snapshot().await;
        assert_eq!(
            snapshot.providers[0].model_lifecycle[0].active_reservations,
            0
        );
    }

    #[tokio::test]
    async fn unconsumed_local_stream_keeps_then_releases_its_resource_reservation() {
        let config = resource_config();
        let provider = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
        );
        let stream = runtime
            .execute_stream(
                "test-app",
                ResponsesRequest {
                    stream: true,
                    background: false,
                    ..summary_request()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.resource_snapshot().await.providers[0].model_lifecycle[0].active_reservations,
            1
        );
        drop(stream);
        assert_eq!(
            runtime.resource_snapshot().await.providers[0].model_lifecycle[0].active_reservations,
            0
        );
    }

    #[tokio::test]
    async fn app_pending_limit_rejects_before_the_provider_queue_is_consumed() {
        let mut config = config();
        config.apps.get_mut("test-app").unwrap().max_pending_jobs = 1;
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone().notified_owned();
        let provider = Arc::new(HangingProvider {
            id: "local".into(),
            started: started.clone(),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
        );
        let request = ResponsesRequest {
            model: "text.summarize".into(),
            input: Value::String("transcript".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            tool_choice: None,
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        };
        let first = tokio::spawn({
            let runtime = runtime.clone();
            let request = request.clone();
            async move { runtime.execute("test-app", request).await }
        });
        ready.await;
        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::AppQueueFull)
        ));
        let first_job = runtime
            .jobs
            .lock()
            .await
            .iter()
            .find(|(_, entry)| {
                !matches!(
                    entry.snapshot.state,
                    JobState::Succeeded
                        | JobState::Failed
                        | JobState::Cancelled
                        | JobState::Expired
                )
            })
            .map(|(id, _)| id.clone())
            .unwrap();
        assert!(runtime.cancel(&first_job).await);
        assert!(matches!(first.await.unwrap(), Err(RuntimeError::Cancelled)));
        let resumed = started.clone().notified_owned();
        let second = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runtime
                    .execute(
                        "test-app",
                        ResponsesRequest {
                            model: "text.summarize".into(),
                            input: Value::String("transcript".into()),
                            instructions: None,
                            stream: false,
                            background: false,
                            metadata: BTreeMap::new(),
                            tools: vec![],
                            tool_choice: None,
                            reasoning: None,
                            temperature: None,
                            top_p: None,
                            max_output_tokens: None,
                            truncation: None,
                            store: None,
                            previous_response_id: None,
                            conversation: None,
                        },
                    )
                    .await
            }
        });
        resumed.await;
        let second_job = runtime
            .jobs
            .lock()
            .await
            .iter()
            .find(|(_, entry)| {
                entry.snapshot.id != first_job
                    && !matches!(
                        entry.snapshot.state,
                        JobState::Succeeded
                            | JobState::Failed
                            | JobState::Cancelled
                            | JobState::Expired
                    )
            })
            .map(|(id, _)| id.clone())
            .unwrap();
        assert!(runtime.cancel(&second_job).await);
        assert!(matches!(
            second.await.unwrap(),
            Err(RuntimeError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn app_intent_acl_rejects_before_job_or_provider_admission() {
        let mut config = config();
        config.apps.get_mut("test-app").unwrap().allowed_intents = Some(Vec::new());
        let provider = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
        );

        assert!(matches!(
            runtime.execute("test-app", summary_request()).await,
            Err(RuntimeError::IntentNotAllowed { app_id, intent })
                if app_id == "test-app" && intent == "text.summarize"
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(runtime.jobs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn app_builtin_tool_acl_rejects_web_search_before_job_or_provider_admission() {
        let config = config();
        let provider = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
        );
        let mut request = summary_request();
        request.tools = vec![json!({"type": "web_search"})];

        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::OverrideNotAllowed {
                field: "tools.web_search"
            })
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(runtime.jobs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn tool_choice_none_does_not_require_hosted_tool_authority() {
        let config = config();
        let provider = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
        );
        let mut request = summary_request();
        request.tools = vec![json!({"type": "web_search"})];
        request.tool_choice = Some(infer_core::ToolChoice::None);

        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::Provider(_))
        ));
        assert!(provider.calls.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn provider_access_request_cannot_expand_the_app_acl() {
        let config = config();
        let provider = Arc::new(UnavailableProvider {
            id: "local".into(),
            calls: AtomicUsize::new(0),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
        );
        let mut request = summary_request();
        request
            .metadata
            .insert("infer.provider_access_class".into(), "subscription".into());

        assert!(matches!(
            runtime.execute("test-app", request).await,
            Err(RuntimeError::OverrideNotAllowed {
                field: "infer.provider_access_class"
            })
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(runtime.jobs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn provider_probe_uses_the_declared_profile_and_a_registered_build() {
        let config = config();
        let local = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), local.clone() as DynProvider)]),
        );
        let report = runtime.probe_provider("local").await.unwrap();
        assert!(report.passed);
        assert_eq!(report.model, "qwen");
        assert!(
            report
                .checks
                .iter()
                .all(|check| matches!(check.status, infer_provider::ProviderProbeStatus::Passed))
        );
        assert_eq!(local.seen_model.lock().await.as_deref(), Some("qwen"));
    }

    #[tokio::test]
    async fn provider_inventory_exposes_capabilities_without_credential_references() {
        let mut config = config();
        config.model_builds.get_mut("qwen_local").unwrap().model_id =
            "/Users/example/.cache/qwen/snapshot/private".into();
        let runtime = Runtime::with_providers(config, BTreeMap::new());
        let inventory = serde_json::to_value(runtime.provider_snapshots()).unwrap();
        assert_eq!(inventory[0]["id"], "cloud");
        assert!(inventory[0].get("api_key_env").is_none());
        assert_eq!(
            inventory[0]["execution_modes"],
            json!(["unary", "server_stream"])
        );
        assert_eq!(inventory[0]["deployments"][0]["id"], "qwen_cloud");
        assert_eq!(inventory[0]["deployments"][0]["model_profile"], "qwen");
        assert_eq!(inventory[1]["id"], "local");
        assert_eq!(inventory[1]["deployments"][0]["id"], "qwen_local");
        assert_eq!(inventory[1]["deployments"][0]["model"], "qwen");
        assert!(!inventory.to_string().contains("/Users/example"));
    }

    #[tokio::test]
    async fn provider_inventory_hides_ratings_that_the_build_cannot_execute() {
        let mut config = config();
        config
            .model_builds
            .get_mut("qwen_local")
            .unwrap()
            .input_modalities = vec![infer_core::Modality::Image];
        let runtime = Runtime::with_providers(config, BTreeMap::new());
        let inventory = serde_json::to_value(runtime.provider_snapshots()).unwrap();
        let local = inventory
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == "local")
            .unwrap();

        assert_eq!(local["deployments"][0]["ratings"], json!({}));
    }

    #[tokio::test]
    async fn durable_background_response_encrypts_payload_and_publishes_result() {
        const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let config = config();
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let directory = tempfile::tempdir().unwrap();
        let spool = Arc::new(EncryptedPayloadSpool::open(directory.path(), KEY, 4096).unwrap());
        let provider = Arc::new(BackgroundProvider {
            id: "local".into(),
            seen_background: Mutex::new(None),
        });
        let runtime = Runtime::with_components(
            config,
            BTreeMap::from([("local".into(), provider.clone() as DynProvider)]),
            Some(store.clone()),
            background_jobs::BackgroundJobs::for_test(spool, 60_000),
            AppCredentials::empty(),
        );
        let mut request = summary_request();
        request.background = true;
        let submission = runtime
            .submit_background("test-app", request)
            .await
            .unwrap();
        assert_eq!(submission.status, "queued");
        let submission_shape = serde_json::to_value(&submission).unwrap();
        assert!(submission_shape["created_at"].as_i64().unwrap() > 0);
        assert_eq!(submission_shape["parallel_tool_calls"], true);
        assert_eq!(submission_shape["tool_choice"], "auto");
        assert_eq!(submission_shape["tools"], json!([]));

        let response = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = runtime
                    .background_response("test-app", &submission.id)
                    .await
                    .unwrap()
                    .unwrap();
                if response["status"] == "completed" {
                    break response;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(response["background"], true);
        assert_eq!(response["model"], "text.summarize");
        assert!(response["created_at"].as_i64().unwrap() > 0);
        assert_eq!(response["parallel_tool_calls"], true);
        assert_eq!(response["tool_choice"], "auto");
        assert_eq!(response["tools"], json!([]));
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "durable summary"
        );
        assert_eq!(*provider.seen_background.lock().await, Some(false));
        assert_eq!(
            store.load_job(&submission.id).unwrap().unwrap().state,
            JobState::Succeeded
        );
        assert!(
            runtime
                .background_response("another_app", &submission.id)
                .await
                .unwrap()
                .is_none()
        );
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let raw = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!raw.windows(10).any(|window| window == b"transcript"));
            assert!(!raw.windows(15).any(|window| window == b"durable summary"));
        }
    }

    #[tokio::test]
    async fn cancelling_background_work_retires_its_encrypted_input() {
        const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let config = config();
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let directory = tempfile::tempdir().unwrap();
        let spool = Arc::new(EncryptedPayloadSpool::open(directory.path(), KEY, 4096).unwrap());
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone().notified_owned();
        let provider = Arc::new(HangingProvider {
            id: "local".into(),
            started,
        });
        let runtime = Runtime::with_components(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
            Some(store.clone()),
            background_jobs::BackgroundJobs::for_test(spool, 60_000),
            AppCredentials::empty(),
        );
        let mut request = summary_request();
        request.background = true;
        let submission = runtime
            .submit_background("test-app", request)
            .await
            .unwrap();
        ready.await;
        let cancellation = runtime
            .cancel_background("test-app", &submission.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancellation["status"], "cancelling");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.load_job(&submission.id).unwrap().unwrap().state == JobState::Cancelled
                    && std::fs::read_dir(directory.path()).unwrap().count() == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn interrupted_background_job_restarts_with_same_id_and_recovery_attempt() {
        const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let config = config();
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let directory = tempfile::tempdir().unwrap();
        let spool = Arc::new(EncryptedPayloadSpool::open(directory.path(), KEY, 4096).unwrap());
        let first_runtime = Runtime::with_components(
            config.clone(),
            BTreeMap::new(),
            Some(store.clone()),
            background_jobs::BackgroundJobs::for_test(spool.clone(), 60_000),
            AppCredentials::empty(),
        );
        let mut request = summary_request();
        request.background = true;
        request
            .metadata
            .insert("infer.priority".into(), "background".into());
        request
            .metadata
            .insert("infer.placement".into(), "local_only".into());
        let request = first_runtime.prepare_responses_request(request).unwrap();
        let request_ref = spool
            .put(
                "test-app",
                infer_core::DurablePayloadKind::ResponsesRequest,
                &serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
        let prepared = first_runtime
            .prepare_job(
                "test-app",
                JobPreparation {
                    logical_model: &request.model,
                    constraints: request.constraints().unwrap(),
                    execution_requirements: request.execution_requirements(),
                    reasoning_effort: request.reasoning_effort(),
                    estimated_tokens: estimate_response_tokens(&request),
                    id_prefix: "resp",
                    expected_data_plane: "responses",
                    durable_payload: Some(&request_ref),
                },
            )
            .await
            .unwrap();
        first_runtime
            .mark(&prepared.job_id, JobState::Running, None)
            .await
            .unwrap();
        first_runtime
            .begin_attempt(&prepared, AttemptTrigger::Initial)
            .await
            .unwrap();
        let job_id = prepared.job_id.clone();
        drop(prepared);
        drop(first_runtime);

        let recovery = store.recover_background_jobs(Some(2)).unwrap();
        assert_eq!(recovery.runnable.len(), 1);
        assert_eq!(recovery.runnable[0].snapshot.id, job_id);
        assert_eq!(
            recovery.runnable[0].snapshot.attempts[0].outcome,
            AttemptOutcome::Interrupted
        );
        let provider = Arc::new(BackgroundProvider {
            id: "local".into(),
            seen_background: Mutex::new(None),
        });
        let second_runtime = Runtime::with_components(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
            Some(store.clone()),
            background_jobs::BackgroundJobs::for_test(spool, 60_000),
            AppCredentials::empty(),
        );
        second_runtime.restore_background(recovery).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if second_runtime
                    .background_response("test-app", &job_id)
                    .await
                    .unwrap()
                    .is_some_and(|response| response["status"] == "completed")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let recovered = store.load_job(&job_id).unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Succeeded);
        assert_eq!(recovered.attempts.len(), 2);
        assert_eq!(recovered.attempts[0].outcome, AttemptOutcome::Interrupted);
        assert_eq!(recovered.attempts[1].trigger, AttemptTrigger::Recovery);
        assert_eq!(recovered.attempts[1].outcome, AttemptOutcome::Succeeded);
    }

    #[tokio::test]
    async fn disabling_background_with_pending_durable_work_fails_startup_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config();
        config.persistence.path = directory
            .path()
            .join("runtime.sqlite3")
            .to_string_lossy()
            .into_owned();
        let store = Arc::new(
            Store::open(
                &config.persistence.path,
                ConfigSnapshot::from_serializable(&config).unwrap(),
            )
            .unwrap(),
        );
        let runtime = Runtime::with_providers_and_store(
            config.clone(),
            BTreeMap::new(),
            Some(store.clone()),
            AppCredentials::empty(),
        );
        let mut request = summary_request();
        request.background = true;
        request
            .metadata
            .insert("infer.priority".into(), "background".into());
        request
            .metadata
            .insert("infer.placement".into(), "local_only".into());
        let request = runtime.prepare_responses_request(request).unwrap();
        let reference = infer_core::DurablePayloadRef {
            blob_id: "pay_00000000000000000000000000000000".into(),
            kind: infer_core::DurablePayloadKind::ResponsesRequest,
            digest: format!("hmac-sha256:{}", "0".repeat(64)),
            plaintext_bytes: 1,
        };
        runtime
            .prepare_job(
                "test-app",
                JobPreparation {
                    logical_model: &request.model,
                    constraints: request.constraints().unwrap(),
                    execution_requirements: request.execution_requirements(),
                    reasoning_effort: request.reasoning_effort(),
                    estimated_tokens: estimate_response_tokens(&request),
                    id_prefix: "resp",
                    expected_data_plane: "responses",
                    durable_payload: Some(&reference),
                },
            )
            .await
            .unwrap();
        drop(runtime);
        drop(store);

        assert!(matches!(
            Runtime::from_config(config).await,
            Err(RuntimeError::BackgroundDisabledWithPendingJobs)
        ));
    }

    #[tokio::test]
    async fn resource_admin_is_an_explicit_app_capability() {
        let mut config = config();
        let runtime = Runtime::with_providers(config.clone(), BTreeMap::new());
        assert!(matches!(
            runtime.authorize_resource_admin("test-app"),
            Err(RuntimeError::ResourceAdminRequired(_))
        ));
        config.apps.get_mut("test-app").unwrap().resource_admin = true;
        let runtime = Runtime::with_providers(config, BTreeMap::new());
        runtime.authorize_resource_admin("test-app").unwrap();
    }

    #[tokio::test]
    async fn rejected_eviction_apply_keeps_requested_and_failed_resource_audit() {
        let config = config();
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::new(),
            Some(store.clone()),
            AppCredentials::empty(),
        );
        let error = runtime
            .apply_resource_eviction(
                "test-app",
                EvictionApplyRequest {
                    expected_deployment: "qwen_local".into(),
                    reason: "maintenance-42".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::Resource(ResourceError::EvictionApply(
                infer_resource::EvictionApplyError::NoActionableTarget
            ))
        ));
        let events = store.resource_audit_events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "eviction.apply_failed");
        assert_eq!(events[1].kind, "eviction.apply_requested");
    }

    #[tokio::test]
    async fn enabled_monitor_without_an_actionable_model_stays_observational() {
        let mut config = config();
        config.resources.eviction.mode = infer_core::EvictionMode::Recommend;
        config.resources.eviction.monitor = infer_core::EvictionMonitorConfig {
            enabled: true,
            poll_interval_ms: 1_000,
            max_lease_ms: 60_000,
        };
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config).unwrap()).unwrap(),
        );
        let runtime = Runtime::with_providers_and_store(
            config,
            BTreeMap::new(),
            Some(store.clone()),
            AppCredentials::empty(),
        );
        runtime
            .grant_eviction_maintenance(
                "test-app",
                MaintenanceLeaseRequest {
                    duration_ms: 10_000,
                    reason: "monitor-test".into(),
                },
            )
            .await
            .unwrap();
        runtime.run_resource_monitor_once().await;
        assert_eq!(
            runtime.eviction_monitor_snapshot().await.last_outcome,
            EvictionMonitorOutcome::NoActionableTarget
        );
        let events = store.resource_audit_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "eviction.maintenance_lease_granted");
    }

    #[test]
    fn stream_completion_usage_is_extracted_before_event_normalization() {
        let normalized = normalize_frame(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"upstream\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"total_tokens\":18}}}\n\n".to_vec(),
            "resp_runtime",
            "text.summarize",
        );
        let usage = normalized.usage.unwrap();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
        assert!(
            String::from_utf8(normalized.bytes)
                .unwrap()
                .contains("resp_runtime")
        );
    }
}
