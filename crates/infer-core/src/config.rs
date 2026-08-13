//! Version-controlled intent, model, build, deployment, provider, and App registry.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
};

use serde::Deserialize;

use crate::{
    BuiltinTool, CapabilityLevel, ContractError, EvaluationStatus, ExecutionMode, Fallback,
    Latency, Modality, Placement, PlacementPreference, PlacementScope, Priority,
    ProviderCapability, ProviderProtocol, ReasoningEffort, ResourceClass, SortKey, string_enum,
};

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub server: ServerConfig,
    /// Optional, read-only registration with the local infrastructure
    /// observer. This does not grant inference or operator authority.
    #[serde(default)]
    pub observer: ObserverConfig,
    pub defaults: DefaultsConfig,
    /// Content-addressed model artifacts shared by in-process runtimes. An
    /// omitted root resolves to the platform application-data directory.
    #[serde(default)]
    pub artifacts: ArtifactStoreConfig,
    /// Native runtime installations are configured independently from model
    /// builds so one verified runtime can serve many deployments.
    #[serde(default)]
    pub runtimes: NativeRuntimesConfig,
    /// Optional RawNIND typed execution assembly. Disabled by default and
    /// fail-closed unless the exact local graph and ORT 1.27 runtime exist.
    #[serde(default)]
    pub raw_foundation: RawFoundationConfig,
    /// Credential storage policy. Secret values are never serialized into the
    /// runtime config or its persistence snapshot.
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    /// Durable background execution is opt-in because it changes payload
    /// retention and requires an external encryption key.
    #[serde(default)]
    pub background: BackgroundConfig,
    /// Hierarchical dispatch limits. A missing value means that particular
    /// scope is intentionally unlimited; it is not a hidden local-first
    /// default. Limits are composed at dispatch time for global, provider,
    /// and App scopes.
    #[serde(default)]
    pub quota: QuotaConfig,
    /// Static, version-controlled inputs to local resource policy. Runtime
    /// observations remain in Resource Manager; this section never encodes a
    /// current memory state or provider-native protocol detail.
    #[serde(default)]
    pub resources: ResourceConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, PolicyProfile>,
    #[serde(default)]
    pub intents: BTreeMap<String, IntentProfile>,
    #[serde(default)]
    pub model_profiles: BTreeMap<String, ModelProfileConfig>,
    #[serde(default)]
    pub model_builds: BTreeMap<String, ModelBuildConfig>,
    #[serde(default)]
    pub deployments: BTreeMap<String, DeploymentConfig>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationConfig {
    #[serde(default)]
    pub enabled: bool,
    pub graph: Option<String>,
    pub graph_sha256: Option<String>,
    pub runtime_library: Option<String>,
    pub runtime_version: Option<String>,
    pub socket_directory: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactStoreConfig {
    /// Platform-specific application-data root when omitted.
    pub root: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRuntimesConfig {
    #[serde(default)]
    pub onnx: OnnxRuntimeConfig,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnnxRuntimeConfig {
    /// Absolute path to the version-pinned ONNX Runtime dynamic library.
    pub library: Option<String>,
    /// Human-readable distribution version recorded in provenance.
    pub version: Option<String>,
    #[serde(default)]
    pub preferred_execution_providers: Vec<OnnxExecutionProvider>,
    /// A provider initialization failure may fall back to CPU only when this
    /// is explicit. Actual routing is always disclosed in result provenance.
    #[serde(default)]
    pub allow_cpu_fallback: bool,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default = "default_managed_credentials_directory")]
    pub managed_credentials_directory: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistenceConfig {
    #[serde(default = "default_persistence_path")]
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_background_payload_directory")]
    pub payload_directory: String,
    /// Name of an environment variable containing exactly 32 key bytes as 64
    /// hexadecimal characters. The key value is never serialized into config.
    pub key_env: Option<String>,
    #[serde(default = "default_background_max_payload_bytes")]
    pub max_payload_bytes: usize,
    #[serde(default = "default_background_result_retention_ms")]
    pub result_retention_ms: u64,
    #[serde(default = "default_background_max_recovery_replays")]
    pub max_recovery_replays: u8,
}

/// Version-controlled limits for costs and short-lived dispatch resources.
/// Provider and App entries refine the global entry; all applicable entries
/// must admit an Attempt in the same reservation transaction.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    #[serde(default)]
    pub global: QuotaLimitConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, QuotaLimitConfig>,
    #[serde(default)]
    pub apps: BTreeMap<String, QuotaLimitConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimitConfig {
    /// Lifetime USD cap for the active SQLite ledger. Operators reset this by
    /// rolling to a new database or a later explicit accounting period.
    pub max_usd: Option<f64>,
    /// Sliding 60-second request allowance.
    pub requests_per_minute: Option<u64>,
    /// Sliding 60-second estimated-token allowance.
    pub tokens_per_minute: Option<u64>,
    /// Number of outstanding Attempts. This may be stricter than the
    /// provider scheduler's own concurrency cap.
    pub max_concurrent_attempts: Option<usize>,
}

/// Version-controlled local resource policy. It deliberately separates a
/// non-mutating recommendation mode from any future automatic action owner.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    /// Host-pressure classification policy. Sampling remains observational;
    /// these thresholds only classify a measured free-memory percentage.
    #[serde(default)]
    pub pressure: PressureThresholdConfig,
    #[serde(default)]
    pub eviction: EvictionPolicyConfig,
    /// Reload-cost measurements are per deployment because quantization,
    /// backend and host hardware all materially affect residency behavior.
    #[serde(default)]
    pub reload_benchmarks: BTreeMap<String, ReloadBenchmarkConfig>,
    /// Optional, explicit node-wide admission budget shared by all local
    /// Providers. Omitted dimensions are intentionally ungoverned: runtime
    /// never invents a memory estimate from a model name or artifact size.
    #[serde(default)]
    pub admission_capacity: AdmissionCapacityConfig,
}

/// Measured host capacity made available to ordinary execution admission.
/// These values are a safety envelope, not a hardware inventory probe.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionCapacityConfig {
    /// Concurrent CPU work units available to configured local deployments.
    pub cpu_slots: Option<u32>,
    /// Unified/system memory budget expressed in MiB. It must be based on an
    /// operator-owned measurement, not a model family heuristic.
    pub unified_memory_mib: Option<u32>,
    /// Accelerator work units shared by configured local deployments.
    pub accelerator_slots: Option<u32>,
    /// Bounded number of jobs waiting for node capacity before their Provider
    /// queue. This bound is active only when at least one capacity dimension
    /// is configured.
    #[serde(default = "default_admission_capacity_max_waiting_jobs")]
    pub max_waiting_jobs: usize,
    /// Shared-capacity priority aging. It uses the same three priority bands
    /// as Provider queues, but has its own owner and bounded queue.
    #[serde(default = "default_admission_capacity_priority_aging_ms")]
    pub priority_aging_ms: u64,
}

impl Default for AdmissionCapacityConfig {
    fn default() -> Self {
        Self {
            cpu_slots: None,
            unified_memory_mib: None,
            accelerator_slots: None,
            max_waiting_jobs: default_admission_capacity_max_waiting_jobs(),
            priority_aging_ms: default_admission_capacity_priority_aging_ms(),
        }
    }
}

/// Per-deployment reservation claim against the optional node-wide budget.
/// A zero value means that dimension is not yet measured for this deployment,
/// so it does not consume an unverified synthetic reservation.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeploymentResourceEstimateConfig {
    #[serde(default)]
    pub cpu_slots: u32,
    #[serde(default)]
    pub unified_memory_mib: u32,
    #[serde(default)]
    pub accelerator_slots: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PressureThresholdConfig {
    /// Read-only host pressure refresh cadence. Native model inventory has a
    /// separate lifecycle and is not queried by this timer.
    #[serde(default = "default_pressure_refresh_interval_ms")]
    pub refresh_interval_ms: u64,
    /// `elevated` when measured free memory is at or below this percentage.
    #[serde(default = "default_elevated_pressure_percent")]
    pub elevated_at_or_below_free_memory_percent: u8,
    /// `critical` takes precedence when measured free memory is at or below
    /// this lower percentage.
    #[serde(default = "default_critical_pressure_percent")]
    pub critical_at_or_below_free_memory_percent: u8,
}

impl Default for PressureThresholdConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: default_pressure_refresh_interval_ms(),
            elevated_at_or_below_free_memory_percent: default_elevated_pressure_percent(),
            critical_at_or_below_free_memory_percent: default_critical_pressure_percent(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvictionPolicyConfig {
    #[serde(default)]
    pub mode: EvictionMode,
    /// Default per-model eligibility for a future automatic action owner.
    /// `recommend` mode remains non-mutating even when this is true.
    #[serde(default)]
    pub automatic_eligible: bool,
    #[serde(default = "default_minimum_resident_ms")]
    pub minimum_resident_ms: u64,
    #[serde(default = "default_max_benchmark_age_ms")]
    pub max_benchmark_age_ms: u64,
    /// Resource-class defaults override the global safety values.
    #[serde(default)]
    pub classes: BTreeMap<ResourceClass, EvictionSafetyOverrideConfig>,
    /// Deployment-specific values take precedence over both class and global
    /// safety values.
    #[serde(default)]
    pub deployments: BTreeMap<String, EvictionSafetyOverrideConfig>,
    #[serde(default)]
    pub monitor: EvictionMonitorConfig,
    #[serde(default)]
    pub elevated: PressureTargetConfig,
    #[serde(default)]
    pub critical: PressureTargetConfig,
}

impl Default for EvictionPolicyConfig {
    fn default() -> Self {
        Self {
            mode: EvictionMode::Disabled,
            automatic_eligible: false,
            minimum_resident_ms: default_minimum_resident_ms(),
            max_benchmark_age_ms: default_max_benchmark_age_ms(),
            classes: BTreeMap::new(),
            deployments: BTreeMap::new(),
            monitor: EvictionMonitorConfig::default(),
            elevated: PressureTargetConfig::default(),
            critical: PressureTargetConfig::default(),
        }
    }
}

/// Background pressure observation remains inert unless explicitly enabled,
/// and even then requires a short-lived runtime maintenance lease.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvictionMonitorConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_eviction_monitor_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_eviction_monitor_max_lease_ms")]
    pub max_lease_ms: u64,
}

impl Default for EvictionMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_ms: default_eviction_monitor_poll_interval_ms(),
            max_lease_ms: default_eviction_monitor_max_lease_ms(),
        }
    }
}

/// Sparse safety override. `None` inherits from the less-specific layer, so
/// adding a minimum residency override cannot accidentally enable eviction.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvictionSafetyOverrideConfig {
    pub automatic_eligible: Option<bool>,
    pub minimum_resident_ms: Option<u64>,
}

#[derive(
    Debug, Clone, Copy, Default, Deserialize, serde::Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum EvictionMode {
    #[default]
    Disabled,
    Recommend,
}

impl std::str::FromStr for EvictionMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "recommend" => Ok(Self::Recommend),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PressureTargetConfig {
    /// Target system-wide free memory after an eviction recommendation. The
    /// value is a percentage because macOS exposes free memory that way.
    pub target_free_memory_percent: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadBenchmarkConfig {
    /// Measured model residency time, excluding user inference. This is a
    /// hard safety input: missing or stale values make a model ineligible.
    pub reload_cost_ms: u64,
    /// Unix milliseconds, emitted by a future benchmark command or recorded
    /// from an equivalent operator-run measurement.
    pub observed_at_unix_ms: u64,
    /// Human-readable evidence reference, kept in the versioned config
    /// snapshot for audit rather than inferred from model size.
    pub evidence: String,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            path: default_persistence_path(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            managed_credentials_directory: default_managed_credentials_directory(),
        }
    }
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            payload_directory: default_background_payload_directory(),
            key_env: None,
            max_payload_bytes: default_background_max_payload_bytes(),
            result_retention_ms: default_background_result_retention_ms(),
            max_recovery_replays: default_background_max_recovery_replays(),
        }
    }
}

fn default_persistence_path() -> String {
    "infer-runtime.sqlite3".into()
}

