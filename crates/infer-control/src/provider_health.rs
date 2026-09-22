//! In-memory provider failure policy and circuit state.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use infer_provider::{ProviderError, ProviderFailureKind};

const FAILURE_THRESHOLD: u32 = 3;
const OPEN_DURATION: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ProviderHealth {
    states: Mutex<BTreeMap<String, State>>,
}

#[derive(Default)]
struct State {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

impl ProviderHealth {
    pub fn unavailable_providers(&self) -> BTreeSet<String> {
        let now = Instant::now();
        let mut states = self.states.lock().expect("provider health mutex poisoned");
        states.retain(|_, state| {
            state.open_until.is_none_or(|until| until > now) || state.consecutive_failures > 0
        });
        states
            .iter()
            .filter_map(|(id, state)| {
                state
                    .open_until
                    .is_some_and(|until| until > now)
                    .then_some(id.clone())
            })
            .collect()
    }
    pub fn record_success(&self, provider: &str) {
        self.states
            .lock()
            .expect("provider health mutex poisoned")
            .remove(provider);
    }
    pub fn record_failure(&self, provider: &str, error: &ProviderError) {
        if error.model_missing() {
            return;
        }
        if !matches!(
            error.kind(),
            ProviderFailureKind::Unavailable
                | ProviderFailureKind::Timeout
                | ProviderFailureKind::RateLimited
        ) {
            return;
        }
        let mut states = self.states.lock().expect("provider health mutex poisoned");
        let state = states.entry(provider.into()).or_default();
        state.consecutive_failures += 1;
        if state.consecutive_failures >= FAILURE_THRESHOLD {
            state.open_until = Some(Instant::now() + OPEN_DURATION);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opens_after_three_retryable_failures() {
        let health = ProviderHealth::default();
        for _ in 0..3 {
            health.record_failure(
                "cloud",
                &ProviderError::Upstream {
                    status: 503,
                    body: String::new(),
                    retry_after: None,
                },
            );
        }
        assert!(health.unavailable_providers().contains("cloud"));
        health.record_success("cloud");
        assert!(!health.unavailable_providers().contains("cloud"));
    }

    #[test]
    fn a_missing_model_does_not_open_the_shared_provider_circuit() {
        let health = ProviderHealth::default();
        for _ in 0..3 {
            health.record_failure(
                "codex",
                &ProviderError::ModelMissing {
                    model: "old".into(),
                },
            );
        }
        assert!(health.unavailable_providers().is_empty());
    }
}
