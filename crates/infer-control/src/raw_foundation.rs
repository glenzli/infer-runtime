//! Authenticated RawNIND control/data-plane assembly.
//!
//! `inferd` constructs this owner only when the exact experimental Build,
//! Deployment, artifact identity, runtime, and App ACL pass startup validation.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use infer_artifact_lease::{
    ARTIFACT_LEASE_CONTRACT, ArtifactLeaseError, ArtifactLeaseRegistry, LeaseRegistrationTicket,
    UNIX_FD_BINDING,
};
use infer_core::{
    AttemptTrigger, CapabilityLevel, ExecutionMode, ExecutionRequirements, Fallback, JobState,
    Latency, Modality, PlacementPreference, PlacementScope, Priority, RequestConstraints,
};
use infer_raw_foundation::{
    RawFoundationArtifactReceipt, RawFoundationExecuteRequest, RawFoundationLeaseRequest,
    RawFoundationPriority, RawNindModel, materialize_foundation_to_lease,
};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{JobPreparation, PreparedRun, Runtime, RuntimeError, unix_time_ms};

const TICKET_TTL_MS: u64 = 30_000;

#[derive(Debug, Error)]
pub enum RawFoundationControlError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Lease(#[from] ArtifactLeaseError),
    #[error(transparent)]
    Raw(#[from] infer_raw_foundation::RawFoundationError),
    #[error("raw foundation Job is unknown, already executed, or belongs to another App")]
    UnknownJob,
    #[error("RawNIND model execution worker failed")]
    Worker,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawFoundationLeaseBinding {
    pub contract: &'static str,
    pub transport: &'static str,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawFoundationLeaseGrant {
    pub object: &'static str,
    pub job_id: String,
    pub ticket_id: String,
    pub expires_at_unix_ms: u64,
    pub daemon_generation: String,
    pub binding: RawFoundationLeaseBinding,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawFoundationProvenance {
    pub provider: String,
    pub deployment: String,
    pub model_profile: String,
    pub model_build: String,
    pub physical_model: String,
    pub exact_revision: String,
    pub graph_sha256: String,
    pub implementation_revision: String,
    pub cache_identity: String,
    pub execution_provider: String,
    pub runtime_version: String,
    pub precision: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawFoundationResponse {
    pub id: String,
    pub object: &'static str,
    pub status: &'static str,
    pub source_revision: String,
    pub artifact: RawFoundationArtifactReceipt,
    pub provenance: RawFoundationProvenance,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawFoundationCancellation {
    pub id: String,
    pub object: &'static str,
    pub status: &'static str,
}

struct PendingRawJob {
    prepared: PreparedRun,
    request: RawFoundationLeaseRequest,
    expires_at_unix_ms: u64,
}

pub struct RawFoundationControl {
    runtime: Arc<Runtime>,
    registry: Arc<ArtifactLeaseRegistry>,
    model: Arc<StdMutex<RawNindModel>>,
    socket_path: PathBuf,
    pending: Mutex<HashMap<String, PendingRawJob>>,
}

impl RawFoundationControl {
    pub fn new(
        runtime: Arc<Runtime>,
        registry: Arc<ArtifactLeaseRegistry>,
        socket_path: PathBuf,
        graph: &Path,
        runtime_library: &Path,
    ) -> Result<Arc<Self>, RawFoundationControlError> {
        RawNindModel::initialize_runtime(runtime_library)?;
        let model = RawNindModel::open_experimental_cpu(graph, "1.27.0")?;
        Ok(Arc::new(Self {
            runtime,
            registry,
            model: Arc::new(StdMutex::new(model)),
            socket_path,
            pending: Mutex::new(HashMap::new()),
        }))
    }

    pub fn registry(&self) -> &Arc<ArtifactLeaseRegistry> {
        &self.registry
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn create_lease(
        self: &Arc<Self>,
        app_id: &str,
        request: RawFoundationLeaseRequest,
    ) -> Result<RawFoundationLeaseGrant, RawFoundationControlError> {
        request.validate()?;
        let priority = match request.priority {
            RawFoundationPriority::Interactive => Priority::Interactive,
            RawFoundationPriority::Background => Priority::Background,
        };
        let constraints = RequestConstraints {
            policy: Some("local-first".into()),
            priority: Some(priority),
            placement: Some(PlacementScope::LocalOnly),
            prefer: Some(PlacementPreference::Local),
            offline_required: Some(true),
            capability_floor: Some(CapabilityLevel::Foundational),
            latency: Some(if priority == Priority::Interactive {
                Latency::Interactive
            } else {
                Latency::Throughput
            }),
            max_cost_usd: Some(0.0),
            fallback: Some(Fallback::None),
            deadline_ms: request.deadline_ms,
            ..RequestConstraints::default()
        };
        let prepared = self
            .runtime
            .prepare_job(
                app_id,
                JobPreparation {
                    logical_model: &request.model,
                    constraints,
                    execution_requirements: ExecutionRequirements {
                        input_modalities: BTreeSet::from([Modality::Image]),
                        execution_mode: ExecutionMode::Unary,
                        ..ExecutionRequirements::default()
                    },
                    reasoning_effort: None,
                    estimated_tokens: 0,
                    id_prefix: "raw",
                    expected_data_plane: "raw.foundation",
                    capability_contract: crate::current_admitted_capability_contract(
                        "infer.raw-foundation@20260811.1",
                    ),
                    durable_payload: None,
                },
            )
            .await?;
        let ticket = self.registry.issue_ticket(
            app_id,
            &prepared.job_id,
            self.registry.daemon_generation(),
            TICKET_TTL_MS,
            now_ms(),
        )?;
        let job_id = prepared.job_id.clone();
        self.pending.lock().await.insert(
            job_id.clone(),
            PendingRawJob {
                prepared,
                request,
                expires_at_unix_ms: ticket.expires_at_unix_ms,
            },
        );
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(TICKET_TTL_MS + 1)).await;
            if let Some(control) = weak.upgrade() {
                let _ = control.expire_due(now_ms()).await;
            }
        });
        Ok(grant(job_id, ticket, &self.socket_path))
    }

    pub async fn execute(
        &self,
        app_id: &str,
        job_id: &str,
        request: RawFoundationExecuteRequest,
    ) -> Result<RawFoundationResponse, RawFoundationControlError> {
        let pending = self
            .pending
            .lock()
            .await
            .remove(job_id)
            .ok_or(RawFoundationControlError::UnknownJob)?;
        if pending.prepared.app_id != app_id {
            self.pending.lock().await.insert(job_id.into(), pending);
            return Err(RawFoundationControlError::UnknownJob);
        }
        if pending.prepared.cancellation.is_cancelled() {
            let _ =
                self.registry
                    .revoke_scope(app_id, job_id, self.registry.daemon_generation())?;
            return Err(RuntimeError::Cancelled.into());
        }
        let prepared = pending.prepared;
        let _reservation = self.runtime.reserve_resource(&prepared).await?;
        let _permit = self.runtime.acquire(&prepared).await?;
        self.runtime
            .mark(&prepared.job_id, JobState::Running, None)
            .await?;
        let attempt = self
            .runtime
            .begin_attempt(&prepared, AttemptTrigger::Initial)
            .await?;
        let raw_request = pending.request.clone().with_lease(request.lease_id);
        let registry = Arc::clone(&self.registry);
        let model = Arc::clone(&self.model);
        let app = app_id.to_owned();
        let job = job_id.to_owned();
        let generation = self.registry.daemon_generation().to_owned();
        let cancellation = prepared.cancellation.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| RawFoundationControlError::Worker)?;
            materialize_foundation_to_lease(
                &registry,
                &raw_request,
                &app,
                &job,
                &generation,
                now_ms(),
                &mut model,
                &cancellation,
            )
            .map_err(Into::into)
        })
        .await
        .map_err(|_| RawFoundationControlError::Worker)?;
        match result {
            Ok(artifact) => {
                self.runtime
                    .complete_vision_success(&prepared, attempt)
                    .await?;
                let identity = self
                    .model
                    .lock()
                    .map_err(|_| RawFoundationControlError::Worker)?
                    .identity()
                    .clone();
                let snapshot = self
                    .runtime
                    .snapshot(&prepared.job_id)
                    .await?
                    .ok_or(RawFoundationControlError::UnknownJob)?;
                Ok(RawFoundationResponse {
                    id: prepared.job_id,
                    object: "raw.foundation",
                    status: "completed",
                    source_revision: pending.request.source_revision,
                    artifact,
                    provenance: RawFoundationProvenance {
                        provider: snapshot.provider,
                        deployment: snapshot.deployment,
                        model_profile: snapshot.model_profile,
                        model_build: snapshot.model_build,
                        physical_model: snapshot.physical_model,
                        exact_revision: identity.exact_revision,
                        graph_sha256: identity.graph_sha256,
                        implementation_revision: identity.implementation_revision,
                        cache_identity: identity.cache_identity,
                        execution_provider: identity.execution_provider,
                        runtime_version: identity.runtime,
                        precision: identity.precision,
                    },
                })
            }
            Err(error) => {
                let runtime_error = match &error {
                    RawFoundationControlError::Lease(ArtifactLeaseError::Cancelled)
                    | RawFoundationControlError::Raw(
                        infer_raw_foundation::RawFoundationError::Lease(
                            ArtifactLeaseError::Cancelled,
                        ),
                    ) => RuntimeError::Cancelled,
                    _ => RuntimeError::Provider(infer_provider::ProviderError::Protocol(
                        "raw foundation execution failed".into(),
                    )),
                };
                let _ = self
                    .runtime
                    .complete_vision_error(&prepared, attempt, runtime_error)
                    .await?;
                Err(error)
            }
        }
    }

    pub async fn cancel(
        &self,
        app_id: &str,
        job_id: &str,
    ) -> Result<RawFoundationCancellation, RawFoundationControlError> {
        if !self.runtime.cancel_for_app(app_id, job_id).await {
            return Err(RawFoundationControlError::UnknownJob);
        }
        self.registry
            .revoke_scope(app_id, job_id, self.registry.daemon_generation())?;
        if self.pending.lock().await.remove(job_id).is_some() {
            self.runtime.mark(job_id, JobState::Cancelled, None).await?;
            self.runtime.metrics.cancelled();
        }
        Ok(RawFoundationCancellation {
            id: job_id.into(),
            object: "raw.foundation.cancellation",
            status: "cancelled",
        })
    }

    pub async fn expire_due(&self, now_unix_ms: u64) -> Result<usize, RawFoundationControlError> {
        self.registry.expire(now_unix_ms)?;
        let mut pending = self.pending.lock().await;
        let expired = pending
            .iter()
            .filter(|(_, job)| job.expires_at_unix_ms <= now_unix_ms)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let jobs = expired
            .iter()
            .filter_map(|id| pending.remove(id))
            .collect::<Vec<_>>();
        drop(pending);
        for job in jobs {
            self.runtime
                .mark(
                    &job.prepared.job_id,
                    JobState::Expired,
                    Some("artifact lease registration expired".into()),
                )
                .await?;
        }
        Ok(expired.len())
    }
}

fn grant(
    job_id: String,
    ticket: LeaseRegistrationTicket,
    socket_path: &Path,
) -> RawFoundationLeaseGrant {
    RawFoundationLeaseGrant {
        object: "raw.foundation.lease",
        job_id,
        ticket_id: ticket.ticket_id,
        expires_at_unix_ms: ticket.expires_at_unix_ms,
        daemon_generation: ticket.daemon_generation,
        binding: RawFoundationLeaseBinding {
            contract: ARTIFACT_LEASE_CONTRACT,
            transport: UNIX_FD_BINDING,
            endpoint: socket_path.display().to_string(),
        },
    }
}

fn now_ms() -> u64 {
    unix_time_ms().try_into().unwrap_or_default()
}
