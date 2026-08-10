//! Per-App admission ownership, independent from provider queue scheduling.

use std::{collections::BTreeMap, sync::Arc};

use infer_core::AppConfig;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Holds one pending-Job slot until the Job reaches a terminal state.
#[derive(Debug)]
pub struct AppAdmissionPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct AppAdmission {
    limits: BTreeMap<String, Arc<Semaphore>>,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AppAdmissionError {
    #[error("application pending Job limit has been reached")]
    PendingLimitReached,
}

impl AppAdmission {
    pub fn new(apps: &BTreeMap<String, AppConfig>) -> Self {
        Self {
            limits: apps
                .iter()
                .map(|(id, app)| (id.clone(), Arc::new(Semaphore::new(app.max_pending_jobs))))
                .collect(),
        }
    }

    pub fn try_admit(&self, app_id: &str) -> Result<AppAdmissionPermit, AppAdmissionError> {
        let limit = self
            .limits
            .get(app_id)
            .expect("validated App must have an admission limit")
            .clone();
        limit
            .try_acquire_owned()
            .map(|permit| AppAdmissionPermit { _permit: permit })
            .map_err(|_| AppAdmissionError::PendingLimitReached)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use infer_core::{AppConfig, RequestOverrideConfig};

    use super::*;

    #[test]
    fn release_at_terminal_state_reopens_the_app_limit() {
        let admission = AppAdmission::new(&BTreeMap::from([(
            "test-app".into(),
            AppConfig {
                credential: infer_core::AppCredentialConfig::Environment {
                    variable: "INFER_TEST_TOKEN".into(),
                },
                observer_access: infer_core::ObserverAccess::None,
                resource_admin: false,
                allowed_intents: None,
                allowed_provider_access_classes: std::collections::BTreeSet::from([
                    infer_core::ProviderAccessClass::Standard,
                ]),
                allowed_cloud_input_modalities: std::collections::BTreeSet::from([
                    infer_core::Modality::Text,
                ]),
                max_pending_jobs: 1,
                default_policy: None,
                allowed_policies: vec![],
                request_overrides: RequestOverrideConfig::default(),
            },
        )]));
        let permit = admission.try_admit("test-app").unwrap();
        assert_eq!(
            admission.try_admit("test-app").unwrap_err(),
            AppAdmissionError::PendingLimitReached
        );
        drop(permit);
        admission.try_admit("test-app").unwrap();
    }
}
