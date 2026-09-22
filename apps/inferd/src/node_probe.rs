//! Payload-free operator probe for configured, explicitly paired node imports.
use futures_util::future::join_all;
use infer_core::{NodePeerConfig, ProviderKind, RuntimeConfig};
use infer_node::{Catalog, NodeClient, NodeError};
use serde::Serialize;

#[derive(Serialize)]
struct ProbeReport {
    ready: bool,
    nodes: Vec<NodeProbe>,
}

#[derive(Serialize)]
struct NodeProbe {
    provider: String,
    status: ProbeStatus,
    available_admissions: Option<usize>,
    imports: Vec<ImportProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<NodeError>,
}

#[derive(Serialize)]
struct ImportProbe {
    export: String,
    /// None means the authenticated Catalog could not be read.
    present: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProbeStatus {
    Ready,
    Busy,
    ContractMismatch,
    Unavailable,
}

pub async fn run(config: &RuntimeConfig) -> anyhow::Result<()> {
    let nodes = join_all(
        config
            .providers
            .iter()
            .filter(|(_, provider)| provider.kind == ProviderKind::TrustedNode)
            .map(|(id, provider)| {
                let peer = provider
                    .node
                    .as_ref()
                    .expect("validated trusted-node provider has pairing");
                probe_one(id.clone(), peer.clone())
            }),
    )
    .await;
    let ready = !nodes.is_empty() && nodes.iter().all(|node| node.status == ProbeStatus::Ready);
    println!(
        "{}",
        serde_json::to_string_pretty(&ProbeReport { ready, nodes })?
    );
    anyhow::ensure!(ready, "one or more configured node imports are not ready");
    Ok(())
}

async fn probe_one(provider: String, peer: NodePeerConfig) -> NodeProbe {
    let catalog = match NodeClient::new(peer.clone()) {
        Ok(client) => client.catalog().await.map(|(_, catalog)| catalog),
        Err(error) => Err(error),
    };
    match catalog {
        Ok(catalog) => from_catalog(provider, &peer, catalog),
        Err(error) => NodeProbe {
            provider,
            status: ProbeStatus::Unavailable,
            available_admissions: None,
            imports: peer
                .imports
                .keys()
                .map(|export| ImportProbe {
                    export: export.clone(),
                    present: None,
                })
                .collect(),
            error: Some(error),
        },
    }
}

fn from_catalog(provider: String, peer: &NodePeerConfig, catalog: Catalog) -> NodeProbe {
    let imports: Vec<_> =
        peer.imports
            .iter()
            .map(|(export, digest)| ImportProbe {
                export: export.clone(),
                present: Some(catalog.offers.iter().any(|offer| {
                    offer.deployment_id == *export && offer.contract_digest == *digest
                })),
            })
            .collect();
    let status = if imports.iter().any(|import| import.present == Some(false)) {
        ProbeStatus::ContractMismatch
    } else if catalog.available_admissions == 0 {
        ProbeStatus::Busy
    } else {
        ProbeStatus::Ready
    };
    NodeProbe {
        provider,
        status,
        available_admissions: Some(catalog.available_admissions),
        imports,
        error: None,
    }
}
