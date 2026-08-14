//! The control plane: admission, policy selection, dispatch, job state, and cancellation.

mod app_admission;
mod attempt_policy;
mod audio_streaming;
mod background_jobs;
mod capacity;
mod image_understanding;
mod metrics;
mod observer;
mod ocr_execution;
mod pressure_observation;
mod provider_assembly;
mod provider_health;
mod raw_foundation;
mod registry;
mod resource_control;
mod resource_monitor;
mod retrieval_execution;
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
use infer_artifact::ArtifactError;
use infer_auth::{AppCredentials, CredentialError};
use infer_core::{
    AppConfig, AttemptOutcome, AttemptSnapshot, AttemptTrigger, AudioEmbeddingProvenance,
    AudioEmbeddingResponse, AudioExecutionRequest, BuiltinTool, ContractError, DurablePayloadRef,
    EventDetectionResult, ExecutionMode, ExecutionRequirements, Fallback, IntentProfile,
    JobListPage, JobPageCursor, JobSnapshot, JobState, Modality, Priority, ProviderCapability,
    ProviderKind, ProviderProtocol, QuotaConfig, RequestConstraints, ResponsesRequest,
    RuntimeConfig,
};
use infer_payload::PayloadError;
use infer_provider::{
    DynAudioDuplexExecutor, DynAudioExecutor, DynAudioStreamExecutor, DynFaceDetectionExecutor,
    DynFaceEmbeddingExecutor, DynFaceParsingExecutor, DynImageEmbeddingExecutor,
    DynImageUnderstandingExecutor, DynOcrExecutor, DynProvider, DynRetrievalExecutor,
    DynSubjectSegmentationExecutor, DynTextEmbeddingExecutor, ProviderError, ProviderModelCatalog,
    probe_responses_provider, probe_responses_provider_with_effort,
};
use infer_resource::{ModelReservation, ResourceError, ResourceManager};
use infer_store::{
    ActiveReservation, AttemptReservation, AuditEvent, AuditEventInput, ConfigSnapshot,
    ExecutionOrigin, QuotaLimits, QuotaResource, Store, StoreError, TelemetryBucket,
    TelemetryWindow, UsageLedgerEntry,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use app_admission::{AppAdmission, AppAdmissionPermit};
use capacity::{NodeCapacity, NodeCapacityError, NodeCapacityReservation};
use metrics::{MetricsSnapshot, RuntimeMetrics};
use provider_assembly::ProviderAssembly;
use provider_health::ProviderHealth;
use registry::{
    Candidate, CandidatePlanningContext, ProviderQueueEstimate, plan_candidates_with_queue,
};
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

/// Bounded operator-only durable-throughput windows. These values are not
/// Consumer-facing model telemetry and never include payload or Job identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryRange {
    LastHour,
    LastDay,
    LastWeek,
}

