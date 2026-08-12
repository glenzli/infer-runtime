//! Durable local Responses lifecycle and encrypted payload orchestration.

use std::{
    collections::BTreeSet,
    env,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use infer_core::{
    CandidateDecisionStatus, DurablePayloadKind, DurablePayloadRef, JobSnapshot, JobState,
    Placement, Priority, ResponsesRequest,
};
use infer_payload::EncryptedPayloadSpool;
use infer_store::{AuditEventInput, BackgroundRecovery, RecoverableBackgroundJob};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    CompletionMode, JobEntry, JobPreparation, PreparedRun, Runtime, RuntimeError, attempt_policy,
    estimate_response_tokens,
    registry::{Candidate, candidate_for_deployment},
};

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundSubmission {
    pub id: String,
    pub object: &'static str,
    pub created_at: i64,
    pub status: &'static str,
    pub background: bool,
    pub model: String,
    pub output: Vec<Value>,
    pub parallel_tool_calls: bool,
    pub tool_choice: &'static str,
    pub tools: Vec<Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultPublication {
    Published,
    Cancelled,
    PersistenceFailed,
}

#[derive(Clone)]
pub(crate) struct BackgroundJobs {
    spool: Option<Arc<EncryptedPayloadSpool>>,
    result_retention_ms: u64,
    max_recovery_replays: u8,
}

impl BackgroundJobs {
    pub(crate) fn from_config(config: &infer_core::BackgroundConfig) -> Result<Self, RuntimeError> {
        let spool = if config.enabled {
            let key_env = config
                .key_env
                .as_ref()
                .expect("enabled background config was validated");
            let key = Zeroizing::new(
                env::var(key_env)
                    .map_err(|_| RuntimeError::BackgroundKeyUnavailable(key_env.clone()))?,
            );
            Some(Arc::new(EncryptedPayloadSpool::open(
                &config.payload_directory,
                &key,
                config.max_payload_bytes,
            )?))
        } else {
            None
        };
        Ok(Self {
            spool,
            result_retention_ms: config.result_retention_ms,
            max_recovery_replays: config.max_recovery_replays,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(spool: Arc<EncryptedPayloadSpool>, result_retention_ms: u64) -> Self {
        Self {
            spool: Some(spool),
            result_retention_ms,
            max_recovery_replays: 2,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            spool: None,
            result_retention_ms: 0,
            max_recovery_replays: 0,
        }
    }

    pub(crate) fn recovery_limit(&self) -> Option<u8> {
        self.spool.as_ref().map(|_| self.max_recovery_replays)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.spool.is_some()
    }

    pub(crate) async fn validate_pending(
        &self,
        payloads: Vec<(String, DurablePayloadRef)>,
    ) -> Result<(), RuntimeError> {
        for (app_id, reference) in payloads {
            // Authentication happens before recovery mutates Job/Attempt
            // metadata. A validly formatted but wrong key therefore fails
            // daemon startup without consuming a replay or deleting input.
            drop(self.get(app_id, reference).await?);
        }
        Ok(())
    }

    async fn put(
        &self,
        app_id: String,
        kind: DurablePayloadKind,
        bytes: Vec<u8>,
    ) -> Result<DurablePayloadRef, RuntimeError> {
        let spool = self.spool.clone().ok_or(RuntimeError::BackgroundDisabled)?;
        let bytes = Zeroizing::new(bytes);
        spawn_blocking(move || spool.put(&app_id, kind, &bytes))
            .await
            .map_err(|_| RuntimeError::BackgroundTaskFailed)?
            .map_err(RuntimeError::from)
    }

    async fn get(
        &self,
        app_id: String,
        reference: DurablePayloadRef,
    ) -> Result<Zeroizing<Vec<u8>>, RuntimeError> {
        let spool = self.spool.clone().ok_or(RuntimeError::BackgroundDisabled)?;
        spawn_blocking(move || spool.get(&app_id, &reference))
            .await
            .map_err(|_| RuntimeError::BackgroundTaskFailed)?
            .map_err(RuntimeError::from)
    }

    pub(crate) async fn delete(&self, reference: DurablePayloadRef) -> Result<(), RuntimeError> {
        let Some(spool) = self.spool.clone() else {
            return Ok(());
        };
        spawn_blocking(move || spool.delete(&reference))
            .await
            .map_err(|_| RuntimeError::BackgroundTaskFailed)?
            .map_err(RuntimeError::from)
    }

    pub(crate) async fn remove_orphans(
        &self,
        references: Vec<DurablePayloadRef>,
    ) -> Result<(), RuntimeError> {
        let spool = self.spool.clone().ok_or(RuntimeError::BackgroundDisabled)?;
        let live = references
            .into_iter()
            .map(|reference| reference.blob_id)
            .collect::<BTreeSet<_>>();
        spawn_blocking(move || spool.remove_orphans(&live))
            .await
            .map_err(|_| RuntimeError::BackgroundTaskFailed)??;
        Ok(())
    }
}

impl Runtime {
    pub async fn submit_background(
        self: &Arc<Self>,
        app_id: &str,
        request: ResponsesRequest,
    ) -> Result<BackgroundSubmission, RuntimeError> {
        let mut request = request;
        if !self.background.is_enabled() {
            return Err(RuntimeError::BackgroundDisabled);
        }
        enforce_local_background_constraints(&mut request)?;
        let request = self.prepare_responses_request(request)?;
        let constraints = request.constraints()?;
        let execution_requirements = request.execution_requirements();
        let serialized =
            serde_json::to_vec(&request).map_err(|_| RuntimeError::BackgroundPayloadFormat)?;
        let request_ref = self
            .background
            .put(
                app_id.into(),
                DurablePayloadKind::ResponsesRequest,
                serialized,
            )
            .await?;
        let preparation = JobPreparation {
            logical_model: &request.model,
            constraints,
            execution_requirements,
            reasoning_effort: request.reasoning_effort(),
            estimated_tokens: estimate_response_tokens(&request),
            id_prefix: "resp",
            expected_data_plane: "responses",
            capability_contract: super::current_admitted_capability_contract(
                "infer.responses@20260812.1",
            ),
            durable_payload: Some(&request_ref),
        };
        let prepared = match self.prepare_job(app_id, preparation).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = self.background.delete(request_ref).await;
                return Err(error);
            }
        };
        let submission = BackgroundSubmission {
            id: prepared.job_id.clone(),
            object: "response",
            created_at: unix_ms() / 1_000,
            status: "queued",
            background: true,
            model: prepared.logical_model.clone(),
            output: Vec::new(),
            parallel_tool_calls: true,
            tool_choice: "auto",
            tools: Vec::new(),
        };
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime.run_background(prepared, request, request_ref).await;
        });
        Ok(submission)
    }

    pub async fn background_response(
        &self,
        app_id: &str,
        job_id: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some(payload) = store.background_job_payload(app_id, job_id)? else {
            return Ok(None);
        };
        if let Some(result) = payload.result {
            if payload
                .result_expires_at_ms
                .is_some_and(|expiry| expiry <= unix_ms())
            {
                let expired = store.expire_background_payloads(unix_ms())?;
                for reference in expired {
                    let _ = self.background.delete(reference).await;
                }
                return Ok(None);
            }
            let bytes = self.background.get(app_id.into(), result).await?;
            let value = serde_json::from_slice(&bytes)
                .map_err(|_| RuntimeError::BackgroundPayloadFormat)?;
            return Ok(Some(value));
        }
        let snapshot = self.snapshot_for_app(app_id, job_id).await?;
        Ok(snapshot.map(|snapshot| background_snapshot(&snapshot, payload.created_at_ms)))
    }

    pub async fn cancel_background(
        &self,
        app_id: &str,
        job_id: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some(payload) = store.background_job_payload(app_id, job_id)? else {
            return Ok(None);
        };
        let cancelled = self.cancel_for_app(app_id, job_id).await;
        let Some(snapshot) = self.snapshot_for_app(app_id, job_id).await? else {
            return Ok(None);
        };
        let mut response = background_snapshot(&snapshot, payload.created_at_ms);
        if cancelled {
            response["status"] = Value::String("cancelling".into());
        }
        Ok(Some(response))
    }

    pub(crate) async fn restore_background(self: &Arc<Self>, recovery: BackgroundRecovery) {
        for reference in recovery.discard {
            let _ = self.background.delete(reference).await;
        }
        for record in recovery.runnable {
            match self.restore_prepared(&record).await {
                Ok((prepared, request)) => {
                    let runtime = Arc::clone(self);
                    tokio::spawn(async move {
                        runtime
                            .run_background(prepared, request, record.request)
                            .await;
                    });
                }
                Err(error) => {
                    let mut snapshot = record.snapshot.clone();
                    snapshot.state = JobState::Failed;
                    let public_error = error.public_message();
                    snapshot.error = Some(public_error.clone());
                    let _ = self.persist_snapshot(
                        &snapshot,
                        AuditEventInput {
                            kind: "background.recovery_failed".into(),
                            details: json!({"reason": public_error}),
                        },
                    );
                    self.retire_background_input(&record.snapshot.id, record.request)
                        .await;
                }
            }
        }
    }

    async fn restore_prepared(
        &self,
        record: &RecoverableBackgroundJob,
    ) -> Result<(PreparedRun, ResponsesRequest), RuntimeError> {
        let bytes = self
            .background
            .get(record.snapshot.app_id.clone(), record.request.clone())
            .await?;
        let mut request: ResponsesRequest =
            serde_json::from_slice(&bytes).map_err(|_| RuntimeError::BackgroundPayloadFormat)?;
        enforce_local_background_constraints(&mut request)?;
        let request = self.prepare_responses_request(request)?;
        if record.snapshot.attempts.len() >= attempt_policy::MAX_ATTEMPTS {
            return Err(RuntimeError::BackgroundAttemptBudgetExhausted);
        }
        let app_permit = self
            .app_admission
            .try_admit(&record.snapshot.app_id)
            .map_err(|_| RuntimeError::AppQueueFull)?;
        let mut targets = persisted_targets(&self.config, &record.snapshot)?;
        let candidate = targets.first().cloned().ok_or(RuntimeError::NoCandidate)?;
        let constraints = request.constraints()?;
        let now = unix_ms();
        let deadline = constraints.deadline_ms.map(|deadline_ms| {
            let elapsed = now.saturating_sub(record.created_at_ms) as u64;
            tokio::time::Instant::now() + Duration::from_millis(deadline_ms.saturating_sub(elapsed))
        });
        let cancellation = CancellationToken::new();
        self.jobs.lock().await.insert(
            record.snapshot.id.clone(),
            JobEntry {
                snapshot: record.snapshot.clone(),
                cancellation: cancellation.clone(),
                admission_permit: Some(app_permit),
            },
        );
        self.metrics.submitted();
        Ok((
            PreparedRun {
                job_id: record.snapshot.id.clone(),
                app_id: record.snapshot.app_id.clone(),
                logical_model: record.snapshot.intent.clone(),
                provider_id: candidate.provider_id.clone(),
                deployment_id: candidate.deployment_id.clone(),
                physical_model: candidate.physical_model.clone(),
                estimated_cost_usd: candidate.estimated_cost_usd,
                estimated_tokens: estimate_response_tokens(&request),
                cancellation,
                priority: Priority::Background,
                submitted_at: tokio::time::Instant::now(),
                deadline,
                existing_attempts: record.snapshot.attempts.len(),
                recovered: true,
                targets: std::mem::take(&mut targets),
            },
            request,
        ))
    }

    async fn run_background(
        self: Arc<Self>,
        prepared: PreparedRun,
        request: ResponsesRequest,
        request_ref: DurablePayloadRef,
    ) {
        let job_id = prepared.job_id.clone();
        let app_id = prepared.app_id.clone();
        match self
            .execute_prepared(request, prepared, CompletionMode::DeferredBackground)
            .await
        {
            Ok(mut response) => {
                response["status"] = Value::String("completed".into());
                response["background"] = Value::Bool(true);
                if let Some(created_at_ms) = self
                    .store
                    .as_ref()
                    .and_then(|store| {
                        store
                            .background_job_payload(&app_id, &job_id)
                            .ok()
                            .flatten()
                    })
                    .map(|payload| payload.created_at_ms)
                {
                    response["created_at"] = json!(created_at_ms / 1_000);
                }
                let serialized = match serde_json::to_vec(&response) {
                    Ok(serialized) => serialized,
                    Err(_) => {
                        let _ = self
                            .mark(
                                &job_id,
                                JobState::Failed,
                                Some("background result serialization failed".into()),
                            )
                            .await;
                        self.retire_background_input(&job_id, request_ref).await;
                        return;
                    }
                };
                let result_ref = match self
                    .background
                    .put(app_id, DurablePayloadKind::ResponsesResult, serialized)
                    .await
                {
                    Ok(reference) => reference,
                    Err(RuntimeError::Payload(infer_payload::PayloadError::TooLarge {
                        ..
                    })) => {
                        let _ = self
                            .mark(
                                &job_id,
                                JobState::Failed,
                                Some(
                                    "background result exceeds the configured payload limit".into(),
                                ),
                            )
                            .await;
                        self.retire_background_input(&job_id, request_ref).await;
                        return;
                    }
                    Err(error) => {
                        // A transient spool failure leaves the durable Job and
                        // encrypted request running. Restart can then replay it
                        // after storage is repaired; consuming the input here
                        // would make the durability promise false.
                        tracing::error!(
                            job_id,
                            error = %error,
                            "background result spool failed; encrypted input retained for restart"
                        );
                        return;
                    }
                };
                match self.publish_background_result(&job_id, &result_ref).await {
                    ResultPublication::Published => {
                        let _ = self.background.delete(request_ref).await;
                    }
                    ResultPublication::Cancelled => {
                        let _ = self.background.delete(result_ref).await;
                        let _ = self.mark(&job_id, JobState::Cancelled, None).await;
                        self.retire_background_input(&job_id, request_ref).await;
                    }
                    ResultPublication::PersistenceFailed => {
                        // The unpublished result is an orphan, but the input
                        // remains referenced by a running Job for restart.
                        let _ = self.background.delete(result_ref).await;
                    }
                }
            }
            Err(_) => self.retire_background_input(&job_id, request_ref).await,
        }
    }

    async fn publish_background_result(
        &self,
        job_id: &str,
        result: &DurablePayloadRef,
    ) -> ResultPublication {
        let Some(store) = &self.store else {
            tracing::error!(job_id, "background result has no metadata store");
            return ResultPublication::PersistenceFailed;
        };
        // Keep terminal arbitration and metadata publication under the same
        // in-memory Job lock. A concurrent cancel therefore wins before this
        // check or observes the committed succeeded state; it cannot persist a
        // second terminal state in the gap between the two.
        let mut jobs = self.jobs.lock().await;
        let Some(entry) = jobs.get_mut(job_id) else {
            tracing::error!(job_id, "background result has no in-memory Job entry");
            return ResultPublication::PersistenceFailed;
        };
        if entry.cancellation.is_cancelled() {
            return ResultPublication::Cancelled;
        }
        let mut snapshot = entry.snapshot.clone();
        snapshot.state = JobState::Succeeded;
        snapshot.error = None;
        let expiry = unix_ms().saturating_add(self.background.result_retention_ms as i64);
        if let Err(error) = store.complete_background_job(&snapshot, result, expiry) {
            tracing::error!(
                job_id,
                error = %error,
                "background result metadata transaction failed"
            );
            return ResultPublication::PersistenceFailed;
        }
        entry.snapshot = snapshot;
        entry.admission_permit.take();
        drop(jobs);
        self.metrics.succeeded();
        ResultPublication::Published
    }

    async fn retire_background_input(&self, job_id: &str, fallback: DurablePayloadRef) {
        let Some(store) = &self.store else {
            return;
        };
        let terminal = store
            .load_job(job_id)
            .ok()
            .flatten()
            .is_some_and(|snapshot| {
                matches!(
                    snapshot.state,
                    JobState::Succeeded
                        | JobState::Failed
                        | JobState::Cancelled
                        | JobState::Expired
                )
            });
        if !terminal {
            return;
        }
        let reference = match store.retire_background_request(job_id) {
            Ok(Some(reference)) => reference,
            Ok(None) => fallback,
            Err(error) => {
                tracing::error!(
                    job_id,
                    error = %error,
                    "background input reference retirement failed; blob retained"
                );
                return;
            }
        };
        let _ = self.background.delete(reference).await;
    }
}

