//! Read-only projection of named text routes granted to one Consumer.
//! Discovery never admits a provider-native model or changes routing grants.
use super::{Runtime, RuntimeError};
use infer_core::{
    CapabilityLevel, ExecutionMode, Modality, Placement, ProviderAccessClass, ProviderCapability,
    ReasoningEffort,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct ConsumerModelCatalog {
    pub deployments: Vec<ConsumerModelDeployment>,
}
#[derive(Debug, Serialize)]
pub struct ConsumerModelDeployment {
    pub deployment_id: String,
    pub model_profile: String,
    pub provider_access_class: ProviderAccessClass,
    pub placement: Placement,
    pub supported_efforts: Vec<ReasoningEffort>,
    pub intents: BTreeMap<String, CapabilityLevel>,
    /// Last observation, independent of configuration admission and execution health.
    pub availability: &'static str,
    pub observation_stale: bool,
}
impl Runtime {
    pub async fn consumer_model_catalog(
        &self,
        app_id: &str,
    ) -> Result<ConsumerModelCatalog, RuntimeError> {
        let app = self
            .config
            .apps
            .get(app_id)
            .ok_or_else(|| RuntimeError::UnknownApp(app_id.into()))?;
        let mut observations = BTreeMap::new();
        for (id, provider) in &self.config.providers {
            if app.allows_provider_access(provider.access_class) {
                if let Some(snapshot) = self.subscription_models.snapshot(id).await {
                    observations.insert(id.clone(), snapshot);
                }
            }
        }
        let mut deployments = Vec::new();
        for (id, deployment) in &self.config.deployments {
            let provider = &self.config.providers[&deployment.provider];
            let build = &self.config.model_builds[&deployment.build];
            let profile = &self.config.model_profiles[&build.profile];
            if !app.allows_provider_access(provider.access_class)
                || !provider
                    .capability_profile
                    .supports(ProviderCapability::Responses)
                || !deployment
                    .supported_execution_modes
                    .contains(&ExecutionMode::Unary)
                || !build.input_modalities.contains(&Modality::Text)
                || !build.output_modalities.contains(&Modality::Text)
                || (provider.placement == Placement::Cloud
                    && !app.allowed_cloud_input_modalities.contains(&Modality::Text))
            {
                continue;
            }
            let intents: BTreeMap<_, _> = profile
                .ratings
                .iter()
                .filter_map(|(intent, rating)| {
                    let definition = self.config.intent(intent)?;
                    if definition.data_plane != "responses"
                        || !app.allows_intent(intent)
                        || !app
                            .routing
                            .as_ref()?
                            .grant_for(intent)
                            .allows_named_deployment(id, &build.profile)
                    {
                        return None;
                    }
                    Some((intent.clone(), rating.level))
                })
                .collect();
            if intents.is_empty() {
                continue;
            }
            let snapshot = observations.get(&deployment.provider);
            let status = snapshot.and_then(|s| {
                s.configured_deployments
                    .iter()
                    .find(|d| d.deployment == *id)
            });
            let availability = match status.map(|s| s.presence) {
                Some(super::subscription_models::ModelPresence::Available) => "available",
                Some(
                    super::subscription_models::ModelPresence::MissingSuspected
                    | super::subscription_models::ModelPresence::MissingConfirmed,
                ) => "unavailable",
                _ => "unknown",
            };
            deployments.push(ConsumerModelDeployment {
                deployment_id: id.clone(),
                model_profile: build.profile.clone(),
                provider_access_class: provider.access_class,
                placement: provider.placement,
                supported_efforts: deployment.supported_efforts.clone(),
                intents,
                availability,
                observation_stale: snapshot.is_some_and(|s| s.observation_error),
            });
        }
        Ok(ConsumerModelCatalog { deployments })
    }
}
