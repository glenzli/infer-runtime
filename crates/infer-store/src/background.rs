//! Atomic metadata and recovery ownership for durable background Jobs.

use infer_core::{AttemptOutcome, DurablePayloadRef, JobSnapshot, JobState};
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::{
    AuditEventInput, Store, StoreError, UsageLedgerEntry, now_ms,
    record_audit_event_in_transaction, settle_in_transaction,
};

#[derive(Debug, Clone)]
pub struct RecoverableBackgroundJob {
    pub snapshot: JobSnapshot,
    pub request: DurablePayloadRef,
    pub created_at_ms: i64,
    pub recovery_replays: u8,
}

#[derive(Debug, Clone, Default)]
pub struct BackgroundRecovery {
    pub runnable: Vec<RecoverableBackgroundJob>,
    /// Payloads whose Jobs cannot run again or no longer need their input.
    pub discard: Vec<DurablePayloadRef>,
}

#[derive(Debug, Clone)]
pub struct BackgroundJobPayload {
    pub state: JobState,
    pub result: Option<DurablePayloadRef>,
    pub result_expires_at_ms: Option<i64>,
    pub created_at_ms: i64,
}

impl Store {
    pub fn pending_background_payloads(
        &self,
    ) -> Result<Vec<(String, DurablePayloadRef)>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT jobs.app_id, background.request_ref_json
             FROM durable_background_jobs background
             JOIN jobs ON jobs.id = background.job_id
             WHERE jobs.state IN ('queued', 'running')
               AND background.request_ref_json IS NOT NULL",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(app_id, reference)| Ok((app_id, serde_json::from_str(&reference)?)))
            .collect()
    }

    pub fn background_payload_refs(&self) -> Result<Vec<DurablePayloadRef>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT request_ref_json, result_ref_json FROM durable_background_jobs")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut references = Vec::new();
        for (request, result) in rows {
            for reference in [request, result].into_iter().flatten() {
                references.push(serde_json::from_str(&reference)?);
            }
        }
        Ok(references)
    }

    pub fn pending_background_jobs(&self) -> Result<usize, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT COUNT(*)
                 FROM durable_background_jobs background
                 JOIN jobs ON jobs.id = background.job_id
                 WHERE jobs.state IN ('queued', 'running')
                   AND background.request_ref_json IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(StoreError::from)
    }

    pub fn persist_background_job_with_event(
        &self,
        snapshot: &JobSnapshot,
        request: &DurablePayloadRef,
        event: AuditEventInput,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        let timestamp = now_ms();
        transaction.execute(
            "INSERT INTO durable_background_jobs(
                job_id, request_ref_json, result_ref_json, recovery_replays,
                result_expires_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, NULL, 0, NULL, ?3, ?3)",
            params![snapshot.id, serde_json::to_string(request)?, timestamp],
        )?;
        record_audit_event_in_transaction(&transaction, &snapshot.id, event)?;
        transaction.commit()?;
        Ok(())
    }

    /// Publishes a background result and terminal Job state in one metadata
    /// transaction. The encrypted blob is atomically written before this call.
    pub fn complete_background_job(
        &self,
        snapshot: &JobSnapshot,
        result: &DurablePayloadRef,
        result_expires_at_ms: i64,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        let changed = transaction.execute(
            "UPDATE durable_background_jobs
             SET request_ref_json = NULL, result_ref_json = ?2,
                 result_expires_at_ms = ?3, updated_at_ms = ?4
             WHERE job_id = ?1",
            params![
                snapshot.id,
                serde_json::to_string(result)?,
                result_expires_at_ms,
                now_ms(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::MissingBackgroundJob(snapshot.id.clone()));
        }
        record_audit_event_in_transaction(
            &transaction,
            &snapshot.id,
            AuditEventInput {
                kind: "background.result_published".into(),
                details: json!({
                    "digest": result.digest,
                    "plaintext_bytes": result.plaintext_bytes,
                    "expires_at_ms": result_expires_at_ms,
                }),
            },
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn background_job_payload(
        &self,
        app_id: &str,
        job_id: &str,
    ) -> Result<Option<BackgroundJobPayload>, StoreError> {
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT jobs.state, background.result_ref_json,
                        background.result_expires_at_ms, background.created_at_ms
                 FROM durable_background_jobs background
                 JOIN jobs ON jobs.id = background.job_id
                 WHERE background.job_id = ?1 AND jobs.app_id = ?2",
                params![job_id, app_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(state, result, result_expires_at_ms, created_at_ms)| {
            Ok(BackgroundJobPayload {
                state: serde_json::from_value(serde_json::Value::String(state))?,
                result: result
                    .map(|result| serde_json::from_str(&result))
                    .transpose()?,
                result_expires_at_ms,
                created_at_ms,
            })
        })
        .transpose()
    }

    pub fn retire_background_request(
        &self,
        job_id: &str,
    ) -> Result<Option<DurablePayloadRef>, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let request = transaction
            .query_row(
                "SELECT request_ref_json FROM durable_background_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(|request| serde_json::from_str(&request))
            .transpose()?;
        transaction.execute(
            "UPDATE durable_background_jobs
             SET request_ref_json = NULL, updated_at_ms = ?2 WHERE job_id = ?1",
            params![job_id, now_ms()],
        )?;
        transaction.commit()?;
        Ok(request)
    }

    /// Clears terminal inputs and expired results transactionally, returning
    /// only encrypted references for best-effort filesystem deletion.
    pub fn expire_background_payloads(
        &self,
        timestamp_ms: i64,
    ) -> Result<Vec<DurablePayloadRef>, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let candidates = transaction
            .prepare(
                "SELECT background.job_id, background.request_ref_json,
                        background.result_ref_json, jobs.state,
                        background.result_expires_at_ms
                 FROM durable_background_jobs background
                 JOIN jobs ON jobs.id = background.job_id
                 WHERE (background.request_ref_json IS NOT NULL
                        AND jobs.state IN ('succeeded', 'failed', 'cancelled', 'expired'))
                    OR (background.result_ref_json IS NOT NULL
                        AND background.result_expires_at_ms <= ?1)",
            )?
            .query_map(params![timestamp_ms], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut expired = Vec::new();
        for (job_id, request, result, state, result_expires_at_ms) in candidates {
            let terminal = matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "expired"
            );
            let result_expired = result_expires_at_ms.is_some_and(|expiry| expiry <= timestamp_ms);
            if terminal && let Some(request) = request {
                expired.push(serde_json::from_str(&request)?);
            }
            if result_expired && let Some(result) = result {
                expired.push(serde_json::from_str(&result)?);
            }
            transaction.execute(
                "UPDATE durable_background_jobs
                 SET request_ref_json = CASE WHEN ?2 THEN NULL ELSE request_ref_json END,
                     result_ref_json = CASE WHEN ?3 THEN NULL ELSE result_ref_json END,
                     result_expires_at_ms = CASE WHEN ?3 THEN NULL ELSE result_expires_at_ms END,
                     updated_at_ms = ?4
                 WHERE job_id = ?1",
                params![job_id, terminal, result_expired, timestamp_ms],
            )?;
        }
        transaction.commit()?;
        Ok(expired)
    }

    /// Requeues durable local work while preserving its Job ID and Attempt
    /// history. Unknown provider outcomes become interrupted before replay.
    pub fn recover_background_jobs(
        &self,
        max_recovery_replays: Option<u8>,
    ) -> Result<BackgroundRecovery, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let rows = transaction
            .prepare(
                "SELECT jobs.id, jobs.snapshot_json, jobs.config_fingerprint,
                        jobs.created_at_ms, background.request_ref_json,
                        background.recovery_replays
                 FROM durable_background_jobs background
                 JOIN jobs ON jobs.id = background.job_id
                 WHERE jobs.state IN ('queued', 'running')
                   AND background.request_ref_json IS NOT NULL",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut recovery = BackgroundRecovery::default();
        for (job_id, snapshot_json, fingerprint, created_at_ms, request_json, replay_count) in rows
        {
            let request: DurablePayloadRef = serde_json::from_str(&request_json)?;
            let mut snapshot: JobSnapshot = serde_json::from_str(&snapshot_json)?;
            let rejection = match max_recovery_replays {
                None => Some("durable background execution is disabled after restart"),
                Some(_) if fingerprint != self.config.fingerprint => {
                    Some("background Job config changed while daemon was stopped")
                }
                Some(maximum) if replay_count >= i64::from(maximum) => {
                    Some("background Job exceeded its recovery replay limit")
                }
                Some(_) => None,
            };
            if let Some(message) = rejection {
                snapshot.state = JobState::Failed;
                snapshot.error = Some(message.into());
                self.persist_job_in_transaction(&transaction, &snapshot)?;
                transaction.execute(
                    "UPDATE durable_background_jobs
                     SET request_ref_json = NULL, updated_at_ms = ?2 WHERE job_id = ?1",
                    params![job_id, now_ms()],
                )?;
                record_audit_event_in_transaction(
                    &transaction,
                    &job_id,
                    AuditEventInput {
                        kind: "background.recovery_rejected".into(),
                        details: json!({"reason": message}),
                    },
                )?;
                recovery.discard.push(request);
                continue;
            }

            for attempt in &mut snapshot.attempts {
                if attempt.outcome == AttemptOutcome::Running {
                    attempt.outcome = AttemptOutcome::Interrupted;
                    attempt.error_kind = Some("interrupted".into());
                    attempt.error =
                        Some("daemon restarted during local background execution".into());
                }
            }
            let reservations = transaction
                .prepare(
                    "SELECT attempt_number, app_id, provider, deployment, amount_usd
                     FROM reservations WHERE job_id = ?1 AND state = 'reserved'",
                )?
                .query_map(params![job_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (attempt_number, app_id, provider, deployment, amount_usd) in reservations {
                settle_in_transaction(
                    &transaction,
                    UsageLedgerEntry {
                        job_id: job_id.clone(),
                        attempt_number,
                        app_id,
                        provider,
                        deployment,
                        outcome: "interrupted".into(),
                        amount_usd,
                        estimated: true,
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                )?;
            }
            snapshot.state = JobState::Queued;
            snapshot.error = None;
            self.persist_job_in_transaction(&transaction, &snapshot)?;
            transaction.execute(
                "UPDATE durable_background_jobs
                 SET recovery_replays = recovery_replays + 1, updated_at_ms = ?2
                 WHERE job_id = ?1",
                params![job_id, now_ms()],
            )?;
            record_audit_event_in_transaction(
                &transaction,
                &job_id,
                AuditEventInput {
                    kind: "background.requeued_after_restart".into(),
                    details: json!({"recovery_replay": replay_count + 1}),
                },
            )?;
            recovery.runnable.push(RecoverableBackgroundJob {
                snapshot,
                request,
                created_at_ms,
                recovery_replays: (replay_count + 1) as u8,
            });
        }
        transaction.commit()?;
        Ok(recovery)
    }
}

#[cfg(test)]
#[path = "background_recovery_tests.rs"]
mod recovery_soak_tests;

#[cfg(test)]
mod tests {
    use infer_core::{
        AttemptSnapshot, AttemptTrigger, CandidateDecision, CandidateDecisionStatus,
        CapabilityLevel, EvaluationStatus, Placement, Priority, RequestConstraints, ResourceClass,
        RoutingDecision,
    };

    use super::*;
    use crate::{AttemptReservation, ConfigSnapshot};

    fn store() -> Store {
        Store::open_in_memory(ConfigSnapshot::from_serializable(&json!({"version": 1})).unwrap())
            .unwrap()
    }

    fn snapshot(state: JobState, outcome: AttemptOutcome) -> JobSnapshot {
        JobSnapshot {
            id: "resp_background".into(),
            app_id: "test-app".into(),
            intent: "text.summarize".into(),
            provider: "local".into(),
            deployment: "small".into(),
            model_profile: "qwen".into(),
            model_build: "qwen_small".into(),
            physical_model: "qwen:2b".into(),
            placement: Placement::Local,
            capability_level: CapabilityLevel::Foundational,
            evaluation_status: EvaluationStatus::Benchmarked,
            resource_class: ResourceClass::Light,
            state,
            policy: "balanced".into(),
            priority: Priority::Background,
            constraints: RequestConstraints::default(),
            routing: RoutingDecision {
                capability_floor: CapabilityLevel::Foundational,
                candidates: vec![CandidateDecision {
                    deployment: "small".into(),
                    provider: "local".into(),
                    status: CandidateDecisionStatus::Eligible,
                    rank: Some(1),
                    reason_codes: Vec::new(),
                }],
            },
            attempts: vec![AttemptSnapshot {
                number: 1,
                provider: "local".into(),
                deployment: "small".into(),
                outcome,
                trigger: AttemptTrigger::Initial,
                error_kind: None,
                error: None,
            }],
            error: None,
        }
    }

    fn payload(kind: infer_core::DurablePayloadKind, id: &str) -> DurablePayloadRef {
        DurablePayloadRef {
            blob_id: id.into(),
            kind,
            digest: "hmac-sha256:test".into(),
            plaintext_bytes: 20,
        }
    }

    #[test]
    fn durable_recovery_interrupts_attempt_settles_reservation_and_requeues() {
        let store = store();
        let snapshot = snapshot(JobState::Running, AttemptOutcome::Running);
        let request = payload(
            infer_core::DurablePayloadKind::ResponsesRequest,
            "pay_00000000000000000000000000000001",
        );
        store
            .persist_background_job_with_event(
                &snapshot,
                &request,
                AuditEventInput {
                    kind: "job.admitted".into(),
                    details: json!({}),
                },
            )
            .unwrap();
        store
            .reserve_attempt(&AttemptReservation {
                job_id: snapshot.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "local".into(),
                deployment: "small".into(),
                amount_usd: 0.0,
                estimated_tokens: 10,
            })
            .unwrap();

        assert_eq!(store.recover_interrupted().unwrap().jobs_failed, 0);
        let recovery = store.recover_background_jobs(Some(2)).unwrap();
        assert_eq!(recovery.runnable.len(), 1);
        assert_eq!(recovery.runnable[0].snapshot.state, JobState::Queued);
        assert_eq!(
            recovery.runnable[0].snapshot.attempts[0].outcome,
            AttemptOutcome::Interrupted
        );
        assert_eq!(recovery.runnable[0].recovery_replays, 1);
        assert_eq!(store.usage_entries().unwrap()[0].outcome, "interrupted");
    }

    #[test]
    fn result_publication_is_atomic_and_expiry_returns_only_blob_references() {
        let store = store();
        let mut snapshot = snapshot(JobState::Running, AttemptOutcome::Succeeded);
        let request = payload(
            infer_core::DurablePayloadKind::ResponsesRequest,
            "pay_00000000000000000000000000000002",
        );
        store
            .persist_background_job_with_event(
                &snapshot,
                &request,
                AuditEventInput {
                    kind: "job.admitted".into(),
                    details: json!({}),
                },
            )
            .unwrap();
        snapshot.state = JobState::Succeeded;
        let result = payload(
            infer_core::DurablePayloadKind::ResponsesResult,
            "pay_00000000000000000000000000000003",
        );
        store
            .complete_background_job(&snapshot, &result, 100)
            .unwrap();
        let published = store
            .background_job_payload("test-app", &snapshot.id)
            .unwrap()
            .unwrap();
        assert_eq!(published.state, JobState::Succeeded);
        assert_eq!(published.result, Some(result.clone()));
        assert_eq!(store.pending_background_jobs().unwrap(), 0);
        let expired = store.expire_background_payloads(100).unwrap();
        assert_eq!(expired, vec![result]);
        assert!(
            store
                .background_job_payload("other", &snapshot.id)
                .unwrap()
                .is_none()
        );
    }
}