fn default_managed_credentials_directory() -> String {
    ".infer-runtime/credentials".into()
}

fn default_background_payload_directory() -> String {
    "infer-runtime-payloads".into()
}

fn default_background_max_payload_bytes() -> usize {
    4 * 1024 * 1024
}

fn default_background_result_retention_ms() -> u64 {
    24 * 60 * 60 * 1_000
}

fn default_background_max_recovery_replays() -> u8 {
    2
}

fn default_minimum_resident_ms() -> u64 {
    300_000
}

fn default_elevated_pressure_percent() -> u8 {
    15
}

fn default_critical_pressure_percent() -> u8 {
    5
}

fn default_eviction_monitor_poll_interval_ms() -> u64 {
    30_000
}

fn default_pressure_refresh_interval_ms() -> u64 {
    30_000
}

fn default_admission_capacity_max_waiting_jobs() -> usize {
    128
}

fn default_admission_capacity_priority_aging_ms() -> u64 {
    10_000
}

fn default_eviction_monitor_max_lease_ms() -> u64 {
    3_600_000
}

fn default_max_benchmark_age_ms() -> u64 {
    30 * 24 * 60 * 60 * 1_000
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_observer_instance_id")]
    pub instance_id: String,
    /// Optional operator deep-link published only in the Infer-owned status
    /// snapshot. Infra Discovery never carries UI links.
    pub console_url: Option<String>,
    /// Optional App id reserved for the explicit HTTP diagnostic fallback.
    /// It is never written to Infra Discovery or the Unix status protocol.
    pub http_credential_id: Option<String>,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance_id: default_observer_instance_id(),
            console_url: None,
            http_credential_id: None,
        }
    }
}

fn default_observer_instance_id() -> String {
    "local".into()
}

fn is_loopback_http_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    authority
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback())
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    pub policy: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// Economic/access boundary for this Provider instance. Subscription
    /// bridges are denied to Apps unless they opt in explicitly.
    #[serde(default)]
    pub access_class: ProviderAccessClass,
    pub capability_profile: ProviderCapabilityProfile,
    pub base_url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// When true, this provider is absent from routing until its configured
    /// environment variable contains a non-empty credential.
    #[serde(default)]
    pub requires_api_key: bool,
    pub placement: Placement,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_max_queue")]
    pub max_queue: usize,
    #[serde(default = "default_priority_aging_ms")]
    pub priority_aging_ms: u64,
    pub api_key_env: Option<String>,
    /// Optional control-plane inventory source for a local provider. It is
    /// intentionally separate from the execution endpoint: model discovery
    /// and lifecycle management have a different protocol and failure policy.
    #[serde(default)]
    pub local_inventory: Option<LocalInventoryConfig>,
}

string_enum!(ProviderKind {
    Responses => "responses",
    CodexAppServer => "codex_app_server",
    AudioWorker => "audio_worker",
    RetrievalWorker => "retrieval_worker",
    OcrWorker => "ocr_worker",
    Onnx => "onnx",
    RawFoundation => "raw_foundation"
});

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Responses => "responses",
            Self::CodexAppServer => "codex_app_server",
            Self::AudioWorker => "audio_worker",
            Self::RetrievalWorker => "retrieval_worker",
            Self::OcrWorker => "ocr_worker",
            Self::Onnx => "onnx",
            Self::RawFoundation => "raw_foundation",
        })
    }
}

string_enum!(ProviderAccessClass {
    Standard => "standard",
    Subscription => "subscription"
});

// `string_enum!` cannot attach `#[default]` to one generated variant.
#[allow(clippy::derivable_impls)]
impl Default for ProviderAccessClass {
    fn default() -> Self {
        Self::Standard
    }
}

/// Explicit opt-in to a local provider's native model inventory API. Runtime
/// routing never guesses a control protocol from a Responses-compatible URL.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInventoryConfig {
    pub kind: LocalInventoryKind,
    /// Native control endpoint, without the protocol-specific path. For
    /// Ollama this is normally `http://127.0.0.1:11434`.
    pub endpoint: Option<String>,
}

string_enum!(LocalInventoryKind {
    OllamaTags => "ollama_tags",
    OnnxSessions => "onnx_sessions"
});

/// Versioned, provider-instance-specific declaration of the wire behaviors the
/// runtime may rely on. A provider kind is only a protocol family; it is not a
/// claim that every optional field in that family is accepted.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilityProfile {
    pub version: u16,
    pub protocol: ProviderProtocol,
    #[serde(default)]
    pub capabilities: BTreeSet<ProviderCapability>,
}

