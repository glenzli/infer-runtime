//! Reservation-safe local model lifecycle and transition arbitration.
//!
//! Native provider control adapters may observe or enact lifecycle changes,
//! but only this module owns the local reservation count and legal transition
//! rules. Eviction eligibility and ordering belong to the sibling planner.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLifecycleState {
    Unknown,
    Absent,
    Loading,
    Benchmarking,
    Ready,
    Draining,
    Unloading,
    Failed,
}

impl ModelLifecycleState {
    /// Valid transitions initiated by the Resource Manager. Native discovery
    /// can still reconcile an externally changed state without pretending it
    /// was an owner-controlled transition.
    pub fn allows_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Unknown, Self::Absent | Self::Ready | Self::Failed)
                    | (Self::Absent, Self::Loading)
                    | (Self::Absent, Self::Benchmarking)
                    | (Self::Loading, Self::Ready | Self::Failed)
                    | (Self::Benchmarking, Self::Absent | Self::Failed)
                    | (Self::Ready, Self::Draining | Self::Failed)
                    | (Self::Draining, Self::Ready | Self::Unloading | Self::Failed)
                    | (Self::Unloading, Self::Absent | Self::Failed)
                    | (Self::Failed, Self::Absent | Self::Loading)
            )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelLifecycleSnapshot {
    pub deployment: String,
    pub model_id: String,
    pub state: ModelLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_vram_bytes: Option<u64>,
    pub active_reservations: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_since_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct LifecycleRecord {
    state: ModelLifecycleState,
    active_reservations: usize,
    state_since_unix_ms: Option<u64>,
    last_used_unix_ms: Option<u64>,
}

impl LifecycleRecord {
    fn unknown() -> Self {
        Self {
            state: ModelLifecycleState::Unknown,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }
    }
}

/// A process-local guard that protects one deployment from eviction. Dropping
/// it is terminal: every response, stream, retry, cancellation, and timeout
/// path releases the reservation without requiring a second async operation.
pub struct ModelReservation {
    deployment: String,
    records: Arc<Mutex<BTreeMap<String, LifecycleRecord>>>,
}

impl Drop for ModelReservation {
    fn drop(&mut self) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        let Some(record) = records.get_mut(&self.deployment) else {
            return;
        };
        debug_assert!(record.active_reservations > 0);
        record.active_reservations = record.active_reservations.saturating_sub(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Load,
    Unload,
    BenchmarkReload,
}

#[derive(Debug, Error)]
pub enum LifecycleOperationError {
    #[error("deployment `{0}` is not owned by this resource manager")]
    UnmanagedDeployment(String),
    #[error("deployment `{deployment}` has {active} active reservation(s)")]
    ActiveReservations { deployment: String, active: usize },
    #[error("deployment `{deployment}` cannot {action:?} from lifecycle state `{state:?}`")]
    InvalidState {
        deployment: String,
        action: LifecycleAction,
        state: ModelLifecycleState,
    },
    #[error("deployment `{deployment}` is transitioning through `{state:?}`")]
    Transitioning {
        deployment: String,
        state: ModelLifecycleState,
    },
}

/// Holds a Resource Manager initiated lifecycle operation open across a native
/// control-plane call. Any dropped/error path restores the previous state, so
/// a transport failure cannot leave routing permanently draining.
pub struct LifecycleOperation {
    deployment: String,
    action: LifecycleAction,
    previous: LifecycleRecord,
    records: Arc<Mutex<BTreeMap<String, LifecycleRecord>>>,
    completed: bool,
    rollback_to_failed: bool,
}

impl LifecycleOperation {
    pub fn complete(mut self, completed_at_unix_ms: u64) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        let Some(record) = records.get_mut(&self.deployment) else {
            return;
        };
        match self.action {
            LifecycleAction::Load => record.state = ModelLifecycleState::Ready,
            LifecycleAction::Unload => {
                // Keep the intermediate transition explicit even though the
                // native API call completed synchronously.
                record.state = ModelLifecycleState::Unloading;
                debug_assert!(
                    record
                        .state
                        .allows_transition_to(ModelLifecycleState::Absent)
                );
                record.state = ModelLifecycleState::Absent;
            }
            LifecycleAction::BenchmarkReload => record.state = ModelLifecycleState::Absent,
        }
        record.state_since_unix_ms = Some(completed_at_unix_ms);
        if self.action == LifecycleAction::Load {
            record.last_used_unix_ms = Some(completed_at_unix_ms);
        }
        self.completed = true;
    }

    /// A partially-run reload measurement may have changed native residency
    /// even when its HTTP call failed. Do not restore the prior `absent`
    /// projection; fail closed until the next native refresh reconciles it.
    pub fn fail(mut self, failed_at_unix_ms: u64) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        let Some(record) = records.get_mut(&self.deployment) else {
            return;
        };
        record.state = ModelLifecycleState::Failed;
        record.state_since_unix_ms = Some(failed_at_unix_ms);
        self.completed = true;
    }
}

