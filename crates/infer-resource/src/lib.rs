//! Local resource discovery and lifecycle state.
//!
//! This crate owns native control-plane observations (starting with Ollama's
//! model tags API). Ordinary inference and scheduling remain separate
//! responsibilities in provider adapters and the control plane. Native model
//! residency changes are exposed only as explicit operator actions.

mod benchmark;
mod eviction;
mod eviction_action;
mod lifecycle;
mod ollama;

pub use benchmark::{
    ReloadBenchmarkMeasurement, ReloadBenchmarkRequest, ReloadBenchmarkRequestError,
};
pub use eviction::{
    EvictionModel, EvictionPlan, EvictionPolicy, EvictionRecommendation, EvictionRequest,
    EvictionSkip, EvictionSkipReason, EvictionTarget, ResolvedEvictionSafety, plan_eviction,
    recommend_eviction, resolve_eviction_safety,
};
pub use eviction_action::{EvictionActionAudit, EvictionApplyError, EvictionApplyRequest};
pub use lifecycle::{
    LifecycleAction, LifecycleOperationError, LifecycleTracker, ModelLifecycleSnapshot,
    ModelLifecycleState, ModelReservation,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
#[cfg(target_os = "macos")]
use std::process::Command;

use infer_core::{
    EvictionPolicyConfig, LocalInventoryKind, PressureThresholdConfig, ReloadBenchmarkConfig,
    ResourceClass, RuntimeConfig,
};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct NativeRunningModel {
    pub resident_memory_bytes: Option<u64>,
    pub resident_accelerator_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct NativeInventory {
    pub installed: BTreeSet<String>,
    pub running: BTreeMap<String, NativeRunningModel>,
}

#[derive(Debug, Error)]
pub enum NativeControlError {
    #[error("native model control operation failed: {0}")]
    Operation(String),
}

/// Provider-specific native lifecycle boundary. It has no user payload API;
/// Resource Manager remains the sole cross-provider owner of arbitration.
#[async_trait]
pub trait NativeModelController: Send + Sync {
    async fn discover(&self) -> Result<NativeInventory, NativeControlError>;
    async fn load(&self, model: &str) -> Result<(), NativeControlError>;
    async fn unload(&self, model: &str) -> Result<(), NativeControlError>;
}

pub type DynNativeModelController = Arc<dyn NativeModelController>;
pub type NativeControllerMap = BTreeMap<String, DynNativeModelController>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryState {
    /// No discovery request has completed since this runtime started. Static
    /// configuration remains usable so adding the observer is non-disruptive.
    Unknown,
    /// A complete inventory was obtained; absent configured models are not
    /// eligible for new routing decisions.
    Ready,
    /// The configured inventory source could not be queried. All deployments
    /// on that source are conservatively excluded until a later refresh works.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPressureLevel {
    Unknown,
    Normal,
    Elevated,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemPressureSnapshot {
    pub source: String,
    pub level: SystemPressureLevel,
    pub last_checked_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_memory_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl SystemPressureSnapshot {
    fn pending(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: 0,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: None,
        }
    }

    fn failed(source: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: unix_ms(),
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: Some(error.into()),
        }
    }

    /// `unknown` before the first observation is startup state, not evidence
    /// that host sampling failed. A completed failed sample always carries a
    /// timestamp and diagnostic error.
    pub fn is_pending(&self) -> bool {
        self.level == SystemPressureLevel::Unknown
            && self.last_checked_unix_ms == 0
            && self.last_error.is_none()
    }
}

/// Pluggable host observation boundary. It deliberately reports observations
/// only; policy and any resource-mutating actions stay with Resource Manager.
pub trait SystemPressureSampler: Send + Sync {
    fn sample(&self) -> SystemPressureSnapshot;
}

/// Default sampler for the current host. Platforms without an implementation
/// report `unknown` instead of manufacturing a pressure value.
pub struct HostPressureSampler {
    thresholds: PressureThresholdConfig,
}

impl HostPressureSampler {
    pub fn new(thresholds: PressureThresholdConfig) -> Self {
        Self { thresholds }
    }
}

impl SystemPressureSampler for HostPressureSampler {
    fn sample(&self) -> SystemPressureSnapshot {
        sample_host_pressure(self.thresholds)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderResourceSnapshot {
    pub provider: String,
    pub kind: LocalInventoryKind,
    pub state: InventoryState,
    pub last_checked_unix_ms: u64,
    pub discovered_models: Vec<String>,
    pub available_deployments: Vec<String>,
    pub unavailable_deployments: Vec<String>,
    pub model_lifecycle: Vec<ModelLifecycleSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Complete resource projection returned to the control API. It contains no
/// credentials, prompts, or payloads.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceSnapshot {
    pub providers: Vec<ProviderResourceSnapshot>,
    pub system_pressure: SystemPressureSnapshot,
    /// A non-mutating policy projection. It is intentionally visible even
    /// when empty so an operator can see why automatic action is unavailable.
    pub eviction_recommendation: EvictionRecommendation,
}

#[derive(Clone)]
struct ManagedInventory {
    kind: LocalInventoryKind,
    controller: DynNativeModelController,
    deployments: BTreeMap<String, String>,
}

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleOperationError),
    #[error(transparent)]
    NativeControl(#[from] NativeControlError),
    #[error(transparent)]
    ReloadBenchmarkRequest(#[from] ReloadBenchmarkRequestError),
    #[error(transparent)]
    EvictionApply(#[from] EvictionApplyError),
    #[error("unknown local resource provider `{0}`")]
    UnknownProvider(String),
    #[error("provider `{provider}` has no managed deployment `{deployment}`")]
    UnknownDeployment {
        provider: String,
        deployment: String,
    },
    #[error("eviction plan names unmanaged deployment `{0}`")]
    UnknownEvictionDeployment(String),
}

#[derive(Debug, Serialize)]
pub struct LifecycleActionResult {
    pub provider: String,
    pub deployment: String,
    pub action: LifecycleAction,
    pub resources: ResourceSnapshot,
}

#[derive(Debug, Serialize)]
pub struct ReloadBenchmarkResult {
    pub provider: String,
    pub deployment: String,
    pub measurement: ReloadBenchmarkMeasurement,
    /// A post-measurement native inventory observation. The expected model
    /// state is `absent`; an unavailable observation is returned rather than
    /// hidden, so operators do not mistake a failed cleanup for success.
    pub resources: ResourceSnapshot,
}

#[derive(Debug, Serialize)]
pub struct EvictionActionResult {
    pub provider: String,
    pub deployment: String,
    pub action: LifecycleAction,
    pub audit: EvictionActionAudit,
    pub resources: ResourceSnapshot,
}

/// Single owner of local inventory observations and their routing admission
/// projection. The manager does not infer any model's ability or quality: it
/// only says whether a configured physical build is presently discoverable.
pub struct ResourceManager {
    pressure_sampler: Arc<dyn SystemPressureSampler>,
    inventories: BTreeMap<String, ManagedInventory>,
    lifecycle: LifecycleTracker,
    eviction_actions: eviction_action::EvictionActionCoordinator,
    eviction_policy: EvictionPolicyConfig,
    deployment_classes: BTreeMap<String, ResourceClass>,
    reload_benchmarks: BTreeMap<String, ReloadBenchmarkConfig>,
    snapshots: RwLock<BTreeMap<String, ProviderResourceSnapshot>>,
    system_pressure: RwLock<SystemPressureSnapshot>,
    pressure_refresh: Mutex<()>,
}

impl ResourceManager {
    pub fn from_config(config: &RuntimeConfig) -> Self {
        Self::with_pressure_sampler_and_controllers(
            config,
            Arc::new(HostPressureSampler::new(config.resources.pressure)),
            BTreeMap::new(),
        )
    }

    pub fn with_native_controllers(
        config: &RuntimeConfig,
        controllers: NativeControllerMap,
    ) -> Self {
        Self::with_pressure_sampler_and_controllers(
            config,
            Arc::new(HostPressureSampler::new(config.resources.pressure)),
            controllers,
        )
    }

    pub fn with_pressure_sampler(
        config: &RuntimeConfig,
        pressure_sampler: Arc<dyn SystemPressureSampler>,
    ) -> Self {
        Self::with_pressure_sampler_and_controllers(config, pressure_sampler, BTreeMap::new())
    }

    pub fn with_pressure_sampler_and_controllers(
        config: &RuntimeConfig,
        pressure_sampler: Arc<dyn SystemPressureSampler>,
        mut controllers: NativeControllerMap,
    ) -> Self {
        let mut inventories = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        for (provider_id, provider) in &config.providers {
            let Some(inventory) = &provider.local_inventory else {
                continue;
            };
            let controller = match inventory.kind {
                LocalInventoryKind::OllamaTags => ollama::OllamaController::new(
                    inventory
                        .endpoint
                        .clone()
                        .expect("validated Ollama inventory has an endpoint"),
                ),
                LocalInventoryKind::OnnxSessions => {
                    controllers.remove(provider_id).unwrap_or_else(|| {
                        Arc::new(UnavailableNativeController {
                            provider: provider_id.clone(),
                        })
                    })
                }
            };
            let deployments = config
                .deployments
                .iter()
                .filter(|(_, deployment)| deployment.provider == *provider_id)
                .filter_map(|(deployment_id, deployment)| {
                    config
                        .model_builds
                        .get(&deployment.build)
                        .map(|build| (deployment_id.clone(), build.model_id.clone()))
                })
                .collect();
            let model_lifecycle = unknown_lifecycle(&deployments);
            inventories.insert(
                provider_id.clone(),
                ManagedInventory {
                    kind: inventory.kind,
                    controller,
                    deployments,
                },
            );
            snapshots.insert(
                provider_id.clone(),
                ProviderResourceSnapshot {
                    provider: provider_id.clone(),
                    kind: inventory.kind,
                    state: InventoryState::Unknown,
                    last_checked_unix_ms: 0,
                    discovered_models: Vec::new(),
                    available_deployments: Vec::new(),
                    unavailable_deployments: Vec::new(),
                    model_lifecycle,
                    last_error: None,
                },
            );
        }
        let lifecycle = LifecycleTracker::new(
            inventories
                .values()
                .flat_map(|inventory| inventory.deployments.keys().cloned()),
        );
        let deployment_classes = config
            .deployments
            .iter()
            .filter(|(_, deployment)| {
                config.providers[&deployment.provider]
                    .local_inventory
                    .is_some()
            })
            .map(|(deployment_id, deployment)| (deployment_id.clone(), deployment.resource_class))
            .collect();
        Self {
            pressure_sampler,
            inventories,
            lifecycle,
            eviction_actions: eviction_action::EvictionActionCoordinator::default(),
            eviction_policy: config.resources.eviction.clone(),
            deployment_classes,
            reload_benchmarks: config.resources.reload_benchmarks.clone(),
            snapshots: RwLock::new(snapshots),
            system_pressure: RwLock::new(SystemPressureSnapshot::pending("host")),
            pressure_refresh: Mutex::new(()),
        }
    }

    /// Refresh only the read-only host pressure observation. This deliberately
    /// does not call provider-native inventory or mutate model lifecycle.
    pub async fn refresh_system_pressure(&self) -> SystemPressureSnapshot {
        let _refresh = self.pressure_refresh.lock().await;
        let sampler = Arc::clone(&self.pressure_sampler);
        let sampled = match tokio::task::spawn_blocking(move || sampler.sample()).await {
            Ok(snapshot) => snapshot,
            Err(_) => SystemPressureSnapshot::failed("host", "system pressure sampler task failed"),
        };
        *self.system_pressure.write().await = sampled.clone();
        sampled
    }

    /// Explicitly refresh all configured local inventories. A failed inventory
    /// is represented in its snapshot rather than aborting other providers.
    pub async fn refresh(&self) -> ResourceSnapshot {
        self.refresh_system_pressure().await;
        let mut refreshed = BTreeMap::new();
        for (provider_id, inventory) in &self.inventories {
            let checked_at = unix_ms();
            let snapshot = match self.fetch_inventory(inventory).await {
                Ok(observation) => ready_snapshot(provider_id, inventory, checked_at, observation),
                Err(error) => unavailable_snapshot(provider_id, inventory, checked_at, error),
            };
            self.lifecycle
                .reconcile(&snapshot.model_lifecycle, checked_at);
            refreshed.insert(provider_id.clone(), snapshot);
        }
        {
            let mut snapshots = self.snapshots.write().await;
            for (provider_id, snapshot) in refreshed {
                snapshots.insert(provider_id, snapshot);
            }
        }
        self.snapshot().await
    }

    pub async fn snapshot(&self) -> ResourceSnapshot {
        let mut providers: Vec<_> = self.snapshots.read().await.values().cloned().collect();
        for provider in &mut providers {
            self.lifecycle.project(&mut provider.model_lifecycle);
        }
        let system_pressure = self.system_pressure.read().await.clone();
        let eviction_recommendation = recommend_eviction(
            unix_ms(),
            &self.eviction_policy,
            &system_pressure,
            BTreeSet::new(),
            eviction_models(
                &providers,
                &self.deployment_classes,
                &self.eviction_policy,
                &self.reload_benchmarks,
            ),
        );
        ResourceSnapshot {
            providers,
            system_pressure,
            eviction_recommendation,
        }
    }

    /// The routing projection is deliberately narrow: only deployments with a
    /// successful negative observation—or a failed configured observer—are
    /// excluded. `Unknown` preserves existing static routing behavior.
    pub async fn unavailable_deployments(&self) -> BTreeSet<String> {
        let mut unavailable: BTreeSet<_> = self
            .snapshots
            .read()
            .await
            .values()
            .flat_map(|snapshot| snapshot.unavailable_deployments.iter().cloned())
            .collect();
        unavailable.extend(self.lifecycle.blocked_deployments());
        unavailable
    }

    /// Protects a local deployment for the full provider attempt lifetime.
    /// Unknown or remote deployments are intentionally not claimed here.
    pub fn reserve_model(
        &self,
        deployment: &str,
    ) -> Result<Option<ModelReservation>, ResourceError> {
        Ok(self.lifecycle.reserve(deployment, unix_ms())?)
    }

    /// Explicit native load action. This is never invoked during ordinary
    /// routing; an authenticated operator must request it through the control
    /// surface. It refreshes first so the state-machine transition is based on
    /// an actual `/api/ps` observation rather than a stale daemon snapshot.
    pub async fn load_deployment(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<LifecycleActionResult, ResourceError> {
        self.lifecycle_action(provider, deployment, LifecycleAction::Load)
            .await
    }

    /// Explicit native unload action. The lifecycle tracker atomically enters
    /// `draining` before the Ollama request, which blocks new reservations and
    /// rejects the action when any active reservation already exists.
    pub async fn unload_deployment(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<LifecycleActionResult, ResourceError> {
        self.lifecycle_action(provider, deployment, LifecycleAction::Unload)
            .await
    }

    /// Apply exactly one freshly recommended eviction target after an
    /// operator confirms the deployment they inspected. Ordinary refresh and
    /// request routing never call this method.
    pub async fn apply_eviction(
        &self,
        request: EvictionApplyRequest,
    ) -> Result<EvictionActionResult, ResourceError> {
        let _action_lease = self.eviction_actions.acquire().await;
        let before = self.refresh().await;
        let prepared = self
            .eviction_actions
            .prepare(&before.eviction_recommendation, request)?;
        self.execute_eviction(prepared).await
    }

    /// Apply the current first target for a control-plane monitor that already
    /// holds a valid maintenance lease. This entry still serializes actions,
    /// refreshes inputs, and runs the same lifecycle arbitration as explicit
    /// approval.
    pub async fn apply_monitored_eviction(
        &self,
        reason: String,
    ) -> Result<EvictionActionResult, ResourceError> {
        let _action_lease = self.eviction_actions.acquire().await;
        let before = self.refresh().await;
        let prepared = self
            .eviction_actions
            .prepare_current(&before.eviction_recommendation, reason)?;
        self.execute_eviction(prepared).await
    }

    async fn execute_eviction(
        &self,
        prepared: eviction_action::PreparedEvictionAction,
    ) -> Result<EvictionActionResult, ResourceError> {
        let selected_at_unix_ms = unix_ms();
        let (provider, controller, model) = self
            .eviction_target(&prepared.selected.deployment)
            .ok_or_else(|| {
                ResourceError::UnknownEvictionDeployment(prepared.selected.deployment.clone())
            })?;
        let operation = self.lifecycle.begin_action(
            &prepared.selected.deployment,
            LifecycleAction::Unload,
            selected_at_unix_ms,
        )?;
        if let Err(error) = controller.unload(&model).await {
            operation.fail(unix_ms());
            // The native server may have completed after a transport error.
            // Reconcile rather than restoring a guessed resident state.
            self.refresh().await;
            return Err(error.into());
        }
        let completed_at_unix_ms = unix_ms();
        operation.complete(completed_at_unix_ms);
        let resources = self.refresh().await;
        Ok(EvictionActionResult {
            provider,
            deployment: prepared.selected.deployment.clone(),
            action: LifecycleAction::Unload,
            audit: EvictionActionAudit {
                reason: prepared.reason,
                selected_at_unix_ms,
                completed_at_unix_ms,
                pressure: prepared.pressure,
                target_free_memory_percent: prepared.target_free_memory_percent,
                current_free_memory_bytes: prepared.current_free_memory_bytes,
                target_free_memory_bytes: prepared.target_free_memory_bytes,
                requested_bytes: prepared.requested_bytes,
                projected_freed_bytes: prepared.projected_freed_bytes,
                plan_shortfall_bytes: prepared.plan_shortfall_bytes,
                selected: prepared.selected,
            },
            resources,
        })
    }

    /// Measure reload time from an observed non-resident state. This is an
    /// explicit, potentially resource-intensive operator action; it performs
    /// no config write and it refuses a model that was already resident.
    pub async fn benchmark_reload(
        &self,
        provider: &str,
        deployment: &str,
        request: ReloadBenchmarkRequest,
    ) -> Result<ReloadBenchmarkResult, ResourceError> {
        request.validate()?;
        self.refresh().await;
        let (controller, model) = self.native_target(provider, deployment)?;
        let operation =
            self.lifecycle
                .begin_action(deployment, LifecycleAction::BenchmarkReload, unix_ms())?;
        let measurement = match benchmark::measure_reload(
            controller.as_ref(),
            &model,
            &request,
            unix_ms(),
            deployment,
        )
        .await
        {
            Ok(measurement) => measurement,
            Err(error) => {
                operation.fail(unix_ms());
                // Native load/unload could have completed after a transport
                // error. Reconcile before exposing failure to avoid retaining
                // a false `absent` projection.
                self.refresh().await;
                return Err(error.into());
            }
        };
        operation.complete(unix_ms());
        let resources = self.refresh().await;
        Ok(ReloadBenchmarkResult {
            provider: provider.into(),
            deployment: deployment.into(),
            measurement,
            resources,
        })
    }

    async fn lifecycle_action(
        &self,
        provider: &str,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<LifecycleActionResult, ResourceError> {
        // This observation is a precondition, not an automatic lifecycle
        // action. It ensures the state machine starts from a real host view.
        self.refresh().await;
        let (controller, model) = self.native_target(provider, deployment)?;
        let operation = self.lifecycle.begin_action(deployment, action, unix_ms())?;
        match action {
            LifecycleAction::Load => controller.load(&model).await?,
            LifecycleAction::Unload => controller.unload(&model).await?,
            LifecycleAction::BenchmarkReload => {
                unreachable!("reload benchmarks use their dedicated measurement owner")
            }
        }
        operation.complete(unix_ms());
        let resources = self.refresh().await;
        Ok(LifecycleActionResult {
            provider: provider.into(),
            deployment: deployment.into(),
            action,
            resources,
        })
    }

    fn native_target(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<(DynNativeModelController, String), ResourceError> {
        let inventory = self
            .inventories
            .get(provider)
            .ok_or_else(|| ResourceError::UnknownProvider(provider.into()))?;
        let model = inventory.deployments.get(deployment).ok_or_else(|| {
            ResourceError::UnknownDeployment {
                provider: provider.into(),
                deployment: deployment.into(),
            }
        })?;
        Ok((Arc::clone(&inventory.controller), model.clone()))
    }

    fn eviction_target(
        &self,
        deployment: &str,
    ) -> Option<(String, DynNativeModelController, String)> {
        self.inventories.iter().find_map(|(provider, inventory)| {
            inventory.deployments.get(deployment).map(|model| {
                (
                    provider.clone(),
                    Arc::clone(&inventory.controller),
                    model.clone(),
                )
            })
        })
    }

    async fn fetch_inventory(
        &self,
        inventory: &ManagedInventory,
    ) -> Result<NativeInventory, NativeControlError> {
        inventory.controller.discover().await
    }
}

struct UnavailableNativeController {
    provider: String,
}

#[async_trait]
impl NativeModelController for UnavailableNativeController {
    async fn discover(&self) -> Result<NativeInventory, NativeControlError> {
        Err(NativeControlError::Operation(format!(
            "provider {} did not register a native controller",
            self.provider
        )))
    }

    async fn load(&self, _model: &str) -> Result<(), NativeControlError> {
        self.discover().await.map(|_| ())
    }

    async fn unload(&self, _model: &str) -> Result<(), NativeControlError> {
        self.discover().await.map(|_| ())
    }
}

fn eviction_models(
    providers: &[ProviderResourceSnapshot],
    deployment_classes: &BTreeMap<String, ResourceClass>,
    eviction_policy: &EvictionPolicyConfig,
    reload_benchmarks: &BTreeMap<String, ReloadBenchmarkConfig>,
) -> Vec<EvictionModel> {
    providers
        .iter()
        .flat_map(|provider| provider.model_lifecycle.iter())
        .filter_map(|model| {
            let benchmark = reload_benchmarks.get(&model.deployment);
            let resource_class = deployment_classes.get(&model.deployment).copied()?;
            Some(EvictionModel {
                deployment: model.deployment.clone(),
                resource_class,
                safety: resolve_eviction_safety(eviction_policy, &model.deployment, resource_class),
                state: model.state,
                resident_memory_bytes: model.resident_memory_bytes,
                active_reservations: model.active_reservations,
                state_since_unix_ms: model.state_since_unix_ms,
                last_used_unix_ms: model.last_used_unix_ms,
                reload_cost_ms: benchmark.map(|benchmark| benchmark.reload_cost_ms),
                benchmark_observed_at_unix_ms: benchmark
                    .map(|benchmark| benchmark.observed_at_unix_ms),
            })
        })
        .collect()
}

fn ready_snapshot(
    provider: &str,
    inventory: &ManagedInventory,
    checked_at: u64,
    observation: NativeInventory,
) -> ProviderResourceSnapshot {
    let mut available_deployments = Vec::new();
    let mut unavailable_deployments = Vec::new();
    for (deployment, model_id) in &inventory.deployments {
        if observation.installed.contains(model_id) {
            available_deployments.push(deployment.clone());
        } else {
            unavailable_deployments.push(deployment.clone());
        }
    }
    ProviderResourceSnapshot {
        provider: provider.into(),
        kind: inventory.kind,
        state: InventoryState::Ready,
        last_checked_unix_ms: checked_at,
        discovered_models: observation.installed.iter().cloned().collect(),
        available_deployments,
        unavailable_deployments,
        model_lifecycle: lifecycle_from_observation(inventory, &observation.running),
        last_error: None,
    }
}

fn unavailable_snapshot(
    provider: &str,
    inventory: &ManagedInventory,
    checked_at: u64,
    error: NativeControlError,
) -> ProviderResourceSnapshot {
    ProviderResourceSnapshot {
        provider: provider.into(),
        kind: inventory.kind,
        state: InventoryState::Unavailable,
        last_checked_unix_ms: checked_at,
        discovered_models: Vec::new(),
        available_deployments: Vec::new(),
        unavailable_deployments: inventory.deployments.keys().cloned().collect(),
        model_lifecycle: unknown_lifecycle(&inventory.deployments),
        last_error: Some(error.to_string()),
    }
}

fn lifecycle_from_observation(
    inventory: &ManagedInventory,
    running: &BTreeMap<String, NativeRunningModel>,
) -> Vec<ModelLifecycleSnapshot> {
    inventory
        .deployments
        .iter()
        .map(|(deployment, model_id)| match running.get(model_id) {
            Some(model) => ModelLifecycleSnapshot {
                deployment: deployment.clone(),
                model_id: model_id.clone(),
                state: ModelLifecycleState::Ready,
                resident_memory_bytes: model.resident_memory_bytes,
                resident_vram_bytes: model.resident_accelerator_memory_bytes,
                active_reservations: 0,
                state_since_unix_ms: None,
                last_used_unix_ms: None,
            },
            None => ModelLifecycleSnapshot {
                deployment: deployment.clone(),
                model_id: model_id.clone(),
                state: ModelLifecycleState::Absent,
                resident_memory_bytes: None,
                resident_vram_bytes: None,
                active_reservations: 0,
                state_since_unix_ms: None,
                last_used_unix_ms: None,
            },
        })
        .collect()
}

fn unknown_lifecycle(deployments: &BTreeMap<String, String>) -> Vec<ModelLifecycleSnapshot> {
    deployments
        .iter()
        .map(|(deployment, model_id)| ModelLifecycleSnapshot {
            deployment: deployment.clone(),
            model_id: model_id.clone(),
            state: ModelLifecycleState::Unknown,
            resident_memory_bytes: None,
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn sample_host_pressure(thresholds: PressureThresholdConfig) -> SystemPressureSnapshot {
    let checked_at = unix_ms();
    let pressure = Command::new("memory_pressure").arg("-Q").output();
    let Ok(pressure) = pressure else {
        return SystemPressureSnapshot {
            source: "macos.memory_pressure".into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: checked_at,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: Some("memory pressure command unavailable".into()),
        };
    };
    if !pressure.status.success() {
        return SystemPressureSnapshot {
            source: "macos.memory_pressure".into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: checked_at,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: Some("memory pressure command failed".into()),
        };
    }
    let Ok(free_memory_percent) = parse_macos_free_memory_percent(&pressure.stdout) else {
        return SystemPressureSnapshot {
            source: "macos.memory_pressure".into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: checked_at,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: Some("memory pressure response was malformed".into()),
        };
    };
    let total_memory_bytes = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse().ok());
    SystemPressureSnapshot {
        source: "macos.memory_pressure".into(),
        level: pressure_level(free_memory_percent, thresholds),
        last_checked_unix_ms: checked_at,
        total_memory_bytes,
        free_memory_percent: Some(free_memory_percent),
        last_error: None,
    }
}

#[cfg(not(target_os = "macos"))]
fn sample_host_pressure(_thresholds: PressureThresholdConfig) -> SystemPressureSnapshot {
    SystemPressureSnapshot::failed("unsupported", "host pressure sampler is not implemented")
}

#[cfg(target_os = "macos")]
fn parse_macos_free_memory_percent(output: &[u8]) -> Result<u8, ()> {
    let output = std::str::from_utf8(output).map_err(|_| ())?;
    output
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("System-wide memory free percentage:")
        })
        .map(str::trim)
        .and_then(|value| value.strip_suffix('%'))
        .map(str::trim)
        .ok_or(())?
        .parse()
        .map_err(|_| ())
}

fn pressure_level(
    free_memory_percent: u8,
    thresholds: PressureThresholdConfig,
) -> SystemPressureLevel {
    if free_memory_percent <= thresholds.critical_at_or_below_free_memory_percent {
        SystemPressureLevel::Critical
    } else if free_memory_percent <= thresholds.elevated_at_or_below_free_memory_percent {
        SystemPressureLevel::Elevated
    } else {
        SystemPressureLevel::Normal
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RuntimeConfig {
        toml::from_str(
            r#"
            [server]
            bind = "127.0.0.1:8787"
            [defaults]
            policy = "balanced"
            [profiles.balanced]
            order = ["cost"]
            [providers.local]
            kind = "responses"
            base_url = "http://127.0.0.1:11434/v1"
            placement = "local"
            [providers.local.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses"]
            [providers.local.local_inventory]
            kind = "ollama_tags"
            endpoint = "http://127.0.0.1:11434"
            [intents."text.summarize"]
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_quality_floor = "basic"
            [model_profiles.small]
            family = "qwen"
            [model_profiles.small.ratings."text.summarize"]
            grade = "basic"
            status = "provisional"
            [model_builds.small]
            profile = "small"
            model_id = "qwen3.5:2b-mlx"
            input_modalities = ["text"]
            output_modalities = ["text"]
            [deployments.small]
            provider = "local"
            build = "small"
            [apps.test-app]
            credential = { source = "environment", variable = "INFER_TEST_TOKEN" }
            "#,
        )
        .unwrap()
    }

    #[test]
    fn ollama_tags_preserve_full_tag_identity() {
        let models =
            ollama::parse_tags(r#"{"models":[{"name":"qwen3.5:2b-mlx"},{"name":"qwen3-vl:4b"}]}"#)
                .unwrap();
        assert!(models.contains("qwen3.5:2b-mlx"));
        assert!(models.contains("qwen3-vl:4b"));
    }

    #[tokio::test]
    async fn fresh_manager_is_observational_until_an_inventory_is_refreshed() {
        let manager = ResourceManager::from_config(&config());
        let snapshot = manager.snapshot().await;
        assert_eq!(snapshot.providers[0].state, InventoryState::Unknown);
        assert_eq!(snapshot.system_pressure.level, SystemPressureLevel::Unknown);
        assert!(manager.unavailable_deployments().await.is_empty());
    }

    #[test]
    fn complete_inventory_excludes_only_missing_configured_deployments() {
        let config = config();
        let manager = ResourceManager::from_config(&config);
        let inventory = manager.inventories.get("local").unwrap();
        let snapshot = ready_snapshot(
            "local",
            inventory,
            1,
            NativeInventory {
                installed: BTreeSet::from(["qwen3-vl:4b".into()]),
                running: BTreeMap::new(),
            },
        );
        assert_eq!(snapshot.state, InventoryState::Ready);
        assert_eq!(snapshot.unavailable_deployments, vec!["small"]);
        assert_eq!(
            snapshot.model_lifecycle[0].state,
            ModelLifecycleState::Absent
        );
    }

    #[test]
    fn ollama_running_models_report_residency_without_affecting_installation() {
        let running = ollama::parse_running(
            r#"{"models":[{"name":"qwen3.5:2b-mlx","size":2048,"size_vram":1024}]}"#,
        )
        .unwrap();
        assert_eq!(running["qwen3.5:2b-mlx"].size_vram, 1024);
        let config = config();
        let manager = ResourceManager::from_config(&config);
        let lifecycle = lifecycle_from_observation(
            manager.inventories.get("local").unwrap(),
            &running
                .into_iter()
                .map(|(name, model)| {
                    (
                        name,
                        NativeRunningModel {
                            resident_memory_bytes: Some(model.size),
                            resident_accelerator_memory_bytes: Some(model.size_vram),
                        },
                    )
                })
                .collect(),
        );
        assert_eq!(lifecycle[0].state, ModelLifecycleState::Ready);
        assert_eq!(lifecycle[0].resident_memory_bytes, Some(2048));
    }

    #[test]
    fn pressure_thresholds_are_conservative_and_deterministic() {
        let defaults = PressureThresholdConfig::default();
        assert_eq!(pressure_level(5, defaults), SystemPressureLevel::Critical);
        assert_eq!(pressure_level(15, defaults), SystemPressureLevel::Elevated);
        assert_eq!(pressure_level(16, defaults), SystemPressureLevel::Normal);

        let tuned = PressureThresholdConfig {
            elevated_at_or_below_free_memory_percent: 80,
            critical_at_or_below_free_memory_percent: 60,
            ..PressureThresholdConfig::default()
        };
        assert_eq!(pressure_level(60, tuned), SystemPressureLevel::Critical);
        assert_eq!(pressure_level(70, tuned), SystemPressureLevel::Elevated);
        assert_eq!(pressure_level(81, tuned), SystemPressureLevel::Normal);
    }

    struct StaticPressureSampler;

    impl SystemPressureSampler for StaticPressureSampler {
        fn sample(&self) -> SystemPressureSnapshot {
            SystemPressureSnapshot {
                source: "test".into(),
                level: SystemPressureLevel::Elevated,
                last_checked_unix_ms: 1,
                total_memory_bytes: Some(1024),
                free_memory_percent: Some(12),
                last_error: None,
            }
        }
    }

    #[tokio::test]
    async fn injected_pressure_sampler_is_observed_without_a_host_dependency() {
        let mut config = config();
        config.providers.clear();
        config.deployments.clear();
        let manager =
            ResourceManager::with_pressure_sampler(&config, Arc::new(StaticPressureSampler));
        let snapshot = manager.refresh().await;
        assert!(snapshot.providers.is_empty());
        assert_eq!(snapshot.system_pressure.source, "test");
        assert_eq!(
            snapshot.system_pressure.level,
            SystemPressureLevel::Elevated
        );
    }

    #[tokio::test]
    async fn resource_snapshot_combines_pressure_with_fresh_configured_benchmark() {
        let mut config = config();
        config.resources.eviction = infer_core::EvictionPolicyConfig {
            mode: infer_core::EvictionMode::Recommend,
            automatic_eligible: true,
            minimum_resident_ms: 0,
            max_benchmark_age_ms: 60_000,
            classes: BTreeMap::new(),
            deployments: BTreeMap::new(),
            monitor: Default::default(),
            elevated: infer_core::PressureTargetConfig {
                target_free_memory_percent: Some(20),
            },
            critical: infer_core::PressureTargetConfig {
                target_free_memory_percent: Some(25),
            },
        };
        config.resources.reload_benchmarks.insert(
            "small".into(),
            ReloadBenchmarkConfig {
                reload_cost_ms: 500,
                observed_at_unix_ms: unix_ms(),
                evidence: "test fixture".into(),
            },
        );
        let manager =
            ResourceManager::with_pressure_sampler(&config, Arc::new(StaticPressureSampler));
        let inventory = manager.inventories.get("local").unwrap();
        let observed_at = unix_ms();
        let provider = ready_snapshot(
            "local",
            inventory,
            observed_at,
            NativeInventory {
                installed: BTreeSet::from(["qwen3.5:2b-mlx".into()]),
                running: BTreeMap::from([(
                    "qwen3.5:2b-mlx".into(),
                    NativeRunningModel {
                        resident_memory_bytes: Some(512),
                        resident_accelerator_memory_bytes: None,
                    },
                )]),
            },
        );
        manager
            .lifecycle
            .reconcile(&provider.model_lifecycle, observed_at);
        manager
            .snapshots
            .write()
            .await
            .insert("local".into(), provider);
        *manager.system_pressure.write().await = StaticPressureSampler.sample();

        let snapshot = manager.snapshot().await;
        let EvictionRecommendation::Planned { plan, .. } = snapshot.eviction_recommendation else {
            panic!("elevated pressure with a fresh benchmark should plan a dry run");
        };
        assert_eq!(plan.targets[0].deployment, "small");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_the_macos_memory_pressure_summary() {
        let free = parse_macos_free_memory_percent(
            b"The system has 17179869184 bytes of physical memory.\nSystem-wide memory free percentage: 17%\n",
        )
        .unwrap();
        assert_eq!(free, 17);
    }
}
