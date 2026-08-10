//! Interactive Console aggregate state.
//!
//! This owner keeps current snapshot identity, stable selections, status, and
//! configuration interaction. Bounded log and statistics lifecycles belong to
//! the sibling telemetry owner.

use std::path::PathBuf;

use serde_json::Value;
use tokio::sync::mpsc::Receiver;

use super::{
    telemetry::{LogView, StatisticsHistory},
    validate_config,
};
use crate::{daemon_supervisor::LogLine, operator_client::ConsoleSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Statistics,
    Jobs,
    Resources,
    Logs,
    Config,
}

impl Tab {
    pub(crate) const ALL: [Self; 6] = [
        Self::Overview,
        Self::Statistics,
        Self::Jobs,
        Self::Resources,
        Self::Logs,
        Self::Config,
    ];

    pub(crate) fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    pub(crate) fn from_index(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }

    pub(crate) fn next(self) -> Self {
        Self::from_index((self.index() + 1) % Self::ALL.len())
    }

    pub(crate) fn previous(self) -> Self {
        Self::from_index((self.index() + Self::ALL.len() - 1) % Self::ALL.len())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JobTarget {
    pub(crate) id: String,
    pub(crate) state: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceTarget {
    pub(crate) provider: String,
    pub(crate) deployment: String,
    pub(crate) model_id: String,
    pub(crate) state: String,
    pub(crate) active_reservations: u64,
    pub(crate) resident_memory_bytes: Option<u64>,
}

pub(crate) struct ConsoleState {
    pub(crate) tab: Tab,
    pub(crate) snapshot: ConsoleSnapshot,
    pub(crate) statistics: StatisticsHistory,
    pub(crate) logs: LogView,
    pub(crate) selected_job: Option<String>,
    pub(crate) job_detail: Option<Value>,
    pub(crate) selected_resource: Option<(String, String)>,
    pub(crate) status: String,
    pub(crate) status_is_error: bool,
    pub(crate) config_status: String,
    pub(crate) config_valid: bool,
    pub(crate) config_path: PathBuf,
}

impl ConsoleState {
    pub(crate) fn new(config_path: PathBuf, max_logs: usize) -> Self {
        let (config_valid, config_status) = validate_config(&config_path);
        Self {
            tab: Tab::Overview,
            snapshot: ConsoleSnapshot::default(),
            statistics: StatisticsHistory::new(),
            logs: LogView::new(max_logs),
            selected_job: None,
            job_detail: None,
            selected_resource: None,
            status: "console initialized; press s to start inferd or attach to an existing daemon"
                .into(),
            status_is_error: false,
            config_status,
            config_valid,
            config_path,
        }
    }

    pub(crate) fn accept_snapshot(&mut self, snapshot: ConsoleSnapshot) {
        self.statistics.record(&snapshot);
        self.snapshot = snapshot;
        let previous_job = self.selected_job.clone();
        let jobs = self.job_targets();
        reconcile_selection(
            &mut self.selected_job,
            jobs.iter().map(|job| job.id.clone()).collect(),
        );
        if self.selected_job != previous_job {
            self.job_detail = None;
        }
        let resources = self.resource_targets();
        reconcile_selection(
            &mut self.selected_resource,
            resources
                .iter()
                .map(|resource| (resource.provider.clone(), resource.deployment.clone()))
                .collect(),
        );
    }

    pub(crate) fn set_status(&mut self, message: impl Into<String>, is_error: bool) {
        self.status = message.into();
        self.status_is_error = is_error;
    }

    pub(crate) fn drain_logs(&mut self, receiver: &mut Receiver<LogLine>) {
        while let Ok(line) = receiver.try_recv() {
            self.logs.push(line);
        }
    }

    pub(crate) fn validate_config(&mut self) {
        let (valid, status) = validate_config(&self.config_path);
        self.config_valid = valid;
        self.config_status = status.clone();
        self.set_status(status, !valid);
    }

    pub(crate) fn job_targets(&self) -> Vec<JobTarget> {
        self.snapshot
            .jobs
            .value
            .as_ref()
            .and_then(|value| value.get("jobs"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|job| {
                Some(JobTarget {
                    id: job.get("id")?.as_str()?.to_owned(),
                    state: job
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                })
            })
            .collect()
    }

    pub(crate) fn selected_job_target(&self) -> Option<JobTarget> {
        let selected = self.selected_job.as_ref()?;
        self.job_targets()
            .into_iter()
            .find(|job| &job.id == selected)
    }

    pub(crate) fn move_job_selection(&mut self, delta: isize) {
        let ids = self
            .job_targets()
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>();
        move_selection(&mut self.selected_job, &ids, delta);
        self.job_detail = None;
    }

    pub(crate) fn resource_targets(&self) -> Vec<ResourceTarget> {
        let mut targets = Vec::new();
        if let Some(providers) = self
            .snapshot
            .resources
            .value
            .as_ref()
            .and_then(|value| value.get("providers"))
            .and_then(Value::as_array)
        {
            for provider in providers {
                let provider_id = provider
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if let Some(models) = provider.get("model_lifecycle").and_then(Value::as_array) {
                    for model in models {
                        let Some(deployment) = model.get("deployment").and_then(Value::as_str)
                        else {
                            continue;
                        };
                        targets.push(ResourceTarget {
                            provider: provider_id.to_owned(),
                            deployment: deployment.to_owned(),
                            model_id: model
                                .get("model_id")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_owned(),
                            state: model
                                .get("state")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_owned(),
                            active_reservations: model
                                .get("active_reservations")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                            resident_memory_bytes: model
                                .get("resident_memory_bytes")
                                .and_then(Value::as_u64),
                        });
                    }
                }
            }
        }
        targets
    }

    pub(crate) fn selected_resource_target(&self) -> Option<ResourceTarget> {
        let selected = self.selected_resource.as_ref()?;
        self.resource_targets()
            .into_iter()
            .find(|resource| resource.provider == selected.0 && resource.deployment == selected.1)
    }

    pub(crate) fn move_resource_selection(&mut self, delta: isize) {
        let ids = self
            .resource_targets()
            .into_iter()
            .map(|resource| (resource.provider, resource.deployment))
            .collect::<Vec<_>>();
        move_selection(&mut self.selected_resource, &ids, delta);
    }
}

fn reconcile_selection<T: Clone + PartialEq>(selected: &mut Option<T>, values: Vec<T>) {
    if selected
        .as_ref()
        .is_none_or(|selected| !values.contains(selected))
    {
        *selected = values.first().cloned();
    }
}

fn move_selection<T: Clone + PartialEq>(selected: &mut Option<T>, values: &[T], delta: isize) {
    if values.is_empty() {
        *selected = None;
        return;
    }
    let current = selected
        .as_ref()
        .and_then(|selected| values.iter().position(|value| value == selected))
        .unwrap_or_default();
    let next = current
        .saturating_add_signed(delta)
        .min(values.len().saturating_sub(1));
    *selected = Some(values[next].clone());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ConsoleState;
    use crate::operator_client::{ConsoleSnapshot, EndpointSnapshot};

    fn endpoint(value: serde_json::Value) -> EndpointSnapshot {
        EndpointSnapshot {
            value: Some(value),
            error: None,
        }
    }

    #[test]
    fn selections_follow_job_and_deployment_identity_across_refreshes() {
        let config = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/infer.example.toml");
        let mut state = ConsoleState::new(config, 50);
        let snapshot = ConsoleSnapshot {
            jobs: endpoint(json!({"jobs": [
                {"id": "job-a", "state": "running"},
                {"id": "job-b", "state": "queued"}
            ]})),
            resources: endpoint(json!({"providers": [{
                "provider": "local",
                "model_lifecycle": [
                    {"deployment": "small", "model_id": "model-a", "state": "ready", "active_reservations": 0},
                    {"deployment": "large", "model_id": "model-b", "state": "absent", "active_reservations": 0}
                ]
            }]})),
            ..ConsoleSnapshot::default()
        };
        state.accept_snapshot(snapshot);
        state.move_job_selection(1);
        state.move_resource_selection(1);
        assert_eq!(state.selected_job.as_deref(), Some("job-b"));
        assert_eq!(
            state.selected_resource,
            Some(("local".into(), "large".into()))
        );

        let reordered = ConsoleSnapshot {
            jobs: endpoint(json!({"jobs": [
                {"id": "job-b", "state": "running"},
                {"id": "job-a", "state": "succeeded"}
            ]})),
            resources: endpoint(json!({"providers": [{
                "provider": "local",
                "model_lifecycle": [
                    {"deployment": "large", "model_id": "model-b", "state": "ready", "active_reservations": 0},
                    {"deployment": "small", "model_id": "model-a", "state": "absent", "active_reservations": 0}
                ]
            }]})),
            ..ConsoleSnapshot::default()
        };
        state.accept_snapshot(reordered);
        assert_eq!(state.selected_job.as_deref(), Some("job-b"));
        assert_eq!(
            state.selected_resource,
            Some(("local".into(), "large".into()))
        );
    }
}
