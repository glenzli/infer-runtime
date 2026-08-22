//! Layered authorization and request narrowing for named model routes.
//!
//! Intent remains the public operation identity. These types authorize a
//! Consumer to narrow execution to Runtime-owned Deployment or Model Profile
//! identities without exposing provider-native physical model strings.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ContractError, RuntimeConfig};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoutingGrantConfig {
    /// Candidates admitted for ordinary capability routing. These are also
    /// valid named targets, so a Consumer can always narrow to its default
    /// execution surface.
    #[serde(default)]
    pub deployment_ids: BTreeSet<String>,
    #[serde(default)]
    pub model_profile_ids: BTreeSet<String>,
    /// Additional Runtime-owned deployment identities that may be selected
    /// only when the Consumer explicitly sends `infer.deployment_ids`.
    /// They never become ordinary capability-routing candidates.
    #[serde(default)]
    pub named_deployment_ids: BTreeSet<String>,
    /// The Model Profile counterpart of `named_deployment_ids`.
    #[serde(default)]
    pub named_model_profile_ids: BTreeSet<String>,
}

impl RoutingGrantConfig {
    pub fn allows_deployment(&self, deployment_id: &str, model_profile_id: &str) -> bool {
        self.deployment_ids.contains(deployment_id)
            || self.model_profile_ids.contains(model_profile_id)
    }

    pub fn allows_named_deployment(&self, deployment_id: &str, model_profile_id: &str) -> bool {
        self.allows_deployment(deployment_id, model_profile_id)
            || self.named_deployment_ids.contains(deployment_id)
            || self.named_model_profile_ids.contains(model_profile_id)
    }

