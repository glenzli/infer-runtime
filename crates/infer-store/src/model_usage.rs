//! Calendar-day model accounting for the local infrastructure observer.
//!
//! This owner only projects settled ledger rows. It does not expose Jobs,
//! Consumers, Providers, request metadata, or any individual ledger entry.

use std::{collections::BTreeMap, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{JobSnapshot, Store, StoreError};

/// The complete settled token and cost total for one effective model on the
/// Runtime host's local calendar day.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyModelUsageModel {
    pub id: String,
    pub execution_origin: ExecutionOrigin,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

/// A non-empty local calendar-day aggregate. Sentinel owns historical storage
/// and upserts this current-day projection instead of adding every poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyModelUsage {
    pub date: String,
    pub models: Vec<DailyModelUsageModel>,
}

/// A Runtime-derived execution family used only to avoid double-counting a
/// source that already has its own infrastructure collector.
///
/// This is recorded when an Attempt settles. It is never inferred from a
/// deployment identifier, model label, or a later version of configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOrigin {
    Codex,
    Other,
}

impl ExecutionOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }
}

impl FromStr for ExecutionOrigin {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "other" => Ok(Self::Other),
            _ => Err(()),
        }
    }
}

impl Store {
    /// Returns the complete usage recorded so far for the current host-local
    /// calendar day. A row without a settled, Runtime-derived execution origin
    /// is omitted so a downstream observer cannot double-count Codex usage.
    pub fn current_local_day_model_usage(&self) -> Result<Option<DailyModelUsage>, StoreError> {
        let connection = self.connection()?;
        let date: String =
            connection.query_row("SELECT date('now', 'localtime')", [], |row| row.get(0))?;
        let mut statement = connection.prepare(
            "SELECT usage_ledger.model_id, jobs.snapshot_json, usage_ledger.execution_origin,
                    COALESCE(usage_ledger.input_tokens, 0),
                    COALESCE(usage_ledger.output_tokens, 0),
                    COALESCE(usage_ledger.total_tokens, 0),
                    usage_ledger.amount_usd
             FROM usage_ledger
             LEFT JOIN jobs ON jobs.id = usage_ledger.job_id
             WHERE usage_day = ?1
             ORDER BY usage_ledger.recorded_at_ms, usage_ledger.job_id, usage_ledger.attempt_number",
        )?;
        let mut models = BTreeMap::<(ExecutionOrigin, String), DailyModelUsageModel>::new();
        for row in statement.query_map([&date], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                non_negative_u64(row.get::<_, i64>(3)?),
                non_negative_u64(row.get::<_, i64>(4)?),
                non_negative_u64(row.get::<_, i64>(5)?),
                row.get::<_, f64>(6)?,
            ))
        })? {
            let (
                stored_model_id,
                snapshot_json,
                stored_origin,
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
            ) = row?;
            let Some(execution_origin) = stored_origin
                .as_deref()
                .and_then(|value| ExecutionOrigin::from_str(value).ok())
            else {
                continue;
            };
            let model_id = stored_model_id.or_else(|| {
                snapshot_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<JobSnapshot>(value).ok())
                    .and_then(|snapshot| public_model_id(&snapshot))
            });
            let Some(id) = model_id else {
                continue;
            };
            let aggregate = models
                .entry((execution_origin, id.clone()))
                .or_insert_with(|| DailyModelUsageModel {
                    id,
                    execution_origin,
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: 0.0,
                });
            aggregate.input_tokens = aggregate.input_tokens.saturating_add(input_tokens);
            aggregate.output_tokens = aggregate.output_tokens.saturating_add(output_tokens);
            aggregate.total_tokens = aggregate.total_tokens.saturating_add(total_tokens);
            aggregate.cost_usd += cost_usd;
        }
        let models = models.into_values().collect::<Vec<_>>();
        if models.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DailyModelUsage { date, models }))
        }
    }
}

pub(crate) fn public_model_id(snapshot: &JobSnapshot) -> Option<String> {
    let physical_model = snapshot.physical_model.trim();
    if !physical_model.is_empty() && !std::path::Path::new(physical_model).is_absolute() {
        return Some(physical_model.into());
    }
    let build = snapshot.model_build.trim();
    (!build.is_empty()).then_some(build.into())
}

