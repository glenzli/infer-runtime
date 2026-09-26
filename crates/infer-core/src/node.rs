//! Explicitly paired, single-hop unary text and bounded Apple image nodes. No discovery grants access.

use crate::config::is_loopback_http_url;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeTlsConfig {
    pub certificate: String,
    pub private_key: String,
    pub ca_certificate: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePeerConfig {
    pub node_id: String,
    /// Numeric socket address; name verification is separate from resolution.
    pub address: String,
    pub server_name: String,
    pub certificate_sha256: String,
    pub tls: NodeTlsConfig,
    /// Operator-approved remote deployment IDs and immutable contract digests.
    pub imports: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExportConfig {
    pub deployment: String,
    pub intent: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeServerConfig {
    pub node_id: String,
    pub bind: String,
    pub tls: NodeTlsConfig,
    /// Owner-only JSON pairing grants. Reloaded for authorization and revocation.
    pub peers_file: String,
    pub max_active: usize,
    pub exports: BTreeMap<String, NodeExportConfig>,
}

pub fn valid_node_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

use crate::{
    ContractError, DeploymentResourceEstimateConfig, ExecutionMode, Modality, Placement,
    ProviderAccessClass, ProviderCapability, ProviderConfig, ProviderKind, RuntimeConfig,
};
use std::collections::BTreeSet;
fn configuration(message: impl Into<String>) -> ContractError {
    ContractError::Configuration(message.into())
}

pub(crate) fn validate_nodes(config: &RuntimeConfig) -> Result<(), ContractError> {
    for deployment in config.deployments.values() {
        let provider = &config.providers[&deployment.provider];
        if provider.kind != ProviderKind::TrustedNode {
            continue;
        }
        let build = &config.model_builds[&deployment.build];
        if !provider
            .node
            .as_ref()
            .is_some_and(|peer| peer.imports.contains_key(&build.model_id))
            || !((build.input_modalities == [Modality::Text]
                && build.output_modalities == [Modality::Text])
                || (build.input_modalities == [Modality::Image]
                    && build.output_modalities == [Modality::Json]
                    && config.model_profiles[&build.profile]
                        .ratings
                        .keys()
                        .all(|intent| {
                            crate::is_apple_image_plane(&config.intents[intent].data_plane)
                        })))
            || deployment.supported_execution_modes != BTreeSet::from([ExecutionMode::Unary])
            || deployment.resource_estimate != DeploymentResourceEstimateConfig::default()
        {
            return Err(configuration(
                "trusted-node imports require approved unary text or bounded Apple image deployments",
            ));
        }
    }
    if let Some(node) = &config.node_server {
        if node.node_id.is_empty()
            || node.bind.parse::<std::net::SocketAddr>().is_err()
            || !(1..=64).contains(&node.max_active)
            || node.exports.is_empty()
            || node.exports.len() > 64
        {
            return Err(configuration(
                "invalid node listener identity, capacity or exports",
            ));
        }
        for export in node.exports.values() {
            let deployment = config
                .deployments
                .get(&export.deployment)
                .ok_or_else(|| configuration("unknown node export deployment"))?;
            let provider = &config.providers[&deployment.provider];
            let build = &config.model_builds[&deployment.build];
            let intent = config
                .intents
                .get(&export.intent)
                .ok_or_else(|| configuration("unknown node export intent"))?;
            if provider.placement != Placement::Local
                || !((provider.kind == ProviderKind::Responses
                    && intent.data_plane == "responses"
                    && build.input_modalities == [Modality::Text]
                    && build.output_modalities == [Modality::Text])
                    || (provider.kind == ProviderKind::AppleImage
                        && crate::is_apple_image_plane(&intent.data_plane)
                        && build.input_modalities == [Modality::Image]
                        && build.output_modalities == [Modality::Json]
                        && build.model_id != "raw_render"))
                || !config.model_profiles[&build.profile]
                    .ratings
                    .contains_key(&export.intent)
                || !deployment
                    .supported_execution_modes
                    .contains(&ExecutionMode::Unary)
            {
                return Err(configuration(
                    "node exports must be assessed local unary text or Apple image deployments",
                ));
            }
            // Text exports require direct local HTTP backends. No cloud or peer relay.
            if provider.kind == ProviderKind::Responses
                && !provider
                    .base_url
                    .as_deref()
                    .is_some_and(is_loopback_http_url)
            {
                return Err(configuration(
                    "node text export requires a numeric loopback HTTP backend",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_provider(id: &str, provider: &ProviderConfig) -> Result<(), ContractError> {
    if provider.kind == ProviderKind::TrustedNode {
        let peer = provider
            .node
            .as_ref()
            .ok_or_else(|| configuration("trusted_node requires node pairing configuration"))?;
        if provider.placement != Placement::TrustedNode
            || provider.local_inventory.is_some()
            || provider.command.is_some()
            || provider.base_url.is_some()
            || provider.api_key_env.is_some()
            || provider.requires_api_key
            || provider.access_class != ProviderAccessClass::Standard
            || peer.address.parse::<std::net::SocketAddr>().is_err()
            || peer.node_id.is_empty()
            || !crate::valid_node_digest(&peer.certificate_sha256)
            || peer.imports.is_empty()
            || peer
                .imports
                .values()
                .any(|digest| !crate::valid_node_digest(digest))
            || provider
                .capability_profile
                .capabilities
                .iter()
                .any(|capability| {
                    !matches!(
                        capability,
                        ProviderCapability::Responses
                            | ProviderCapability::Instructions
                            | ProviderCapability::ReasoningEffort
                            | ProviderCapability::Temperature
                            | ProviderCapability::TopP
                            | ProviderCapability::MaxOutputTokens
                            | ProviderCapability::Truncation
                            | ProviderCapability::Metadata
                    )
                })
        {
            return Err(configuration(format!(
                "provider {id} has invalid trusted-node pairing or execution scope"
            )));
        }
    } else if provider.node.is_some() {
        return Err(configuration(
            "node pairing belongs only to trusted_node providers",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn local() -> RuntimeConfig {
        let config: RuntimeConfig = toml::from_str(
            r#"
            [server]
            bind = "127.0.0.1:8787"
            [defaults]
            policy = "local-first"
            [profiles.local-first]
            order = ["placement"]
            [providers.local]
            kind = "responses"
            placement = "local"
            base_url = "http://127.0.0.1:11434/v1"
            [providers.local.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses"]
            [intents."text.summarize"]
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_capability_floor = "foundational"
            [model_profiles.text]
            family = "test"
            [model_profiles.text.ratings."text.summarize"]
            level = "foundational"
            status = "provisional"
            [model_builds.text]
            profile = "text"
            model_id = "shared"
            input_modalities = ["text"]
            output_modalities = ["text"]
            [deployments.text]
            provider = "local"
            build = "text"
        "#,
        )
        .unwrap();
        config.validate().unwrap();
        config
    }
    fn tls() -> NodeTlsConfig {
        NodeTlsConfig {
            certificate: "cert.pem".into(),
            private_key: "key.pem".into(),
            ca_certificate: "ca.pem".into(),
        }
    }
    fn remote() -> RuntimeConfig {
        let mut config = local();
        let provider = config.providers.get_mut("local").unwrap();
        provider.kind = ProviderKind::TrustedNode;
        provider.placement = Placement::TrustedNode;
        provider.base_url = None;
        provider.node = Some(NodePeerConfig {
            node_id: "b".into(),
            address: "127.0.0.1:8843".into(),
            server_name: "b.test".into(),
            certificate_sha256: "a".repeat(64),
            tls: tls(),
            imports: BTreeMap::from([("shared".into(), "b".repeat(64))]),
        });
        config.validate().unwrap();
        config
    }
    #[test]
    fn node_placement_and_protocol_cannot_be_relaxed() {
        for placement in [Placement::Local, Placement::Cloud] {
            let mut config = remote();
            config.providers.get_mut("local").unwrap().placement = placement;
            assert!(config.validate().is_err());
        }
        let mut config = remote();
        config
            .providers
            .get_mut("local")
            .unwrap()
            .capability_profile
            .capabilities
            .insert(ProviderCapability::Streaming);
        assert!(config.validate().is_err());
    }
    #[test]
    fn remote_resources_are_not_reserved_on_ingress_and_imports_require_exact_approval() {
        let mut config = remote();
        config
            .deployments
            .get_mut("text")
            .unwrap()
            .resource_estimate
            .unified_memory_mib = 1024;
        assert!(config.validate().is_err());
        let mut config = remote();
        config.model_builds.get_mut("text").unwrap().model_id = "not-imported".into();
        assert!(config.validate().is_err());
        let mut config = remote();
        config
            .deployments
            .get_mut("text")
            .unwrap()
            .supported_execution_modes
            .insert(ExecutionMode::ServerStream);
        assert!(config.validate().is_err());
    }
    #[test]
    fn node_exports_cannot_relay_peers_or_cloud() {
        let server = NodeServerConfig {
            node_id: "b".into(),
            bind: "127.0.0.1:8843".into(),
            tls: tls(),
            peers_file: "peers.json".into(),
            max_active: 2,
            exports: BTreeMap::from([(
                "shared".into(),
                NodeExportConfig {
                    deployment: "text".into(),
                    intent: "text.summarize".into(),
                },
            )]),
        };
        let mut config = local();
        config.node_server = Some(server.clone());
        config.validate().unwrap();
        config.providers.get_mut("local").unwrap().placement = Placement::Cloud;
        assert!(config.validate().is_err());
        let mut config = remote();
        config.node_server = Some(server);
        assert!(config.validate().is_err());
    }
}
