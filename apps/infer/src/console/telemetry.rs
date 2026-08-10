//! Bounded, session-local telemetry projections for the operator Console.
//!
//! Cumulative daemon counters remain authoritative. This owner derives a
//! rolling trend window and filtered child-process logs without persistence or
//! unbounded transport.

use std::collections::VecDeque;

use serde_json::Value;

use crate::{
    daemon_supervisor::{LogLevel, LogLine, LogSource},
    operator_client::ConsoleSnapshot,
};

const STATISTICS_CAPACITY: usize = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFilter {
    All,
    Errors,
    System,
    Stdout,
    Stderr,
}

impl LogFilter {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Errors => "warn+error",
            Self::System => "console",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::System,
            Self::System => Self::Stdout,
            Self::Stdout => Self::Stderr,
            Self::Stderr => Self::All,
        }
    }
}

pub(crate) struct LogView {
    lines: VecDeque<LogLine>,
    max_lines: usize,
    pub(crate) filter: LogFilter,
    pub(crate) query: String,
    pub(crate) editing_query: bool,
    pub(crate) follow: bool,
    offset_from_bottom: usize,
}

impl LogView {
    pub(crate) fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines: max_lines.max(50),
            filter: LogFilter::All,
            query: String::new(),
            editing_query: false,
            follow: true,
            offset_from_bottom: 0,
        }
    }

    pub(crate) fn push(&mut self, line: LogLine) {
        if !self.follow && self.matches(&line) {
            self.offset_from_bottom = self.offset_from_bottom.saturating_add(1);
        }
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
        self.clamp_offset();
    }

    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.offset_from_bottom = 0;
        self.follow = true;
    }

    pub(crate) fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.jump_to_end();
    }

    pub(crate) fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.offset_from_bottom = 0;
        }
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        self.follow = false;
        self.offset_from_bottom = self
            .offset_from_bottom
            .saturating_add(amount)
            .min(self.maximum_offset());
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(amount);
        if self.offset_from_bottom == 0 {
            self.follow = true;
        }
    }

    pub(crate) fn jump_to_start(&mut self) {
        self.follow = false;
        self.offset_from_bottom = self.maximum_offset();
    }

    pub(crate) fn jump_to_end(&mut self) {
        self.follow = true;
        self.offset_from_bottom = 0;
    }

    pub(crate) fn begin_query(&mut self) {
        self.editing_query = true;
    }

    pub(crate) fn finish_query(&mut self) {
        self.editing_query = false;
        self.jump_to_end();
    }

    pub(crate) fn cancel_query(&mut self) {
        self.editing_query = false;
    }

    pub(crate) fn push_query_char(&mut self, value: char) {
        self.query.push(value);
        self.jump_to_end();
    }

    pub(crate) fn pop_query_char(&mut self) {
        self.query.pop();
        self.jump_to_end();
    }

    pub(crate) fn visible(&self, height: usize) -> Vec<&LogLine> {
        let matches = self
            .lines
            .iter()
            .filter(|line| self.matches(line))
            .collect::<Vec<_>>();
        let end = matches
            .len()
            .saturating_sub(self.offset_from_bottom.min(matches.len()));
        let start = end.saturating_sub(height);
        matches[start..end].to_vec()
    }

    pub(crate) fn len(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn matching_count(&self) -> usize {
        self.lines.iter().filter(|line| self.matches(line)).count()
    }

    pub(crate) fn level_count(&self, level: LogLevel) -> usize {
        self.lines.iter().filter(|line| line.level == level).count()
    }

    pub(crate) fn offset_from_bottom(&self) -> usize {
        self.offset_from_bottom
    }

    fn matches(&self, line: &LogLine) -> bool {
        let source_matches = match self.filter {
            LogFilter::All => true,
            LogFilter::Errors => line.level != LogLevel::Info,
            LogFilter::System => line.source == LogSource::System,
            LogFilter::Stdout => line.source == LogSource::Stdout,
            LogFilter::Stderr => line.source == LogSource::Stderr,
        };
        source_matches
            && (self.query.is_empty()
                || line
                    .text
                    .to_ascii_lowercase()
                    .contains(&self.query.to_ascii_lowercase()))
    }

    fn clamp_offset(&mut self) {
        self.offset_from_bottom = self.offset_from_bottom.min(self.maximum_offset());
    }

    fn maximum_offset(&self) -> usize {
        self.matching_count().saturating_sub(1)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MetricCounters {
    submitted: u64,
    dispatched: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
    expired: u64,
    queue_rejected: u64,
    queue_wait_ms_total: u64,
}

impl MetricCounters {
    fn from_value(value: &Value) -> Self {
        let number = |key| value.get(key).and_then(Value::as_u64).unwrap_or_default();
        Self {
            submitted: number("submitted"),
            dispatched: number("dispatched"),
            succeeded: number("succeeded"),
            failed: number("failed"),
            cancelled: number("cancelled"),
            expired: number("expired"),
            queue_rejected: number("queue_rejected"),
            queue_wait_ms_total: number("queue_wait_ms_total"),
        }
    }

    fn reset_since(self, previous: Self) -> bool {
        self.submitted < previous.submitted
            || self.dispatched < previous.dispatched
            || self.succeeded < previous.succeeded
            || self.failed < previous.failed
            || self.cancelled < previous.cancelled
            || self.expired < previous.expired
            || self.queue_rejected < previous.queue_rejected
            || self.queue_wait_ms_total < previous.queue_wait_ms_total
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MetricPoint {
    pub(crate) throughput_per_minute: u64,
    pub(crate) failures_per_minute: u64,
    pub(crate) queue_depth: u64,
    pub(crate) active_attempts: u64,
    pub(crate) average_queue_wait_ms: u64,
    pub(crate) free_memory_percent: Option<u64>,
}

pub(crate) struct StatisticsHistory {
    samples: VecDeque<MetricPoint>,
    previous: Option<(u64, MetricCounters)>,
}

impl StatisticsHistory {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(STATISTICS_CAPACITY),
            previous: None,
        }
    }

    pub(crate) fn record(&mut self, snapshot: &ConsoleSnapshot) {
        let Some(metrics) = snapshot.metrics.value.as_ref() else {
            return;
        };
        let counters = MetricCounters::from_value(metrics);
        let (elapsed_ms, delta) = match self.previous {
            Some((previous_at, previous)) if !counters.reset_since(previous) => (
                snapshot
                    .refreshed_at_unix_ms
                    .saturating_sub(previous_at)
                    .max(1),
                MetricCounters {
                    submitted: counters.submitted.saturating_sub(previous.submitted),
                    dispatched: counters.dispatched.saturating_sub(previous.dispatched),
                    succeeded: counters.succeeded.saturating_sub(previous.succeeded),
                    failed: counters.failed.saturating_sub(previous.failed),
                    cancelled: counters.cancelled.saturating_sub(previous.cancelled),
                    expired: counters.expired.saturating_sub(previous.expired),
                    queue_rejected: counters
                        .queue_rejected
                        .saturating_sub(previous.queue_rejected),
                    queue_wait_ms_total: counters
                        .queue_wait_ms_total
                        .saturating_sub(previous.queue_wait_ms_total),
                },
            ),
            _ => (1, MetricCounters::default()),
        };
        self.previous = Some((snapshot.refreshed_at_unix_ms, counters));
        let completed = delta
            .succeeded
            .saturating_add(delta.failed)
            .saturating_add(delta.cancelled)
            .saturating_add(delta.expired);
        let failed = delta.failed.saturating_add(delta.expired);
        let (queue_depth, active_attempts) = queue_totals(metrics);
        let free_memory_percent = snapshot
            .resources
            .value
            .as_ref()
            .and_then(|value| value.pointer("/system_pressure/free_memory_percent"))
            .and_then(Value::as_u64);
        self.samples.push_back(MetricPoint {
            throughput_per_minute: per_minute(completed, elapsed_ms),
            failures_per_minute: per_minute(failed, elapsed_ms),
            queue_depth,
            active_attempts,
            average_queue_wait_ms: delta
                .queue_wait_ms_total
                .checked_div(delta.dispatched)
                .unwrap_or_default(),
            free_memory_percent,
        });
        while self.samples.len() > STATISTICS_CAPACITY {
            self.samples.pop_front();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.samples.clear();
        self.previous = None;
    }

    pub(crate) fn len(&self) -> usize {
        self.samples.len()
    }

    pub(crate) fn latest(&self) -> MetricPoint {
        self.samples.back().copied().unwrap_or_default()
    }

    pub(crate) fn throughput(&self) -> Vec<u64> {
        self.samples
            .iter()
            .map(|sample| sample.throughput_per_minute)
            .collect()
    }

    pub(crate) fn failures(&self) -> Vec<u64> {
        self.samples
            .iter()
            .map(|sample| sample.failures_per_minute)
            .collect()
    }

    pub(crate) fn queue_depth(&self) -> Vec<u64> {
        self.samples
            .iter()
            .map(|sample| sample.queue_depth)
            .collect()
    }

    pub(crate) fn active_attempts(&self) -> Vec<u64> {
        self.samples
            .iter()
            .map(|sample| sample.active_attempts)
            .collect()
    }

    pub(crate) fn free_memory(&self) -> Vec<u64> {
        self.samples
            .iter()
            .map(|sample| sample.free_memory_percent.unwrap_or_default())
            .collect()
    }
}

fn queue_totals(metrics: &Value) -> (u64, u64) {
    metrics
        .get("provider_queues")
        .and_then(Value::as_object)
        .map(|queues| {
            queues
                .values()
                .fold((0_u64, 0_u64), |(queued, active), queue| {
                    (
                        queued
                            .saturating_add(number_at(queue, "pending_interactive"))
                            .saturating_add(number_at(queue, "pending_normal"))
                            .saturating_add(number_at(queue, "pending_background")),
                        active.saturating_add(number_at(queue, "active")),
                    )
                })
        })
        .unwrap_or_default()
}

fn number_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn per_minute(delta: u64, elapsed_ms: u64) -> u64 {
    delta.saturating_mul(60_000) / elapsed_ms.max(1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LogFilter, LogView, StatisticsHistory};
    use crate::{
        daemon_supervisor::{LogLevel, LogLine, LogSource},
        operator_client::{ConsoleSnapshot, EndpointSnapshot},
    };

    fn endpoint(value: serde_json::Value) -> EndpointSnapshot {
        EndpointSnapshot {
            value: Some(value),
            error: None,
        }
    }

    fn metrics_snapshot(at: u64, succeeded: u64, failed: u64) -> ConsoleSnapshot {
        let mut snapshot = ConsoleSnapshot {
            generation: at,
            refreshed_at_unix_ms: at,
            ..ConsoleSnapshot::default()
        };
        snapshot.metrics = endpoint(json!({
            "submitted": succeeded + failed,
            "dispatched": succeeded + failed,
            "succeeded": succeeded,
            "failed": failed,
            "cancelled": 0,
            "expired": 0,
            "queue_rejected": 0,
            "queue_wait_ms_total": (succeeded + failed) * 10,
            "provider_queues": {"local": {
                "pending_interactive": 1,
                "pending_normal": 2,
                "pending_background": 3,
                "active": 1
            }}
        }));
        snapshot.resources = endpoint(json!({
            "system_pressure": {"free_memory_percent": 42},
            "providers": []
        }));
        snapshot
    }

    #[test]
    fn statistics_uses_counter_deltas_and_resets_without_spikes() {
        let mut history = StatisticsHistory::new();
        history.record(&metrics_snapshot(1_000, 10, 0));
        history.record(&metrics_snapshot(2_000, 12, 1));
        let latest = history.latest();
        assert_eq!(latest.throughput_per_minute, 180);
        assert_eq!(latest.failures_per_minute, 60);
        assert_eq!(latest.queue_depth, 6);
        assert_eq!(latest.average_queue_wait_ms, 10);
        assert_eq!(latest.free_memory_percent, Some(42));

        history.record(&metrics_snapshot(3_000, 1, 0));
        assert_eq!(history.latest().throughput_per_minute, 0);
    }

    #[test]
    fn log_view_filters_searches_and_preserves_a_paused_viewport() {
        let mut logs = LogView::new(50);
        logs.push(LogLine {
            recorded_at_unix_ms: 1,
            source: LogSource::Stdout,
            level: LogLevel::Info,
            text: "request complete".into(),
        });
        logs.push(LogLine {
            recorded_at_unix_ms: 2,
            source: LogSource::Stderr,
            level: LogLevel::Error,
            text: "worker failed".into(),
        });
        logs.cycle_filter();
        assert_eq!(logs.filter, LogFilter::Errors);
        assert_eq!(logs.matching_count(), 1);
        logs.query = "worker".into();
        assert_eq!(logs.visible(10)[0].text, "worker failed");
        logs.scroll_up(1);
        assert!(!logs.follow);
    }
}
