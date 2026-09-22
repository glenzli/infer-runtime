//! Observed subscription model availability and operator lifecycle projection.
//!
//! A complete provider-native catalog is evidence about one instant, not a
//! change to static admission. Failed observations never create missing-model
//! evidence, and one missing model never opens the provider-wide circuit.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use infer_core::{
    Fallback, NamedRouteRequest, ProviderKind, RequestConstraints, RoutingGrantConfig,
    RuntimeConfig,
};
use infer_provider::{DynProvider, ProviderModelCatalog, ProviderModelInfo};
use serde::Serialize;
use tokio::{
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval_at, timeout},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPresence {
    Unknown,
    Available,
    MissingSuspected,
    MissingConfirmed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfiguredModelStatus {
    pub deployment: String,
    pub model: String,
    pub presence: ModelPresence,
    pub first_missing_unix_ms: Option<u64>,
    pub consecutive_missing_observations: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionModelSnapshot {
    pub provider: String,
    pub models: Vec<ProviderModelInfo>,
    pub configured_deployments: Vec<ConfiguredModelStatus>,
    pub last_success_unix_ms: Option<u64>,
    /// A failed refresh is not evidence that any model has been retired.
    pub observation_error: bool,
}

#[derive(Default)]
struct Observed {
    last_attempt: Option<Instant>,
    last_success_unix_ms: Option<u64>,
    catalog: Option<ProviderModelCatalog>,
    missing_counts: BTreeMap<String, u32>,
    first_missing_unix_ms: BTreeMap<String, u64>,
    last_missing_unix_ms: BTreeMap<String, u64>,
    observation_error: bool,
}

struct SubscriptionProvider {
    provider: DynProvider,
    configured: BTreeMap<String, String>,
    observed: Mutex<Observed>,
}

pub struct SubscriptionModels {
    providers: BTreeMap<String, SubscriptionProvider>,
}

impl SubscriptionModels {
    pub fn new(config: &RuntimeConfig, providers: &BTreeMap<String, DynProvider>) -> Arc<Self> {
        let mut entries = BTreeMap::new();
        for (id, provider_config) in &config.providers {
            if provider_config.kind != ProviderKind::CodexAppServer {
                continue;
            }
            let Some(provider) = providers.get(id) else {
                continue;
            };
            let configured = config
                .deployments
                .iter()
                .filter(|(_, deployment)| deployment.provider == *id)
                .map(|(deployment_id, deployment)| {
                    (
                        deployment_id.clone(),
                        config.model_builds[&deployment.build].model_id.clone(),
                    )
                })
                .collect();
            entries.insert(
                id.clone(),
                SubscriptionProvider {
                    provider: Arc::clone(provider),
                    configured,
                    observed: Mutex::new(Observed::default()),
                },
            );
        }
        Arc::new(Self { providers: entries })
    }

    pub fn contains(&self, provider_id: &str) -> bool {
        self.providers.contains_key(provider_id)
    }

    /// A dispatch-time catalog may be newer than the routing observation.
    /// Recheck on the next admission without treating the miss as a provider
    /// outage or synthesizing an incomplete catalog.
    pub async fn invalidate(&self, provider_id: &str) {
        if let Some(entry) = self.providers.get(provider_id) {
            entry.observed.lock().await.last_attempt = None;
        }
    }

    /// The same single-flight refresh serves routing and the operator view.
    /// A failed read retains the last complete observation.
    pub async fn refresh_if_due(&self, provider_id: &str) {
        self.refresh(provider_id, false).await;
    }

    /// An explicit operator read may request a current complete catalog.
    pub async fn refresh_now(&self, provider_id: &str) {
        self.refresh(provider_id, true).await;
    }

    async fn refresh(&self, provider_id: &str, force: bool) {
        let Some(entry) = self.providers.get(provider_id) else {
            return;
        };
        let mut observed = entry.observed.lock().await;
        if !force
            && observed
                .last_attempt
                .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL)
        {
            return;
        }
        observed.last_attempt = Some(Instant::now());
        match timeout(OBSERVATION_TIMEOUT, entry.provider.model_catalog()).await {
            Ok(Ok(Some(catalog))) => {
                let now = unix_ms();
                let present = catalog
                    .models
                    .iter()
                    .map(|model| model.model.as_str())
                    .collect::<BTreeSet<_>>();
                for model in entry.configured.values() {
                    if present.contains(model.as_str()) {
                        observed.missing_counts.remove(model);
                        observed.first_missing_unix_ms.remove(model);
                        observed.last_missing_unix_ms.remove(model);
                    } else {
                        let spaced = observed.last_missing_unix_ms.get(model).is_none_or(|last| {
                            now.saturating_sub(*last) >= REFRESH_INTERVAL.as_millis() as u64
                        });
                        if spaced {
                            let count = observed.missing_counts.entry(model.clone()).or_default();
                            *count = count.saturating_add(1);
                            observed.last_missing_unix_ms.insert(model.clone(), now);
                        }
                        observed
                            .first_missing_unix_ms
                            .entry(model.clone())
                            .or_insert(now);
                    }
                }
                observed.catalog = Some(catalog);
                observed.last_success_unix_ms = Some(now);
                observed.observation_error = false;
            }
            _ => observed.observation_error = true,
        }
    }

    pub async fn unavailable_deployments(&self) -> BTreeSet<String> {
        let mut unavailable = BTreeSet::new();
        for entry in self.providers.values() {
            let observed = entry.observed.lock().await;
            let Some(catalog) = observed.catalog.as_ref() else {
                continue;
            };
            let present = catalog
                .models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<BTreeSet<_>>();
            for (deployment, model) in &entry.configured {
                if !present.contains(model.as_str()) {
                    unavailable.insert(deployment.clone());
                }
            }
        }
        unavailable
    }

    pub async fn snapshot(&self, provider_id: &str) -> Option<SubscriptionModelSnapshot> {
        let entry = self.providers.get(provider_id)?;
        let observed = entry.observed.lock().await;
        let present = observed
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .models
                    .iter()
                    .map(|model| model.model.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let configured_deployments = entry
            .configured
            .iter()
            .map(|(deployment, model)| {
                let count = observed.missing_counts.get(model).copied().unwrap_or(0);
                ConfiguredModelStatus {
                    deployment: deployment.clone(),
                    model: model.clone(),
                    presence: if observed.catalog.is_none() {
                        ModelPresence::Unknown
                    } else if present.contains(model.as_str()) {
                        ModelPresence::Available
                    } else if count >= 2 {
                        ModelPresence::MissingConfirmed
                    } else {
                        ModelPresence::MissingSuspected
                    },
                    first_missing_unix_ms: observed.first_missing_unix_ms.get(model).copied(),
                    consecutive_missing_observations: count,
                }
            })
            .collect();
        Some(SubscriptionModelSnapshot {
            provider: provider_id.to_owned(),
            models: observed
                .catalog
                .as_ref()
                .map(|catalog| catalog.models.clone())
                .unwrap_or_default(),
            configured_deployments,
            last_success_unix_ms: observed.last_success_unix_ms,
            observation_error: observed.observation_error,
        })
    }

    pub fn spawn(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move { refresh_loop(weak).await });
    }
}

/// Preserve the Consumer's first choice. A configured successor is added only
/// when that choice was observed missing and fallback was explicitly allowed.
pub fn expand_named_successor(
    constraints: &mut RequestConstraints,
    grant: Option<&RoutingGrantConfig>,
    missing: &BTreeSet<String>,
) -> Option<String> {
    let source = match constraints.named_route.as_ref() {
        Some(NamedRouteRequest::Deployments(ids)) => {
            ids.first().filter(|id| missing.contains(*id)).cloned()
        }
        _ => None,
    }?;
    if constraints
        .fallback
        .is_some_and(|fallback| fallback != Fallback::None)
        && let Some(successor) = grant.and_then(|grant| grant.successor_deployments.get(&source))
        && let Some(NamedRouteRequest::Deployments(ids)) = constraints.named_route.as_mut()
        && !ids.contains(successor)
    {
        ids.insert(1, successor.clone());
    }
    Some(source)
}

async fn refresh_loop(models: Weak<SubscriptionModels>) {
    let mut ticks = interval_at(Instant::now(), REFRESH_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let Some(models) = models.upgrade() else {
            break;
        };
        for provider in models.providers.keys() {
            models.refresh_if_due(provider).await;
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use async_trait::async_trait;
    use infer_core::{NamedRouteRequest, RequestConstraints};
    use infer_provider::{Provider, ProviderByteStream, ProviderError};
    use serde_json::Value;

    use super::*;

    struct FakeCatalog {
        observations: Mutex<VecDeque<Result<Vec<String>, ()>>>,
    }

    #[async_trait]
    impl Provider for FakeCatalog {
        fn id(&self) -> &str {
            "codex-subscription"
        }

        async fn execute(&self, _: infer_core::ResponsesRequest) -> Result<Value, ProviderError> {
            Err(ProviderError::InvalidInput("test provider".into()))
        }

        async fn execute_stream(
            &self,
            _: infer_core::ResponsesRequest,
        ) -> Result<ProviderByteStream, ProviderError> {
            Err(ProviderError::InvalidInput("test provider".into()))
        }

        async fn model_catalog(&self) -> Result<Option<ProviderModelCatalog>, ProviderError> {
            let next = self
                .observations
                .lock()
                .await
                .pop_front()
                .expect("test observation");
            let models = next.map_err(|_| ProviderError::Protocol("test failure".into()))?;
            Ok(Some(ProviderModelCatalog {
                provider: self.id().into(),
                models: models
                    .into_iter()
                    .map(|model| ProviderModelInfo {
                        id: model.clone(),
                        model,
                        display_name: String::new(),
                        description: String::new(),
                        input_modalities: Vec::new(),
                        supported_reasoning_efforts: Vec::new(),
                        default_reasoning_effort: String::new(),
                        is_default: false,
                        hidden: false,
                        upgrade: None,
                        admitted: true,
                    })
                    .collect(),
            }))
        }
    }

    #[tokio::test]
    async fn complete_absence_is_model_scoped_and_failed_reads_do_not_confirm_it() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let all_models = config
            .deployments
            .values()
            .filter(|deployment| deployment.provider == "codex-subscription")
            .map(|deployment| config.model_builds[&deployment.build].model_id.clone())
            .collect::<BTreeSet<_>>();
        let without_old_sol = all_models
            .iter()
            .filter(|model| model.as_str() != "gpt-5.6-sol")
            .cloned()
            .collect::<Vec<_>>();
        let fake = Arc::new(FakeCatalog {
            observations: Mutex::new(VecDeque::from([
                Ok(without_old_sol.clone()),
                Err(()),
                Ok(without_old_sol),
                Ok(all_models.into_iter().collect()),
            ])),
        });
        let provider: DynProvider = fake;
        let models = SubscriptionModels::new(
            &config,
            &BTreeMap::from([("codex-subscription".into(), provider)]),
        );
        models.refresh_if_due("codex-subscription").await;
        let snapshot = models.snapshot("codex-subscription").await.unwrap();
        let old_sol = snapshot
            .configured_deployments
            .iter()
            .find(|model| model.model == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(old_sol.presence, ModelPresence::MissingSuspected);
        assert_eq!(
            models.unavailable_deployments().await,
            BTreeSet::from(["codex_gpt_5_6_sol".into()])
        );

        models.invalidate("codex-subscription").await;
        models
            .providers
            .get("codex-subscription")
            .unwrap()
            .observed
            .lock()
            .await
            .last_missing_unix_ms
            .insert("gpt-5.6-sol".into(), 0);
        models.refresh_if_due("codex-subscription").await;
        let snapshot = models.snapshot("codex-subscription").await.unwrap();
        assert!(snapshot.observation_error);
        assert_eq!(
            snapshot
                .configured_deployments
                .iter()
                .find(|model| model.model == "gpt-5.6-sol")
                .unwrap()
                .consecutive_missing_observations,
            1
        );

        models.invalidate("codex-subscription").await;
        models.refresh_if_due("codex-subscription").await;
        let snapshot = models.snapshot("codex-subscription").await.unwrap();
        assert_eq!(
            snapshot
                .configured_deployments
                .iter()
                .find(|model| model.model == "gpt-5.6-sol")
                .unwrap()
                .presence,
            ModelPresence::MissingConfirmed
        );

        models.invalidate("codex-subscription").await;
        models.refresh_if_due("codex-subscription").await;
        assert!(models.unavailable_deployments().await.is_empty());
    }

    #[tokio::test]
    async fn a_failed_first_read_is_unknown_not_missing() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let provider: DynProvider = Arc::new(FakeCatalog {
            observations: Mutex::new(VecDeque::from([Err(())])),
        });
        let models = SubscriptionModels::new(
            &config,
            &BTreeMap::from([("codex-subscription".into(), provider)]),
        );
        models.refresh_if_due("codex-subscription").await;
        let snapshot = models.snapshot("codex-subscription").await.unwrap();
        assert!(snapshot.observation_error);
        assert!(snapshot.last_success_unix_ms.is_none());
        assert!(
            snapshot
                .configured_deployments
                .iter()
                .all(|model| model.presence == ModelPresence::Unknown)
        );
        assert!(models.unavailable_deployments().await.is_empty());
    }

    #[tokio::test]
    async fn operator_refresh_bypasses_the_hourly_cache() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let fake = Arc::new(FakeCatalog {
            observations: Mutex::new(VecDeque::from([
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(vec!["gpt-5.6-sol".into()]),
            ])),
        });
        let provider: DynProvider = fake.clone();
        let models = SubscriptionModels::new(
            &config,
            &BTreeMap::from([("codex-subscription".into(), provider)]),
        );
        models.refresh_if_due("codex-subscription").await;
        models
            .providers
            .get("codex-subscription")
            .unwrap()
            .observed
            .lock()
            .await
            .last_attempt = Some(Instant::now() - Duration::from_secs(30 * 60));
        models.refresh_if_due("codex-subscription").await;
        assert_eq!(fake.observations.lock().await.len(), 2);
        models
            .providers
            .get("codex-subscription")
            .unwrap()
            .observed
            .lock()
            .await
            .last_attempt = Some(Instant::now() - Duration::from_secs(61 * 60));
        models.refresh_if_due("codex-subscription").await;
        assert_eq!(fake.observations.lock().await.len(), 1);
        models.refresh_now("codex-subscription").await;
        let snapshot = models.snapshot("codex-subscription").await.unwrap();
        assert_eq!(
            snapshot
                .configured_deployments
                .iter()
                .find(|model| model.model == "gpt-5.6-sol")
                .unwrap()
                .presence,
            ModelPresence::Available
        );
        assert!(fake.observations.lock().await.is_empty());
    }

    #[test]
    fn successor_requires_observed_absence_and_explicit_fallback() {
        let grant = RoutingGrantConfig {
            successor_deployments: BTreeMap::from([("old".into(), "new".into())]),
            ..Default::default()
        };
        let missing = BTreeSet::from(["old".into()]);
        let mut constraints = RequestConstraints {
            named_route: Some(NamedRouteRequest::Deployments(vec!["old".into()])),
            ..Default::default()
        };
        assert_eq!(
            expand_named_successor(&mut constraints, Some(&grant), &missing),
            Some("old".into())
        );
        assert_eq!(
            constraints.named_route,
            Some(NamedRouteRequest::Deployments(vec!["old".into()]))
        );
        constraints.fallback = Some(Fallback::Equivalent);
        assert_eq!(
            expand_named_successor(&mut constraints, Some(&grant), &missing),
            Some("old".into())
        );
        assert_eq!(
            constraints.named_route,
            Some(NamedRouteRequest::Deployments(vec![
                "old".into(),
                "new".into()
            ]))
        );
    }
}