fn enforce_local_background_constraints(
    request: &mut ResponsesRequest,
) -> Result<(), RuntimeError> {
    if !request.background || request.stream {
        return Err(RuntimeError::BackgroundLocalOnly);
    }
    match request.metadata.get("infer.placement").map(String::as_str) {
        None | Some("local_only") => {}
        Some(_) => return Err(RuntimeError::BackgroundLocalOnly),
    }
    match request.metadata.get("infer.priority").map(String::as_str) {
        None | Some("background") => {}
        Some(_) => return Err(RuntimeError::BackgroundLocalOnly),
    }
    request
        .metadata
        .insert("infer.placement".into(), "local_only".into());
    request
        .metadata
        .insert("infer.priority".into(), "background".into());
    Ok(())
}

fn persisted_targets(
    config: &infer_core::RuntimeConfig,
    snapshot: &JobSnapshot,
) -> Result<Vec<Candidate>, RuntimeError> {
    let fallback = snapshot
        .constraints
        .fallback
        .unwrap_or(infer_core::Fallback::None);
    let mut decisions = snapshot
        .routing
        .candidates
        .iter()
        .filter(|decision| {
            decision.status == CandidateDecisionStatus::Eligible
                || (fallback == infer_core::Fallback::AllowLowerCapability
                    && decision.status == CandidateDecisionStatus::FallbackEligible)
        })
        .collect::<Vec<_>>();
    decisions.sort_by_key(|decision| decision.rank.unwrap_or(usize::MAX));
    if fallback == infer_core::Fallback::None {
        decisions.truncate(1);
    }
    let targets = decisions
        .into_iter()
        .filter_map(|decision| {
            candidate_for_deployment(config, &snapshot.intent, &decision.deployment)
        })
        .filter(|candidate| candidate.placement == Placement::Local)
        .collect::<Vec<_>>();
    if targets.is_empty() {
        Err(RuntimeError::NoCandidate)
    } else {
        Ok(targets)
    }
}

fn background_snapshot(snapshot: &JobSnapshot, created_at_ms: i64) -> Value {
    json!({
        "id": snapshot.id,
        "object": "response",
        "created_at": created_at_ms / 1_000,
        "status": match snapshot.state {
            JobState::Queued => "queued",
            JobState::Running => "in_progress",
            JobState::Succeeded => "completed",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Expired => "incomplete",
        },
        "background": true,
        "model": snapshot.intent,
        "output": [],
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "error": snapshot.error,
    })
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
#[path = "background_jobs_tests.rs"]
mod tests;
