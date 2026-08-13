//! Typed client for the opt-in RawNIND foundation data plane.
//!
//! HTTP carries only a bounded descriptor and one-shot lease identity.  The
//! sample and unpublished artifact travel solely as two already-open local
//! descriptors over the owner-only Unix socket binding.

use std::fs::File;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{Client, Error, Result};

pub const RAW_FOUNDATION_CAPABILITIES: &[&str] = &["infer.raw-foundation@20260811.1"];
pub const RAW_FOUNDATION_INTENT: &str = "raw.materialize_foundation";
pub const RAW_FOUNDATION_LEASE_ENDPOINT: &str = "/infer/v1/raw/foundations/leases";
pub const RAW_FOUNDATION_ENDPOINT: &str = "/infer/v1/raw/foundations";
pub const RAW_FOUNDATION_STAGING_SCHEMA: &str = "infer.raw-foundation-staging@20260811.1";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationLeaseRequest {
    pub model: String,
    pub priority: RawFoundationPriority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    pub source_revision: String,
    pub source: RawFoundationSource,
    pub staging: RawFoundationStagingDescriptor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawFoundationPriority {
    Interactive,
    Background,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationSource {
    pub sha256: String,
    pub size_bytes: u64,
    pub pixel_contract_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationStagingDescriptor {
    pub schema: String,
    pub width: u32,
    pub height: u32,
    pub cfa: String,
    pub black_levels: [u16; 4],
    pub white_levels: [u16; 4],
    pub sample_format: String,
    pub sample_bytes: u64,
    pub decoded_samples_sha256: String,
    pub decoder_provider_id: String,
    pub decoder_provider_version: String,
}

/// Ticket values are intentionally not `Debug`; applications must not log a
/// grant before the one-shot descriptor registration consumes it.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationLeaseGrant {
    pub object: String,
    pub job_id: String,
    pub ticket_id: String,
    pub expires_at_unix_ms: u64,
    pub daemon_generation: String,
    pub binding: RawFoundationLeaseBinding,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationLeaseBinding {
    pub contract: String,
    pub transport: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RawFoundationArtifactReceipt {
    pub cache_key_sha256: String,
    pub artifact_identity_sha256: String,
    pub artifact_file_sha256: String,
    pub artifact_file_bytes: u64,
    pub sequence_sha256: String,
    pub payload_sha256: String,
    pub output_width: usize,
    pub output_height: usize,
    pub tile_inferences: usize,
    pub maximum_accumulator_rows: usize,
    pub explicit_full_output_buffers: usize,
    pub implementation_revision: String,
    pub cache_identity: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationProvenance {
    pub provider: String,
    pub deployment: String,
    pub model_profile: String,
    pub model_build: String,
    pub physical_model: String,
    pub exact_revision: String,
    pub graph_sha256: String,
    pub implementation_revision: String,
    pub cache_identity: String,
    pub execution_provider: String,
    pub runtime_version: String,
    pub precision: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationExecuteResponse {
    pub id: String,
    pub object: String,
    pub status: String,
    pub source_revision: String,
    pub artifact: RawFoundationArtifactReceipt,
    pub provenance: RawFoundationProvenance,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationCancellation {
    pub id: String,
    pub object: String,
    pub status: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawFoundationExecuteRequest<'a> {
    job_id: &'a str,
    lease_id: &'a str,
}

impl Client {
    pub async fn create_raw_foundation_lease(
        &self,
        request: &RawFoundationLeaseRequest,
    ) -> Result<RawFoundationLeaseGrant> {
        self.send_capability_json(
            RAW_FOUNDATION_CAPABILITIES,
            Method::POST,
            RAW_FOUNDATION_LEASE_ENDPOINT,
            Some(request),
        )
        .await
    }

    /// Registers two existing descriptors. No file path, bytes, or caller
    /// supplied App identity is placed on the control plane.
    #[cfg(unix)]
    pub fn register_raw_foundation_handles(
        &self,
        grant: &RawFoundationLeaseGrant,
        input: &File,
        output: &File,
    ) -> Result<String> {
        use std::os::fd::AsRawFd;

        if grant.binding.contract != "infer-runtime.artifact-lease@20260811.1"
            || grant.binding.transport != "uds-scm-rights"
            || grant.binding.endpoint.is_empty()
        {
            return Err(Error::ContractMismatch);
        }
        let (lease_id, _expires_at) = infer_artifact_lease::register_unix_handles(
            std::path::Path::new(&grant.binding.endpoint),
            &grant.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .map_err(|error| {
            Error::Input(format!("RAW artifact lease registration failed: {error}"))
        })?;
        Ok(lease_id)
    }

    #[cfg(not(unix))]
    pub fn register_raw_foundation_handles(
        &self,
        _grant: &RawFoundationLeaseGrant,
        _input: &File,
        _output: &File,
    ) -> Result<String> {
        Err(Error::Input(
            "RAW foundation descriptor transfer is not implemented on this platform".into(),
        ))
    }

    pub async fn execute_raw_foundation(
        &self,
        job_id: &str,
        lease_id: &str,
    ) -> Result<RawFoundationExecuteResponse> {
        self.send_capability_json(
            RAW_FOUNDATION_CAPABILITIES,
            Method::POST,
            RAW_FOUNDATION_ENDPOINT,
            Some(&RawFoundationExecuteRequest { job_id, lease_id }),
        )
        .await
    }

    pub async fn cancel_raw_foundation(&self, job_id: &str) -> Result<RawFoundationCancellation> {
        if job_id.is_empty() || job_id.contains('/') {
            return Err(Error::MalformedResponse("invalid RAW Job id".into()));
        }
        self.send_capability_json(
            RAW_FOUNDATION_CAPABILITIES,
            Method::POST,
            &format!("/infer/v1/raw/foundations/{job_id}/cancel"),
            Option::<&()>::None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_request_has_no_path_or_payload_field() {
        let encoded = serde_json::to_string(&RawFoundationLeaseRequest {
            model: RAW_FOUNDATION_INTENT.into(),
            priority: RawFoundationPriority::Background,
            deadline_ms: None,
            source_revision: "revision".into(),
            source: RawFoundationSource {
                sha256: "a".repeat(64),
                size_bytes: 1,
                pixel_contract_sha256: "b".repeat(64),
            },
            staging: RawFoundationStagingDescriptor {
                schema: RAW_FOUNDATION_STAGING_SCHEMA.into(),
                width: 2,
                height: 2,
                cfa: "RGGB".into(),
                black_levels: [0; 4],
                white_levels: [1; 4],
                sample_format: "uint16-le-row-major-active-bayer".into(),
                sample_bytes: 8,
                decoded_samples_sha256: "c".repeat(64),
                decoder_provider_id: "decoder".into(),
                decoder_provider_version: "v1".into(),
            },
        })
        .unwrap();
        assert!(!encoded.contains("path"));
        assert!(!encoded.contains("base64"));
    }
}
