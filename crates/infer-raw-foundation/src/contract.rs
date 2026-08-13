use serde::{Deserialize, Serialize};

use crate::RawFoundationError;

pub const RAW_FOUNDATION_INTENT: &str = "raw.materialize_foundation";
pub const RAW_FOUNDATION_ENDPOINT: &str = "/infer/v1/raw/foundations";
pub const RAW_FOUNDATION_LEASE_ENDPOINT: &str = "/infer/v1/raw/foundations/leases";
/// Public capability identity. Keep this byte-for-byte aligned with the
/// published Catalog, API capability header, and official Consumer client.
pub const RAW_FOUNDATION_CONTRACT: &str = "infer.raw-foundation@20260811.1";
pub const RAW_FOUNDATION_STAGING_SCHEMA: &str = "infer.raw-foundation-staging@20260811.1";
const SOURCE_PIXEL_CONTRACT_SHA256: &str =
    "e1998069001c14d01251cc3d6e2bc2aa66b807f3f17d246e7ee7270528302f7f";
const MAX_DIMENSION: u32 = 100_000;
const MAX_ID_BYTES: usize = 256;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationRequest {
    pub model: String,
    pub lease_id: String,
    pub source_revision: String,
    pub source: RawFoundationSource,
    pub staging: RawFoundationStagingDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RawFoundationPriority {
    Interactive,
    Background,
}

impl RawFoundationLeaseRequest {
    pub fn with_lease(self, lease_id: String) -> RawFoundationRequest {
        RawFoundationRequest {
            model: self.model,
            lease_id,
            source_revision: self.source_revision,
            source: self.source,
            staging: self.staging,
        }
    }

    pub fn validate(&self) -> Result<(), RawFoundationError> {
        if matches!(self.deadline_ms, Some(0 | 3_600_001..)) {
            return invalid("deadline_ms must be between 1 and 3600000");
        }
        self.clone()
            .with_lease("lease_validation".into())
            .validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationExecuteRequest {
    pub job_id: String,
    pub lease_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawFoundationSource {
    pub sha256: String,
    pub size_bytes: u64,
    pub pixel_contract_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
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

impl RawFoundationRequest {
    pub fn validate(&self) -> Result<(), RawFoundationError> {
        validate_id(&self.model, "model")?;
        if self.model != RAW_FOUNDATION_INTENT {
            return invalid("model must be raw.materialize_foundation");
        }
        validate_id(&self.lease_id, "lease_id")?;
        validate_id(&self.source_revision, "source_revision")?;
        validate_sha256(&self.source.sha256, "source.sha256")?;
        if self.source.size_bytes == 0 || self.source.size_bytes > MAX_SOURCE_BYTES {
            return invalid("source.size_bytes is outside the supported bound");
        }
        if self.source.pixel_contract_sha256 != SOURCE_PIXEL_CONTRACT_SHA256 {
            return invalid("source pixel contract is unsupported");
        }
        self.staging.validate()
    }
}

impl RawFoundationStagingDescriptor {
    pub fn validate(&self) -> Result<(), RawFoundationError> {
        if self.schema != RAW_FOUNDATION_STAGING_SCHEMA {
            return invalid("staging schema is unsupported");
        }
        if self.width < 4
            || self.height < 4
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            return invalid("staging dimensions are outside the supported bound");
        }
        if self.cfa.len() != 4 {
            return invalid("staging CFA must describe exactly one 2x2 Bayer cell");
        }
        let mut sites = self.cfa.as_bytes().to_vec();
        sites.sort_unstable();
        if sites != b"BGGR" {
            return invalid("staging CFA must contain one R, two G, and one B site");
        }
        if self
            .black_levels
            .iter()
            .zip(self.white_levels)
            .any(|(black, white)| *black >= white)
        {
            return invalid("every staging black level must be below its white level");
        }
        if self
            .white_levels
            .iter()
            .any(|white| *white != self.white_levels[0])
        {
            return invalid("RawNIND currently requires one shared sensor white level");
        }
        if self.sample_format != "uint16-le-row-major-active-bayer" {
            return invalid("staging sample format is unsupported");
        }
        let expected = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|pixels| pixels.checked_mul(2))
            .ok_or_else(|| RawFoundationError::InvalidRequest("sample size overflow".into()))?;
        if self.sample_bytes != expected {
            return invalid("staging sample_bytes does not match width and height");
        }
        validate_sha256(
            &self.decoded_samples_sha256,
            "staging.decoded_samples_sha256",
        )?;
        validate_id(&self.decoder_provider_id, "staging.decoder_provider_id")?;
        validate_id(
            &self.decoder_provider_version,
            "staging.decoder_provider_version",
        )
    }
}

fn validate_id(value: &str, field: &str) -> Result<(), RawFoundationError> {
    if value.trim().is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control)
    {
        return invalid(format!("{field} is required and must be bounded UTF-8"));
    }
    Ok(())
}

fn validate_sha256(value: &str, field: &str) -> Result<(), RawFoundationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid(format!("{field} must be a lowercase SHA-256"));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T, RawFoundationError> {
    Err(RawFoundationError::InvalidRequest(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RawFoundationRequest {
        RawFoundationRequest {
            model: RAW_FOUNDATION_INTENT.into(),
            lease_id: "lease_123".into(),
            source_revision: "source:v1".into(),
            source: RawFoundationSource {
                sha256: "1".repeat(64),
                size_bytes: 42,
                pixel_contract_sha256: SOURCE_PIXEL_CONTRACT_SHA256.into(),
            },
            staging: RawFoundationStagingDescriptor {
                schema: RAW_FOUNDATION_STAGING_SCHEMA.into(),
                width: 8,
                height: 6,
                cfa: "RGGB".into(),
                black_levels: [64, 65, 66, 67],
                white_levels: [4095; 4],
                sample_format: "uint16-le-row-major-active-bayer".into(),
                sample_bytes: 96,
                decoded_samples_sha256: "2".repeat(64),
                decoder_provider_id: "shadow.test".into(),
                decoder_provider_version: "1".into(),
            },
        }
    }

    #[test]
    fn strict_wire_rejects_unknown_duplicate_and_semantically_invalid_fields() {
        request().validate().unwrap();
        let mut value = serde_json::to_value(request()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("path".into(), "/tmp/x".into());
        assert!(serde_json::from_value::<RawFoundationRequest>(value).is_err());
        let duplicate = serde_json::to_string(&request()).unwrap().replace(
            "\"lease_id\":\"lease_123\"",
            "\"lease_id\":\"lease_123\",\"lease_id\":\"other\"",
        );
        assert!(serde_json::from_str::<RawFoundationRequest>(&duplicate).is_err());
        let mut invalid = request();
        invalid.staging.white_levels[2] = 4094;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn endpoint_and_contract_id_are_frozen_independently_of_activation() {
        assert_eq!(RAW_FOUNDATION_ENDPOINT, "/infer/v1/raw/foundations");
        assert_eq!(RAW_FOUNDATION_CONTRACT, "infer.raw-foundation@20260811.1");
    }
}
