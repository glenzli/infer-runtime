//! Payload-free terminal Job throughput aggregates.
//!
//! This is intentionally derived from the durable Job projection rather than
//! process-local counters. A Console reconnect or daemon restart must not make
//! an operator-facing historical chart appear empty.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::Serialize;

use super::{Store, StoreError, now_ms};

/// One contiguous terminal-Job time bucket. It deliberately contains no Job
/// identifier, App identity, provider identity, error, or payload metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryBucket {
    pub started_at_ms: i64,
    pub succeeded: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub expired: u64,
}

/// Bounded, durable terminal-Job history for an operator presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryWindow {
    pub window_started_at_ms: i64,
    pub window_ends_at_ms: i64,
    pub bucket_width_ms: i64,
    pub buckets: Vec<TelemetryBucket>,
}

impl Store {
    /// Returns fixed-width terminal-Job aggregates ending at the current
    /// wall-clock instant. `window_ms` and `bucket_width_ms` are bounded by
    /// the control-plane caller; zero or inverted values are rejected here as
    /// a second line of defense against accidental unbounded SQL work.
    pub fn telemetry_window(
        &self,
        window_ms: i64,
        bucket_width_ms: i64,
    ) -> Result<TelemetryWindow, StoreError> {
        telemetry_window_at(self, now_ms(), window_ms, bucket_width_ms)
    }
}

fn telemetry_window_at(
    store: &Store,
    ends_at_ms: i64,
    window_ms: i64,
    bucket_width_ms: i64,
) -> Result<TelemetryWindow, StoreError> {
    if window_ms <= 0 || bucket_width_ms <= 0 || window_ms % bucket_width_ms != 0 {
        return Err(StoreError::InvalidTelemetryWindow);
    }
    let bucket_count = window_ms / bucket_width_ms;
    if !(1..=1_024).contains(&bucket_count) {
        return Err(StoreError::InvalidTelemetryWindow);
    }
    let window_started_at_ms = ends_at_ms.saturating_sub(window_ms);
    let mut buckets = (0..bucket_count)
        .map(|index| TelemetryBucket {
            started_at_ms: window_started_at_ms + index * bucket_width_ms,
            succeeded: 0,
            failed: 0,
            cancelled: 0,
            expired: 0,
        })
        .collect::<Vec<_>>();

    let connection = store.connection()?;
    let mut statement = connection.prepare(
        "SELECT ((updated_at_ms - ?1) / ?2) AS bucket_index, state, COUNT(*)
         FROM jobs
         WHERE updated_at_ms >= ?1
           AND updated_at_ms < ?3
           AND state IN ('succeeded', 'failed', 'cancelled', 'expired')
         GROUP BY bucket_index, state",
    )?;
    let rows = statement.query_map(
        params![window_started_at_ms, bucket_width_ms, ends_at_ms],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let mut aggregate = BTreeMap::<(usize, String), u64>::new();
    for row in rows {
        let (index, state, count) = row?;
        if let Ok(index) = usize::try_from(index)
            && index < buckets.len()
        {
            aggregate.insert((index, state), count.max(0) as u64);
        }
    }
    for ((index, state), count) in aggregate {
        let bucket = &mut buckets[index];
        match state.as_str() {
            "succeeded" => bucket.succeeded = count,
            "failed" => bucket.failed = count,
            "cancelled" => bucket.cancelled = count,
            "expired" => bucket.expired = count,
            _ => unreachable!("SQL state filter limits terminal states"),
        }
    }

    Ok(TelemetryWindow {
        window_started_at_ms,
        window_ends_at_ms: ends_at_ms,
        bucket_width_ms,
        buckets,
    })
}

#[cfg(test)]
mod tests {
    use infer_core::{AttemptOutcome, JobState};

    use super::*;
    use crate::{ConfigSnapshot, tests::job};

    #[test]
    fn history_is_durable_and_zero_fills_missing_buckets() {
        let store = Store::open_in_memory(
            ConfigSnapshot::from_serializable(&serde_json::json!({})).unwrap(),
        )
        .unwrap();
        store
            .persist_job(&job(JobState::Succeeded, AttemptOutcome::Succeeded))
            .unwrap();
        {
            let connection = store.connection().unwrap();
            connection
                .execute(
                    "UPDATE jobs SET updated_at_ms = 1_500 WHERE id = 'resp_test'",
                    [],
                )
                .unwrap();
        }
        let history = telemetry_window_at(&store, 4_000, 4_000, 1_000).unwrap();
        assert_eq!(history.buckets.len(), 4);
        assert_eq!(history.buckets[1].succeeded, 1);
        assert!(
            history
                .buckets
                .iter()
                .enumerate()
                .all(|(index, bucket)| index == 1 || bucket.succeeded == 0)
        );
    }

    #[test]
    fn rejects_unbounded_or_misaligned_windows() {
        let store = Store::open_in_memory(
            ConfigSnapshot::from_serializable(&serde_json::json!({})).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            telemetry_window_at(&store, 10, 0, 1),
            Err(StoreError::InvalidTelemetryWindow)
        ));
        assert!(matches!(
            telemetry_window_at(&store, 10, 10, 3),
            Err(StoreError::InvalidTelemetryWindow)
        ));
        assert!(matches!(
            telemetry_window_at(&store, 2_000, 2_000, 1),
            Err(StoreError::InvalidTelemetryWindow)
        ));
    }
}