impl ProviderCapabilityProfile {
    pub fn supports(&self, capability: ProviderCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

fn default_concurrency() -> usize {
    1
}
fn default_max_queue() -> usize {
    64
}
fn default_priority_aging_ms() -> u64 {
    10_000
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfile {
    pub order: Vec<SortKey>,
}

/// A stable workload contract exposed through the Responses `model` field.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntentProfile {
    #[serde(default = "default_data_plane")]
    pub data_plane: String,
    #[serde(default)]
    pub input_modalities: Vec<Modality>,
    #[serde(default)]
    pub output_modalities: Vec<Modality>,
    #[serde(default)]
    pub required_features: Vec<String>,
    pub default_capability_floor: CapabilityLevel,
    pub default_policy: Option<String>,
    /// Applied only when a Responses caller omits `max_output_tokens`.
    /// Explicit request values always win.
    pub default_max_output_tokens: Option<u32>,
    /// Applied only when a Responses caller omits the entire `reasoning`
    /// object. This lets lightweight intents disable thinking without making
    /// placement or model identity part of the intent contract.
    pub default_reasoning_effort: Option<ReasoningEffort>,
}

fn default_data_plane() -> String {
    "responses".into()
}

/// Semantic model identity and workload-specific, evidence-bearing ratings.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfileConfig {
    pub family: String,
    #[serde(default)]
    pub ratings: BTreeMap<String, CapabilityRating>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRating {
    pub level: CapabilityLevel,
    pub status: EvaluationStatus,
    pub eval_profile: Option<String>,
    pub score: Option<f64>,
    pub evaluated_at: Option<String>,
}

/// A concrete artifact/quantization of a semantic model profile.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBuildConfig {
    pub profile: String,
    pub model_id: String,
    pub variant: Option<String>,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub input_modalities: Vec<Modality>,
    #[serde(default)]
    pub output_modalities: Vec<Modality>,
    #[serde(default)]
    pub features: Vec<String>,
    /// Supply-chain ownership and provenance for this exact Build. This is a
    /// deployment fact, not a legal opinion inferred by the runtime.
    #[serde(default)]
    pub provenance: ModelBuildProvenanceConfig,
    /// Evidence recorded by the operator who admitted the Build. Unknown or
    /// unreviewed facts remain explicit instead of being guessed from a model
    /// name or provider cache.
    #[serde(default)]
    pub license: ModelBuildLicenseConfig,
    /// Present only for ONNX builds. This is execution identity, not a public
    /// tensor API: typed adapters remain the sole consumer-facing boundary.
    #[serde(default)]
    pub onnx: Option<OnnxModelBuildConfig>,
    /// Present only for trusted, typed local worker Builds. The immutable file
    /// set is content-addressed; Consumers never see artifact paths or worker
    /// protocol details.
    #[serde(default)]
    pub local_worker: Option<LocalWorkerModelBuildConfig>,
    /// Policy and evidence identity for a typed AudioSet event Build. The
    /// executable files remain owned by `local_worker` and ArtifactStore.
    #[serde(default)]
    pub audio_event: Option<AudioEventModelBuildConfig>,
}

string_enum!(ModelSourceKind {
    Unknown => "unknown",
    UserManaged => "user_managed",
    ProviderManaged => "provider_managed",
    OrganizationManaged => "organization_managed",
    RuntimeDownloadable => "runtime_downloadable",
    RuntimeBundled => "runtime_bundled"
});

// `string_enum!` cannot attach `#[default]` to one generated variant.
#[allow(clippy::derivable_impls)]
impl Default for ModelSourceKind {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelBuildProvenanceConfig {
    #[serde(default)]
    pub source_kind: ModelSourceKind,
    pub upstream: Option<String>,
    pub source_revision: Option<String>,
    pub artifact_sha256: Option<String>,
}

string_enum!(ModelLicenseStatus {
    Unreviewed => "unreviewed",
    Declared => "declared",
    Verified => "verified",
    Restricted => "restricted",
    Unknown => "unknown"
});

// `string_enum!` cannot attach `#[default]` to one generated variant.
#[allow(clippy::derivable_impls)]
impl Default for ModelLicenseStatus {
    fn default() -> Self {
        Self::Unreviewed
    }
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelBuildLicenseConfig {
    #[serde(default)]
    pub status: ModelLicenseStatus,
    pub expression: Option<String>,
    pub license_url: Option<String>,
    pub license_text_sha256: Option<String>,
    pub reviewed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OnnxModelBuildConfig {
    pub adapter: OnnxAdapterKind,
    pub artifact: ArtifactIdentityConfig,
    /// Additional immutable files required by the typed adapter, such as a
    /// tokenizer. Every file is content-addressed and verified by the same
    /// Build manifest as the executable graph.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auxiliary_artifacts: BTreeMap<String, ArtifactIdentityConfig>,
    pub opset: u32,
    #[serde(default)]
    pub inputs: Vec<TensorContractConfig>,
    #[serde(default)]
    pub outputs: Vec<TensorContractConfig>,
    /// Present for image adapters. Text adapters instead carry an explicit
    /// tokenizer contract so a placeholder image schema is never required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preprocessing: Option<ImagePreprocessConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_preprocessing: Option<TextPreprocessConfig>,
    /// Shared semantic identity for vector outputs. Image and text Builds may
    /// claim the same space only when every field is identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_space: Option<EmbeddingSpaceConfig>,
    pub postprocessing_identity: String,
    /// Adapter-owned constants such as score/NMS thresholds. They are part of
    /// Build identity but are never accepted from a Consumer request.
    #[serde(default)]
    pub postprocessing_parameters: BTreeMap<String, f64>,
    #[serde(default)]
    pub allowed_execution_providers: Vec<OnnxExecutionProvider>,
    pub precision: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentityConfig {
    pub sha256: String,
    pub size_bytes: u64,
    pub source_url: String,
    pub source_revision: String,
    pub license_spdx: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkerModelBuildConfig {
    pub adapter: LocalWorkerAdapterKind,
    /// SHA-256 over sorted `relative-name NUL file-sha256 LF` records.
    pub artifact_set_sha256: String,
    pub runtime: String,
    pub precision: String,
    pub postprocessing_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_space: Option<EmbeddingSpaceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preprocessing_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_execution_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_execution_provider: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AudioEventModelBuildConfig {
    pub model_archive_sha256: String,
    pub runtime: String,
    pub runtime_version: String,
    pub decoder: String,
    pub preprocessing_identity: String,
    pub ontology_id: String,
    pub ontology_revision: String,
    pub class_id_namespace: String,
    pub class_count: usize,
    pub ontology_artifact_sha256: String,
    pub ontology_license_spdx: String,
    pub training_data_license_spdx: String,
    pub policy_revision: String,
    pub score_kind: String,
    pub window_seconds: f64,
    pub hop_seconds: f64,
    pub event_score_threshold: f64,
    pub smoothing_method: String,
    pub smoothing_window_frames: usize,
    pub max_classes_per_window: usize,
    pub max_events: usize,
    pub speech_class_set_revision: String,
    pub speech_present_threshold: f64,
    pub speech_absent_threshold: f64,
    pub max_audio_seconds: u64,
}

string_enum!(LocalWorkerAdapterKind {
    Qwen3Embedding => "qwen3_embedding",
    Qwen3Reranker => "qwen3_reranker",
    PpOcrv6 => "pp_ocrv6",
    YamnetAudioEvents => "yamnet_audio_events"
});

impl std::fmt::Display for LocalWorkerAdapterKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Qwen3Embedding => "qwen3_embedding",
            Self::Qwen3Reranker => "qwen3_reranker",
            Self::PpOcrv6 => "pp_ocrv6",
            Self::YamnetAudioEvents => "yamnet_audio_events",
        })
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TensorContractConfig {
    pub name: String,
    pub dtype: String,
    #[serde(default)]
    pub shape: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImagePreprocessConfig {
    /// Stable identifier included in result provenance and vector-space
    /// identity. Any semantic preprocessing change requires a new identifier.
    pub identity: String,
    pub orientation: String,
    pub resize: String,
    pub color_space: String,
    pub channel_order: String,
    pub layout: String,
    pub dtype: String,
    #[serde(default)]
    pub mean: Vec<f32>,
    #[serde(default)]
    pub scale: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TextPreprocessConfig {
    pub identity: String,
    /// Key in `auxiliary_artifacts` containing the tokenizer.json bytes.
    pub tokenizer_artifact: String,
    pub lowercase: bool,
    pub padding: String,
    pub truncation: bool,
    pub max_length: usize,
    pub pad_token_id: u32,
    pub eos_token_id: u32,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingSpaceConfig {
    pub identity: String,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
}

string_enum!(OnnxAdapterKind {
    YunetFaceDetection => "yunet_face_detection",
    SfaceEmbedding => "sface_embedding",
    SiglipImageEmbedding => "siglip_image_embedding",
    SiglipTextEmbedding => "siglip_text_embedding"
});
string_enum!(OnnxExecutionProvider { Coreml => "coreml", Cpu => "cpu" });

/// A runnable model build attached to one provider endpoint.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    pub provider: String,
    pub build: String,
    #[serde(default = "default_resource_class")]
    pub resource_class: ResourceClass,
    #[serde(default)]
    pub supported_efforts: Vec<ReasoningEffort>,
    #[serde(default)]
    pub estimated_cost_usd: f64,
    /// An optional measured claim against `resources.admission_capacity`.
    /// The reservation is local execution control data, never a model rating
    /// or public Consumer selection parameter.
    #[serde(default)]
    pub resource_estimate: DeploymentResourceEstimateConfig,
    /// Execution shapes verified for this concrete deployment. The default is
    /// unary-only so a new provider cannot accidentally inherit streaming.
    #[serde(default = "default_execution_modes")]
    pub supported_execution_modes: BTreeSet<ExecutionMode>,
}

fn default_execution_modes() -> BTreeSet<ExecutionMode> {
    BTreeSet::from([ExecutionMode::Unary])
}

fn default_resource_class() -> ResourceClass {
    ResourceClass::Standard
}

string_enum!(ObserverAccess {
    None => "none",
    Summary => "summary"
});

// `string_enum!` cannot attach `#[default]` to one generated variant.
#[allow(clippy::derivable_impls)]
impl Default for ObserverAccess {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub credential: AppCredentialConfig,
    /// A dedicated observer identity is restricted to the summary snapshot on
    /// the HTTP layer. It cannot reuse ordinary Consumer or operator routes.
    #[serde(default)]
    pub observer_access: ObserverAccess,
    /// Grants native local-resource mutation and resource-audit inspection.
    /// Ordinary inference Apps remain unprivileged by default.
    #[serde(default)]
    pub resource_admin: bool,
    /// Explicit operator-only grant for every configured Intent. This is
    /// separate from `allowed_intents` so omission never widens an ordinary
    /// Consumer's authority.
    #[serde(default)]
    pub allow_all_intents: bool,
    /// Stable Intent ids this App may submit. Omission and an explicit empty
    /// list both deny inference unless the operator-only all-Intent grant is
    /// present.
    #[serde(default)]
    pub allowed_intents: Option<Vec<String>>,
    /// Optional named-routing execution boundary. Absence preserves legacy
    /// capability routing while denying Consumer-supplied named targets.
    #[serde(default)]
    pub routing: Option<crate::AppRoutingConfig>,
    /// Hosted Responses tools are an independent data/side-effect boundary.
    /// A Provider capability and Intent grant never imply this App authority.
    #[serde(default)]
    pub allowed_builtin_tools: BTreeSet<BuiltinTool>,
    /// Optional allowlist for the public `voice` values accepted by
    /// `speech.synthesize`. Omitting it denies speech aliases; an explicit
    /// list lets a Consumer depend only on Runtime-owned aliases.
    #[serde(default)]
    pub allowed_speech_voice_aliases: Option<Vec<String>>,
    /// Explicit operator-only grant for every Runtime-owned speech alias.
    #[serde(default)]
    pub allow_all_speech_voice_aliases: bool,
    /// Provider economic/access classes this App may consume. The default is
    /// deliberately standard-only so adding a subscription bridge never
    /// widens an existing Consumer's authority.
    #[serde(default = "default_provider_access_classes")]
    pub allowed_provider_access_classes: BTreeSet<ProviderAccessClass>,
    /// Modalities this App may send to cloud-placed providers. This is an
    /// egress boundary, separate from provider economics/access entitlement.
    /// Omission denies cloud payload egress; every allowed modality must be an
    /// explicit App grant.
    #[serde(default)]
    pub allowed_cloud_input_modalities: BTreeSet<Modality>,
    #[serde(default = "default_app_max_pending_jobs")]
    pub max_pending_jobs: usize,
    pub default_policy: Option<String>,
    #[serde(default)]
    pub allowed_policies: Vec<String>,
    #[serde(default)]
    pub request_overrides: RequestOverrideConfig,
}

impl AppConfig {
    pub fn allows_intent(&self, intent: &str) -> bool {
        self.allow_all_intents
            || self
                .allowed_intents
                .as_ref()
                .is_some_and(|allowed| allowed.iter().any(|candidate| candidate == intent))
    }

    pub fn allows_speech_voice(&self, voice: &str) -> bool {
        self.allow_all_speech_voice_aliases
            || self
                .allowed_speech_voice_aliases
                .as_ref()
                .is_some_and(|allowed| allowed.iter().any(|candidate| candidate == voice))
    }

    pub fn allows_builtin_tool(&self, tool: BuiltinTool) -> bool {
        self.allowed_builtin_tools.contains(&tool)
    }

    pub fn allows_provider_access(&self, access_class: ProviderAccessClass) -> bool {
        self.allowed_provider_access_classes.contains(&access_class)
    }

    pub fn allows_cloud_inputs(&self, modalities: &BTreeSet<Modality>) -> bool {
        modalities.is_subset(&self.allowed_cloud_input_modalities)
    }
}

fn default_provider_access_classes() -> BTreeSet<ProviderAccessClass> {
    BTreeSet::from([ProviderAccessClass::Standard])
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppCredentialConfig {
    /// Runtime-generated credential stored in the protected local credential
    /// directory. Appropriate for local operator identities.
    Managed,
    /// Externally provisioned consumer credential resolved once at startup.
    Environment { variable: String },
}

fn default_app_max_pending_jobs() -> usize {
    32
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestOverrideConfig {
    #[serde(default)]
    pub priority: Vec<Priority>,
    #[serde(default)]
    pub placement: Vec<PlacementScope>,
    #[serde(default)]
    pub prefer: Vec<PlacementPreference>,
    #[serde(default)]
    pub offline_required: bool,
    #[serde(default)]
    pub capability_floor: Vec<CapabilityLevel>,
    #[serde(default)]
    pub latency: Vec<Latency>,
    pub max_cost_usd: Option<NumericRange>,
    #[serde(default)]
    pub fallback: Vec<Fallback>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct NumericRange {
    pub min: f64,
    pub max: f64,
}

impl RuntimeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ContractError> {
        let source = fs::read_to_string(path)
            .map_err(|error| ContractError::Configuration(error.to_string()))?;
        let config: Self = toml::from_str(&source)
            .map_err(|error| ContractError::Configuration(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.server.bind.trim().is_empty() {
            return Err(configuration("server.bind is required"));
        }
        self.validate_observer()?;
        if self.persistence.path.trim().is_empty() {
            return Err(configuration("persistence.path is required"));
        }
        if self.auth.managed_credentials_directory.trim().is_empty() {
            return Err(configuration(
                "auth.managed_credentials_directory is required",
            ));
        }
        if self
            .artifacts
            .root
            .as_deref()
            .is_some_and(|root| root.trim().is_empty())
        {
            return Err(configuration("artifacts.root cannot be empty"));
        }
        self.validate_background()?;
        self.require_profile(&self.defaults.policy)?;
        self.validate_providers()?;
        self.validate_intents()?;
        self.validate_models()?;
        self.validate_deployments()?;
        self.validate_raw_foundation()?;
        self.validate_apps()?;
        self.validate_quota()?;
        self.validate_resources()?;
        Ok(())
    }

    fn validate_raw_foundation(&self) -> Result<(), ContractError> {
        if !self.raw_foundation.enabled {
            return Ok(());
        }
        let raw = &self.raw_foundation;
        let required = [
            raw.graph.as_deref(),
            raw.graph_sha256.as_deref(),
            raw.runtime_library.as_deref(),
            raw.runtime_version.as_deref(),
            raw.socket_directory.as_deref(),
        ];
        if required
            .into_iter()
            .any(|value| value.is_none_or(str::is_empty))
        {
            return Err(configuration(
                "enabled raw_foundation requires graph, graph_sha256, runtime_library, runtime_version, and socket_directory",
            ));
        }
        let digest = raw.graph_sha256.as_deref().unwrap_or_default();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(configuration(
                "raw_foundation.graph_sha256 must be SHA-256 hex",
            ));
        }
        if raw.runtime_version.as_deref() != Some("1.27.0") {
            return Err(configuration(
                "experimental RawNIND Build requires raw_foundation.runtime_version = 1.27.0",
            ));
        }
        let intent = self
            .intents
            .get("raw.materialize_foundation")
            .ok_or_else(|| {
                configuration("enabled raw_foundation requires intent raw.materialize_foundation")
            })?;
        if intent.data_plane != "raw.foundation"
            || intent.input_modalities != [Modality::Image]
            || intent.output_modalities != [Modality::Image]
            || intent.default_capability_floor != CapabilityLevel::Foundational
            || intent.default_policy.as_deref() != Some("local-first")
            || !intent
                .required_features
                .iter()
                .any(|feature| feature == "rawnind_foundation_ort127_exp1")
        {
            return Err(configuration(
                "raw.materialize_foundation must preserve the frozen RAW data-plane, modalities, and feature identity",
            ));
        }
        let profile = self.model_profiles.get("rawnind").ok_or_else(|| {
            configuration("enabled raw_foundation requires model profile rawnind")
        })?;
        let rating = profile
            .ratings
            .get("raw.materialize_foundation")
            .ok_or_else(|| {
                configuration("model profile rawnind must rate raw.materialize_foundation")
            })?;
        if profile.family != "rawnind"
            || rating.level != CapabilityLevel::Foundational
            || rating.status != EvaluationStatus::Benchmarked
            || rating.eval_profile.as_deref().is_none_or(str::is_empty)
        {
            return Err(configuration(
                "RawNIND activation requires a benchmarked foundational rating with eval_profile evidence",
            ));
        }
        let build = self
            .model_builds
            .get("rawnind_ort127_exp1")
            .ok_or_else(|| {
                configuration("enabled raw_foundation requires model build rawnind_ort127_exp1")
            })?;
        if build.profile != "rawnind"
            || build.model_id != "darktable-ai/rawnind-public-bayer"
            || build.variant.as_deref() != Some("onnxruntime-1.27.0-cpu-fp32")
            || build.input_modalities != [Modality::Image]
            || build.output_modalities != [Modality::Image]
            || !build
                .features
                .iter()
                .any(|feature| feature == "rawnind_foundation_ort127_exp1")
        {
            return Err(configuration(
                "RawNIND activation requires the exact rawnind_ort127_exp1 Build identity",
            ));
        }
        let deployment = self.deployments.get("rawnind_ort127_exp1").ok_or_else(|| {
            configuration("enabled raw_foundation requires deployment rawnind_ort127_exp1")
        })?;
        if deployment.provider != "raw-foundation-local"
            || deployment.build != "rawnind_ort127_exp1"
            || deployment.resource_class != ResourceClass::Heavy
            || deployment.estimated_cost_usd != 0.0
            || !deployment
                .supported_execution_modes
                .contains(&ExecutionMode::Unary)
        {
            return Err(configuration(
                "RawNIND activation requires unary deployment rawnind_ort127_exp1 on raw-foundation-local",
            ));
        }
        Ok(())
    }

    fn validate_observer(&self) -> Result<(), ContractError> {
        if self.observer.instance_id.is_empty()
            || !self
                .observer
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(configuration(
                "observer.instance_id must be a non-empty path-safe identifier",
            ));
        }
        if let Some(console_url) = &self.observer.console_url
            && !is_loopback_http_url(console_url)
        {
            return Err(configuration("observer.console_url must use loopback HTTP"));
        }
        if let Some(credential_id) = &self.observer.http_credential_id {
            if credential_id.is_empty() {
                return Err(configuration("observer.http_credential_id cannot be empty"));
            }
            let app = self.apps.get(credential_id).ok_or_else(|| {
                configuration(format!(
                    "observer.http_credential_id names unknown App {credential_id}"
                ))
            })?;
            if app.observer_access != ObserverAccess::Summary {
                return Err(configuration(format!(
                    "observer HTTP App {credential_id} must set observer_access = \"summary\""
                )));
            }
        }
        if !self.observer.enabled {
            return Ok(());
        }
        let bind = self
            .server
            .bind
            .parse::<std::net::SocketAddr>()
            .map_err(|_| configuration("observer requires server.bind to be a socket address"))?;
        if !bind.ip().is_loopback() {
            return Err(configuration(
                "observer registration requires a loopback server.bind",
            ));
        }
        Ok(())
    }

    fn validate_background(&self) -> Result<(), ContractError> {
        if !self.background.enabled {
            return Ok(());
        }
        if self.background.payload_directory.trim().is_empty() {
            return Err(configuration(
                "background.payload_directory is required when background execution is enabled",
            ));
        }
        if self
            .background
            .key_env
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err(configuration(
                "background.key_env is required when background execution is enabled",
            ));
        }
        if !(1..=25 * 1024 * 1024).contains(&self.background.max_payload_bytes) {
            return Err(configuration(
                "background.max_payload_bytes must be between 1 byte and 25 MiB",
            ));
        }
        if !(60_000..=30 * 24 * 60 * 60 * 1_000).contains(&self.background.result_retention_ms) {
            return Err(configuration(
                "background.result_retention_ms must be between 1 minute and 30 days",
            ));
        }
        if self.background.max_recovery_replays == 0 {
            return Err(configuration(
                "background.max_recovery_replays must be positive",
            ));
        }
        Ok(())
    }

    fn validate_providers(&self) -> Result<(), ContractError> {
        for (id, provider) in &self.providers {
            match provider.kind {
                ProviderKind::Responses
                    if provider.base_url.as_deref().is_none_or(str::is_empty) =>
                {
                    return Err(configuration(format!("provider {id} needs base_url")));
                }
                ProviderKind::AudioWorker
                | ProviderKind::RetrievalWorker
                | ProviderKind::OcrWorker
                    if provider.command.as_deref().is_none_or(str::is_empty) =>
                {
                    return Err(configuration(format!(
                        "local worker provider {id} needs command"
                    )));
                }
                ProviderKind::CodexAppServer
                    if provider.command.as_deref().is_none_or(str::is_empty) =>
                {
                    return Err(configuration(format!(
                        "Codex App Server provider {id} needs command"
                    )));
                }
                _ => {}
            }
            let expected_protocol = match provider.kind {
                ProviderKind::Responses => ProviderProtocol::Responses,
                ProviderKind::CodexAppServer => ProviderProtocol::CodexAppServer,
                ProviderKind::AudioWorker => ProviderProtocol::AudioWorker,
                ProviderKind::RetrievalWorker => ProviderProtocol::RetrievalWorker,
                ProviderKind::OcrWorker => ProviderProtocol::OcrWorker,
                ProviderKind::Onnx => ProviderProtocol::Onnx,
                // The specialized RawFoundationControl owns graph execution;
                // this provider contributes only scheduling/admission and
                // uses the same verified native runtime protocol family.
                ProviderKind::RawFoundation => ProviderProtocol::Onnx,
            };
            if provider.capability_profile.version != 1
                || provider.capability_profile.protocol != expected_protocol
            {
                return Err(configuration(format!(
                    "provider {id} needs capability profile version 1 for its protocol"
                )));
            }
            if matches!(
                expected_protocol,
                ProviderProtocol::Responses | ProviderProtocol::CodexAppServer
            ) && !provider
                .capability_profile
                .supports(ProviderCapability::Responses)
            {
                return Err(configuration(format!(
                    "responses provider {id} must declare responses capability"
                )));
            }
            if matches!(
                expected_protocol,
                ProviderProtocol::AudioWorker
                    | ProviderProtocol::RetrievalWorker
                    | ProviderProtocol::OcrWorker
                    | ProviderProtocol::Onnx
            ) && !provider.capability_profile.capabilities.is_empty()
            {
                return Err(configuration(format!(
                    "non-Responses provider {id} cannot declare Responses capabilities"
                )));
            }
            if provider
                .capability_profile
                .supports(ProviderCapability::ImageGeneration)
                && expected_protocol != ProviderProtocol::CodexAppServer
            {
                return Err(configuration(format!(
                    "provider {id} cannot declare the first-slice image_generation capability outside Codex App Server"
                )));
            }
            if provider.max_concurrency == 0
                || provider.max_queue == 0
                || provider.priority_aging_ms == 0
            {
                return Err(configuration(format!(
                    "provider {id} concurrency, queue, and aging limits must be positive"
                )));
            }
            if provider.requires_api_key
                && provider.api_key_env.as_deref().is_none_or(str::is_empty)
            {
                return Err(configuration(format!("provider {id} requires api_key_env")));
            }
            if provider.kind == ProviderKind::CodexAppServer {
                if provider.placement != Placement::Cloud {
                    return Err(configuration(format!(
                        "Codex App Server provider {id} must use cloud placement because its transport is local but inference leaves the machine"
                    )));
                }
                if provider.access_class != ProviderAccessClass::Subscription {
                    return Err(configuration(format!(
                        "Codex App Server provider {id} must use subscription access_class"
                    )));
                }
                if provider.requires_api_key || provider.api_key_env.is_some() {
                    return Err(configuration(format!(
                        "Codex App Server provider {id} uses the Codex account session, not a runtime API key"
                    )));
                }
            }
            if let Some(inventory) = &provider.local_inventory {
                if provider.placement != Placement::Local {
                    return Err(configuration(format!(
                        "provider {id} local_inventory requires local placement"
                    )));
                }
                match inventory.kind {
                    LocalInventoryKind::OllamaTags => {
                        if provider.kind != ProviderKind::Responses
                            || inventory
                                .endpoint
                                .as_deref()
                                .is_none_or(|endpoint| endpoint.trim().is_empty())
                        {
                            return Err(configuration(format!(
                                "provider {id} ollama_tags inventory needs a Responses provider and endpoint"
                            )));
                        }
                    }
                    LocalInventoryKind::OnnxSessions => {
                        if provider.kind != ProviderKind::Onnx || inventory.endpoint.is_some() {
                            return Err(configuration(format!(
                                "provider {id} onnx_sessions inventory needs an ONNX provider and no endpoint"
                            )));
                        }
                    }
                }
            }
            if provider.kind == ProviderKind::Onnx
                && provider
                    .local_inventory
                    .as_ref()
                    .is_none_or(|inventory| inventory.kind != LocalInventoryKind::OnnxSessions)
            {
                return Err(configuration(format!(
                    "ONNX provider {id} needs onnx_sessions local_inventory"
                )));
            }
        }
        let has_onnx = self
            .providers
            .values()
            .any(|provider| provider.kind == ProviderKind::Onnx);
        if has_onnx {
            let runtime = &self.runtimes.onnx;
            if runtime
                .library
                .as_deref()
                .is_none_or(|path| path.trim().is_empty())
                || runtime
                    .version
                    .as_deref()
                    .is_none_or(|version| version.trim().is_empty())
                || runtime.preferred_execution_providers.is_empty()
            {
                return Err(configuration(
                    "ONNX providers require runtimes.onnx library, version, and preferred_execution_providers",
                ));
            }
            if runtime
                .preferred_execution_providers
                .contains(&OnnxExecutionProvider::Cpu)
                && runtime.preferred_execution_providers.last() != Some(&OnnxExecutionProvider::Cpu)
            {
                return Err(configuration(
                    "runtimes.onnx CPU execution provider must be last",
                ));
            }
        }
        Ok(())
    }

    fn validate_intents(&self) -> Result<(), ContractError> {
        for (id, intent) in &self.intents {
            if id.trim().is_empty()
                || !matches!(
                    intent.data_plane.as_str(),
                    "responses"
                        | "audio.transcription"
                        | "audio.alignment"
                        | "audio.event_detection"
                        | "audio.speech"
                        | "audio.voice_clone"
                        | "vision.face_detection"
                        | "vision.face_embedding"
                        | "vision.image_embedding"
                        | "vision.text_embedding"
                        | "vision.image_description"
                        | "vision.classification_review"
                        | "text.embedding"
                        | "text.rerank"
                        | "document.ocr"
                        | "raw.foundation"
                )
                || intent.input_modalities.is_empty()
                || intent.output_modalities.is_empty()
            {
                return Err(configuration(format!(
                    "intent {id} needs a supported data plane and input/output modalities"
                )));
            }
            if let Some(profile) = &intent.default_policy {
                self.require_profile(profile)?;
            }
            if id == crate::IMAGE_GENERATION_INTENT
                && (intent.data_plane != "responses"
                    || intent.input_modalities != vec![Modality::Text]
                    || intent.output_modalities != vec![Modality::Image]
                    || intent.required_features != vec!["image_generation".to_owned()])
            {
                return Err(configuration(
                    "image.generate must remain unary text-to-image Responses inference",
                ));
            }
            if intent.default_max_output_tokens == Some(0) {
                return Err(configuration(format!(
                    "intent {id} default_max_output_tokens must be positive"
                )));
            }
        }
        Ok(())
    }

    fn validate_models(&self) -> Result<(), ContractError> {
        for (id, model) in &self.model_profiles {
            if model.family.trim().is_empty() {
                return Err(configuration(format!("model profile {id} needs family")));
            }
            for (intent, rating) in &model.ratings {
                if !self.intents.contains_key(intent) {
                    return Err(configuration(format!(
                        "model profile {id} rates unknown intent {intent}"
                    )));
                }
                if rating
                    .score
                    .is_some_and(|score| !score.is_finite() || !(0.0..=1.0).contains(&score))
                {
                    return Err(configuration(format!(
                        "model profile {id} has invalid score for {intent}"
                    )));
                }
                if rating.status == EvaluationStatus::Benchmarked && rating.eval_profile.is_none() {
                    return Err(configuration(format!(
                        "benchmarked rating {id}/{intent} needs eval_profile"
                    )));
                }
            }
        }
        for (id, build) in &self.model_builds {
            if !self.model_profiles.contains_key(&build.profile) {
                return Err(configuration(format!(
                    "model build {id} names unknown profile {}",
                    build.profile
                )));
            }
            if build.model_id.trim().is_empty() {
                return Err(configuration(format!("model build {id} needs model_id")));
            }
            validate_build_supply_chain(id, build)?;
            if let Some(audio_event) = &build.audio_event {
                let digest = |value: &str| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                };
                let unit = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
                if build.local_worker.as_ref().map(|worker| worker.adapter)
                    != Some(LocalWorkerAdapterKind::YamnetAudioEvents)
                    || !digest(&audio_event.model_archive_sha256)
                    || !digest(&audio_event.ontology_artifact_sha256)
                    || audio_event.runtime.trim().is_empty()
                    || audio_event.runtime_version.trim().is_empty()
                    || audio_event.decoder != "ffmpeg"
                    || audio_event.preprocessing_identity.trim().is_empty()
                    || audio_event.ontology_id != "audioset"
                    || audio_event.ontology_revision.trim().is_empty()
                    || audio_event.class_id_namespace != "audioset_mid"
                    || audio_event.class_count == 0
                    || audio_event.ontology_license_spdx.trim().is_empty()
                    || audio_event.training_data_license_spdx.trim().is_empty()
                    || audio_event.policy_revision.trim().is_empty()
                    || audio_event.score_kind != "raw_sigmoid"
                    || !audio_event.window_seconds.is_finite()
                    || audio_event.window_seconds <= 0.0
                    || !audio_event.hop_seconds.is_finite()
                    || audio_event.hop_seconds <= 0.0
                    || !unit(audio_event.event_score_threshold)
                    || audio_event.smoothing_method.trim().is_empty()
                    || audio_event.smoothing_window_frames == 0
                    || audio_event.smoothing_window_frames.is_multiple_of(2)
                    || audio_event.max_classes_per_window == 0
                    || audio_event.max_events == 0
                    || audio_event.speech_class_set_revision.trim().is_empty()
                    || !unit(audio_event.speech_present_threshold)
                    || !unit(audio_event.speech_absent_threshold)
                    || audio_event.speech_absent_threshold >= audio_event.speech_present_threshold
                    || audio_event.max_audio_seconds == 0
                {
                    return Err(configuration(format!(
                        "audio-event model build {id} needs a typed YAMNet worker and complete artifact, ontology, decode, policy, and license identity"
                    )));
                }
            }
            if let Some(onnx) = &build.onnx {
                if onnx.opset == 0
                    || onnx.artifact.size_bytes == 0
                    || onnx.artifact.sha256.len() != 64
                    || !onnx
                        .artifact
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    || onnx.artifact.source_url.trim().is_empty()
                    || onnx.artifact.source_revision.trim().is_empty()
                    || onnx.artifact.license_spdx.trim().is_empty()
                    || onnx.inputs.is_empty()
                    || onnx.outputs.is_empty()
                    || onnx.postprocessing_identity.trim().is_empty()
                    || onnx
                        .postprocessing_parameters
                        .values()
                        .any(|value| !value.is_finite())
                    || onnx.allowed_execution_providers.is_empty()
                    || onnx.precision.trim().is_empty()
                    || build.model_id != format!("sha256:{}", onnx.artifact.sha256)
                    || onnx.inputs.iter().chain(&onnx.outputs).any(|tensor| {
                        tensor.name.trim().is_empty()
                            || tensor.dtype.trim().is_empty()
                            || tensor.shape.is_empty()
                    })
                {
                    return Err(configuration(format!(
                        "ONNX model build {id} needs complete artifact, tensor, preprocessing, provider, and precision identity"
                    )));
                }
                let valid_preprocessing = match onnx.adapter {
                    OnnxAdapterKind::YunetFaceDetection
                    | OnnxAdapterKind::SfaceEmbedding
                    | OnnxAdapterKind::SiglipImageEmbedding => {
                        onnx.preprocessing
                            .as_ref()
                            .is_some_and(|preprocess| !preprocess.identity.trim().is_empty())
                            && onnx.text_preprocessing.is_none()
                    }
                    OnnxAdapterKind::SiglipTextEmbedding => {
                        let Some(text) = onnx.text_preprocessing.as_ref() else {
                            return Err(configuration(format!(
                                "ONNX text model build {id} needs a tokenizer preprocessing contract"
                            )));
                        };
                        onnx.preprocessing.is_none()
                            && !text.identity.trim().is_empty()
                            && !text.tokenizer_artifact.trim().is_empty()
                            && text.max_length > 0
                            && text.padding == "max_length"
                            && text.truncation
                            && onnx
                                .auxiliary_artifacts
                                .contains_key(&text.tokenizer_artifact)
                    }
                };
                let valid_embedding_space = match onnx.adapter {
                    OnnxAdapterKind::SiglipImageEmbedding
                    | OnnxAdapterKind::SiglipTextEmbedding => {
                        onnx.embedding_space.as_ref().is_some_and(|space| {
                            !space.identity.trim().is_empty()
                                && space.dimensions > 0
                                && space.normalized
                                && space.distance_metric == "cosine"
                        })
                    }
                    OnnxAdapterKind::SfaceEmbedding => {
                        onnx.embedding_space.as_ref().is_none_or(|space| {
                            !space.identity.trim().is_empty()
                                && space.dimensions == 128
                                && space.normalized
                                && space.distance_metric == "cosine"
                        })
                    }
                    OnnxAdapterKind::YunetFaceDetection => onnx.embedding_space.is_none(),
                };
                let valid_auxiliary_artifacts =
                    onnx.auxiliary_artifacts.iter().all(|(name, artifact)| {
                        !name.trim().is_empty()
                            && artifact.size_bytes > 0
                            && artifact.sha256.len() == 64
                            && artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && !artifact.source_url.trim().is_empty()
                            && !artifact.source_revision.trim().is_empty()
                            && !artifact.license_spdx.trim().is_empty()
                    });
                if !valid_preprocessing || !valid_embedding_space || !valid_auxiliary_artifacts {
                    return Err(configuration(format!(
                        "ONNX model build {id} has an incomplete adapter preprocessing, embedding-space, or auxiliary-artifact contract"
                    )));
                }
                let tensor_names: BTreeSet<_> = onnx
                    .inputs
                    .iter()
                    .map(|tensor| ("input", tensor.name.as_str()))
                    .chain(
                        onnx.outputs
                            .iter()
                            .map(|tensor| ("output", tensor.name.as_str())),
                    )
                    .collect();
                if tensor_names.len() != onnx.inputs.len() + onnx.outputs.len() {
                    return Err(configuration(format!(
                        "ONNX model build {id} has duplicate tensor names"
                    )));
                }
            }
        }
        let mut embedding_spaces = BTreeMap::new();
        for (id, onnx) in self
            .model_builds
            .iter()
            .filter_map(|(id, build)| build.onnx.as_ref().map(|onnx| (id, onnx)))
        {
            let Some(space) = onnx.embedding_space.as_ref() else {
                continue;
            };
            if let Some((existing_build, existing)) =
                embedding_spaces.insert(space.identity.as_str(), (id.as_str(), space))
                && existing != space
            {
                return Err(configuration(format!(
                    "ONNX builds {existing_build} and {id} claim embedding space {} with different contracts",
                    space.identity
                )));
            }
        }
        Ok(())
    }

    fn validate_deployments(&self) -> Result<(), ContractError> {
        for (id, deployment) in &self.deployments {
            let provider = self.providers.get(&deployment.provider).ok_or_else(|| {
                configuration(format!(
                    "deployment {id} names unknown provider {}",
                    deployment.provider
                ))
            })?;
            let build = self.model_builds.get(&deployment.build).ok_or_else(|| {
                configuration(format!(
                    "deployment {id} names unknown build {}",
                    deployment.build
                ))
            })?;
            let model = &self.model_profiles[&build.profile];
            for intent_id in model.ratings.keys() {
                let data_plane = self.intents[intent_id].data_plane.as_str();
                let compatible = provider_serves_data_plane(provider, data_plane);
                if !compatible {
                    return Err(configuration(format!(
                        "deployment {id} provider kind {} cannot serve {data_plane}",
                        provider.kind
                    )));
                }
            }
            if provider.kind == ProviderKind::Onnx && build.onnx.is_none() {
                return Err(configuration(format!(
                    "ONNX deployment {id} requires model_builds.{}.onnx",
                    deployment.build
                )));
            }
            if provider.kind != ProviderKind::Onnx && build.onnx.is_some() {
                return Err(configuration(format!(
                    "ONNX model build {} must be deployed by an ONNX provider",
                    deployment.build
                )));
            }
            let worker_provider = matches!(
                provider.kind,
                ProviderKind::RetrievalWorker | ProviderKind::OcrWorker
            ) || (provider.kind == ProviderKind::AudioWorker
                && build.local_worker.is_some());
            if worker_provider && build.local_worker.is_none() {
                return Err(configuration(format!(
                    "local worker deployment {id} requires model_builds.{}.local_worker",
                    deployment.build
                )));
            }
            if !worker_provider && build.local_worker.is_some() {
                return Err(configuration(format!(
                    "local worker model build {} must be deployed by a typed local worker provider",
                    deployment.build
                )));
            }
            if let Some(worker) = &build.local_worker {
                let adapter_matches = matches!(
                    (provider.kind, worker.adapter),
                    (
                        ProviderKind::RetrievalWorker,
                        LocalWorkerAdapterKind::Qwen3Embedding
                            | LocalWorkerAdapterKind::Qwen3Reranker
                    ) | (ProviderKind::OcrWorker, LocalWorkerAdapterKind::PpOcrv6)
                        | (
                            ProviderKind::AudioWorker,
                            LocalWorkerAdapterKind::YamnetAudioEvents
                        )
                );
                if !adapter_matches {
                    return Err(configuration(format!(
                        "local worker model build {} adapter is incompatible with provider {id}",
                        deployment.build
                    )));
                }
            }
            let serves_audio_events = model
                .ratings
                .keys()
                .any(|intent_id| self.intents[intent_id].data_plane == "audio.event_detection");
            if serves_audio_events != build.audio_event.is_some() {
                return Err(configuration(format!(
                    "audio-event deployment {id} must pair its Intent with one exact Build policy identity"
                )));
            }
            if !deployment.estimated_cost_usd.is_finite() || deployment.estimated_cost_usd < 0.0 {
                return Err(configuration(format!(
                    "deployment {id} has invalid estimated cost"
                )));
            }
            if deployment.supported_execution_modes.is_empty() {
                return Err(configuration(format!(
                    "deployment {id} must support at least one execution mode"
                )));
            }
        }
        Ok(())
    }

    fn validate_apps(&self) -> Result<(), ContractError> {
        for (id, app) in &self.apps {
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || app.max_pending_jobs == 0
            {
                return Err(configuration(format!(
                    "app {id} needs a path-safe id and positive max_pending_jobs"
                )));
            }
            if let AppCredentialConfig::Environment { variable } = &app.credential
                && variable.trim().is_empty()
            {
                return Err(configuration(format!(
                    "app {id} environment credential needs variable"
                )));
            }
            if app.observer_access == ObserverAccess::Summary
                && (app.resource_admin
                    || app.allow_all_intents
                    || app.allow_all_speech_voice_aliases
                    || !app.allowed_builtin_tools.is_empty()
                    || app
                        .allowed_intents
                        .as_ref()
                        .is_none_or(|intents| !intents.is_empty()))
            {
                return Err(configuration(format!(
                    "observer App {id} must set resource_admin = false, allowed_intents = [], and allowed_builtin_tools = []"
                )));
            }
            if app.allow_all_intents && !app.resource_admin {
                return Err(configuration(format!(
                    "app {id} allow_all_intents requires resource_admin = true"
                )));
            }
            if app.resource_admin && !app.allow_all_intents && app.allowed_intents.is_none() {
                return Err(configuration(format!(
                    "resource-admin app {id} must explicitly set allow_all_intents or allowed_intents"
                )));
            }
            if app.allow_all_intents && app.allowed_intents.is_some() {
                return Err(configuration(format!(
                    "app {id} must choose allow_all_intents or allowed_intents, not both"
                )));
            }
            if app.allow_all_speech_voice_aliases && !app.resource_admin {
                return Err(configuration(format!(
                    "app {id} allow_all_speech_voice_aliases requires resource_admin = true"
                )));
            }
            if app.allow_all_speech_voice_aliases && app.allowed_speech_voice_aliases.is_some() {
                return Err(configuration(format!(
                    "app {id} must choose allow_all_speech_voice_aliases or allowed_speech_voice_aliases, not both"
                )));
            }
            if let Some(profile) = &app.default_policy {
                self.require_profile(profile)?;
            }
            for profile in &app.allowed_policies {
                self.require_profile(profile)?;
            }
            if let Some(intents) = &app.allowed_intents {
                let mut unique = BTreeSet::new();
                for intent in intents {
                    if !self.intents.contains_key(intent) {
                        return Err(configuration(format!(
                            "app {id} allowed_intents names unknown intent {intent}"
                        )));
                    }
                    if !unique.insert(intent) {
                        return Err(configuration(format!(
                            "app {id} allowed_intents contains duplicate intent {intent}"
                        )));
                    }
                }
            }
            if let Some(routing) = &app.routing {
                routing.validate(id, self)?;
                if let Some(allowed_intents) = &app.allowed_intents {
                    for intent in routing.intents.keys() {
                        if !allowed_intents.contains(intent) {
                            return Err(configuration(format!(
                                "app {id} routing rule for {intent} exceeds allowed_intents"
                            )));
                        }
                    }
                }
            }
            if let Some(aliases) = &app.allowed_speech_voice_aliases {
                let mut unique = BTreeSet::new();
                for alias in aliases {
                    if alias != crate::audio::SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1 {
                        return Err(configuration(format!(
                            "app {id} allowed_speech_voice_aliases names unknown Runtime voice alias {alias}"
                        )));
                    }
                    if !unique.insert(alias) {
                        return Err(configuration(format!(
                            "app {id} allowed_speech_voice_aliases contains duplicate alias {alias}"
                        )));
                    }
                }
            }
            if let Some(range) = &app.request_overrides.max_cost_usd
                && (!range.min.is_finite()
                    || !range.max.is_finite()
                    || range.min < 0.0
                    || range.min > range.max)
            {
                return Err(configuration(format!(
                    "app {id} has invalid max_cost_usd range"
                )));
            }
        }
        Ok(())
    }

    fn validate_quota(&self) -> Result<(), ContractError> {
        validate_quota_limit("quota.global", &self.quota.global)?;
        for (id, limit) in &self.quota.providers {
            if !self.providers.contains_key(id) {
                return Err(configuration(format!(
                    "quota.providers names unknown provider {id}"
                )));
            }
            validate_quota_limit(&format!("quota.providers.{id}"), limit)?;
        }
        for (id, limit) in &self.quota.apps {
            if !self.apps.contains_key(id) {
                return Err(configuration(format!("quota.apps names unknown app {id}")));
            }
            validate_quota_limit(&format!("quota.apps.{id}"), limit)?;
        }
        Ok(())
    }

    fn validate_resources(&self) -> Result<(), ContractError> {
        let eviction = &self.resources.eviction;
        let pressure = self.resources.pressure;
        if !(5_000..=3_600_000).contains(&pressure.refresh_interval_ms) {
            return Err(configuration(
                "resources.pressure.refresh_interval_ms must be 5000..=3600000",
            ));
        }
        if pressure.elevated_at_or_below_free_memory_percent > 99
            || pressure.critical_at_or_below_free_memory_percent > 99
        {
            return Err(configuration(
                "resources.pressure thresholds must be 0..=99",
            ));
        }
        if pressure.critical_at_or_below_free_memory_percent
            > pressure.elevated_at_or_below_free_memory_percent
        {
            return Err(configuration(
                "resources.pressure critical threshold must not exceed elevated threshold",
            ));
        }
        let capacity = &self.resources.admission_capacity;
        for (name, value) in [
            ("cpu_slots", capacity.cpu_slots),
            ("unified_memory_mib", capacity.unified_memory_mib),
            ("accelerator_slots", capacity.accelerator_slots),
        ] {
            if value == Some(0) {
                return Err(configuration(format!(
                    "resources.admission_capacity.{name} must be positive when configured"
                )));
            }
        }
        if (capacity.cpu_slots.is_some()
            || capacity.unified_memory_mib.is_some()
            || capacity.accelerator_slots.is_some())
            && !(1..=4_096).contains(&capacity.max_waiting_jobs)
        {
            return Err(configuration(
                "resources.admission_capacity.max_waiting_jobs must be 1..=4096 when capacity is enabled",
            ));
        }
        if (capacity.cpu_slots.is_some()
            || capacity.unified_memory_mib.is_some()
            || capacity.accelerator_slots.is_some())
            && !(1_000..=3_600_000).contains(&capacity.priority_aging_ms)
        {
            return Err(configuration(
                "resources.admission_capacity.priority_aging_ms must be 1000..=3600000 when capacity is enabled",
            ));
        }
        for (deployment_id, deployment) in &self.deployments {
            let estimate = &deployment.resource_estimate;
            let provider = &self.providers[&deployment.provider];
            if provider.placement != Placement::Local
                && *estimate != DeploymentResourceEstimateConfig::default()
            {
                return Err(configuration(format!(
                    "deployments.{deployment_id}.resource_estimate is only valid for local placement"
                )));
            }
            for (name, claim, limit) in [
                ("cpu_slots", estimate.cpu_slots, capacity.cpu_slots),
                (
                    "unified_memory_mib",
                    estimate.unified_memory_mib,
                    capacity.unified_memory_mib,
                ),
                (
                    "accelerator_slots",
                    estimate.accelerator_slots,
                    capacity.accelerator_slots,
                ),
            ] {
                if limit.is_some_and(|limit| claim > limit) {
                    return Err(configuration(format!(
                        "deployments.{deployment_id}.resource_estimate.{name} exceeds resources.admission_capacity.{name}"
                    )));
                }
            }
        }
        if eviction.max_benchmark_age_ms == 0 {
            return Err(configuration(
                "resources.eviction.max_benchmark_age_ms must be positive",
            ));
        }
        if !(1_000..=3_600_000).contains(&eviction.monitor.poll_interval_ms) {
            return Err(configuration(
                "resources.eviction.monitor.poll_interval_ms must be 1000..=3600000",
            ));
        }
        if !(60_000..=86_400_000).contains(&eviction.monitor.max_lease_ms) {
            return Err(configuration(
                "resources.eviction.monitor.max_lease_ms must be 60000..=86400000",
            ));
        }
        if eviction.monitor.enabled && eviction.mode != EvictionMode::Recommend {
            return Err(configuration(
                "resources.eviction.monitor.enabled requires eviction mode recommend",
            ));
        }
        for (level, target) in [
            ("elevated", &eviction.elevated),
            ("critical", &eviction.critical),
        ] {
            if target
                .target_free_memory_percent
                .is_some_and(|percent| !(1..=100).contains(&percent))
            {
                return Err(configuration(format!(
                    "resources.eviction.{level}.target_free_memory_percent must be 1..=100"
                )));
            }
        }
        for (level, target, trigger) in [
            (
                "elevated",
                eviction.elevated.target_free_memory_percent,
                pressure.elevated_at_or_below_free_memory_percent,
            ),
            (
                "critical",
                eviction.critical.target_free_memory_percent,
                pressure.critical_at_or_below_free_memory_percent,
            ),
        ] {
            if target.is_some_and(|percent| percent <= trigger) {
                return Err(configuration(format!(
                    "resources.eviction.{level} target must exceed its pressure trigger"
                )));
            }
        }
        if let (Some(elevated), Some(critical)) = (
            eviction.elevated.target_free_memory_percent,
            eviction.critical.target_free_memory_percent,
        ) && critical < elevated
        {
            return Err(configuration(
                "resources.eviction.critical target must be at least the elevated target",
            ));
        }
        for deployment_id in eviction.deployments.keys() {
            let deployment = self.deployments.get(deployment_id).ok_or_else(|| {
                configuration(format!(
                    "resources.eviction.deployments names unknown deployment {deployment_id}"
                ))
            })?;
            let provider = &self.providers[&deployment.provider];
            if provider.local_inventory.is_none() {
                return Err(configuration(format!(
                    "resources.eviction.deployments.{deployment_id} needs a native local inventory"
                )));
            }
        }
        for (deployment_id, benchmark) in &self.resources.reload_benchmarks {
            let deployment = self.deployments.get(deployment_id).ok_or_else(|| {
                configuration(format!(
                    "resources.reload_benchmarks names unknown deployment {deployment_id}"
                ))
            })?;
            let provider = &self.providers[&deployment.provider];
            if provider.local_inventory.is_none() {
                return Err(configuration(format!(
                    "resources.reload_benchmarks.{deployment_id} needs a native local inventory"
                )));
            }
            if benchmark.reload_cost_ms == 0
                || benchmark.observed_at_unix_ms == 0
                || benchmark.evidence.trim().is_empty()
            {
                return Err(configuration(format!(
                    "resources.reload_benchmarks.{deployment_id} needs positive cost/time and evidence"
                )));
            }
        }
        Ok(())
    }

    fn require_profile(&self, profile: &str) -> Result<(), ContractError> {
        if self.profiles.contains_key(profile) {
            Ok(())
        } else {
            Err(configuration(format!("unknown policy profile {profile}")))
        }
    }

    pub fn intent(&self, name: &str) -> Option<&IntentProfile> {
        self.intents.get(name)
    }
}

impl ProviderConfig {
    /// A missing optional credential is fine; a required one makes the
    /// provider unavailable to routing rather than a late upstream 401.
    pub fn is_configured(&self) -> bool {
        !self.requires_api_key
            || self
                .api_key_env
                .as_ref()
                .is_some_and(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    }
}

fn provider_serves_data_plane(provider: &ProviderConfig, data_plane: &str) -> bool {
    match provider.kind {
        ProviderKind::Responses => {
            data_plane == "responses"
                || (matches!(
                    data_plane,
                    "vision.image_description" | "vision.classification_review"
                ) && provider
                    .local_inventory
                    .as_ref()
                    .is_some_and(|inventory| inventory.kind == LocalInventoryKind::OllamaTags))
        }
        ProviderKind::CodexAppServer => data_plane == "responses",
        ProviderKind::AudioWorker => data_plane.starts_with("audio."),
        ProviderKind::RetrievalWorker => matches!(data_plane, "text.embedding" | "text.rerank"),
        ProviderKind::OcrWorker => data_plane == "document.ocr",
        ProviderKind::Onnx => data_plane.starts_with("vision."),
        // Raw foundation execution is a typed ONNX graph contract with a
        // dedicated controller and API, not a generic tensor surface.
        ProviderKind::RawFoundation => data_plane == "raw.foundation",
    }
}

fn validate_build_supply_chain(id: &str, build: &ModelBuildConfig) -> Result<(), ContractError> {
    let provenance = &build.provenance;
    for (field, value) in [
        ("provenance.upstream", provenance.upstream.as_deref()),
        (
            "provenance.source_revision",
            provenance.source_revision.as_deref(),
        ),
        ("license.expression", build.license.expression.as_deref()),
        ("license.license_url", build.license.license_url.as_deref()),
        ("license.reviewed_at", build.license.reviewed_at.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(configuration(format!(
                "model build {id} has an empty {field}"
            )));
        }
    }
    for (field, digest) in [
        (
            "provenance.artifact_sha256",
            provenance.artifact_sha256.as_deref(),
        ),
        (
            "license.license_text_sha256",
            build.license.license_text_sha256.as_deref(),
        ),
    ] {
        if digest.is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(configuration(format!(
                "model build {id} has an invalid {field}"
            )));
        }
    }
    if let Some(worker) = &build.local_worker {
        if worker.runtime.trim().is_empty()
            || worker.precision.trim().is_empty()
            || worker.postprocessing_identity.trim().is_empty()
            || worker.artifact_set_sha256.len() != 64
            || !worker
                .artifact_set_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(configuration(format!(
                "local worker model build {id} needs artifacts and complete execution identity"
            )));
        }
        match worker.adapter {
            LocalWorkerAdapterKind::Qwen3Embedding => {
                if worker.embedding_space.is_none()
                    || worker
                        .tokenizer_identity
                        .as_deref()
                        .is_none_or(str::is_empty)
                    || worker
                        .instruction_revision
                        .as_deref()
                        .is_none_or(str::is_empty)
                {
                    return Err(configuration(format!(
                        "Qwen3 embedding build {id} needs tokenizer, instruction and embedding-space identity"
                    )));
                }
            }
            LocalWorkerAdapterKind::Qwen3Reranker => {
                if worker
                    .tokenizer_identity
                    .as_deref()
                    .is_none_or(str::is_empty)
                    || worker
                        .instruction_revision
                        .as_deref()
                        .is_none_or(str::is_empty)
                {
                    return Err(configuration(format!(
                        "Qwen3 reranker build {id} needs tokenizer and instruction identity"
                    )));
                }
            }
            LocalWorkerAdapterKind::PpOcrv6 => {
                if worker
                    .preprocessing_identity
                    .as_deref()
                    .is_none_or(str::is_empty)
                    || worker
                        .requested_execution_provider
                        .as_deref()
                        .is_none_or(str::is_empty)
                    || worker
                        .actual_execution_provider
                        .as_deref()
                        .is_none_or(str::is_empty)
                {
                    return Err(configuration(format!(
                        "PP-OCRv6 build {id} needs preprocessing and execution-provider identity"
                    )));
                }
            }
            LocalWorkerAdapterKind::YamnetAudioEvents => {
                let Some(audio_event) = &build.audio_event else {
                    return Err(configuration(format!(
                        "YAMNet audio-event build {id} needs audio_event identity"
                    )));
                };
                if worker.preprocessing_identity.as_deref()
                    != Some(audio_event.preprocessing_identity.as_str())
                    || worker.postprocessing_identity != audio_event.policy_revision
                {
                    return Err(configuration(format!(
                        "YAMNet audio-event build {id} worker identity does not match its policy"
                    )));
                }
            }
        }
    }
    if matches!(
        build.license.status,
        ModelLicenseStatus::Declared
            | ModelLicenseStatus::Verified
            | ModelLicenseStatus::Restricted
    ) && build
        .license
        .expression
        .as_deref()
        .is_none_or(str::is_empty)
    {
        return Err(configuration(format!(
            "model build {id} with a reviewed license status needs license.expression"
        )));
    }
    if matches!(
        provenance.source_kind,
        ModelSourceKind::RuntimeDownloadable | ModelSourceKind::RuntimeBundled
    ) && (provenance.upstream.as_deref().is_none_or(str::is_empty)
        || provenance
            .source_revision
            .as_deref()
            .is_none_or(str::is_empty)
        || provenance.artifact_sha256.is_none()
        || build.license.status != ModelLicenseStatus::Verified
        || build.license.expression.is_none()
        || build.license.license_url.is_none()
        || build.license.license_text_sha256.is_none())
    {
        return Err(configuration(format!(
            "runtime-managed model build {id} needs immutable provenance and a verified license receipt"
        )));
    }
    Ok(())
}

fn configuration(message: impl Into<String>) -> ContractError {
    ContractError::Configuration(message.into())
}

fn validate_quota_limit(name: &str, limit: &QuotaLimitConfig) -> Result<(), ContractError> {
    if limit
        .max_usd
        .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err(configuration(format!(
            "{name}.max_usd must be finite and non-negative"
        )));
    }
    if limit.requests_per_minute == Some(0) {
        return Err(configuration(format!(
            "{name}.requests_per_minute must be positive"
        )));
    }
    if limit.tokens_per_minute == Some(0) {
        return Err(configuration(format!(
            "{name}.tokens_per_minute must be positive"
        )));
    }
    if limit.max_concurrent_attempts == Some(0) {
        return Err(configuration(format!(
            "{name}.max_concurrent_attempts must be positive"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use super::{
        BuiltinTool, ModelLicenseStatus, ModelSourceKind, ObserverAccess, ProviderCapability,
        ProviderProtocol, QuotaLimitConfig, RuntimeConfig, provider_serves_data_plane,
    };

    #[test]
    fn checked_in_example_registry_validates() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        RuntimeConfig::load(path).expect("example registry must remain valid");
    }

    #[test]
    fn audio_event_registry_fails_closed_on_policy_and_ontology_drift() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(&path).unwrap();
        config
            .model_builds
            .get_mut("yamnet_tfhub_v1_tensorflow_2_20")
            .unwrap()
            .audio_event
            .as_mut()
            .unwrap()
            .ontology_artifact_sha256 = "not-a-digest".into();
        assert!(config.validate().is_err());

        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .model_builds
            .get_mut("yamnet_tfhub_v1_tensorflow_2_20")
            .unwrap()
            .audio_event
            .as_mut()
            .unwrap()
            .smoothing_method = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn example_builds_expose_supply_chain_ownership_without_guessing_licenses() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        assert!(config.model_builds.values().all(|build| {
            build.provenance.upstream.is_some()
                && matches!(
                    build.provenance.source_kind,
                    ModelSourceKind::UserManaged
                        | ModelSourceKind::ProviderManaged
                        | ModelSourceKind::RuntimeDownloadable
                        | ModelSourceKind::RuntimeBundled
                )
        }));
        assert!(config.model_builds.values().all(|build| {
            !matches!(
                build.provenance.source_kind,
                ModelSourceKind::RuntimeDownloadable | ModelSourceKind::RuntimeBundled
            ) || (build.provenance.source_revision.is_some()
                && build.provenance.artifact_sha256.is_some()
                && build.license.status == ModelLicenseStatus::Verified
                && build.license.license_text_sha256.is_some())
        }));
        assert_eq!(
            config.model_builds["qwen3_5_2b_mlx"].license.status,
            ModelLicenseStatus::Unreviewed
        );
        assert_eq!(
            config.model_builds["yunet_2026may_onnx"]
                .provenance
                .artifact_sha256
                .as_deref(),
            Some("ebafce4e3c118d6554634be5c27ab333b4c047a9a8c3faf1d7cf93101c22f0f0")
        );
    }

    #[test]
    fn runtime_managed_builds_require_immutable_verified_license_receipts() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let build = config.model_builds.get_mut("qwen3_5_2b_mlx").unwrap();
        build.provenance.source_kind = ModelSourceKind::RuntimeDownloadable;
        assert!(config.validate().is_err());

        let build = config.model_builds.get_mut("qwen3_5_2b_mlx").unwrap();
        build.provenance.source_revision = Some("immutable-revision".into());
        build.provenance.artifact_sha256 = Some("a".repeat(64));
        build.license.status = ModelLicenseStatus::Verified;
        build.license.expression = Some("Apache-2.0".into());
        build.license.license_url = Some("https://example.invalid/license".into());
        build.license.license_text_sha256 = Some("b".repeat(64));
        config.validate().unwrap();
    }

    #[test]
    fn image_generation_is_codex_only_and_routed_as_text_to_image() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let intent = &config.intents[crate::IMAGE_GENERATION_INTENT];
        assert_eq!(intent.input_modalities, vec![super::Modality::Text]);
        assert_eq!(intent.output_modalities, vec![super::Modality::Image]);
        assert!(
            config.providers["codex-subscription"]
                .capability_profile
                .supports(ProviderCapability::ImageGeneration)
        );
        assert!(
            config.model_profiles["codex_gpt_5_6_luna"]
                .ratings
                .contains_key(crate::IMAGE_GENERATION_INTENT)
        );

        config
            .providers
            .get_mut("codex-subscription")
            .unwrap()
            .capability_profile
            .protocol = ProviderProtocol::Responses;
        assert!(config.validate().is_err());
    }

    #[test]
    fn typed_qwen_vision_requires_an_explicit_local_ollama_native_adapter() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let mut cloud = config.providers["ollama-local"].clone();
        cloud.placement = super::Placement::Cloud;
        cloud.local_inventory = None;
        cloud.base_url = Some("https://example.invalid/v1".into());
        config.providers.insert("fake-cloud-vlm".into(), cloud);
        config
            .deployments
            .get_mut("ollama_qwen3_vl_4b")
            .unwrap()
            .provider = "fake-cloud-vlm".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn configuration_rejects_unknown_keys_at_root_and_nested_levels() {
        let source = include_str!("../../../config/infer.example.toml");
        let root_error =
            toml::from_str::<RuntimeConfig>(&format!("unknown_root = true\n{source}")).unwrap_err();
        assert!(root_error.to_string().contains("unknown_root"));

        let nested = source.replacen("[server]\n", "[server]\nunknown_server_option = true\n", 1);
        let nested_error = toml::from_str::<RuntimeConfig>(&nested).unwrap_err();
        assert!(nested_error.to_string().contains("unknown_server_option"));
    }

    #[test]
    fn authentication_config_requires_safe_app_ids_and_credential_sources() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.auth.managed_credentials_directory.clear();
        assert!(config.validate().is_err());

        config.auth.managed_credentials_directory = ".infer-runtime/credentials".into();
        let app = config.apps.remove("local-operator").unwrap();
        config.apps.insert("../operator".into(), app.clone());
        assert!(config.validate().is_err());

        config.apps.clear();
        let mut external = app;
        external.credential = super::AppCredentialConfig::Environment {
            variable: String::new(),
        };
        config.apps.insert("external-app".into(), external);
        assert!(config.validate().is_err());
    }

    #[test]
    fn app_intent_acl_omission_denies_and_operator_all_is_explicit() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let app = config.apps.get_mut("example-local-consumer").unwrap();
        app.allowed_intents = None;
        assert!(!app.allows_intent("reasoning.solve"));

        app.allow_all_intents = true;
        assert!(config.validate().is_err());
        let app = config.apps.get_mut("example-local-consumer").unwrap();
        app.allow_all_intents = false;

        app.allowed_intents = Some(vec!["text.summarize".into()]);
        assert!(app.allows_intent("text.summarize"));
        assert!(!app.allows_intent("reasoning.solve"));
        config.validate().unwrap();

        config
            .apps
            .get_mut("example-local-consumer")
            .unwrap()
            .allowed_intents = Some(vec!["missing.intent".into()]);
        assert!(config.validate().is_err());

        config
            .apps
            .get_mut("example-local-consumer")
            .unwrap()
            .allowed_intents = Some(vec!["text.summarize".into(), "text.summarize".into()]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn resource_admin_cannot_have_an_ambiguous_intent_grant() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let operator = config.apps.get_mut("local-operator").unwrap();
        operator.allow_all_intents = false;
        assert!(config.validate().is_err());
        config
            .apps
            .get_mut("local-operator")
            .unwrap()
            .allowed_intents = Some(Vec::new());
        config.validate().unwrap();
    }

    #[test]
    fn app_routing_defaults_and_intent_rules_cross_validate_registry_identity() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let app = config.apps.get_mut("example-local-consumer").unwrap();
        app.routing = Some(crate::AppRoutingConfig {
            deployment_ids: BTreeSet::from(["ollama_qwen3_5_2b".into()]),
            model_profile_ids: BTreeSet::new(),
            intents: BTreeMap::from([
                (
                    "text.summarize".into(),
                    crate::RoutingGrantConfig {
                        deployment_ids: BTreeSet::from(["ollama_qwen3_5_4b".into()]),
                        model_profile_ids: BTreeSet::new(),
                    },
                ),
                ("audio.align".into(), crate::RoutingGrantConfig::default()),
            ]),
        });
        config.validate().unwrap();
        let routing = config.apps["example-local-consumer"]
            .routing
            .as_ref()
            .unwrap();
        assert!(
            routing
                .grant_for("text.proofread")
                .deployment_ids
                .contains("ollama_qwen3_5_2b")
        );
        assert!(
            routing
                .grant_for("text.summarize")
                .deployment_ids
                .contains("ollama_qwen3_5_4b")
        );
        assert!(routing.grant_for("audio.align").deployment_ids.is_empty());

        config
            .apps
            .get_mut("example-local-consumer")
            .unwrap()
            .routing
            .as_mut()
            .unwrap()
            .deployment_ids
            .insert("missing".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn tracked_shape_fixture_freezes_the_minimum_text_edit_and_tts_grants() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let intent = &config.intents["text.edit"];
        assert_eq!(intent.input_modalities, vec![crate::Modality::Text]);
        assert_eq!(intent.output_modalities, vec![crate::Modality::Text]);
        assert_eq!(
            config.model_profiles["qwen3_5_4b"].ratings["text.edit"].level,
            crate::CapabilityLevel::Foundational
        );
        assert_eq!(
            config.model_profiles["qwen3_5_4b"].ratings["text.edit"].status,
            crate::EvaluationStatus::Provisional
        );

        let app = &config.apps["example-shape-consumer"];
        assert_eq!(
            app.allowed_intents.as_deref(),
            Some(&["text.edit".into(), "speech.synthesize".into()][..])
        );
        assert_eq!(
            app.allowed_provider_access_classes,
            BTreeSet::from([crate::ProviderAccessClass::Standard])
        );
        assert!(app.allowed_cloud_input_modalities.is_empty());
        assert!(app.allowed_builtin_tools.is_empty());
        assert_eq!(
            app.request_overrides.capability_floor,
            vec![
                crate::CapabilityLevel::Foundational,
                crate::CapabilityLevel::Capable
            ]
        );
        assert_eq!(app.request_overrides.fallback, vec![crate::Fallback::None]);
        assert_eq!(
            app.routing.as_ref().unwrap().global_grant(),
            crate::RoutingGrantConfig::default()
        );

        let edit = app.routing.as_ref().unwrap().grant_for("text.edit");
        assert_eq!(
            edit.deployment_ids,
            BTreeSet::from(["ollama_qwen3_5_4b".into()])
        );
        assert!(edit.model_profile_ids.is_empty());
        let speech = app.routing.as_ref().unwrap().grant_for("speech.synthesize");
        assert_eq!(
            speech.deployment_ids,
            BTreeSet::from(["mlx_qwen3_tts_custom_voice_1_7b".into()])
        );
        assert!(speech.model_profile_ids.is_empty());
    }

    #[test]
    fn routing_config_rejects_unknown_nested_fields() {
        let source = include_str!("../../../config/infer.example.toml");
        let invalid = source.replacen(
            "[apps.example-shape-consumer.routing]\n",
            "[apps.example-shape-consumer.routing]\nphysical_models = [\"forbidden\"]\n",
            1,
        );
        assert!(toml::from_str::<RuntimeConfig>(&invalid).is_err());
    }

    #[test]
    fn intent_routing_rule_cannot_exceed_the_app_intent_ceiling() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .apps
            .get_mut("example-shape-consumer")
            .unwrap()
            .routing
            .as_mut()
            .unwrap()
            .intents
            .insert(
                "text.summarize".into(),
                crate::RoutingGrantConfig::default(),
            );
        assert!(config.validate().is_err());
    }

    #[test]
    fn app_builtin_tool_acl_is_explicit_and_defaults_to_deny() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let app = config.apps.get_mut("example-local-consumer").unwrap();
        assert!(!app.allows_builtin_tool(BuiltinTool::WebSearch));
        app.allowed_builtin_tools.insert(BuiltinTool::WebSearch);
        assert!(app.allows_builtin_tool(BuiltinTool::WebSearch));
        config.validate().unwrap();
    }

