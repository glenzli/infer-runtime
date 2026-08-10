//! Redacted infrastructure snapshot ownership.
//!
//! This projection may aggregate existing control-plane owners, but it never
//! exports Job identities, request metadata, provider errors, filesystem paths,
//! credentials, or the accounting ledger.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use infer_core::{ObserverAccess, ObserverConfig};
use infer_observer::{
    IssueSeverity, MetricKind, ObserverIdentity, ObserverIssue, ObserverMetric, ObserverRedaction,
    ObserverSnapshot, ObserverStatus, ObserverStatusState, SNAPSHOT_SCHEMA,
    STATUS_PROTOCOL_VERSION, now_rfc3339,
};
use infer_resource::{ModelLifecycleState, SystemPressureLevel, SystemPressureSnapshot};
use serde_json::{Value, json};

use super::{Runtime, RuntimeError};

pub(super) struct ObserverRuntimeState {
    identity: ObserverIdentity,
    started: Instant,
    sequence: AtomicU64,
}

impl ObserverRuntimeState {
    pub(super) fn new(config: &ObserverConfig) -> Self {
        Self {
            identity: ObserverIdentity::new(
                "infer-runtime",
                &config.instance_id,
                config.console_url.clone(),
            ),
            started: Instant::now(),
            sequence: AtomicU64::new(0),
        }
    }
}

impl Runtime {
    pub fn authorize_non_observer(&self, app_id: &str) -> Result<(), RuntimeError> {
        match self.config.apps.get(app_id) {
            Some(app) if app.observer_access == ObserverAccess::None => Ok(()),
            Some(_) => Err(RuntimeError::ObserverCredentialRestricted),
            None => Err(RuntimeError::UnknownApp(app_id.into())),
        }
    }

    pub fn authorize_observer_summary(&self, app_id: &str) -> Result<(), RuntimeError> {
        match self.config.apps.get(app_id) {
            Some(app) if app.observer_access == ObserverAccess::Summary => Ok(()),
            Some(_) => Err(RuntimeError::ObserverAccessRequired(app_id.into())),
            None => Err(RuntimeError::UnknownApp(app_id.into())),
        }
    }

    pub fn observer_identity(&self) -> ObserverIdentity {
        self.observer.identity.clone()
    }

