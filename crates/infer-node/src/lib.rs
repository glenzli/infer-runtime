//! Versioned, mutually authenticated single-hop text execution between runtimes.
//! Pairing and leases are independent of local Consumer Infra Discovery.
mod client;
mod server;
mod tls;
mod wire;

pub use client::NodeClient;
pub use server::{NodeExecutor, NodeServer, PeerGrant, PeerGrants};
pub use wire::{
    AppleNodeRequest, Catalog, IMAGE_PROTOCOL, MAX_NODE_IMAGE_BYTES, NodeAttempt, NodeError, Offer,
    PROTOCOL, TaskKey, TaskState,
};

pub fn contract_digest<T: serde::Serialize>(contract: &T) -> Result<String, NodeError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(contract).map_err(|_| NodeError::Protocol)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
