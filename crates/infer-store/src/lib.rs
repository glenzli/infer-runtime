//! SQLite persistence for immutable configuration snapshots, Jobs, Attempts,
//! reservations, and usage ledger entries.

mod background;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use infer_core::{
    AttemptOutcome, JobListItem, JobListPage, JobPageCursor, JobSnapshot, JobState, Priority,
    QuotaConfig, QuotaLimitConfig,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 6;
const RATE_WINDOW_MS: i64 = 60_000;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("store mutex poisoned")]
    Poisoned,
    #[error("expected a string enum value in persisted metadata")]
    NonStringEnum,
    #[error("reservation amount must be finite and non-negative")]
    InvalidReservationAmount,
    #[error("reservation token estimate exceeds SQLite integer range")]
    InvalidReservationTokens,
    #[error("durable background Job `{0}` is missing")]
    MissingBackgroundJob(String),
    #[error("quota exceeded for {scope} {resource}")]
    QuotaExceeded {
        scope: String,
        resource: QuotaResource,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaResource {
    Usd,
    RequestsPerMinute,
    TokensPerMinute,
    ConcurrentAttempts,
}

impl std::fmt::Display for QuotaResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Usd => "USD budget",
            Self::RequestsPerMinute => "requests per minute",
            Self::TokensPerMinute => "tokens per minute",
            Self::ConcurrentAttempts => "concurrent attempts",
        })
    }
}

#[derive(Clone, Copy)]
enum QuotaScope<'a> {
    Global,
    Provider(&'a str),
    App(&'a str),
}

impl<'a> QuotaScope<'a> {
    fn filters(self) -> (Option<&'a str>, Option<&'a str>) {
        match self {
            Self::Global => (None, None),
            Self::Provider(provider) => (None, Some(provider)),
            Self::App(app) => (Some(app), None),
        }
    }

    fn name(self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Provider(provider) => format!("provider:{provider}"),
            Self::App(app) => format!("app:{app}"),
        }
    }
}

/// The persistence owner translates the version-controlled core config into
/// its transaction-local limits. This prevents the control plane from doing
/// race-prone prechecks before it writes a reservation.
#[derive(Debug, Clone, Default)]
pub struct QuotaLimits {
    pub global: QuotaLimitConfig,
    pub providers: BTreeMap<String, QuotaLimitConfig>,
    pub apps: BTreeMap<String, QuotaLimitConfig>,
}