impl Drop for LifecycleOperation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        if let Some(record) = records.get_mut(&self.deployment) {
            if self.rollback_to_failed {
                record.state = ModelLifecycleState::Failed;
                record.state_since_unix_ms = None;
            } else {
                *record = self.previous;
            }
        }
    }
}

/// State shared by every Resource Manager operation. It is intentionally a
/// synchronous, short-held lock so `ModelReservation::drop` is reliable even
/// when an async task is cancelled during unwinding.
#[derive(Clone)]
pub struct LifecycleTracker {
    records: Arc<Mutex<BTreeMap<String, LifecycleRecord>>>,
}

impl LifecycleTracker {
    pub fn new(deployments: impl IntoIterator<Item = String>) -> Self {
        Self {
            records: Arc::new(Mutex::new(
                deployments
                    .into_iter()
                    .map(|deployment| (deployment, LifecycleRecord::unknown()))
                    .collect(),
            )),
        }
    }

    /// Reconcile native observations. This does not validate owner-controlled
    /// transitions: another tool may have loaded or unloaded an Ollama model
    /// outside this runtime. It does preserve a stable `ready` start time
    /// across refreshes, which is required for anti-thrashing policy.
    pub fn reconcile(&self, observations: &[ModelLifecycleSnapshot], observed_at_unix_ms: u64) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        for observation in observations {
            let record = records
                .entry(observation.deployment.clone())
                .or_insert_with(LifecycleRecord::unknown);
            // A refresh may overlap an explicit native control call. The
            // operation guard owns these intermediate states; accepting a
            // slightly stale `/api/ps` observation here would otherwise
            // reopen routing while an unload is still in flight.
            if matches!(
                record.state,
                ModelLifecycleState::Loading
                    | ModelLifecycleState::Benchmarking
                    | ModelLifecycleState::Draining
                    | ModelLifecycleState::Unloading
            ) {
                continue;
            }
            if record.state != observation.state {
                record.state = observation.state;
                record.state_since_unix_ms = Some(observed_at_unix_ms);
                if observation.state == ModelLifecycleState::Ready {
                    record.last_used_unix_ms = Some(observed_at_unix_ms);
                }
            }
        }
    }

    /// Returns `None` for deployments not owned by this local resource
    /// manager, allowing cloud and future node attempts to retain their own
    /// reservation mechanisms.
    pub fn reserve(
        &self,
        deployment: &str,
        used_at_unix_ms: u64,
    ) -> Result<Option<ModelReservation>, LifecycleOperationError> {
        let Ok(mut records) = self.records.lock() else {
            return Err(LifecycleOperationError::Transitioning {
                deployment: deployment.into(),
                state: ModelLifecycleState::Unknown,
            });
        };
        let Some(record) = records.get_mut(deployment) else {
            return Ok(None);
        };
        if matches!(
            record.state,
            ModelLifecycleState::Loading
                | ModelLifecycleState::Benchmarking
                | ModelLifecycleState::Draining
                | ModelLifecycleState::Unloading
                | ModelLifecycleState::Failed
        ) {
            return Err(LifecycleOperationError::Transitioning {
                deployment: deployment.into(),
                state: record.state,
            });
        }
        record.active_reservations = record.active_reservations.saturating_add(1);
        record.last_used_unix_ms = Some(used_at_unix_ms);
        Ok(Some(ModelReservation {
            deployment: deployment.into(),
            records: Arc::clone(&self.records),
        }))
    }

    pub fn begin_action(
        &self,
        deployment: &str,
        action: LifecycleAction,
        started_at_unix_ms: u64,
    ) -> Result<LifecycleOperation, LifecycleOperationError> {
        let Ok(mut records) = self.records.lock() else {
            return Err(LifecycleOperationError::Transitioning {
                deployment: deployment.into(),
                state: ModelLifecycleState::Unknown,
            });
        };
        let record = records
            .get_mut(deployment)
            .ok_or_else(|| LifecycleOperationError::UnmanagedDeployment(deployment.into()))?;
        if matches!(
            action,
            LifecycleAction::Unload | LifecycleAction::BenchmarkReload
        ) && record.active_reservations != 0
        {
            return Err(LifecycleOperationError::ActiveReservations {
                deployment: deployment.into(),
                active: record.active_reservations,
            });
        }
        let required_state = match action {
            LifecycleAction::Load => {
                matches!(
                    record.state,
                    ModelLifecycleState::Absent | ModelLifecycleState::Failed
                )
            }
            LifecycleAction::Unload => record.state == ModelLifecycleState::Ready,
            LifecycleAction::BenchmarkReload => record.state == ModelLifecycleState::Absent,
        };
        if !required_state {
            return Err(LifecycleOperationError::InvalidState {
                deployment: deployment.into(),
                action,
                state: record.state,
            });
        }
        let previous = *record;
        record.state = match action {
            LifecycleAction::Load => ModelLifecycleState::Loading,
            LifecycleAction::Unload => ModelLifecycleState::Draining,
            LifecycleAction::BenchmarkReload => ModelLifecycleState::Benchmarking,
        };
        record.state_since_unix_ms = Some(started_at_unix_ms);
        Ok(LifecycleOperation {
            deployment: deployment.into(),
            action,
            previous,
            records: Arc::clone(&self.records),
            completed: false,
            rollback_to_failed: action == LifecycleAction::BenchmarkReload,
        })
    }

    pub fn blocked_deployments(&self) -> BTreeSet<String> {
        let Ok(records) = self.records.lock() else {
            return BTreeSet::new();
        };
        records
            .iter()
            .filter(|(_, record)| {
                matches!(
                    record.state,
                    ModelLifecycleState::Loading
                        | ModelLifecycleState::Benchmarking
                        | ModelLifecycleState::Draining
                        | ModelLifecycleState::Unloading
                        | ModelLifecycleState::Failed
                )
            })
            .map(|(deployment, _)| deployment.clone())
            .collect()
    }

    pub fn project(&self, models: &mut [ModelLifecycleSnapshot]) {
        let Ok(records) = self.records.lock() else {
            return;
        };
        for model in models {
            let Some(record) = records.get(&model.deployment) else {
                continue;
            };
            model.active_reservations = record.active_reservations;
            model.state = record.state;
            model.state_since_unix_ms = record.state_since_unix_ms;
            model.last_used_unix_ms = record.last_used_unix_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[test]
    fn owner_transitions_cannot_skip_draining_or_loading() {
        assert!(ModelLifecycleState::Absent.allows_transition_to(ModelLifecycleState::Loading));
        assert!(
            ModelLifecycleState::Absent.allows_transition_to(ModelLifecycleState::Benchmarking)
        );
        assert!(ModelLifecycleState::Loading.allows_transition_to(ModelLifecycleState::Ready));
        assert!(ModelLifecycleState::Ready.allows_transition_to(ModelLifecycleState::Draining));
        assert!(ModelLifecycleState::Draining.allows_transition_to(ModelLifecycleState::Unloading));
        assert!(!ModelLifecycleState::Absent.allows_transition_to(ModelLifecycleState::Ready));
        assert!(!ModelLifecycleState::Ready.allows_transition_to(ModelLifecycleState::Unloading));
    }

    #[test]
    fn reservation_guard_releases_when_its_scope_ends() {
        let tracker = LifecycleTracker::new(["local".into()]);
        let mut models = vec![ModelLifecycleSnapshot {
            deployment: "local".into(),
            model_id: "qwen".into(),
            state: ModelLifecycleState::Unknown,
            resident_memory_bytes: None,
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }];
        {
            let _reservation = tracker.reserve("local", 42).unwrap().unwrap();
            tracker.project(&mut models);
            assert_eq!(models[0].active_reservations, 1);
            assert_eq!(models[0].last_used_unix_ms, Some(42));
        }
        tracker.project(&mut models);
        assert_eq!(models[0].active_reservations, 0);
    }

    #[test]
    fn unload_is_blocked_by_reservations_and_transitions_atomically() {
        let tracker = LifecycleTracker::new(["local".into()]);
        let mut models = vec![ModelLifecycleSnapshot {
            deployment: "local".into(),
            model_id: "qwen".into(),
            state: ModelLifecycleState::Ready,
            resident_memory_bytes: Some(100),
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }];
        tracker.reconcile(&models, 10);
        let reservation = tracker.reserve("local", 11).unwrap().unwrap();
        assert!(matches!(
            tracker.begin_action("local", LifecycleAction::Unload, 12),
            Err(LifecycleOperationError::ActiveReservations { .. })
        ));
        drop(reservation);
        let operation = tracker
            .begin_action("local", LifecycleAction::Unload, 13)
            .unwrap();
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Draining);
        assert!(matches!(
            tracker.reserve("local", 14),
            Err(LifecycleOperationError::Transitioning { .. })
        ));
        operation.complete(15);
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Absent);
    }

    #[test]
    fn reservation_and_eviction_race_remains_atomic_under_repetition() {
        for iteration in 0..512 {
            let tracker = LifecycleTracker::new(["local".into()]);
            let models = vec![ModelLifecycleSnapshot {
                deployment: "local".into(),
                model_id: "qwen".into(),
                state: ModelLifecycleState::Ready,
                resident_memory_bytes: Some(100),
                resident_vram_bytes: None,
                active_reservations: 0,
                state_since_unix_ms: None,
                last_used_unix_ms: None,
            }];
            tracker.reconcile(&models, 1);
            let start = Arc::new(Barrier::new(2));
            let hold = Arc::new(Barrier::new(2));

            let reservation_tracker = tracker.clone();
            let reservation_start = Arc::clone(&start);
            let reservation_hold = Arc::clone(&hold);
            let reservation = thread::spawn(move || {
                reservation_start.wait();
                let result = reservation_tracker.reserve("local", 2);
                let won = matches!(result, Ok(Some(_)));
                reservation_hold.wait();
                drop(result);
                won
            });

            let eviction_tracker = tracker.clone();
            let eviction = thread::spawn(move || {
                start.wait();
                let result = eviction_tracker.begin_action("local", LifecycleAction::Unload, 2);
                let won = result.is_ok();
                hold.wait();
                drop(result);
                won
            });

            let reservation_won = reservation.join().unwrap();
            let eviction_won = eviction.join().unwrap();
            assert_ne!(
                reservation_won, eviction_won,
                "exactly one side must win iteration {iteration}"
            );
        }
    }

    #[test]
    fn failed_native_action_rolls_back_its_lifecycle_state() {
        let tracker = LifecycleTracker::new(["local".into()]);
        let mut models = vec![ModelLifecycleSnapshot {
            deployment: "local".into(),
            model_id: "qwen".into(),
            state: ModelLifecycleState::Absent,
            resident_memory_bytes: None,
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }];
        tracker.reconcile(&models, 1);
        {
            let _operation = tracker
                .begin_action("local", LifecycleAction::Load, 2)
                .unwrap();
            tracker.project(&mut models);
            assert_eq!(models[0].state, ModelLifecycleState::Loading);
        }
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Absent);
    }

    #[test]
    fn reload_benchmark_is_exclusive_and_returns_to_absent() {
        let tracker = LifecycleTracker::new(["local".into()]);
        let mut models = vec![ModelLifecycleSnapshot {
            deployment: "local".into(),
            model_id: "qwen".into(),
            state: ModelLifecycleState::Absent,
            resident_memory_bytes: None,
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }];
        tracker.reconcile(&models, 1);
        let operation = tracker
            .begin_action("local", LifecycleAction::BenchmarkReload, 2)
            .unwrap();
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Benchmarking);
        assert!(matches!(
            tracker.reserve("local", 3),
            Err(LifecycleOperationError::Transitioning { .. })
        ));
        operation.complete(4);
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Absent);
    }

    #[test]
    fn stale_observation_cannot_reopen_a_deployment_while_it_drains() {
        let tracker = LifecycleTracker::new(["local".into()]);
        let mut models = vec![ModelLifecycleSnapshot {
            deployment: "local".into(),
            model_id: "qwen".into(),
            state: ModelLifecycleState::Ready,
            resident_memory_bytes: Some(100),
            resident_vram_bytes: None,
            active_reservations: 0,
            state_since_unix_ms: None,
            last_used_unix_ms: None,
        }];
        tracker.reconcile(&models, 1);
        let operation = tracker
            .begin_action("local", LifecycleAction::Unload, 2)
            .unwrap();
        tracker.reconcile(&models, 3);
        tracker.project(&mut models);
        assert_eq!(models[0].state, ModelLifecycleState::Draining);
        drop(operation);
    }
}