fn non_negative_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use infer_core::{AttemptOutcome, JobState};
    use serde_json::json;

    use crate::{
        AttemptReservation, ConfigSnapshot, ExecutionOrigin, Store, UsageLedgerEntry, tests::job,
    };

    fn config() -> ConfigSnapshot {
        ConfigSnapshot::from_serializable(&json!({"version": 1})).unwrap()
    }

    fn reservation(
        job_id: String,
        attempt_number: usize,
        amount_usd: f64,
        estimated_tokens: u64,
    ) -> AttemptReservation {
        AttemptReservation {
            job_id,
            attempt_number,
            app_id: "test-app".into(),
            provider: "cloud".into(),
            deployment: "deepseek_flash".into(),
            amount_usd,
            estimated_tokens,
        }
    }

    #[test]
    fn current_local_day_aggregates_only_safe_model_identity() {
        let store = Store::open_in_memory(config()).unwrap();
        let first = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        store.persist_job(&first).unwrap();
        store
            .reserve_attempt(&reservation(first.id.clone(), 1, 0.12, 30))
            .unwrap();
        store
            .settle_attempt(UsageLedgerEntry {
                job_id: first.id.clone(),
                attempt_number: 1,
                app_id: first.app_id.clone(),
                provider: first.provider.clone(),
                deployment: first.deployment.clone(),
                outcome: "succeeded".into(),
                amount_usd: 0.10,
                estimated: true,
                input_tokens: Some(10),
                output_tokens: Some(20),
                total_tokens: Some(30),
                execution_origin: Some(ExecutionOrigin::Other),
            })
            .unwrap();

        let mut second = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        second.id = "resp_second".into();
        second.physical_model = first.physical_model.clone();
        store.persist_job(&second).unwrap();
        store
            .reserve_attempt(&reservation(second.id.clone(), 1, 0.03, 5))
            .unwrap();
        store
            .settle_attempt(UsageLedgerEntry {
                job_id: second.id,
                attempt_number: 1,
                app_id: second.app_id,
                provider: second.provider,
                deployment: second.deployment,
                outcome: "succeeded".into(),
                amount_usd: 0.02,
                estimated: true,
                input_tokens: Some(2),
                output_tokens: Some(3),
                total_tokens: Some(5),
                execution_origin: Some(ExecutionOrigin::Other),
            })
            .unwrap();

        let usage = store.current_local_day_model_usage().unwrap().unwrap();
        assert_eq!(usage.models.len(), 1);
        assert_eq!(usage.models[0].id, "deepseek-v4-flash");
        assert_eq!(usage.models[0].execution_origin, ExecutionOrigin::Other);
        assert_eq!(usage.models[0].input_tokens, 12);
        assert_eq!(usage.models[0].output_tokens, 23);
        assert_eq!(usage.models[0].total_tokens, 35);
        assert!((usage.models[0].cost_usd - 0.12).abs() < f64::EPSILON);
    }

    #[test]
    fn absolute_physical_model_is_replaced_with_the_runtime_build_identity() {
        let mut snapshot = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        snapshot.model_build = "safe-build".into();
        snapshot.physical_model = "/private/model.onnx".into();
        assert_eq!(
            super::public_model_id(&snapshot).as_deref(),
            Some("safe-build")
        );
    }

    #[test]
    fn row_without_a_recorded_execution_origin_is_omitted_fail_closed() {
        let store = Store::open_in_memory(config()).unwrap();
        let snapshot = job(JobState::Succeeded, AttemptOutcome::Succeeded);
        store.persist_job(&snapshot).unwrap();
        store
            .reserve_attempt(&reservation(snapshot.id.clone(), 1, 0.0, 0))
            .unwrap();
        store
            .settle_attempt(UsageLedgerEntry {
                job_id: snapshot.id.clone(),
                attempt_number: 1,
                app_id: snapshot.app_id.clone(),
                provider: snapshot.provider.clone(),
                deployment: snapshot.deployment.clone(),
                outcome: "succeeded".into(),
                amount_usd: 0.0,
                estimated: true,
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(3),
                execution_origin: None,
            })
            .unwrap();
        assert!(store.current_local_day_model_usage().unwrap().is_none());
    }
}
