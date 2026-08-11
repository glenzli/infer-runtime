//! Capability-scoped, process-local leases for large Consumer artifacts.
//!
//! This crate deliberately does not own model artifacts, public inference
//! endpoints, durable payloads, or application caches. An authenticated
//! control plane may mint a short-lived registration ticket; the local data
//! plane then exchanges already-open file handles for a one-shot lease. Paths
//! and payload bytes never enter the wire contract.

#[cfg(unix)]
mod registry;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use registry::{
    ArtifactLeaseError, ArtifactLeaseIdentity, ArtifactLeaseRegistry, ConsumedArtifactLease,
    LeaseRegistrationTicket,
};

#[cfg(unix)]
pub use unix::{
    ArtifactLeaseUnixServer, ArtifactLeaseUnixService, FixtureCopyReceipt, UnixLeaseProtocolError,
    copy_striped_and_hash, copy_striped_and_hash_with_cancel, register_unix_handles,
};

/// Product-level protocol identity shared by every local platform binding.
pub const ARTIFACT_LEASE_CONTRACT: &str = "infer-runtime.artifact-lease@20260811.1";

/// Frozen Windows binding identity. It is documentation/test evidence only;
/// no Windows implementation is claimed in Phase 1.
pub const WINDOWS_HANDLE_BINDING: &str = "owner-only-named-pipe-duplicated-handle";

/// Unix binding identity under the shared artifact-lease contract.
pub const UNIX_FD_BINDING: &str = "uds-scm-rights";