impl TelemetryRange {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "1h" => Some(Self::LastHour),
            "24h" => Some(Self::LastDay),
            "7d" => Some(Self::LastWeek),
            _ => None,
        }
    }

    fn dimensions(self) -> (i64, i64) {
        match self {
            Self::LastHour => (60 * 60 * 1_000, 5 * 60 * 1_000),
            Self::LastDay => (24 * 60 * 60 * 1_000, 60 * 60 * 1_000),
            Self::LastWeek => (7 * 24 * 60 * 60 * 1_000, 2 * 60 * 60 * 1_000),
        }
    }
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
    pub readiness: Option<infer_provider::ProviderRuntimeReadiness>,
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
    /// Non-sensitive supply-chain ownership; local paths and receipts remain
    /// outside the operator inventory projection.
    pub source_kind: infer_core::ModelSourceKind,
    pub license_status: infer_core::ModelLicenseStatus,
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
    #[error("application `{app_id}` is not permitted to request the named route for `{intent}`")]
    NamedRouteNotAllowed { app_id: String, intent: String },
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
    NodeCapacity(#[from] NodeCapacityError),
    #[error(transparent)]
    MaintenanceLease(#[from] MaintenanceLeaseError),
    #[error("response was cancelled before execution started")]
    Cancelled,
    #[error("execution queue is full")]
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

impl RuntimeError {
    pub fn public_message(&self) -> String {
        match self {
            Self::Provider(error) => error.public_message().into(),
            Self::Credential(_) => "credential subsystem unavailable".into(),
            Self::Artifact(_) => "artifact store unavailable".into(),
            Self::Store(_) => "persistence subsystem unavailable".into(),
            Self::Resource(ResourceError::NativeControl(_)) => {
                "native resource control unavailable".into()
            }
            Self::BackgroundKeyUnavailable(_) => "durable background key is unavailable".into(),
            _ => self.to_string(),
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

/// Holds every execution-wide reservation until an Attempt finishes. Provider
/// scheduling remains per Provider; this guard only adds explicitly measured
/// node-wide local capacity and native model lifecycle protection.
struct ExecutionResourceReservation {
    _node_capacity: NodeCapacityReservation,
    _model: Option<ModelReservation>,
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

/// Private bounded JSON frame returned by the CLAP worker. Public semantic
/// identity is attached only by `normalize_audio_embedding_output`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerAudioEmbedding {
    embedding: Vec<f32>,
    dimensions: usize,
    normalized: bool,
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
    capability_contract: &'a str,
    durable_payload: Option<&'a DurablePayloadRef>,
}

tokio::task_local! {
    /// Exact capability identity admitted by the HTTP boundary for this one
    /// request. It is scoped to the handler future and cannot leak across
    /// requests. Non-HTTP/internal callers retain their explicit data-plane
    /// default, preserving the existing control API.
    static ADMITTED_CAPABILITY_CONTRACT: &'static str;
}

pub async fn with_admitted_capability_contract<F>(contract: &'static str, future: F) -> F::Output
where
    F: std::future::Future,
{
    ADMITTED_CAPABILITY_CONTRACT.scope(contract, future).await
}

pub fn current_admitted_capability_contract(default: &'static str) -> &'static str {
    ADMITTED_CAPABILITY_CONTRACT
        .try_with(|contract| *contract)
        .unwrap_or(default)
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
    face_parsing_executors: BTreeMap<String, DynFaceParsingExecutor>,
    subject_segmentation_executors: BTreeMap<String, DynSubjectSegmentationExecutor>,
    image_embedding_executors: BTreeMap<String, DynImageEmbeddingExecutor>,
    text_embedding_executors: BTreeMap<String, DynTextEmbeddingExecutor>,
    image_understanding_executors: BTreeMap<String, DynImageUnderstandingExecutor>,
    retrieval_executors: BTreeMap<String, DynRetrievalExecutor>,
    ocr_executors: BTreeMap<String, DynOcrExecutor>,
    schedulers: BTreeMap<String, ProviderScheduler>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    metrics: RuntimeMetrics,
    health: ProviderHealth,
    provider_readiness: BTreeMap<String, infer_provider::ProviderRuntimeReadiness>,
    resources: Arc<ResourceManager>,
    node_capacity: NodeCapacity,
    _pressure_observation: Option<pressure_observation::PressureObservation>,
    resource_monitor: EvictionMonitor,
    app_admission: AppAdmission,
    /// Test/in-process callers may omit persistence, but the daemon
    /// constructor always initializes this before it accepts work.
    store: Option<Arc<Store>>,
    background: background_jobs::BackgroundJobs,
    observer: observer::ObserverRuntimeState,
}

impl Runtime {
    fn unavailable_providers(&self) -> BTreeSet<String> {
        let mut unavailable = self.health.unavailable_providers();
        unavailable.extend(
            self.provider_readiness
                .iter()
                .filter_map(|(id, readiness)| (!readiness.is_ready()).then_some(id.clone())),
        );
        unavailable
    }

    async fn wait_before_retry(
        &self,
        prepared: &PreparedRun,
        kind: infer_provider::ProviderFailureKind,
        retry_index: usize,
        retry_after: Option<Duration>,
    ) {
        let delay = attempt_policy::retry_delay(kind, retry_index, retry_after, &prepared.job_id);
        if delay.is_zero() {
            return;
        }
        let wake = prepared.deadline.map_or_else(
            || Instant::now() + delay,
            |deadline| (Instant::now() + delay).min(deadline),
        );
        tokio::select! {
            _ = prepared.cancellation.cancelled() => {},
            _ = tokio::time::sleep_until(wake) => {},
        }
    }

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
        let app_admission = AppAdmission::new(&config.apps);
        let resource_monitor = EvictionMonitor::new(config.resources.eviction.monitor.clone());
        let assembly = ProviderAssembly::from_config(&config)?;
        let pressure_refresh_interval =
            Duration::from_millis(config.resources.pressure.refresh_interval_ms);
        let resources = Arc::new(ResourceManager::with_native_controllers(
            &config,
            assembly.native_controllers,
        ));
        let node_capacity = NodeCapacity::from_config(&config.resources.admission_capacity);
        let pressure_observation = pressure_observation::PressureObservation::start(
            Arc::clone(&resources),
            pressure_refresh_interval,
        )
        .await;
        let runtime = Arc::new(Self {
            config: Arc::new(config),
            credentials,
            providers: assembly.providers,
            audio_executors: assembly.audio_executors,
            audio_stream_executors: assembly.audio_stream_executors,
            audio_duplex_executors: assembly.audio_duplex_executors,
            face_detection_executors: assembly.face_detection_executors,
            face_embedding_executors: assembly.face_embedding_executors,
            face_parsing_executors: assembly.face_parsing_executors,
            subject_segmentation_executors: assembly.subject_segmentation_executors,
            image_embedding_executors: assembly.image_embedding_executors,
            text_embedding_executors: assembly.text_embedding_executors,
            image_understanding_executors: assembly.image_understanding_executors,
            retrieval_executors: assembly.retrieval_executors,
            ocr_executors: assembly.ocr_executors,
            schedulers: assembly.schedulers,
            jobs: Mutex::new(HashMap::new()),
            metrics: RuntimeMetrics::default(),
            health: ProviderHealth::default(),
            provider_readiness: assembly.readiness,
            resources,
            node_capacity,
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
        let node_capacity = NodeCapacity::from_config(&config.resources.admission_capacity);
        let resource_monitor = EvictionMonitor::new(config.resources.eviction.monitor.clone());
        let schedulers = config
            .providers
            .iter()
            .map(|(id, provider)| {
                (
                    id.clone(),
                    ProviderScheduler::new(
                        provider.max_concurrency,
                        provider.max_queue,
                        Duration::from_millis(provider.priority_aging_ms),
                    ),
                )
            })
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
            face_parsing_executors: BTreeMap::new(),
            subject_segmentation_executors: BTreeMap::new(),
            image_embedding_executors: BTreeMap::new(),
            text_embedding_executors: BTreeMap::new(),
            image_understanding_executors: BTreeMap::new(),
            retrieval_executors: BTreeMap::new(),
            ocr_executors: BTreeMap::new(),
            schedulers,
            jobs: Mutex::new(HashMap::new()),
            metrics: RuntimeMetrics::default(),
            health: ProviderHealth::default(),
            provider_readiness: BTreeMap::new(),
            resources,
            node_capacity,
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

    /// Constructor for typed local-worker contract tests and future embedded
    /// integrations. Retrieval and OCR remain separate executor families even
    /// though they share the common Job/admission control plane.
    pub fn with_local_worker_executors(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        credentials: AppCredentials,
        retrieval_executors: BTreeMap<String, DynRetrievalExecutor>,
        ocr_executors: BTreeMap<String, DynOcrExecutor>,
    ) -> Arc<Self> {
        let mut runtime = Self::with_providers_and_credentials(config, providers, credentials);
        {
            let runtime =
                Arc::get_mut(&mut runtime).expect("newly constructed Runtime has one owner");
            runtime.retrieval_executors = retrieval_executors;
            runtime.ocr_executors = ocr_executors;
        }
        runtime
    }

    /// Constructor for typed file-audio provider contract tests. Executor ids
    /// must match configured Provider ids.
    pub fn with_audio_executors(
        config: RuntimeConfig,
        providers: BTreeMap<String, DynProvider>,
        credentials: AppCredentials,
        executors: BTreeMap<String, DynAudioExecutor>,
    ) -> Arc<Self> {
        let mut runtime = Self::with_providers_and_credentials(config, providers, credentials);
        Arc::get_mut(&mut runtime)
            .expect("newly constructed Runtime has one owner")
            .audio_executors = executors;
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
                    capability_contract: current_admitted_capability_contract(
                        "infer.responses@20260812.1",
                    ),
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
                            Some(error.public_message().into()),
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
                            let error = last_error.as_ref().expect("retry has an error");
                            self.wait_before_retry(
                                &prepared,
                                kind,
                                retry_index,
                                error.retry_after(),
                            )
                            .await;
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
        self.mark(
            &prepared.job_id,
            JobState::Failed,
            Some(error.public_message().into()),
        )
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
                    capability_contract: current_admitted_capability_contract(
                        "infer.responses@20260812.1",
                    ),
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
                            Some(error.public_message().into()),
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
                            let error = last_error.as_ref().expect("retry has an error");
                            self.wait_before_retry(
                                &prepared,
                                kind,
                                retry_index,
                                error.retry_after(),
                            )
                            .await;
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
        self.mark(
            &prepared.job_id,
            JobState::Failed,
            Some(error.public_message().into()),
        )
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
        let node_capacity = self.node_capacity.snapshot().await.unwrap_or_default();
        self.metrics.snapshot(queues, node_capacity)
    }

    /// A point-in-time, best-effort scheduler observation for route ordering.
    /// It is deliberately kept out of the durable Job decision: it can change
    /// between planning and provider admission, while the chosen plan remains
    /// the auditable record.
    async fn provider_queue_estimates(&self) -> BTreeMap<String, ProviderQueueEstimate> {
        let mut estimates = BTreeMap::new();
        for (provider_id, scheduler) in &self.schedulers {
            if let Ok(snapshot) = scheduler.snapshot().await {
                estimates.insert(
                    provider_id.clone(),
                    ProviderQueueEstimate {
                        estimated_wait_ms: snapshot.estimated_wait_ms,
                        estimated_service_ms: snapshot.estimated_service_ms,
                    },
                );
            }
        }
        estimates
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

    /// Durable terminal-Job aggregates for the operator Console. This is a
    /// separate lifetime from `metrics()`: process counters intentionally
    /// reset at daemon start, while this projection survives restarts.
    pub fn telemetry(&self, range: TelemetryRange) -> Result<TelemetryWindow, RuntimeError> {
        let (window_ms, bucket_width_ms) = range.dimensions();
        match &self.store {
            Some(store) => Ok(store.telemetry_window(window_ms, bucket_width_ms)?),
            None => {
                let ends_at_ms = unix_time_ms();
                let starts_at_ms = ends_at_ms.saturating_sub(window_ms);
                Ok(TelemetryWindow {
                    window_started_at_ms: starts_at_ms,
                    window_ends_at_ms: ends_at_ms,
                    bucket_width_ms,
                    buckets: (0..(window_ms / bucket_width_ms))
                        .map(|index| TelemetryBucket {
                            started_at_ms: starts_at_ms + index * bucket_width_ms,
                            succeeded: 0,
                            failed: 0,
                            cancelled: 0,
                            expired: 0,
                        })
                        .collect(),
                })
            }
        }
    }

    /// Safe, operator-visible provider inventory. Credentials are represented
    /// only by `configured`; environment variable names and values remain out
    /// of this control response.
    pub fn provider_snapshots(&self) -> Vec<ProviderSnapshot> {
        let circuit_open = self.health.unavailable_providers();
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
                                source_kind: build.provenance.source_kind,
                                license_status: build.license.status,
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
                    kind: provider.kind.to_string(),
                    access_class: provider.access_class,
                    placement: provider.placement,
                    configured: provider.is_configured(),
                    max_concurrency: provider.max_concurrency,
                    max_queue: provider.max_queue,
                    capability_profile: provider.capability_profile.clone(),
                    execution_modes,
                    deployments,
                    circuit_open: circuit_open.contains(id),
                    readiness: self.provider_readiness.get(id).cloned(),
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
        let embedding_revision = match &request {
            AudioExecutionRequest::Embedding(request) => {
                Some((Some(request.source_revision.clone()), None))
            }
            AudioExecutionRequest::TextEmbedding(request) => {
                Some((None, Some(request.query_revision.clone())))
            }
            _ => None,
        };
        let constraints = request.constraints()?;
        let is_event_detection = matches!(&request, AudioExecutionRequest::EventDetection(_));
        let is_audio_embedding = matches!(
            &request,
            AudioExecutionRequest::Embedding(_) | AudioExecutionRequest::TextEmbedding(_)
        );
        let is_transcription = matches!(&request, AudioExecutionRequest::Transcription(_));
        let (expected_data_plane, input_modalities) = match &request {
            AudioExecutionRequest::Transcription(_) => {
                ("audio.transcription", BTreeSet::from([Modality::Audio]))
            }
            AudioExecutionRequest::Alignment(_) => (
                "audio.alignment",
                BTreeSet::from([Modality::Audio, Modality::Text]),
            ),
            AudioExecutionRequest::EventDetection(_) => {
                ("audio.event_detection", BTreeSet::from([Modality::Audio]))
            }
            AudioExecutionRequest::Embedding(_) => {
                ("audio.embedding", BTreeSet::from([Modality::Audio]))
            }
            AudioExecutionRequest::TextEmbedding(_) => {
                ("audio.embedding", BTreeSet::from([Modality::Text]))
            }
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
                    capability_contract: current_admitted_capability_contract(
                        match expected_data_plane {
                            "audio.transcription" => "infer.audio.transcription@20260814.1",
                            "audio.alignment" => "infer.audio.alignment@20260811.1",
                            "audio.event_detection" => "infer.audio.event-detection@20260813.2",
                            "audio.embedding" => "infer.audio.embedding@20260815.1",
                            "audio.speech" => "infer.audio.speech@20260811.1",
                            "audio.voice_clone" => "infer.audio.voice-clone@20260811.1",
                            _ => unreachable!("validated audio data plane"),
                        },
                    ),
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
        let upstream = executor.execute(
            &prepared.physical_model,
            request,
            prepared.cancellation.clone(),
        );
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
        let result = match result {
            Ok(output) if is_event_detection => {
                let validation = match &output {
                    AudioExecutionOutput::Json(value) => {
                        self.validate_event_detection_output(&prepared, value)
                    }
                    AudioExecutionOutput::Audio { .. } => Err(ProviderError::Protocol(
                        "sound-event executor returned audio".into(),
                    )),
                };
                validation.map(|()| output).map_err(RuntimeError::Provider)
            }
            Ok(mut output) if is_audio_embedding => {
                let validation = match &mut output {
                    AudioExecutionOutput::Json(value) => self.normalize_audio_embedding_output(
                        &prepared,
                        value,
                        &prepared.job_id,
                        &logical_model,
                        embedding_revision
                            .clone()
                            .expect("embedding request has a revision"),
                    ),
                    AudioExecutionOutput::Audio { .. } => Err(ProviderError::Protocol(
                        "audio embedding executor returned audio".into(),
                    )),
                };
                validation.map(|()| output).map_err(RuntimeError::Provider)
            }
            other => other,
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
                    normalize_audio_json(value, &prepared.job_id, &logical_model, is_transcription);
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
                        Some(provider.public_message().into()),
                        None,
                    )
                    .await?;
                }
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some(error.public_message()),
                )
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
            capability_contract,
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
        let routing_grant = app
            .routing
            .as_ref()
            .map(|routing| routing.grant_for(logical_model));
        if constraints.named_route.as_ref().is_some_and(|requested| {
            routing_grant
                .as_ref()
                .is_none_or(|grant| !grant.allows_request(requested, &self.config))
        }) {
            return Err(RuntimeError::NamedRouteNotAllowed {
                app_id: app_id.to_owned(),
                intent: logical_model.to_owned(),
            });
        }
        let policy_name = effective_policy(&self.config, app, intent, &constraints)?;
        let profile = self
            .config
            .profiles
            .get(&policy_name)
            .expect("validated profile");
        let unavailable_providers = self.unavailable_providers();
        let unavailable_deployments = self.resources.unavailable_deployments().await;
        let queue_estimates = self.provider_queue_estimates().await;
        let plan = plan_candidates_with_queue(
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
                routing_grant: routing_grant.as_ref(),
                unavailable_providers: &unavailable_providers,
                unavailable_deployments: &unavailable_deployments,
            },
            &queue_estimates,
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
            consumer_core_contract: infer_core::CONSUMER_CORE_CONTRACT.into(),
            capability_contract: Some(capability_contract.into()),
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

    fn validate_event_detection_output(
        &self,
        prepared: &PreparedRun,
        value: &Value,
    ) -> Result<(), ProviderError> {
        let result: EventDetectionResult = serde_json::from_value(value.clone())?;
        result
            .validate()
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let deployment = self
            .config
            .deployments
            .get(&prepared.deployment_id)
            .ok_or_else(|| ProviderError::Protocol("selected Deployment disappeared".into()))?;
        let build = self
            .config
            .model_builds
            .get(&deployment.build)
            .ok_or_else(|| ProviderError::Protocol("selected Model Build disappeared".into()))?;
        let manifest = build.audio_event.as_ref().ok_or_else(|| {
            ProviderError::Protocol("selected Build has no audio-event identity".into())
        })?;
        let matches = result.provenance.model
            == build
                .provenance
                .source_revision
                .as_deref()
                .unwrap_or_default()
            && result.provenance.model_archive_sha256 == manifest.model_archive_sha256
            && result.provenance.artifact_set_sha256
                == build
                    .local_worker
                    .as_ref()
                    .map(|worker| worker.artifact_set_sha256.as_str())
                    .unwrap_or_default()
            && result.provenance.model_license_spdx
                == build.license.expression.as_deref().unwrap_or_default()
            && result.provenance.training_data_license_spdx == manifest.training_data_license_spdx
            && result.provenance.runtime == manifest.runtime
            && result.provenance.runtime_version == manifest.runtime_version
            && result.provenance.decoder == manifest.decoder
            && result.provenance.preprocessing_identity == manifest.preprocessing_identity
            && result.ontology.id == manifest.ontology_id
            && result.ontology.revision == manifest.ontology_revision
            && result.ontology.class_id_namespace == manifest.class_id_namespace
            && result.ontology.class_count == manifest.class_count
            && result.ontology.artifact_sha256 == manifest.ontology_artifact_sha256
            && result.ontology.license_spdx == manifest.ontology_license_spdx
            && result.policy.revision == manifest.policy_revision
            && result.policy.score_kind == manifest.score_kind
            && (result.policy.event_score_threshold - manifest.event_score_threshold).abs()
                <= f64::EPSILON
            && result.policy.smoothing.method == manifest.smoothing_method
            && result.policy.smoothing.window_frames == manifest.smoothing_window_frames
            && result.policy.max_classes_per_window == manifest.max_classes_per_window
            && result.policy.max_events == manifest.max_events
            && result.policy.speech_class_set_revision == manifest.speech_class_set_revision
            && (result.policy.speech_present_threshold - manifest.speech_present_threshold).abs()
                <= f64::EPSILON
            && (result.policy.speech_absent_threshold - manifest.speech_absent_threshold).abs()
                <= f64::EPSILON
            && result.policy.max_audio_seconds == manifest.max_audio_seconds
            && (result.coverage.window_seconds - manifest.window_seconds).abs() <= f64::EPSILON
            && (result.coverage.hop_seconds - manifest.hop_seconds).abs() <= f64::EPSILON;
        if !matches {
            return Err(ProviderError::Protocol(
                "sound-event result identity does not match the selected Build".into(),
            ));
        }
        Ok(())
    }

    /// Converts the deliberately minimal local-worker frame into the stable
    /// Consumer response and binds it to the selected Build. The worker never
    /// gets to choose embedding-space or execution provenance.
    fn normalize_audio_embedding_output(
        &self,
        prepared: &PreparedRun,
        value: &mut Value,
        job_id: &str,
        logical_model: &str,
        (source_revision, query_revision): (Option<String>, Option<String>),
    ) -> Result<(), ProviderError> {
        let raw: WorkerAudioEmbedding = serde_json::from_value(value.clone()).map_err(|_| {
            ProviderError::Protocol("audio embedding worker returned an invalid result".into())
        })?;
        if !raw.normalized || raw.dimensions != raw.embedding.len() {
            return Err(ProviderError::Protocol(
                "audio embedding worker returned invalid dimensions or normalization".into(),
            ));
        }
        let deployment = self
            .config
            .deployments
            .get(&prepared.deployment_id)
            .ok_or_else(|| ProviderError::Protocol("selected Deployment disappeared".into()))?;
        let build = self
            .config
            .model_builds
            .get(&deployment.build)
            .ok_or_else(|| ProviderError::Protocol("selected Model Build disappeared".into()))?;
        let worker = build.local_worker.as_ref().ok_or_else(|| {
            ProviderError::Protocol("selected Build has no local-worker identity".into())
        })?;
        let embedding_space = worker.embedding_space.clone().ok_or_else(|| {
            ProviderError::Protocol("selected Build has no embedding-space identity".into())
        })?;
        let provenance = AudioEmbeddingProvenance {
            build: deployment.build.clone(),
            artifact_set_sha256: worker.artifact_set_sha256.clone(),
            runtime: worker.runtime.clone(),
            precision: worker.precision.clone(),
            requested_execution_provider: worker.requested_execution_provider.clone().ok_or_else(
                || {
                    ProviderError::Protocol(
                        "selected Build has no requested execution provider".into(),
                    )
                },
            )?,
            actual_execution_provider: worker.actual_execution_provider.clone().ok_or_else(
                || {
                    ProviderError::Protocol(
                        "selected Build has no actual execution provider".into(),
                    )
                },
            )?,
            preprocessing_identity: worker.preprocessing_identity.clone().ok_or_else(|| {
                ProviderError::Protocol("selected Build has no preprocessing identity".into())
            })?,
            tokenizer_identity: worker.tokenizer_identity.clone().ok_or_else(|| {
                ProviderError::Protocol("selected Build has no tokenizer identity".into())
            })?,
        };
        let response = AudioEmbeddingResponse {
            id: job_id.into(),
            object: "audio.embedding".into(),
            model: logical_model.into(),
            source_revision,
            query_revision,
            embedding: raw.embedding,
            embedding_space,
            provenance,
        };
        response
            .validate()
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        *value = serde_json::to_value(response)?;
        Ok(())
    }

    async fn reserve_resource(
        &self,
        prepared: &PreparedRun,
    ) -> Result<ExecutionResourceReservation, RuntimeError> {
        let deployment = self
            .config
            .deployments
            .get(&prepared.deployment_id)
            .expect("prepared target must name a configured deployment");
        let node_capacity = match self
            .node_capacity
            .reserve(
                prepared.job_id.clone(),
                &deployment.resource_estimate,
                prepared.priority,
                prepared.deadline,
                prepared.cancellation.clone(),
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(NodeCapacityError::Cancelled) => {
                self.mark(&prepared.job_id, JobState::Cancelled, None)
                    .await?;
                self.metrics.cancelled();
                return Err(RuntimeError::Cancelled);
            }
            Err(NodeCapacityError::DeadlineExpired) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Expired,
                    Some("request deadline expired during node-capacity admission".into()),
                )
                .await?;
                self.metrics.expired();
                return Err(RuntimeError::DeadlineExpired);
            }
            Err(NodeCapacityError::QueueFull) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some("execution queue is full".into()),
                )
                .await?;
                self.metrics.queue_rejected();
                return Err(RuntimeError::QueueFull);
            }
            Err(error) => {
                self.mark(
                    &prepared.job_id,
                    JobState::Failed,
                    Some("node-wide local capacity is unavailable".into()),
                )
                .await?;
                self.metrics.failed();
                return Err(RuntimeError::NodeCapacity(error));
            }
        };
        match self.resources.reserve_model(&prepared.deployment_id) {
            Ok(reservation) => Ok(ExecutionResourceReservation {
                _node_capacity: node_capacity,
                _model: reservation,
            }),
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
                    execution_origin: execution_origin_for_provider(
                        &self.config,
                        &prepared.provider_id,
                    ),
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
        resource_reservation: ExecutionResourceReservation,
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
                        let _ = runtime.finish_attempt(&prepared, attempt_number, AttemptOutcome::Failed, Some(attempt_policy::kind_code(error.kind()).into()), Some(error.public_message().into()), None).await;
                        attempt_finished = true;
                        failed = true;
                        yield sse_error("upstream_error", error.public_message());
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

/// Produces the infrastructure accounting origin from the configured Provider
/// kind selected for this Attempt. This deliberately does not inspect the
/// provider identifier or physical model label: those are mutable names and
/// cannot safely drive cross-collector de-duplication.
fn execution_origin_for_provider(
    config: &RuntimeConfig,
    provider_id: &str,
) -> Option<ExecutionOrigin> {
    config
        .providers
        .get(provider_id)
        .map(|provider| match provider.kind {
            ProviderKind::CodexAppServer => ExecutionOrigin::Codex,
            _ => ExecutionOrigin::Other,
        })
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

fn normalize_audio_json(
    value: &mut Value,
    job_id: &str,
    logical_model: &str,
    is_transcription: bool,
) {
    if !value.is_object() {
        *value = json!({ "output": value.take() });
    }
    let object = value
        .as_object_mut()
        .expect("audio value was normalized to an object");
    object.insert("id".into(), Value::String(job_id.into()));
    object.insert("model".into(), Value::String(logical_model.into()));
    if is_transcription {
        normalize_transcription_language(object);
    }
}

fn normalize_transcription_language(object: &mut serde_json::Map<String, Value>) {
    // The scalar public field is strictly document-level. Preserve every
    // provider-reported string in a typed evidence set instead of collapsing
    // mixed-language input into an undifferentiated null.
    let raw_language = object.remove("language");
    let raw_evidence = object.remove("language_evidence");
    let evidence = raw_evidence
        .and_then(normalize_provider_language_evidence)
        .or_else(|| raw_language.and_then(input_set_language_evidence));

    let Some((evidence, languages)) = evidence else {
        object.insert("language".into(), Value::Null);
        return;
    };
    let language = match languages.as_slice() {
        [language] => Value::String(language.clone()),
        _ => Value::Null,
    };
    object.insert("language".into(), language);
    object.insert("language_evidence".into(), evidence);
}

fn input_set_language_evidence(value: Value) -> Option<(Value, Vec<String>)> {
    let languages = match value {
        Value::String(language) if !language.is_empty() => vec![language],
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(language) if !language.is_empty() => Some(language),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    let languages = unique_languages(languages)?;
    Some((
        json!({
            "kind": "input_set",
            "source": "provider_reported",
            "languages": languages,
        }),
        languages,
    ))
}

fn normalize_provider_language_evidence(value: Value) -> Option<(Value, Vec<String>)> {
    let Value::Object(mut evidence) = value else {
        return None;
    };
    if evidence.remove("source")?.as_str()? != "provider_reported" {
        return None;
    }
    match evidence.remove("kind")?.as_str()? {
        "input_set" => {
            let languages = match evidence.remove("languages")? {
                Value::Array(values) => values
                    .into_iter()
                    .map(|value| match value {
                        Value::String(language) if !language.is_empty() => Some(language),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            let languages = unique_languages(languages)?;
            Some((
                json!({
                    "kind": "input_set",
                    "source": "provider_reported",
                    "languages": languages,
                }),
                languages,
            ))
        }
        "segments" => {
            let Value::Array(segments) = evidence.remove("segments")? else {
                return None;
            };
            let mut normalized = Vec::with_capacity(segments.len());
            let mut languages = Vec::with_capacity(segments.len());
            for segment in segments {
                let Value::Object(mut segment) = segment else {
                    return None;
                };
                let language = segment.remove("language")?.as_str()?.to_owned();
                if language.is_empty() {
                    return None;
                }
                let start_seconds = segment.remove("start_seconds")?.as_f64()?;
                let end_seconds = segment.remove("end_seconds")?.as_f64()?;
                if !start_seconds.is_finite()
                    || !end_seconds.is_finite()
                    || start_seconds < 0.0
                    || end_seconds < start_seconds
                {
                    return None;
                }
                let score = match segment.remove("score") {
                    Some(score) => {
                        let score = score.as_f64()?;
                        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                            return None;
                        }
                        Some(score)
                    }
                    None => None,
                };
                let mut canonical = json!({
                    "language": language,
                    "start_seconds": start_seconds,
                    "end_seconds": end_seconds,
                });
                if let Some(score) = score {
                    canonical["score"] = json!(score);
                }
                normalized.push(canonical);
                languages.push(language);
            }
            let languages = unique_languages(languages)?;
            Some((
                json!({
                    "kind": "segments",
                    "source": "provider_reported",
                    "segments": normalized,
                }),
                languages,
            ))
        }
        _ => None,
    }
}

fn unique_languages(languages: Vec<String>) -> Option<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let languages: Vec<_> = languages
        .into_iter()
        .filter(|language| seen.insert(language.clone()))
        .collect();
    (!languages.is_empty()).then_some(languages)
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
        CapabilityLevel, LocalInventoryConfig, LocalInventoryKind, ProviderKind, QuotaLimitConfig,
        RuntimeConfig,
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
                retry_after: None,
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
                retry_after: None,
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
            allowed_intents = ["text.summarize"]
            allowed_cloud_input_modalities = ["text"]
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

    #[test]
    fn transcription_language_candidates_preserve_typed_multi_language_evidence() {
        let mut single = json!({"text":"provider output", "language":["Chinese"]});
        normalize_audio_json(&mut single, "audio_single", "audio.transcribe", true);
        assert_eq!(single["id"], "audio_single");
        assert_eq!(single["model"], "audio.transcribe");
        assert_eq!(single["language"], "Chinese");
        assert_eq!(
            single["language_evidence"],
            json!({
                "kind": "input_set",
                "source": "provider_reported",
                "languages": ["Chinese"],
            })
        );

        let mut ambiguous = json!({"text":"provider output", "language":["Chinese", "English"]});
        normalize_audio_json(&mut ambiguous, "audio_ambiguous", "audio.transcribe", true);
        assert!(ambiguous["language"].is_null());
        assert_eq!(
            ambiguous["language_evidence"],
            json!({
                "kind": "input_set",
                "source": "provider_reported",
                "languages": ["Chinese", "English"],
            })
        );

        let mut segmented = json!({
            "text": "provider output",
            "language": ["Chinese", "English"],
            "language_evidence": {
                "kind": "segments",
                "source": "provider_reported",
                "segments": [
                    {"language": "Chinese", "start_seconds": 0.0, "end_seconds": 2.0},
                    {"language": "English", "start_seconds": 2.0, "end_seconds": 4.0, "score": 0.9}
                ]
            }
        });
        normalize_audio_json(&mut segmented, "audio_segmented", "audio.transcribe", true);
        assert!(segmented["language"].is_null());
        assert_eq!(
            segmented["language_evidence"],
            json!({
                "kind": "segments",
                "source": "provider_reported",
                "segments": [
                    {"language": "Chinese", "start_seconds": 0.0, "end_seconds": 2.0},
                    {"language": "English", "start_seconds": 2.0, "end_seconds": 4.0, "score": 0.9}
                ]
            })
        );
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
    async fn admitted_capability_identity_is_request_scoped_and_persisted_on_the_job() {
        let store = Arc::new(
            Store::open_in_memory(ConfigSnapshot::from_serializable(&config()).unwrap()).unwrap(),
        );
        let fake = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers_and_store(
            config(),
            BTreeMap::from([("local".into(), fake as DynProvider)]),
            Some(store),
            AppCredentials::empty(),
        );

        let response = with_admitted_capability_contract(
            "infer.responses@20991231.1",
            runtime.execute("test-app", summary_request()),
        )
        .await
        .unwrap();
        let job = runtime
            .snapshot(response["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job.capability_contract.as_deref(),
            Some("infer.responses@20991231.1")
        );

        let outside = runtime
            .execute("test-app", summary_request())
            .await
            .unwrap();
        let outside_job = runtime
            .snapshot(outside["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outside_job.capability_contract.as_deref(),
            Some("infer.responses@20260812.1")
        );
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
        assert_eq!(
            accounting.usage_ledger[0].execution_origin,
            Some(ExecutionOrigin::Other)
        );
        assert!(accounting.active_reservations.is_empty());
        let observer = runtime.observer_snapshot().await.unwrap();
        let usage_daily = &observer.extensions["infer-runtime"]["usage_daily"];
        assert_eq!(usage_daily["schema"], "infer-runtime.usage.daily");
        assert_eq!(usage_daily["schema_version"], "20260813.3");
        assert_eq!(usage_daily["calendar"], "host_local");
        let days = usage_daily["days"].as_array().unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0]["models"][0]["id"], "qwen");
        assert_eq!(days[0]["models"][0]["execution_origin"], "other");
        assert_eq!(days[0]["models"][0]["total_tokens"], 0);
        assert_eq!(days[0]["models"][0]["cost_usd"], 0.0);
        let audit = runtime.audit_events(job_id).unwrap();
        assert!(audit.iter().any(|event| event.kind == "job.admitted"));
        assert!(audit.iter().any(|event| event.kind == "attempt.opened"));
        assert!(audit.iter().any(|event| event.kind == "attempt.finished"));
    }

    #[test]
    fn usage_execution_origin_uses_provider_kind_not_provider_or_model_name() {
        let mut config = config();
        let provider = config.providers.get_mut("local").unwrap();
        provider.kind = ProviderKind::CodexAppServer;

        assert_eq!(
            execution_origin_for_provider(&config, "local"),
            Some(ExecutionOrigin::Codex)
        );
        assert_eq!(
            execution_origin_for_provider(&config, "missing"),
            None,
            "unknown source must fail closed rather than be guessed"
        );
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
    async fn named_route_acl_rejects_before_job_or_provider_admission() {
        let mut config = config();
        config.apps.get_mut("test-app").unwrap().routing = Some(infer_core::AppRoutingConfig {
            deployment_ids: BTreeSet::from(["qwen_local".into()]),
            model_profile_ids: BTreeSet::new(),
            intents: BTreeMap::new(),
        });
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
            .insert("infer.deployment_ids".into(), "qwen_cloud".into());
        let result = runtime.execute("test-app", request).await;
        assert!(matches!(
            result,
            Err(RuntimeError::NamedRouteNotAllowed { app_id, intent })
                if app_id == "test-app" && intent == "text.summarize"
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(runtime.jobs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn named_request_succeeds_and_records_the_current_core_revision() {
        let mut config = config();
        config.apps.get_mut("test-app").unwrap().routing = Some(infer_core::AppRoutingConfig {
            deployment_ids: BTreeSet::from(["qwen_local".into()]),
            model_profile_ids: BTreeSet::new(),
            intents: BTreeMap::new(),
        });
        let provider = Arc::new(FakeProvider {
            id: "local".into(),
            seen_model: Mutex::new(None),
        });
        let runtime = Runtime::with_providers(
            config,
            BTreeMap::from([("local".into(), provider as DynProvider)]),
        );
        let mut request = summary_request();
        request
            .metadata
            .insert("infer.deployment_ids".into(), "qwen_local".into());
        let response = runtime.execute("test-app", request).await.unwrap();
        let job = runtime
            .snapshot(response["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job.consumer_core_contract,
            "infer-runtime.consumer-core@20260813.1"
        );
        assert_eq!(job.routing.named_route.unwrap().kind, "deployment");
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
                    capability_contract: "infer.responses@20260812.1",
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
                    capability_contract: "infer.responses@20260812.1",
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