    pub fn allows_request(&self, request: &NamedRouteRequest, config: &RuntimeConfig) -> bool {
        match request {
            NamedRouteRequest::Deployments(ids) => ids.iter().all(|id| {
                self.named_deployment_ids.contains(id)
                    || self.deployment_ids.contains(id)
                    || config.deployments.get(id).is_some_and(|deployment| {
                        let profile = &config.model_builds[&deployment.build].profile;
                        self.model_profile_ids.contains(profile)
                            || self.named_model_profile_ids.contains(profile)
                    })
            }),
            NamedRouteRequest::ModelProfiles(ids) => ids.iter().all(|id| {
                self.model_profile_ids.contains(id) || self.named_model_profile_ids.contains(id)
            }),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppRoutingConfig {
    #[serde(default)]
    pub deployment_ids: BTreeSet<String>,
    #[serde(default)]
    pub model_profile_ids: BTreeSet<String>,
    #[serde(default)]
    pub named_deployment_ids: BTreeSet<String>,
    #[serde(default)]
    pub named_model_profile_ids: BTreeSet<String>,
    #[serde(default)]
    pub intents: BTreeMap<String, RoutingGrantConfig>,
}

impl AppRoutingConfig {
    pub fn global_grant(&self) -> RoutingGrantConfig {
        RoutingGrantConfig {
            deployment_ids: self.deployment_ids.clone(),
            model_profile_ids: self.model_profile_ids.clone(),
            named_deployment_ids: self.named_deployment_ids.clone(),
            named_model_profile_ids: self.named_model_profile_ids.clone(),
        }
    }

    /// An Intent-specific rule replaces, rather than merges with, the global
    /// grant. The global grant is consulted only when no specific rule exists.
    pub fn grant_for(&self, intent: &str) -> RoutingGrantConfig {
        self.intents
            .get(intent)
            .cloned()
            .unwrap_or_else(|| self.global_grant())
    }

    pub(crate) fn validate(
        &self,
        app_id: &str,
        config: &RuntimeConfig,
    ) -> Result<(), ContractError> {
        validate_grant(app_id, "global", &self.global_grant(), config)?;
        for (intent, grant) in &self.intents {
            if !config.intents.contains_key(intent) {
                return Err(configuration(format!(
                    "app {app_id} routing rule names unknown intent {intent}"
                )));
            }
            validate_grant(app_id, intent, grant, config)?;
            for deployment_id in grant
                .deployment_ids
                .iter()
                .chain(grant.named_deployment_ids.iter())
            {
                let deployment = &config.deployments[deployment_id];
                let profile = &config.model_builds[&deployment.build].profile;
                if !config.model_profiles[profile].ratings.contains_key(intent) {
                    return Err(configuration(format!(
                        "app {app_id} routing rule for {intent} grants incompatible deployment {deployment_id}"
                    )));
                }
            }
            for profile_id in grant
                .model_profile_ids
                .iter()
                .chain(grant.named_model_profile_ids.iter())
            {
                if !config.model_profiles[profile_id]
                    .ratings
                    .contains_key(intent)
                {
                    return Err(configuration(format!(
                        "app {app_id} routing rule for {intent} grants incompatible model profile {profile_id}"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_grant(
    app_id: &str,
    scope: &str,
    grant: &RoutingGrantConfig,
    config: &RuntimeConfig,
) -> Result<(), ContractError> {
    for deployment in &grant.deployment_ids {
        if !config.deployments.contains_key(deployment) {
            return Err(configuration(format!(
                "app {app_id} routing {scope} grants unknown deployment {deployment}"
            )));
        }
    }
    for profile in &grant.model_profile_ids {
        if !config.model_profiles.contains_key(profile) {
            return Err(configuration(format!(
                "app {app_id} routing {scope} grants unknown model profile {profile}"
            )));
        }
    }
    for deployment in &grant.named_deployment_ids {
        if !config.deployments.contains_key(deployment) {
            return Err(configuration(format!(
                "app {app_id} routing {scope} grants unknown named deployment {deployment}"
            )));
        }
    }
    for profile in &grant.named_model_profile_ids {
        if !config.model_profiles.contains_key(profile) {
            return Err(configuration(format!(
                "app {app_id} routing {scope} grants unknown named model profile {profile}"
            )));
        }
    }
    Ok(())
}

fn configuration(message: String) -> ContractError {
    ContractError::Configuration(message)
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "ordered_ids", rename_all = "snake_case")]
pub enum NamedRouteRequest {
    #[serde(rename = "deployment")]
    Deployments(Vec<String>),
    #[serde(rename = "model_profile")]
    ModelProfiles(Vec<String>),
}

impl NamedRouteRequest {
    pub fn rank(&self, deployment_id: &str, model_profile_id: &str) -> Option<usize> {
        match self {
            Self::Deployments(ids) => ids.iter().position(|id| id == deployment_id),
            Self::ModelProfiles(ids) => ids.iter().position(|id| id == model_profile_id),
        }
    }
}

pub(crate) fn parse_ordered_ids(value: &str) -> Result<Vec<String>, &'static str> {
    let ids = value.split(',').map(str::trim).collect::<Vec<_>>();
    if ids.is_empty()
        || ids.len() > 16
        || ids.iter().any(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err("expected 1-16 unique comma-separated Runtime IDs");
    }
    Ok(ids.into_iter().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_rule_replaces_routing_default_and_empty_rule_denies_all() {
        let routing = AppRoutingConfig {
            deployment_ids: BTreeSet::from(["global-deployment".into()]),
            model_profile_ids: BTreeSet::from(["global-profile".into()]),
            named_deployment_ids: BTreeSet::new(),
            named_model_profile_ids: BTreeSet::new(),
            intents: BTreeMap::from([
                (
                    "text.edit".into(),
                    RoutingGrantConfig {
                        deployment_ids: BTreeSet::from(["editor".into()]),
                        model_profile_ids: BTreeSet::new(),
                        ..Default::default()
                    },
                ),
                ("text.deny".into(), RoutingGrantConfig::default()),
            ]),
        };
        assert!(
            routing
                .grant_for("text.other")
                .deployment_ids
                .contains("global-deployment")
        );
        let edit = routing.grant_for("text.edit");
        assert!(edit.deployment_ids.contains("editor"));
        assert!(!edit.deployment_ids.contains("global-deployment"));
        assert_eq!(
            routing.grant_for("text.deny"),
            RoutingGrantConfig::default()
        );
    }

    #[test]
    fn ordered_named_request_is_bounded_unique_and_one_kind_only() {
        assert_eq!(
            parse_ordered_ids("first,second").unwrap(),
            vec!["first", "second"]
        );
        for invalid in ["", "first,first", "first,,second", "bad/id"] {
            assert!(parse_ordered_ids(invalid).is_err());
        }
    }

    #[test]
    fn named_only_grant_never_enters_ordinary_candidate_routing() {
        let grant = RoutingGrantConfig {
            deployment_ids: BTreeSet::from(["luna".into()]),
            named_deployment_ids: BTreeSet::from(["terra".into()]),
            ..Default::default()
        };
        assert!(grant.allows_deployment("luna", "luna-profile"));
        assert!(!grant.allows_deployment("terra", "terra-profile"));
        assert!(grant.allows_named_deployment("terra", "terra-profile"));
    }
}