    pub async fn observer_snapshot(&self) -> Result<ObserverSnapshot, RuntimeError> {
        let captured_at = now_rfc3339();
        let metrics_snapshot = self.metrics().await;
        let providers = self.provider_snapshots();
        let resources = self.resource_snapshot().await;
        let accounting = match &self.store {
            Some(store) => store.accounting_summary()?,
            None => infer_store::AccountingSummary::default(),
        };

        let active_attempts = metrics_snapshot
            .provider_queues
            .values()
            .map(|queue| queue.active)
            .sum::<usize>();
        let queued_interactive = metrics_snapshot
            .provider_queues
            .values()
            .map(|queue| queue.pending_interactive)
            .sum::<usize>();
        let queued_normal = metrics_snapshot
            .provider_queues
            .values()
            .map(|queue| queue.pending_normal)
            .sum::<usize>();
        let queued_background = metrics_snapshot
            .provider_queues
            .values()
            .map(|queue| queue.pending_background)
            .sum::<usize>();
        let queued_total = queued_interactive + queued_normal + queued_background;

        let configured_providers = providers
            .iter()
            .filter(|provider| provider.configured)
            .count();
        let circuit_open_providers = providers
            .iter()
            .filter(|provider| provider.configured && provider.circuit_open)
            .count();
        let available_providers = configured_providers.saturating_sub(circuit_open_providers);

        let pressure = pressure_name(resources.system_pressure.level);
        let pressure_condition = pressure_condition(&resources.system_pressure);
        let resident_models = resources
            .providers
            .iter()
            .flat_map(|provider| &provider.model_lifecycle)
            .filter(|model| model.state == ModelLifecycleState::Ready)
            .count();
        let active_model_reservations = resources
            .providers
            .iter()
            .flat_map(|provider| &provider.model_lifecycle)
            .map(|model| model.active_reservations)
            .sum::<usize>();

        let global_usd_limit = self.config.quota.global.max_usd;
        let budget_state = budget_state(
            accounting.settled_usd + accounting.reserved_usd,
            global_usd_limit,
        );

        let mut reason_codes = Vec::new();
        let mut issues = Vec::new();
        if configured_providers == 0 || available_providers == 0 {
            reason_codes.push("infer.provider.none_available".into());
            issues.push(issue(
                "infer.provider.none_available",
                IssueSeverity::Critical,
                None,
                &captured_at,
            ));
        }
        for provider in providers.iter().filter(|provider| provider.circuit_open) {
            reason_codes.push("infer.provider.circuit_open".into());
            issues.push(issue(
                "infer.provider.circuit_open",
                IssueSeverity::Warning,
                Some(provider.id.clone()),
                &captured_at,
            ));
        }
        match pressure_condition {
            PressureCondition::Elevated => {
                reason_codes.push("infer.resource.pressure_elevated".into());
                issues.push(issue(
                    "infer.resource.pressure_elevated",
                    IssueSeverity::Warning,
                    None,
                    &captured_at,
                ));
            }
            PressureCondition::Critical => {
                reason_codes.push("infer.resource.pressure_critical".into());
                issues.push(issue(
                    "infer.resource.pressure_critical",
                    IssueSeverity::Critical,
                    None,
                    &captured_at,
                ));
            }
            PressureCondition::Failed => {
                reason_codes.push("infer.resource.pressure_unknown".into());
            }
            PressureCondition::Pending | PressureCondition::Normal => {}
        }
        if matches!(budget_state, "warning" | "exhausted") {
            let code = format!("infer.budget.{budget_state}");
            reason_codes.push(code.clone());
            issues.push(issue(
                &code,
                if budget_state == "exhausted" {
                    IssueSeverity::Critical
                } else {
                    IssueSeverity::Warning
                },
                None,
                &captured_at,
            ));
        }
        reason_codes.sort();
        reason_codes.dedup();

        let status = if configured_providers == 0 || available_providers == 0 {
            ObserverStatusState::Unavailable
        } else if !reason_codes.is_empty() {
            ObserverStatusState::Degraded
        } else if pressure_condition == PressureCondition::Pending {
            ObserverStatusState::Starting
        } else {
            ObserverStatusState::Healthy
        };

        let active_metric = metric(
            "infer.workload.active_attempts",
            MetricKind::Gauge,
            active_attempts,
        );
        let queued_metric = metric(
            "infer.workload.queued_jobs",
            MetricKind::Gauge,
            queued_total,
        );
        let pressure_metric = metric("infer.resources.pressure", MetricKind::State, pressure);
        let mut metrics = vec![
            metric(
                "infer.runtime.uptime_seconds",
                MetricKind::Gauge,
                self.observer.started.elapsed().as_secs(),
            )
            .with_unit("seconds"),
            active_metric.clone(),
            queued_metric.clone(),
            pressure_metric.clone(),
            metric(
                "infer.workload.submitted_total",
                MetricKind::Counter,
                metrics_snapshot.submitted,
            ),
            metric(
                "infer.workload.succeeded_total",
                MetricKind::Counter,
                metrics_snapshot.succeeded,
            ),
            metric(
                "infer.workload.failed_total",
                MetricKind::Counter,
                metrics_snapshot.failed,
            ),
            metric(
                "infer.workload.queue_rejected_total",
                MetricKind::Counter,
                metrics_snapshot.queue_rejected,
            ),
            metric(
                "infer.providers.configured",
                MetricKind::Gauge,
                configured_providers,
            ),
            metric(
                "infer.providers.available",
                MetricKind::Gauge,
                available_providers,
            ),
            metric(
                "infer.providers.circuit_open",
                MetricKind::Gauge,
                circuit_open_providers,
            ),
            metric(
                "infer.resources.resident_models",
                MetricKind::Gauge,
                resident_models,
            ),
            metric(
                "infer.resources.active_model_reservations",
                MetricKind::Gauge,
                active_model_reservations,
            ),
            metric(
                "infer.budget.settled_usd",
                MetricKind::Counter,
                accounting.settled_usd,
            )
            .with_unit("usd"),
            metric(
                "infer.budget.reserved_usd",
                MetricKind::Gauge,
                accounting.reserved_usd,
            )
            .with_unit("usd"),
            metric(
                "infer.budget.active_reservations",
                MetricKind::Gauge,
                accounting.active_reservations,
            ),
        ];
        if let Some(free_memory_percent) = resources.system_pressure.free_memory_percent {
            metrics.push(
                metric(
                    "infer.resources.free_memory_percent",
                    MetricKind::Gauge,
                    free_memory_percent,
                )
                .with_unit("percent"),
            );
        }
        if let Some(limit) = global_usd_limit {
            metrics.push(
                metric("infer.budget.global_usd_limit", MetricKind::Gauge, limit).with_unit("usd"),
            );
        }

        let provider_details = providers
            .iter()
            .map(|provider| {
                let queue = metrics_snapshot.provider_queues.get(&provider.id);
                json!({
                    "id": provider.id,
                    "kind": provider.kind,
                    "placement": provider.placement,
                    "configured": provider.configured,
                    "circuit_open": provider.circuit_open,
                    "active": queue.map_or(0, |queue| queue.active),
                    "queued": queue.map_or(0, |queue| {
                        queue.pending_interactive + queue.pending_normal + queue.pending_background
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "infer-runtime".into(),
            json!({
                "workload": {
                    "queued": {
                        "interactive": queued_interactive,
                        "normal": queued_normal,
                        "background": queued_background
                    },
                    "cancelled_total": metrics_snapshot.cancelled,
                    "expired_total": metrics_snapshot.expired,
                    "queue_wait_ms_total": metrics_snapshot.queue_wait_ms_total
                },
                "providers": {
                    "configured": configured_providers,
                    "available": available_providers,
                    "circuit_open": circuit_open_providers,
                    "items": provider_details
                },
                "resources": {
                    "pressure": pressure,
                    "free_memory_percent": resources.system_pressure.free_memory_percent,
                    "resident_models": resident_models,
                    "active_model_reservations": active_model_reservations
                },
                "budget": {
                    "state": budget_state,
                    "settled_usd": accounting.settled_usd,
                    "settled_entries": accounting.settled_entries,
                    "reserved_usd": accounting.reserved_usd,
                    "global_usd_limit": global_usd_limit,
                    "active_reservations": accounting.active_reservations
                }
            }),
        );

        Ok(ObserverSnapshot {
            schema: SNAPSHOT_SCHEMA.into(),
            schema_version: STATUS_PROTOCOL_VERSION.into(),
            service: self.observer.identity.snapshot_service(),
            sequence: self.observer.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            captured_at,
            status: ObserverStatus {
                state: status,
                reason_codes,
            },
            headline_metrics: vec![active_metric.id, queued_metric.id, pressure_metric.id],
            metrics,
            issues,
            extensions,
            links: self.observer.identity.links.clone(),
            redaction: ObserverRedaction::default(),
        })
    }
}

fn metric(id: &str, kind: MetricKind, value: impl Into<Value>) -> ObserverMetric {
    ObserverMetric::new(id, kind, value)
}

fn pressure_name(level: SystemPressureLevel) -> &'static str {
    match level {
        SystemPressureLevel::Unknown => "unknown",
        SystemPressureLevel::Normal => "normal",
        SystemPressureLevel::Elevated => "elevated",
        SystemPressureLevel::Critical => "critical",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureCondition {
    Pending,
    Normal,
    Elevated,
    Critical,
    Failed,
}

fn pressure_condition(snapshot: &SystemPressureSnapshot) -> PressureCondition {
    match snapshot.level {
        SystemPressureLevel::Unknown if snapshot.is_pending() => PressureCondition::Pending,
        SystemPressureLevel::Unknown => PressureCondition::Failed,
        SystemPressureLevel::Normal => PressureCondition::Normal,
        SystemPressureLevel::Elevated => PressureCondition::Elevated,
        SystemPressureLevel::Critical => PressureCondition::Critical,
    }
}

fn budget_state(used: f64, limit: Option<f64>) -> &'static str {
    match limit {
        None => "unbounded",
        Some(limit) if limit == 0.0 || used >= limit => "exhausted",
        Some(limit) if used / limit >= 0.8 => "warning",
        Some(_) => "ok",
    }
}

fn issue(
    code: &str,
    severity: IssueSeverity,
    subject_id: Option<String>,
    observed_at: &str,
) -> ObserverIssue {
    ObserverIssue {
        code: code.into(),
        severity,
        subject_id,
        observed_at: observed_at.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{PressureCondition, budget_state, pressure_condition};
    use infer_resource::{SystemPressureLevel, SystemPressureSnapshot};

    #[test]
    fn budget_summary_has_bounded_states() {
        assert_eq!(budget_state(100.0, None), "unbounded");
        assert_eq!(budget_state(7.9, Some(10.0)), "ok");
        assert_eq!(budget_state(8.0, Some(10.0)), "warning");
        assert_eq!(budget_state(10.0, Some(10.0)), "exhausted");
    }

    #[test]
    fn pending_and_failed_unknown_pressure_have_distinct_health_semantics() {
        let pending = SystemPressureSnapshot {
            source: "host".into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: 0,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: None,
        };
        let failed = SystemPressureSnapshot {
            source: "host".into(),
            level: SystemPressureLevel::Unknown,
            last_checked_unix_ms: 1,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: Some("sample failed".into()),
        };

        assert_eq!(pressure_condition(&pending), PressureCondition::Pending);
        assert_eq!(pressure_condition(&failed), PressureCondition::Failed);
    }
}