impl From<&QuotaConfig> for QuotaLimits {
    fn from(config: &QuotaConfig) -> Self {
        Self {
            global: config.global.clone(),
            providers: config.providers.clone(),
            apps: config.apps.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigSnapshot {
    pub fingerprint: String,
    pub contents_json: String,
}

impl ConfigSnapshot {
    pub fn from_serializable(config: &impl Serialize) -> Result<Self, StoreError> {
        let contents_json = serde_json::to_string(config)?;
        let mut digest = Sha256::new();
        digest.update(contents_json.as_bytes());
        let fingerprint = format!("sha256:{:x}", digest.finalize());
        Ok(Self {
            fingerprint,
            contents_json,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLedgerEntry {
    pub job_id: String,
    pub attempt_number: usize,
    pub app_id: String,
    pub provider: String,
    pub deployment: String,
    pub outcome: String,
    pub amount_usd: f64,
    /// True when the provider did not return usage or this deployment has no
    /// configured token pricing, so the ledger uses the admission estimate.
    pub estimated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveReservation {
    pub job_id: String,
    pub attempt_number: usize,
    pub app_id: String,
    pub provider: String,
    pub deployment: String,
    pub amount_usd: f64,
    pub estimated_tokens: u64,
}

/// Bounded accounting projection for infrastructure observation. It is
/// computed in SQLite and never materializes or exposes per-Job ledger rows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AccountingSummary {
    pub settled_usd: f64,
    pub settled_entries: u64,
    pub reserved_usd: f64,
    pub active_reservations: u64,
}

/// All data needed to atomically admit one Attempt into the accounting store.
/// Keeping it together makes future reservation dimensions additive without
/// repeatedly widening Store's public method signature.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptReservation {
    pub job_id: String,
    pub attempt_number: usize,
    pub app_id: String,
    pub provider: String,
    pub deployment: String,
    pub amount_usd: f64,
    pub estimated_tokens: u64,
}

/// A payload-free lifecycle or routing record attached to a durable Job.
/// Details may include identifiers, policy codes, and counters but never a
/// request body, uploaded file, credential, or raw upstream error body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: i64,
    pub job_id: String,
    pub kind: String,
    pub details: serde_json::Value,
    pub recorded_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct AuditEventInput {
    pub kind: String,
    pub details: serde_json::Value,
}

/// A durable control-plane event that is not attached to a Job. Resource
/// lifecycle actions need their own table because inventing a synthetic Job
/// would corrupt both domains' identity and foreign-key semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceAuditEvent {
    pub id: i64,
    pub actor: String,
    pub kind: String,
    pub deployment: String,
    pub config_fingerprint: String,
    pub details: serde_json::Value,
    pub recorded_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ResourceAuditEventInput {
    pub actor: String,
    pub kind: String,
    pub deployment: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedRecovery {
    pub jobs_failed: usize,
    pub attempts_interrupted: usize,
    pub reservations_settled: usize,
}

/// Synchronous SQLite owner. Calls are small metadata transactions; a future
/// high-throughput implementation may put this owner behind a dedicated worker
/// without changing its consistency contract.
pub struct Store {
    connection: Mutex<Connection>,
    config: ConfigSnapshot,
    quota: QuotaLimits,
}

pub use background::{BackgroundJobPayload, BackgroundRecovery, RecoverableBackgroundJob};

impl Store {
    pub fn open(path: impl AsRef<Path>, config: ConfigSnapshot) -> Result<Self, StoreError> {
        Self::open_with_quota(path, config, QuotaLimits::default())
    }

    pub fn open_with_quota(
        path: impl AsRef<Path>,
        config: ConfigSnapshot,
        quota: QuotaLimits,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            connection: Mutex::new(connection),
            config,
            quota,
        };
        store.migrate()?;
        store.record_config_snapshot()?;
        Ok(store)
    }

    pub fn open_in_memory(config: ConfigSnapshot) -> Result<Self, StoreError> {
        Self::open_in_memory_with_quota(config, QuotaLimits::default())
    }

    pub fn open_in_memory_with_quota(
        config: ConfigSnapshot,
        quota: QuotaLimits,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            connection: Mutex::new(connection),
            config,
            quota,
        };
        store.migrate()?;
        store.record_config_snapshot()?;
        Ok(store)
    }

    pub fn config_fingerprint(&self) -> &str {
        &self.config.fingerprint
    }

    pub fn persist_job(&self, snapshot: &JobSnapshot) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        transaction.commit()?;
        Ok(())
    }

    /// Commits a terminal Attempt projection and its ledger settlement as one
    /// transaction. Recovery therefore never has to infer whether a completed
    /// provider result was charged before the daemon stopped.
    pub fn persist_job_and_settle(
        &self,
        snapshot: &JobSnapshot,
        entry: UsageLedgerEntry,
    ) -> Result<(), StoreError> {
        if !entry.amount_usd.is_finite() || entry.amount_usd < 0.0 {
            return Err(StoreError::InvalidReservationAmount);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        settle_in_transaction(&transaction, entry)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn persist_job_with_event(
        &self,
        snapshot: &JobSnapshot,
        event: AuditEventInput,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        record_audit_event_in_transaction(&transaction, &snapshot.id, event)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn persist_job_and_settle_with_event(
        &self,
        snapshot: &JobSnapshot,
        entry: UsageLedgerEntry,
        event: AuditEventInput,
    ) -> Result<(), StoreError> {
        if !entry.amount_usd.is_finite() || entry.amount_usd < 0.0 {
            return Err(StoreError::InvalidReservationAmount);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.persist_job_in_transaction(&transaction, snapshot)?;
        settle_in_transaction(&transaction, entry)?;
        record_audit_event_in_transaction(&transaction, &snapshot.id, event)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_resource_audit_event(
        &self,
        event: ResourceAuditEventInput,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO resource_audit_events(
                actor, kind, deployment, config_fingerprint, details_json, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.actor,
                event.kind,
                event.deployment,
                self.config.fingerprint,
                serde_json::to_string(&event.details)?,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    /// Newest resource events first. The caller supplies a bounded operator
    /// view rather than loading an unbounded control history into memory.
    pub fn resource_audit_events(
        &self,
        limit: usize,
    ) -> Result<Vec<ResourceAuditEvent>, StoreError> {
        let limit = limit.clamp(1, 1_000) as i64;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, actor, kind, deployment, config_fingerprint, details_json,
                    recorded_at_ms
             FROM resource_audit_events ORDER BY id DESC LIMIT ?1",
        )?;
        statement
            .query_map(params![limit], |row| {
                let details: String = row.get(5)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    details,
                    row.get(6)?,
                ))
            })?
            .map(|event| {
                let (id, actor, kind, deployment, config_fingerprint, details, recorded_at_ms) =
                    event?;
                Ok(ResourceAuditEvent {
                    id,
                    actor,
                    kind,
                    deployment,
                    config_fingerprint,
                    details: serde_json::from_str(&details)?,
                    recorded_at_ms,
                })
            })
            .collect()
    }

    fn persist_job_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        snapshot: &JobSnapshot,
    ) -> Result<(), StoreError> {
        let snapshot_json = serde_json::to_string(snapshot)?;
        let updated_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO jobs (
                id, app_id, intent, provider, deployment, state, priority, config_fingerprint,
                snapshot_json, error, created_at_ms, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
            ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                deployment = excluded.deployment,
                state = excluded.state,
                priority = excluded.priority,
                snapshot_json = excluded.snapshot_json,
                error = excluded.error,
                updated_at_ms = excluded.updated_at_ms",
            params![
                snapshot.id,
                snapshot.app_id,
                snapshot.intent,
                snapshot.provider,
                snapshot.deployment,
                enum_code(&snapshot.state)?,
                enum_code(&snapshot.priority)?,
                self.config.fingerprint,
                snapshot_json,
                snapshot.error,
                updated_at_ms,
            ],
        )?;
        transaction.execute(
            "DELETE FROM attempts WHERE job_id = ?1",
            params![snapshot.id],
        )?;
        for attempt in &snapshot.attempts {
            transaction.execute(
                "INSERT INTO attempts (
                    job_id, number, provider, deployment, outcome, trigger,
                    error_kind, error, snapshot_json, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    snapshot.id,
                    attempt.number as i64,
                    attempt.provider,
                    attempt.deployment,
                    enum_code(&attempt.outcome)?,
                    enum_code(&attempt.trigger)?,
                    attempt.error_kind,
                    attempt.error,
                    serde_json::to_string(attempt)?,
                    updated_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    pub fn load_job(&self, job_id: &str) -> Result<Option<JobSnapshot>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT snapshot_json FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|snapshot| serde_json::from_str(&snapshot))
            .transpose()
            .map_err(StoreError::from)
    }

    /// App-scoped keyset pagination over lightweight metadata. The query reads
    /// at most `limit + 1` rows and never deserializes full Job snapshots.
    pub fn job_page(
        &self,
        app_id: &str,
        priority: Option<Priority>,
        state: Option<JobState>,
        cursor: Option<&JobPageCursor>,
        limit: usize,
    ) -> Result<JobListPage, StoreError> {
        let limit = limit.clamp(1, 1_000);
        let mut sql = String::from(
            "SELECT id, app_id, intent, provider, deployment, state, priority,
                    created_at_ms, updated_at_ms
             FROM jobs WHERE app_id = ?",
        );
        let mut parameters = vec![SqlValue::Text(app_id.into())];
        if let Some(priority) = priority {
            sql.push_str(" AND priority = ?");
            parameters.push(SqlValue::Text(enum_code(&priority)?));
        }
        if let Some(state) = state {
            sql.push_str(" AND state = ?");
            parameters.push(SqlValue::Text(enum_code(&state)?));
        }
        if let Some(cursor) = cursor {
            sql.push_str(" AND (created_at_ms < ? OR (created_at_ms = ? AND id < ?))");
            parameters.push(SqlValue::Integer(cursor.created_at_ms));
            parameters.push(SqlValue::Integer(cursor.created_at_ms));
            parameters.push(SqlValue::Text(cursor.id.clone()));
        }
        sql.push_str(" ORDER BY created_at_ms DESC, id DESC LIMIT ?");
        parameters.push(SqlValue::Integer((limit + 1) as i64));

        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit;
        let jobs = rows
            .into_iter()
            .take(limit)
            .map(
                |(
                    id,
                    app_id,
                    intent,
                    provider,
                    deployment,
                    state,
                    priority,
                    created_at_ms,
                    updated_at_ms,
                )| {
                    Ok(JobListItem {
                        id,
                        app_id,
                        intent,
                        provider,
                        deployment,
                        state: serde_json::from_value(serde_json::Value::String(state))?,
                        priority: serde_json::from_value(serde_json::Value::String(priority))?,
                        created_at_ms,
                        updated_at_ms,
                    })
                },
            )
            .collect::<Result<Vec<_>, StoreError>>()?;
        let next_cursor = has_more.then(|| {
            let last = jobs
                .last()
                .expect("a non-empty over-limit page has a last row");
            JobPageCursor {
                created_at_ms: last.created_at_ms,
                id: last.id.clone(),
            }
            .to_string()
        });
        Ok(JobListPage { jobs, next_cursor })
    }

    /// Creates a conservative per-Attempt reservation. Every configured
    /// global/provider/App limit is checked and the row is inserted in the
    /// same SQLite transaction, so concurrent callers cannot oversell a
    /// budget, rate window, or attempt slot.
    pub fn reserve_attempt(&self, reservation: &AttemptReservation) -> Result<(), StoreError> {
        if !reservation.amount_usd.is_finite() || reservation.amount_usd < 0.0 {
            return Err(StoreError::InvalidReservationAmount);
        }
        let estimated_tokens_sql = i64::try_from(reservation.estimated_tokens)
            .map_err(|_| StoreError::InvalidReservationTokens)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.check_quota(
            &transaction,
            QuotaScope::Global,
            &self.quota.global,
            reservation.amount_usd,
            reservation.estimated_tokens,
        )?;
        if let Some(limit) = self.quota.providers.get(&reservation.provider) {
            self.check_quota(
                &transaction,
                QuotaScope::Provider(&reservation.provider),
                limit,
                reservation.amount_usd,
                reservation.estimated_tokens,
            )?;
        }
        if let Some(limit) = self.quota.apps.get(&reservation.app_id) {
            self.check_quota(
                &transaction,
                QuotaScope::App(&reservation.app_id),
                limit,
                reservation.amount_usd,
                reservation.estimated_tokens,
            )?;
        }
        transaction.execute(
            "INSERT INTO reservations (
                job_id, attempt_number, app_id, provider, deployment, amount_usd, estimated_tokens,
                state, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reserved', ?8)",
            params![
                reservation.job_id,
                reservation.attempt_number as i64,
                reservation.app_id,
                reservation.provider,
                reservation.deployment,
                reservation.amount_usd,
                estimated_tokens_sql,
                now_ms(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn settle_attempt(&self, entry: UsageLedgerEntry) -> Result<(), StoreError> {
        if !entry.amount_usd.is_finite() || entry.amount_usd < 0.0 {
            return Err(StoreError::InvalidReservationAmount);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let reservation = transaction
            .query_row(
                "SELECT state FROM reservations WHERE job_id = ?1 AND attempt_number = ?2",
                params![entry.job_id, entry.attempt_number as i64],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if reservation.as_deref() == Some("reserved") {
            transaction.execute(
                "UPDATE reservations SET state = 'settled', settled_amount_usd = ?3, settled_at_ms = ?4
                 WHERE job_id = ?1 AND attempt_number = ?2",
                params![
                    entry.job_id,
                    entry.attempt_number as i64,
                    entry.amount_usd,
                    now_ms(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO usage_ledger (
                    job_id, attempt_number, app_id, provider, deployment, outcome,
                    amount_usd, estimated, input_tokens, output_tokens, total_tokens,
                    entry_json, recorded_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    entry.job_id,
                    entry.attempt_number as i64,
                    entry.app_id,
                    entry.provider,
                    entry.deployment,
                    entry.outcome,
                    entry.amount_usd,
                    entry.estimated as i64,
                    entry.input_tokens.map(|value| value as i64),
                    entry.output_tokens.map(|value| value as i64),
                    entry.total_tokens.map(|value| value as i64),
                    serde_json::to_string(&entry)?,
                    now_ms(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Resolves records left mid-flight by a prior process. No provider call is
    /// replayed. An outstanding reservation is conservatively charged at its
    /// estimate, making the ledger finite and preventing double reservation.
    pub fn recover_interrupted(&self) -> Result<InterruptedRecovery, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut job_statement = transaction.prepare(
            "SELECT id, snapshot_json FROM jobs
                 WHERE state IN ('queued', 'running')
                   AND NOT EXISTS (
                       SELECT 1 FROM durable_background_jobs background
                       WHERE background.job_id = jobs.id
                   )",
        )?;
        let interrupted_jobs = job_statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(job_statement);
        let mut attempts_interrupted = 0;
        for (job_id, snapshot_json) in &interrupted_jobs {
            let mut snapshot: JobSnapshot = serde_json::from_str(snapshot_json)?;
            snapshot.state = JobState::Failed;
            snapshot.error = Some("daemon restarted; execution result unknown".into());
            transaction.execute(
                "UPDATE jobs SET state = 'failed', error = ?2, snapshot_json = ?3, updated_at_ms = ?4
                 WHERE id = ?1",
                params![
                    job_id,
                    snapshot.error,
                    serde_json::to_string(&snapshot)?,
                    now_ms(),
                ],
            )?;
            for attempt in &mut snapshot.attempts {
                if attempt.outcome == AttemptOutcome::Running {
                    attempt.outcome = AttemptOutcome::Interrupted;
                    attempt.error_kind = Some("interrupted".into());
                    attempt.error = Some("daemon restarted; execution result unknown".into());
                    transaction.execute(
                        "UPDATE attempts SET outcome = ?3, error_kind = ?4, error = ?5,
                         snapshot_json = ?6, updated_at_ms = ?7
                         WHERE job_id = ?1 AND number = ?2",
                        params![
                            job_id,
                            attempt.number as i64,
                            enum_code(&attempt.outcome)?,
                            attempt.error_kind,
                            attempt.error,
                            serde_json::to_string(attempt)?,
                            now_ms(),
                        ],
                    )?;
                    attempts_interrupted += 1;
                }
            }
            transaction.execute(
                "UPDATE jobs SET snapshot_json = ?2 WHERE id = ?1",
                params![job_id, serde_json::to_string(&snapshot)?],
            )?;
        }
        let mut statement = transaction.prepare(
            "SELECT job_id, attempt_number, app_id, provider, deployment, amount_usd
             FROM reservations WHERE state = 'reserved'",
        )?;
        let pending = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (job_id, attempt_number, app_id, provider, deployment, amount_usd) in &pending {
            settle_in_transaction(
                &transaction,
                UsageLedgerEntry {
                    job_id: job_id.clone(),
                    attempt_number: *attempt_number,
                    app_id: app_id.clone(),
                    provider: provider.clone(),
                    deployment: deployment.clone(),
                    outcome: "interrupted".into(),
                    amount_usd: *amount_usd,
                    estimated: true,
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                },
            )?;
        }
        transaction.commit()?;
        Ok(InterruptedRecovery {
            jobs_failed: interrupted_jobs.len(),
            attempts_interrupted,
            reservations_settled: pending.len(),
        })
    }

    pub fn usage_entries(&self) -> Result<Vec<UsageLedgerEntry>, StoreError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT entry_json FROM usage_ledger ORDER BY id")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|entry| serde_json::from_str(&entry?).map_err(StoreError::from))
            .collect()
    }

    pub fn active_reservations(&self) -> Result<Vec<ActiveReservation>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT job_id, attempt_number, app_id, provider, deployment, amount_usd, estimated_tokens
             FROM reservations WHERE state = 'reserved' ORDER BY created_at_ms, job_id, attempt_number",
        )?;
        statement
            .query_map([], |row| {
                Ok(ActiveReservation {
                    job_id: row.get(0)?,
                    attempt_number: row.get::<_, i64>(1)? as usize,
                    app_id: row.get(2)?,
                    provider: row.get(3)?,
                    deployment: row.get(4)?,
                    amount_usd: row.get(5)?,
                    estimated_tokens: row.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn accounting_summary(&self) -> Result<AccountingSummary, StoreError> {
        let connection = self.connection()?;
        let (settled_usd, settled_entries) = connection.query_row(
            "SELECT COALESCE(SUM(amount_usd), 0), COUNT(*) FROM usage_ledger",
            [],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let (reserved_usd, active_reservations) = connection.query_row(
            "SELECT COALESCE(SUM(amount_usd), 0), COUNT(*)
             FROM reservations WHERE state = 'reserved'",
            [],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(AccountingSummary {
            settled_usd,
            settled_entries: settled_entries.max(0) as u64,
            reserved_usd,
            active_reservations: active_reservations.max(0) as u64,
        })
    }

    pub fn audit_events(&self, job_id: &str) -> Result<Vec<AuditEvent>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, job_id, kind, details_json, recorded_at_ms
             FROM audit_events WHERE job_id = ?1 ORDER BY id",
        )?;
        statement
            .query_map(params![job_id], |row| {
                let details_json: String = row.get(3)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    details_json,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .map(|event| {
                let (id, job_id, kind, details_json, recorded_at_ms) = event?;
                Ok(AuditEvent {
                    id,
                    job_id,
                    kind,
                    details: serde_json::from_str(&details_json)?,
                    recorded_at_ms,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()
    }

    fn check_quota(
        &self,
        transaction: &Transaction<'_>,
        scope: QuotaScope<'_>,
        limit: &QuotaLimitConfig,
        amount_usd: f64,
        estimated_tokens: u64,
    ) -> Result<(), StoreError> {
        let (app_id, provider) = scope.filters();
        if let Some(max_usd) = limit.max_usd {
            let settled: f64 = transaction.query_row(
                "SELECT COALESCE(SUM(amount_usd), 0) FROM usage_ledger
                 WHERE (?1 IS NULL OR app_id = ?1) AND (?2 IS NULL OR provider = ?2)",
                params![app_id, provider],
                |row| row.get(0),
            )?;
            let reserved: f64 = transaction.query_row(
                "SELECT COALESCE(SUM(amount_usd), 0) FROM reservations
                 WHERE state = 'reserved'
                   AND (?1 IS NULL OR app_id = ?1)
                   AND (?2 IS NULL OR provider = ?2)",
                params![app_id, provider],
                |row| row.get(0),
            )?;
            if settled + reserved + amount_usd > max_usd {
                return Err(StoreError::QuotaExceeded {
                    scope: scope.name(),
                    resource: QuotaResource::Usd,
                });
            }
        }
        let cutoff = now_ms() - RATE_WINDOW_MS;
        if let Some(max_requests) = limit.requests_per_minute {
            let recent: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM reservations
                 WHERE created_at_ms >= ?1
                   AND (?2 IS NULL OR app_id = ?2)
                   AND (?3 IS NULL OR provider = ?3)",
                params![cutoff, app_id, provider],
                |row| row.get(0),
            )?;
            if recent as u64 >= max_requests {
                return Err(StoreError::QuotaExceeded {
                    scope: scope.name(),
                    resource: QuotaResource::RequestsPerMinute,
                });
            }
        }
        if let Some(max_tokens) = limit.tokens_per_minute {
            let recent: i64 = transaction.query_row(
                "SELECT COALESCE(SUM(estimated_tokens), 0) FROM reservations
                 WHERE created_at_ms >= ?1
                   AND (?2 IS NULL OR app_id = ?2)
                   AND (?3 IS NULL OR provider = ?3)",
                params![cutoff, app_id, provider],
                |row| row.get(0),
            )?;
            if recent.max(0) as u64 + estimated_tokens > max_tokens {
                return Err(StoreError::QuotaExceeded {
                    scope: scope.name(),
                    resource: QuotaResource::TokensPerMinute,
                });
            }
        }
        if let Some(max_concurrent) = limit.max_concurrent_attempts {
            let outstanding: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM reservations
                 WHERE state = 'reserved'
                   AND (?1 IS NULL OR app_id = ?1)
                   AND (?2 IS NULL OR provider = ?2)",
                params![app_id, provider],
                |row| row.get(0),
            )?;
            if outstanding as usize >= max_concurrent {
                return Err(StoreError::QuotaExceeded {
                    scope: scope.name(),
                    resource: QuotaResource::ConcurrentAttempts,
                });
            }
        }
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::Poisoned)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS config_snapshots (
                fingerprint TEXT PRIMARY KEY,
                contents_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                app_id TEXT NOT NULL,
                intent TEXT NOT NULL,
                provider TEXT NOT NULL,
                deployment TEXT NOT NULL,
                state TEXT NOT NULL,
                priority TEXT NOT NULL DEFAULT 'normal',
                config_fingerprint TEXT NOT NULL REFERENCES config_snapshots(fingerprint),
                snapshot_json TEXT NOT NULL,
                error TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS attempts (
                job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                number INTEGER NOT NULL,
                provider TEXT NOT NULL,
                deployment TEXT NOT NULL,
                outcome TEXT NOT NULL,
                trigger TEXT NOT NULL,
                error_kind TEXT,
                error TEXT,
                snapshot_json TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (job_id, number)
             );
             CREATE TABLE IF NOT EXISTS reservations (
                job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                attempt_number INTEGER NOT NULL,
                app_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                deployment TEXT NOT NULL,
                amount_usd REAL NOT NULL,
                estimated_tokens INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                settled_amount_usd REAL,
                settled_at_ms INTEGER,
                PRIMARY KEY (job_id, attempt_number)
             );
             CREATE TABLE IF NOT EXISTS usage_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id TEXT NOT NULL,
                attempt_number INTEGER NOT NULL,
                app_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                deployment TEXT NOT NULL,
                outcome TEXT NOT NULL,
                amount_usd REAL NOT NULL,
                estimated INTEGER NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                entry_json TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL,
                UNIQUE (job_id, attempt_number)
             );
             CREATE TABLE IF NOT EXISTS audit_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                details_json TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS resource_audit_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                actor TEXT NOT NULL,
                kind TEXT NOT NULL,
                deployment TEXT NOT NULL,
                config_fingerprint TEXT NOT NULL REFERENCES config_snapshots(fingerprint),
                details_json TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS durable_background_jobs (
                job_id TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
                request_ref_json TEXT,
                result_ref_json TEXT,
                recovery_replays INTEGER NOT NULL DEFAULT 0,
                result_expires_at_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
             );",
        )?;
        let has_estimated_tokens = transaction
            .prepare("PRAGMA table_info(reservations)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "estimated_tokens");
        if !has_estimated_tokens {
            transaction.execute_batch(
                "ALTER TABLE reservations ADD COLUMN estimated_tokens INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        let has_priority = transaction
            .prepare("PRAGMA table_info(jobs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "priority");
        if !has_priority {
            transaction.execute_batch(
                "ALTER TABLE jobs ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'",
            )?;
            let persisted = transaction
                .prepare("SELECT id, snapshot_json FROM jobs")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, snapshot_json) in persisted {
                if let Ok(snapshot) = serde_json::from_str::<JobSnapshot>(&snapshot_json) {
                    transaction.execute(
                        "UPDATE jobs SET priority = ?2 WHERE id = ?1",
                        params![id, enum_code(&snapshot.priority)?],
                    )?;
                }
            }
        }
        transaction.execute_batch(
            "CREATE INDEX IF NOT EXISTS jobs_app_created_id
                 ON jobs(app_id, created_at_ms DESC, id DESC);
             CREATE INDEX IF NOT EXISTS jobs_app_state_priority_created_id
                 ON jobs(app_id, state, priority, created_at_ms DESC, id DESC);",
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO schema_migrations(version) VALUES (?1)",
            params![SCHEMA_VERSION],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn record_config_snapshot(&self) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR IGNORE INTO config_snapshots(fingerprint, contents_json, created_at_ms)
             VALUES (?1, ?2, ?3)",
            params![self.config.fingerprint, self.config.contents_json, now_ms(),],
        )?;
        Ok(())
    }
}

fn enum_code(value: &impl Serialize) -> Result<String, StoreError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(StoreError::NonStringEnum),
    }
}

fn settle_in_transaction(
    transaction: &Transaction<'_>,
    entry: UsageLedgerEntry,
) -> Result<(), StoreError> {
    let changed = transaction.execute(
        "UPDATE reservations SET state = 'settled', settled_amount_usd = ?3, settled_at_ms = ?4
         WHERE job_id = ?1 AND attempt_number = ?2 AND state = 'reserved'",
        params![
            entry.job_id,
            entry.attempt_number as i64,
            entry.amount_usd,
            now_ms(),
        ],
    )?;
    // An admission rejection has no reservation and therefore must never
    // manufacture a charge. The same rule makes a duplicate terminal update
    // idempotent.
    if changed == 0 {
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO usage_ledger (
            job_id, attempt_number, app_id, provider, deployment, outcome,
            amount_usd, estimated, input_tokens, output_tokens, total_tokens,
            entry_json, recorded_at_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            entry.job_id,
            entry.attempt_number as i64,
            entry.app_id,
            entry.provider,
            entry.deployment,
            entry.outcome,
            entry.amount_usd,
            entry.estimated as i64,
            entry.input_tokens.map(|value| value as i64),
            entry.output_tokens.map(|value| value as i64),
            entry.total_tokens.map(|value| value as i64),
            serde_json::to_string(&entry)?,
            now_ms(),
        ],
    )?;
    Ok(())
}

fn record_audit_event_in_transaction(
    transaction: &Transaction<'_>,
    job_id: &str,
    event: AuditEventInput,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO audit_events(job_id, kind, details_json, recorded_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            job_id,
            event.kind,
            serde_json::to_string(&event.details)?,
            now_ms(),
        ],
    )?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock moved before Unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Barrier},
        thread,
    };

    use infer_core::{
        AttemptOutcome, AttemptSnapshot, AttemptTrigger, CandidateDecision,
        CandidateDecisionStatus, CapabilityLevel, EvaluationStatus, JobState, Placement, Priority,
        QuotaLimitConfig, RequestConstraints, ResourceClass, RoutingDecision,
    };
    use serde_json::json;

    use super::*;

    fn config() -> ConfigSnapshot {
        ConfigSnapshot::from_serializable(&json!({"version": 1, "policy": "balanced"})).unwrap()
    }

    fn job(state: JobState, outcome: AttemptOutcome) -> JobSnapshot {
        JobSnapshot {
            id: "resp_test".into(),
            app_id: "test-app".into(),
            intent: "text.summarize".into(),
            consumer_core_contract: infer_core::CONSUMER_CORE_CONTRACT.into(),
            capability_contract: Some("infer.responses@20260812.1".into()),
            provider: "cloud".into(),
            deployment: "deepseek_flash".into(),
            model_profile: "deepseek".into(),
            model_build: "deepseek_flash".into(),
            physical_model: "deepseek-v4-flash".into(),
            placement: Placement::Cloud,
            capability_level: CapabilityLevel::Capable,
            evaluation_status: EvaluationStatus::Provisional,
            resource_class: ResourceClass::Standard,
            state,
            policy: "balanced".into(),
            priority: Priority::Normal,
            constraints: RequestConstraints::default(),
            routing: RoutingDecision {
                capability_floor: CapabilityLevel::Foundational,
                named_route: None,
                candidates: vec![CandidateDecision {
                    deployment: "deepseek_flash".into(),
                    provider: "cloud".into(),
                    status: CandidateDecisionStatus::Eligible,
                    rank: Some(1),
                    reason_codes: vec![],
                }],
            },
            attempts: vec![AttemptSnapshot {
                number: 1,
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                outcome,
                trigger: AttemptTrigger::Initial,
                error_kind: None,
                error: None,
            }],
            error: None,
        }
    }

    fn reservation(
        job_id: String,
        attempt_number: usize,
        amount_usd: f64,
        tokens: u64,
    ) -> AttemptReservation {
        AttemptReservation {
            job_id,
            attempt_number,
            app_id: "test-app".into(),
            provider: "cloud".into(),
            deployment: "deepseek_flash".into(),
            amount_usd,
            estimated_tokens: tokens,
        }
    }

    fn quota_store(global: QuotaLimitConfig) -> Store {
        Store::open_in_memory_with_quota(
            config(),
            QuotaLimits {
                global,
                ..QuotaLimits::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn persists_a_config_bound_job_attempt_and_usage_entry() {
        let store = Store::open_in_memory(config()).unwrap();
        let snapshot = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        store.persist_job(&snapshot).unwrap();
        store
            .reserve_attempt(&AttemptReservation {
                job_id: snapshot.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                amount_usd: 0.05,
                estimated_tokens: 30,
            })
            .unwrap();
        store
            .settle_attempt(UsageLedgerEntry {
                job_id: snapshot.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                outcome: "succeeded".into(),
                amount_usd: 0.03,
                estimated: false,
                input_tokens: Some(10),
                output_tokens: Some(20),
                total_tokens: Some(30),
            })
            .unwrap();
        let restored = store.load_job(&snapshot.id).unwrap().unwrap();
        assert_eq!(restored.state, JobState::Succeeded);
        assert_eq!(restored.attempts[0].outcome, AttemptOutcome::Succeeded);
        assert_eq!(store.usage_entries().unwrap()[0].total_tokens, Some(30));
        assert!(!store.usage_entries().unwrap()[0].estimated);
    }

    #[test]
    fn observer_accounting_summary_aggregates_without_returning_ledger_identity() {
        let store = Store::open_in_memory(config()).unwrap();
        let first = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        store.persist_job(&first).unwrap();
        store
            .reserve_attempt(&reservation(first.id.clone(), 1, 0.05, 30))
            .unwrap();
        assert_eq!(
            store.accounting_summary().unwrap(),
            AccountingSummary {
                settled_usd: 0.0,
                settled_entries: 0,
                reserved_usd: 0.05,
                active_reservations: 1,
            }
        );
        store
            .settle_attempt(UsageLedgerEntry {
                job_id: first.id,
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                outcome: "succeeded".into(),
                amount_usd: 0.03,
                estimated: false,
                input_tokens: Some(10),
                output_tokens: Some(20),
                total_tokens: Some(30),
            })
            .unwrap();
        assert_eq!(
            store.accounting_summary().unwrap(),
            AccountingSummary {
                settled_usd: 0.03,
                settled_entries: 1,
                reserved_usd: 0.0,
                active_reservations: 0,
            }
        );
    }

    #[test]
    fn pages_job_metadata_with_stable_tie_breaking_and_filters() {
        let store = Store::open_in_memory(config()).unwrap();
        for index in 0..5 {
            let mut snapshot = job(
                if index == 4 {
                    JobState::Failed
                } else {
                    JobState::Succeeded
                },
                AttemptOutcome::Succeeded,
            );
            snapshot.id = format!("resp_page_{index}");
            snapshot.priority = if index % 2 == 0 {
                Priority::Background
            } else {
                Priority::Normal
            };
            store.persist_job(&snapshot).unwrap();
        }
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = store
                .job_page("test-app", None, None, cursor.as_ref(), 2)
                .unwrap();
            assert!(page.jobs.len() <= 2);
            ids.extend(page.jobs.into_iter().map(|job| job.id));
            cursor = page
                .next_cursor
                .map(|cursor| cursor.parse::<JobPageCursor>().unwrap());
            if cursor.is_none() {
                break;
            }
        }
        ids.sort();
        assert_eq!(
            ids,
            (0..5)
                .map(|index| format!("resp_page_{index}"))
                .collect::<Vec<_>>()
        );
        let filtered = store
            .job_page(
                "test-app",
                Some(Priority::Background),
                Some(JobState::Succeeded),
                None,
                100,
            )
            .unwrap();
        assert_eq!(filtered.jobs.len(), 2);
        assert!(
            filtered
                .jobs
                .iter()
                .all(|job| job.priority == Priority::Background
                    && job.state == JobState::Succeeded)
        );
        assert!(
            store
                .job_page("another_app", None, None, None, 100)
                .unwrap()
                .jobs
                .is_empty()
        );
        let connection = store.connection().unwrap();
        let filtered_plan = connection
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM jobs
                 WHERE app_id = ?1 AND priority = ?2 AND state = ?3
                 ORDER BY created_at_ms DESC, id DESC LIMIT ?4",
                params!["test-app", "background", "succeeded", 100],
                |row| row.get::<_, String>(3),
            )
            .unwrap();
        assert!(filtered_plan.contains("jobs_app_state_priority_created_id"));
    }

    #[test]
    fn traverses_more_than_one_hundred_thousand_jobs_with_bounded_pages() {
        const JOB_COUNT: usize = 100_005;
        const PAGE_SIZE: usize = 257;

        let store = Store::open_in_memory(config()).unwrap();
        let fingerprint = store.config.fingerprint.clone();
        {
            let mut connection = store.connection().unwrap();
            let transaction = connection.transaction().unwrap();
            let mut insert = transaction
                .prepare(
                    "INSERT INTO jobs(
                        id, app_id, intent, provider, deployment, state, priority,
                        config_fingerprint, snapshot_json, error, created_at_ms, updated_at_ms
                     ) VALUES (?1, 'test-app', 'text.summarize', 'local', 'small', 'queued',
                               'background', ?2, '{}', NULL, ?3, ?3)",
                )
                .unwrap();
            for index in 0..JOB_COUNT {
                // Groups of 100 share a timestamp so the ID tie-breaker is
                // exercised at every page boundary.
                insert
                    .execute(params![
                        format!("job_{index:06}"),
                        fingerprint,
                        (index / 100) as i64,
                    ])
                    .unwrap();
            }
            drop(insert);
            transaction.commit().unwrap();
        }

        let mut cursor = None;
        let mut previous: Option<(i64, String)> = None;
        let mut visited = 0;
        loop {
            let page = store
                .job_page(
                    "test-app",
                    Some(Priority::Background),
                    Some(JobState::Queued),
                    cursor.as_ref(),
                    PAGE_SIZE,
                )
                .unwrap();
            assert!(page.jobs.len() <= PAGE_SIZE);
            for current in &page.jobs {
                if let Some((previous_time, previous_id)) = &previous {
                    assert!(
                        current.created_at_ms < *previous_time
                            || (current.created_at_ms == *previous_time
                                && current.id < *previous_id)
                    );
                }
                previous = Some((current.created_at_ms, current.id.clone()));
                visited += 1;
            }
            cursor = page
                .next_cursor
                .map(|cursor| cursor.parse::<JobPageCursor>().unwrap());
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(visited, JOB_COUNT);
    }

    #[test]
    fn migration_backfills_priority_for_existing_job_metadata() {
        let config = config();
        let mut snapshot = job(JobState::Queued, AttemptOutcome::Running);
        snapshot.priority = Priority::Background;
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                 CREATE TABLE config_snapshots (
                    fingerprint TEXT PRIMARY KEY,
                    contents_json TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE jobs (
                    id TEXT PRIMARY KEY,
                    app_id TEXT NOT NULL,
                    intent TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    deployment TEXT NOT NULL,
                    state TEXT NOT NULL,
                    config_fingerprint TEXT NOT NULL REFERENCES config_snapshots(fingerprint),
                    snapshot_json TEXT NOT NULL,
                    error TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO schema_migrations(version) VALUES (4);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO config_snapshots(fingerprint, contents_json, created_at_ms)
                 VALUES (?1, ?2, 1)",
                params![config.fingerprint, config.contents_json],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO jobs(
                    id, app_id, intent, provider, deployment, state, config_fingerprint,
                    snapshot_json, error, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, NULL, 1, 1)",
                params![
                    snapshot.id,
                    snapshot.app_id,
                    snapshot.intent,
                    snapshot.provider,
                    snapshot.deployment,
                    config.fingerprint,
                    serde_json::to_string(&snapshot).unwrap(),
                ],
            )
            .unwrap();
        let store = Store {
            connection: Mutex::new(connection),
            config,
            quota: QuotaLimits::default(),
        };
        store.migrate().unwrap();
        let connection = store.connection().unwrap();
        let priority = connection
            .query_row(
                "SELECT priority FROM jobs WHERE id = ?1",
                params![snapshot.id],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(priority, "background");
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version = 6",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn resource_actions_have_a_bounded_non_job_audit_stream() {
        let store = Store::open_in_memory(config()).unwrap();
        store
            .record_resource_audit_event(ResourceAuditEventInput {
                actor: "test-app".into(),
                kind: "eviction.apply_requested".into(),
                deployment: "ollama_qwen3_5_2b".into(),
                details: json!({"reason":"maintenance-42"}),
            })
            .unwrap();
        let events = store.resource_audit_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "test-app");
        assert_eq!(events[0].deployment, "ollama_qwen3_5_2b");
        assert_eq!(events[0].config_fingerprint, config().fingerprint);
        assert_eq!(events[0].details["reason"], "maintenance-42");
    }

    #[test]
    fn recovery_fails_in_flight_work_without_replaying_and_settles_its_reservation() {
        let store = Store::open_in_memory(config()).unwrap();
        let snapshot = job(JobState::Running, AttemptOutcome::Running);
        store.persist_job(&snapshot).unwrap();
        store
            .reserve_attempt(&AttemptReservation {
                job_id: snapshot.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                amount_usd: 0.05,
                estimated_tokens: 30,
            })
            .unwrap();
        let recovery = store.recover_interrupted().unwrap();
        assert_eq!(recovery.jobs_failed, 1);
        assert_eq!(recovery.attempts_interrupted, 1);
        assert_eq!(recovery.reservations_settled, 1);
        let recovered = store.load_job(&snapshot.id).unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Failed);
        assert_eq!(recovered.attempts[0].outcome, AttemptOutcome::Interrupted);
        let usage = store.usage_entries().unwrap();
        assert_eq!(usage[0].outcome, "interrupted");
        assert!(usage[0].estimated);
        assert_eq!(usage[0].amount_usd, 0.05);
    }

    #[test]
    fn identical_config_snapshots_have_an_identical_fingerprint() {
        assert_eq!(config().fingerprint, config().fingerprint);
    }

    #[test]
    fn atomically_rejects_a_second_reservation_that_exceeds_an_app_budget() {
        let store = Store::open_in_memory_with_quota(
            config(),
            QuotaLimits {
                apps: BTreeMap::from([(
                    "test-app".into(),
                    QuotaLimitConfig {
                        max_usd: Some(0.05),
                        ..QuotaLimitConfig::default()
                    },
                )]),
                ..QuotaLimits::default()
            },
        )
        .unwrap();
        let first = job(JobState::Running, AttemptOutcome::Running);
        let mut second = job(JobState::Running, AttemptOutcome::Running);
        second.id = "job_second".into();
        store.persist_job(&first).unwrap();
        store.persist_job(&second).unwrap();
        store
            .reserve_attempt(&AttemptReservation {
                job_id: first.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                amount_usd: 0.03,
                estimated_tokens: 20,
            })
            .unwrap();
        let error = store
            .reserve_attempt(&AttemptReservation {
                job_id: second.id.clone(),
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                amount_usd: 0.03,
                estimated_tokens: 20,
            })
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::QuotaExceeded {
                resource: QuotaResource::Usd,
                ..
            }
        ));
    }

    #[test]
    fn concurrent_budget_reservations_never_oversell_the_global_cap() {
        let store = Arc::new(quota_store(QuotaLimitConfig {
            max_usd: Some(0.10),
            ..QuotaLimitConfig::default()
        }));
        let mut reservations = Vec::new();
        for index in 0..8 {
            let mut snapshot = job(JobState::Running, AttemptOutcome::Running);
            snapshot.id = format!("resp_concurrent_{index}");
            store.persist_job(&snapshot).unwrap();
            reservations.push(reservation(snapshot.id, 1, 0.03, 20));
        }
        let barrier = Arc::new(Barrier::new(reservations.len()));
        let mut handles = Vec::new();
        for reservation in reservations {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.reserve_attempt(&reservation)
            }));
        }
        let results = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation worker must not panic"))
            .collect::<Vec<_>>();
        let accepted = results.iter().filter(|result| result.is_ok()).count();
        let rejected = results
            .iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(StoreError::QuotaExceeded {
                        resource: QuotaResource::Usd,
                        ..
                    })
                )
            })
            .count();
        assert_eq!(accepted, 3);
        assert_eq!(rejected, 5);
        let reservations = store.active_reservations().unwrap();
        assert_eq!(reservations.len(), 3);
        assert!(
            reservations
                .iter()
                .map(|entry| entry.amount_usd)
                .sum::<f64>()
                <= 0.10
        );
    }

    #[test]
    fn rate_token_and_concurrency_limits_are_enforced_without_a_provider() {
        let requests = quota_store(QuotaLimitConfig {
            requests_per_minute: Some(2),
            ..QuotaLimitConfig::default()
        });
        for index in 0..3 {
            let mut snapshot = job(JobState::Running, AttemptOutcome::Running);
            snapshot.id = format!("resp_rpm_{index}");
            requests.persist_job(&snapshot).unwrap();
            let result = requests.reserve_attempt(&reservation(snapshot.id, 1, 0.0, 1));
            if index < 2 {
                result.unwrap();
            } else {
                assert!(matches!(
                    result,
                    Err(StoreError::QuotaExceeded {
                        resource: QuotaResource::RequestsPerMinute,
                        ..
                    })
                ));
            }
        }

        let tokens = quota_store(QuotaLimitConfig {
            tokens_per_minute: Some(100),
            ..QuotaLimitConfig::default()
        });
        for (index, count) in [60, 41].into_iter().enumerate() {
            let mut snapshot = job(JobState::Running, AttemptOutcome::Running);
            snapshot.id = format!("resp_tpm_{index}");
            tokens.persist_job(&snapshot).unwrap();
            let result = tokens.reserve_attempt(&reservation(snapshot.id, 1, 0.0, count));
            if index == 0 {
                result.unwrap();
            } else {
                assert!(matches!(
                    result,
                    Err(StoreError::QuotaExceeded {
                        resource: QuotaResource::TokensPerMinute,
                        ..
                    })
                ));
            }
        }

        let concurrent = quota_store(QuotaLimitConfig {
            max_concurrent_attempts: Some(1),
            ..QuotaLimitConfig::default()
        });
        let first = job(JobState::Running, AttemptOutcome::Running);
        let mut second = job(JobState::Running, AttemptOutcome::Running);
        second.id = "resp_concurrent_second".into();
        concurrent.persist_job(&first).unwrap();
        concurrent.persist_job(&second).unwrap();
        concurrent
            .reserve_attempt(&reservation(first.id.clone(), 1, 0.0, 1))
            .unwrap();
        assert!(matches!(
            concurrent.reserve_attempt(&reservation(second.id.clone(), 1, 0.0, 1)),
            Err(StoreError::QuotaExceeded {
                resource: QuotaResource::ConcurrentAttempts,
                ..
            })
        ));
        concurrent
            .settle_attempt(UsageLedgerEntry {
                job_id: first.id,
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "cloud".into(),
                deployment: "deepseek_flash".into(),
                outcome: "succeeded".into(),
                amount_usd: 0.0,
                estimated: true,
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
            })
            .unwrap();
        concurrent
            .reserve_attempt(&reservation(second.id, 1, 0.0, 1))
            .unwrap();
    }
}