    #[test]
    fn app_speech_voice_allowlist_omission_denies_and_aliases_are_explicit() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let app = config.apps.get_mut("example-local-consumer").unwrap();
        app.allowed_speech_voice_aliases = None;
        assert!(!app.allows_speech_voice("legacy-provider-speaker"));

        app.allowed_speech_voice_aliases =
            Some(vec![crate::audio::SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1.into()]);
        assert!(app.allows_speech_voice(crate::audio::SPEECH_VOICE_ZH_BRIGHT_FEMALE_V1));
        assert!(!app.allows_speech_voice("Vivian"));
        config.validate().unwrap();

        config
            .apps
            .get_mut("example-local-consumer")
            .unwrap()
            .allowed_speech_voice_aliases = Some(vec!["unknown.voice.v1".into()]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn observer_identity_is_explicit_deny_all_and_loopback_only() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let observer = config.apps.get("infra-sentinel").unwrap();
        assert_eq!(observer.observer_access, ObserverAccess::Summary);
        assert_eq!(observer.allowed_intents, Some(Vec::new()));
        assert!(!observer.resource_admin);

        config
            .apps
            .get_mut("infra-sentinel")
            .unwrap()
            .resource_admin = true;
        assert!(config.validate().is_err());
        config
            .apps
            .get_mut("infra-sentinel")
            .unwrap()
            .resource_admin = false;
        config
            .apps
            .get_mut("infra-sentinel")
            .unwrap()
            .allowed_intents = None;
        assert!(config.validate().is_err());
        config
            .apps
            .get_mut("infra-sentinel")
            .unwrap()
            .allowed_intents = Some(Vec::new());
        config.server.bind = "0.0.0.0:8787".into();
        assert!(config.validate().is_err());
        config.server.bind = "127.0.0.1:8787".into();
        config.observer.enabled = false;
        config.observer.console_url = Some("https://example.com/console".into());
        assert!(config.validate().is_err());

        let source = include_str!("../../../config/infer.example.toml");
        for retired in [
            "display_name = \"Infer Runtime\"",
            "credential_id = \"infra-sentinel\"",
        ] {
            let with_retired_field = source.replacen(
                "instance_id = \"local\"",
                &format!("instance_id = \"local\"\n{retired}"),
                1,
            );
            assert!(toml::from_str::<RuntimeConfig>(&with_retired_field).is_err());
        }
    }

    #[test]
    fn quota_limits_require_known_scopes_and_non_negative_usd() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.quota.apps.insert(
            "unknown".into(),
            QuotaLimitConfig {
                max_usd: Some(1.0),
                ..QuotaLimitConfig::default()
            },
        );
        assert!(config.validate().is_err());
        config.quota.apps.clear();
        config.quota.global.max_usd = Some(-1.0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn intent_default_output_budget_must_be_positive() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .intents
            .get_mut("text.summarize")
            .unwrap()
            .default_max_output_tokens = Some(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn durable_background_requires_key_reference_and_bounded_retention() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.background.enabled = true;
        config.background.key_env = None;
        assert!(config.validate().is_err());

        config.background.key_env = Some("INFER_BACKGROUND_KEY".into());
        config.background.result_retention_ms = 59_999;
        assert!(config.validate().is_err());
        config.background.result_retention_ms = 60_000;
        config.background.max_payload_bytes = 25 * 1024 * 1024 + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn reload_benchmarks_must_name_local_deployments_and_have_evidence() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.resources.reload_benchmarks.insert(
            "unknown".into(),
            super::ReloadBenchmarkConfig {
                reload_cost_ms: 1,
                observed_at_unix_ms: 1,
                evidence: "test".into(),
            },
        );
        assert!(config.validate().is_err());
        config.resources.reload_benchmarks.clear();
        config.resources.reload_benchmarks.insert(
            "ollama_qwen3_5_2b".into(),
            super::ReloadBenchmarkConfig {
                reload_cost_ms: 0,
                observed_at_unix_ms: 1,
                evidence: "test".into(),
            },
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn eviction_deployment_overrides_only_name_native_local_deployments() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.resources.eviction.deployments.insert(
            "unknown".into(),
            super::EvictionSafetyOverrideConfig::default(),
        );
        assert!(config.validate().is_err());

        config.resources.eviction.deployments.clear();
        config.resources.eviction.deployments.insert(
            "deepseek_v4_flash".into(),
            super::EvictionSafetyOverrideConfig::default(),
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn eviction_monitor_requires_recommend_mode_and_bounded_timing() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.resources.eviction.mode = super::EvictionMode::Disabled;
        config.resources.eviction.monitor.enabled = true;
        assert!(config.validate().is_err());

        config.resources.eviction.mode = super::EvictionMode::Recommend;
        config.resources.eviction.monitor.poll_interval_ms = 999;
        assert!(config.validate().is_err());
        config.resources.eviction.monitor.poll_interval_ms = 1_000;
        config.resources.eviction.monitor.max_lease_ms = 59_999;
        assert!(config.validate().is_err());
    }

    #[test]
    fn pressure_thresholds_are_ordered_and_targets_restore_headroom() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .resources
            .pressure
            .critical_at_or_below_free_memory_percent = 16;
        assert!(config.validate().is_err());

        config
            .resources
            .pressure
            .critical_at_or_below_free_memory_percent = 5;
        config
            .resources
            .eviction
            .elevated
            .target_free_memory_percent = Some(15);
        assert!(config.validate().is_err());
    }

    #[test]
    fn pressure_refresh_cadence_is_low_frequency_and_bounded() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.resources.pressure.refresh_interval_ms = 4_999;
        assert!(config.validate().is_err());

        config.resources.pressure.refresh_interval_ms = 5_000;
        assert!(config.validate().is_ok());
        config.resources.pressure.refresh_interval_ms = 3_600_001;
        assert!(config.validate().is_err());
    }

    #[test]
    fn local_admission_capacity_requires_explicit_positive_measured_claims() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config.resources.admission_capacity.cpu_slots = Some(0);
        assert!(config.validate().is_err());

        config.resources.admission_capacity.cpu_slots = Some(2);
        config
            .deployments
            .get_mut("ollama_qwen3_5_2b")
            .unwrap()
            .resource_estimate
            .cpu_slots = 3;
        assert!(config.validate().is_err());

        config
            .deployments
            .get_mut("ollama_qwen3_5_2b")
            .unwrap()
            .resource_estimate
            .cpu_slots = 1;
        assert!(config.validate().is_ok());

        config
            .deployments
            .get_mut("deepseek_v4_flash")
            .unwrap()
            .resource_estimate
            .cpu_slots = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn raw_foundation_activation_requires_exact_ort127_identity() {
        let source = format!(
            "{}\n{}",
            include_str!("../../../config/infer.example.toml"),
            r#"
[intents."raw.materialize_foundation"]
data_plane = "raw.foundation"
input_modalities = ["image"]
output_modalities = ["image"]
required_features = ["rawnind_foundation_ort127_exp1"]
default_capability_floor = "foundational"
default_policy = "local-first"

[model_profiles.rawnind]
family = "rawnind"

[model_profiles.rawnind.ratings."raw.materialize_foundation"]
level = "foundational"
status = "benchmarked"
eval_profile = "rawnind-config-test-v1"

[model_builds.rawnind_ort127_exp1]
profile = "rawnind"
model_id = "darktable-ai/rawnind-public-bayer"
variant = "onnxruntime-1.27.0-cpu-fp32"
input_modalities = ["image"]
output_modalities = ["image"]
features = ["rawnind_foundation_ort127_exp1"]

[deployments.rawnind_ort127_exp1]
provider = "raw-foundation-local"
build = "rawnind_ort127_exp1"
resource_class = "heavy"
estimated_cost_usd = 0.0
supported_execution_modes = ["unary"]
"#
        );
        let mut config: RuntimeConfig = toml::from_str(&source).unwrap();
        config.raw_foundation.enabled = true;
        assert!(config.validate().is_err());

        config.raw_foundation.graph = Some("/models/model_bayer.onnx".into());
        config.raw_foundation.graph_sha256 = Some("a".repeat(64));
        config.raw_foundation.runtime_library = Some("/runtimes/libonnxruntime.dylib".into());
        config.raw_foundation.runtime_version = Some("1.27.0".into());
        config.raw_foundation.socket_directory = Some("/tmp/infer-runtime/raw".into());
        assert!(config.validate().is_ok());

        config.raw_foundation.runtime_version = Some("1.24.4".into());
        assert!(config.validate().is_err());

        config.raw_foundation.runtime_version = Some("1.27.0".into());
        config.deployments.remove("rawnind_ort127_exp1");
        assert!(config.validate().is_err());
    }

    #[test]
    fn raw_foundation_provider_is_data_plane_scoped() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let onnx = &config.providers["onnx-local"];
        let raw = &config.providers["raw-foundation-local"];

        assert!(!provider_serves_data_plane(onnx, "raw.foundation"));
        assert!(provider_serves_data_plane(onnx, "vision.image_embedding"));
        assert!(!provider_serves_data_plane(onnx, "audio.transcription"));
        assert!(provider_serves_data_plane(raw, "raw.foundation"));
        assert!(!provider_serves_data_plane(raw, "vision.image_embedding"));
    }

    #[test]
    fn shared_embedding_space_rejects_cross_build_contract_drift() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .model_builds
            .get_mut("siglip2_base_patch16_224_text_onnx_cpu_v1")
            .unwrap()
            .onnx
            .as_mut()
            .unwrap()
            .embedding_space
            .as_mut()
            .unwrap()
            .dimensions = 767;
        assert!(config.validate().is_err());
    }
}
