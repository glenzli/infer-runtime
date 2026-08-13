use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Infra Discovery protocol identifier for the Consumer Core.
pub const CONSUMER_CORE_PROTOCOL: &str = "infer-runtime.consumer-core";
/// Opaque application protocol version advertised through Infra Discovery.
pub const CONSUMER_CORE_VERSION: &str = "20260813.1";
/// Ordered by Consumer preference. Discovery uses an exact set intersection;
/// opaque versions are never compared lexically.
pub const SUPPORTED_CONSUMER_CORE_VERSIONS: &[&str] = &[CONSUMER_CORE_VERSION];
/// Full immutable wire identity carried by every Consumer request.
pub const CONSUMER_CORE: &str = "infer-runtime.consumer-core@20260813.1";
pub const CONSUMER_CORE_HEADER: &str = "Infer-Consumer-Contract";
pub const CAPABILITY_CONTRACT_HEADER: &str = "Infer-Capability-Contract";

pub const CAPABILITY_CATALOG_SCHEMA: &str = "infer-runtime.capability-catalog";
pub const CAPABILITY_CATALOG_VERSION: &str = "20260813.1";
pub const CONSUMER_OPENAPI_SHA256: &str =
    "08dcb1cc800b3eb5a8f63d45e8767404a40e8d36d7be391f5e7dd73c085f5801";

pub(crate) fn expected_capability_schema(identity: &str) -> Option<(&'static str, &'static str)> {
    Some(match identity {
        "infer.responses@20260812.1" => (
            "/infer/v1/capability-schemas/infer.responses/20260812.1/openapi.json",
            "abfb3b4b9a3c5d3831d56bb877ecfdd43d62b4442ba101a5ef071ec2740adbd5",
        ),
        "infer.audio.transcription@20260811.1" => (
            "/infer/v1/capability-schemas/infer.audio.transcription/20260811.1/openapi.json",
            "53ee5993abbaa3ccc04a5b5f77f3457fbd2f29cccda0b33b0959b6b900c25e59",
        ),
        "infer.audio.event-detection@20260813.2" => (
            "/infer/v1/capability-schemas/infer.audio.event-detection/20260813.2/openapi.json",
            "a7179c88c03a768299835bd84c4c6f8d68e47f3c51a96ed4fe01387cf6fb8613",
        ),
        "infer.audio.alignment@20260811.1" => (
            "/infer/v1/capability-schemas/infer.audio.alignment/20260811.1/openapi.json",
            "76c7f4ab7d5e6333808aceec558822d9deceb2918bc478e326593d302dcb96e8",
        ),
        "infer.audio.speech@20260811.1" => (
            "/infer/v1/capability-schemas/infer.audio.speech/20260811.1/openapi.json",
            "19d29d6799a6cee1a6d24a63f9a9aab73ab925dd2e79f7181fbfe922f6906c68",
        ),
        "infer.audio.voice-clone@20260811.1" => (
            "/infer/v1/capability-schemas/infer.audio.voice-clone/20260811.1/openapi.json",
            "ac182b38d13a91dd5bb3d69f8f4715807357fc9a75e8cdbdaacf2b15931b0ef8",
        ),
        "infer.audio.transcription-stream@20260811.1" => (
            "/infer/v1/capability-schemas/infer.audio.transcription-stream/20260811.1/openapi.json",
            "855d7a886689e442d5aa6260271061bc1a317bf81fbb28d6257071d4e2f81726",
        ),
        "infer.vision.face-detection@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.face-detection/20260811.1/openapi.json",
            "3f5c9be22302de8c243c3133704282fa21e6d74e12990c0d1f32deb086986d33",
        ),
        "infer.vision.face-embedding@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.face-embedding/20260811.1/openapi.json",
            "e225a4e06bcadb7274074d3af5aec4d0b96e476f81cac1b4f5e10e33a7d02ccc",
        ),
        "infer.vision.subject-segmentation@20260813.1" => (
            "/infer/v1/capability-schemas/infer.vision.subject-segmentation/20260813.1/openapi.json",
            "4df56eefaa7ced43ef3c823f933e9bea689bee23f60d3279c3d84f75414bdb1d",
        ),
        "infer.vision.face-parsing@20260813.1" => (
            "/infer/v1/capability-schemas/infer.vision.face-parsing/20260813.1/openapi.json",
            "663e549c77528811a2c78406ef87b34b6d0add5c0702eec042a49bf7c8968b39",
        ),
        "infer.vision.image-embedding@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.image-embedding/20260811.1/openapi.json",
            "1f30793c1c7866f1cd4239e35dcdb10842b609f021731882810d4de143df42e9",
        ),
        "infer.vision.text-embedding@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.text-embedding/20260811.1/openapi.json",
            "b1bc536f7ded5f1397c739c2b1a282d460ff4b59923fb4f6fd3c564a8627be71",
        ),
        "infer.vision.image-description@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.image-description/20260811.1/openapi.json",
            "e655e711e8101ed5f3c11c52fff1eb5fee1ebebb5f51cdd265d0a983d8aa5793",
        ),
        "infer.vision.classification-review@20260811.1" => (
            "/infer/v1/capability-schemas/infer.vision.classification-review/20260811.1/openapi.json",
            "c191467016d770f86c0e8df132127293543a2edef8d336dea358a8256bc6157b",
        ),
        "infer.text.embedding@20260812.1" => (
            "/infer/v1/capability-schemas/infer.text.embedding/20260812.1/openapi.json",
            "93beba6b0076f757c982dc7a9cfb4029e9c585461dc4596f154f138309993040",
        ),
        "infer.text.rerank@20260812.1" => (
            "/infer/v1/capability-schemas/infer.text.rerank/20260812.1/openapi.json",
            "9de8641a5dd9e575ac7d88c0b12a32964b6a4a4d7c4fca7ba2fa6cfbac0f6979",
        ),
        "infer.document.ocr@20260812.1" => (
            "/infer/v1/capability-schemas/infer.document.ocr/20260812.1/openapi.json",
            "bad84843af128390030b40d845cca93072ef3ee96fede280773399ad35893aa9",
        ),
        "infer.raw-foundation@20260811.1" => (
            "/infer/v1/capability-schemas/infer.raw-foundation/20260811.1/openapi.json",
            "bb897cf44c58aec6c7173dda5ea43bba78993d0e4233aca7ce3168835d6c3597",
        ),
        _ => return None,
    })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Core bootstrap response. Unknown response fields are intentionally ignored
/// so additive server metadata does not break an older SDK build.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContractManifest {
    pub schema: String,
    pub schema_version: String,
    pub core_contract: String,
    pub supported_core_contracts: Vec<String>,
    pub capability_catalog: CapabilityCatalogReference,
    pub openapi_url: String,
    pub openapi_sha256: String,
    pub error_codes: Vec<String>,
    pub consumer_routes: Vec<ContractRoute>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityCatalogReference {
    pub schema: String,
    pub schema_version: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContractRoute {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityCatalog {
    pub schema: String,
    pub schema_version: String,
    pub core_contract: String,
    pub capabilities: Vec<CapabilityEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityEntry {
    pub id: String,
    pub schema_version: String,
    pub stability: Stability,
    pub schema: CapabilitySchemaReference,
    pub routes: Vec<CapabilityRoute>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilitySchemaReference {
    pub format: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stability {
    Stable,
    Experimental,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityRoute {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_modes: Vec<String>,
}

impl ContractManifest {
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema != "infer-runtime.consumer-core"
            || self.schema_version != CONSUMER_CORE_VERSION
            || self.core_contract != CONSUMER_CORE
            || self.supported_core_contracts != [CONSUMER_CORE]
            || self.capability_catalog.schema != CAPABILITY_CATALOG_SCHEMA
            || self.capability_catalog.schema_version != CAPABILITY_CATALOG_VERSION
            || self.capability_catalog.url != "/infer/v1/capabilities"
            || self.openapi_url != "/infer/v1/openapi.json"
            || self.openapi_sha256 != CONSUMER_OPENAPI_SHA256
            || self.error_codes.is_empty()
        {
            return Err(crate::Error::ContractMismatch);
        }
        Ok(())
    }
}

impl CapabilityCatalog {
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema != CAPABILITY_CATALOG_SCHEMA
            || self.schema_version != CAPABILITY_CATALOG_VERSION
            || self.core_contract != CONSUMER_CORE
        {
            return Err(crate::Error::ContractMismatch);
        }
        let mut identities = BTreeSet::new();
        let mut routes = BTreeSet::new();
        for capability in &self.capabilities {
            if capability.id.is_empty()
                || capability.schema_version.is_empty()
                || capability.routes.is_empty()
                || capability.schema.format != "openapi-3.1"
                || !capability
                    .schema
                    .url
                    .starts_with("/infer/v1/capability-schemas/")
                || !is_sha256(&capability.schema.sha256)
            {
                return Err(crate::Error::ContractMismatch);
            }
            if !identities.insert((capability.id.as_str(), capability.schema_version.as_str())) {
                return Err(crate::Error::ContractMismatch);
            }
            for route in &capability.routes {
                if route.method.is_empty() || !route.path.starts_with('/') {
                    return Err(crate::Error::ContractMismatch);
                }
                let route_identity = (
                    route.method.as_str(),
                    route.path.as_str(),
                    capability.schema_version.as_str(),
                );
                if !routes.insert(route_identity) {
                    return Err(crate::Error::ContractMismatch);
                }
            }
        }
        Ok(())
    }

    pub fn require_exact(&self, identity: &str) -> crate::Result<()> {
        let Some((id, version)) = identity.rsplit_once('@') else {
            return Err(crate::Error::ContractMismatch);
        };
        let Some((expected_url, expected_sha256)) = expected_capability_schema(identity) else {
            return Err(crate::Error::ContractMismatch);
        };
        if self.capabilities.iter().any(|capability| {
            capability.id == id
                && capability.schema_version == version
                && capability.schema.url == expected_url
                && capability.schema.sha256 == expected_sha256
        }) {
            Ok(())
        } else {
            Err(crate::Error::ContractMismatch)
        }
    }
    /// Select the Consumer's first preferred exact identity. Versions are
    /// opaque strings; neither side infers compatibility from date ordering.
    pub fn select_preferred(
        &self,
        capability_id: &str,
        supported: &[&'static str],
    ) -> crate::Result<&'static str> {
        for identity in supported {
            let Some((id, version)) = identity.rsplit_once('@') else {
                continue;
            };
            if id != capability_id {
                continue;
            }
            if self.capabilities.iter().any(|capability| {
                capability.id == id
                    && capability.schema_version == version
                    && expected_capability_schema(identity).is_some_and(|(url, digest)| {
                        capability.schema.url == url && capability.schema.sha256 == digest
                    })
            }) {
                return Ok(identity);
            }
        }
        Err(crate::Error::ContractMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_manifest_requires_exact_dated_identity() {
        let mut manifest = ContractManifest {
            schema: "infer-runtime.consumer-core".into(),
            schema_version: CONSUMER_CORE_VERSION.into(),
            core_contract: CONSUMER_CORE.into(),
            supported_core_contracts: vec![CONSUMER_CORE.into()],
            capability_catalog: CapabilityCatalogReference {
                schema: CAPABILITY_CATALOG_SCHEMA.into(),
                schema_version: CAPABILITY_CATALOG_VERSION.into(),
                url: "/infer/v1/capabilities".into(),
            },
            openapi_url: "/infer/v1/openapi.json".into(),
            openapi_sha256: CONSUMER_OPENAPI_SHA256.into(),
            error_codes: vec!["invalid_request_error".into()],
            consumer_routes: Vec::new(),
        };
        manifest.validate().unwrap();
        manifest.core_contract = "0.1.0-candidate.4".into();
        assert!(matches!(
            manifest.validate(),
            Err(crate::Error::ContractMismatch)
        ));
    }

    #[test]
    fn capability_catalog_rejects_duplicate_identities_and_routes() {
        let capability = CapabilityEntry {
            id: "infer.example".into(),
            schema_version: "20260812.1".into(),
            stability: Stability::Experimental,
            schema: CapabilitySchemaReference {
                format: "openapi-3.1".into(),
                url: "/infer/v1/capability-schemas/infer.example/20260812.1/openapi.json".into(),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            },
            routes: vec![CapabilityRoute {
                method: "POST".into(),
                path: "/infer/v1/example".into(),
                execution_modes: vec!["unary".into()],
            }],
        };
        let mut catalog = CapabilityCatalog {
            schema: CAPABILITY_CATALOG_SCHEMA.into(),
            schema_version: CAPABILITY_CATALOG_VERSION.into(),
            core_contract: CONSUMER_CORE.into(),
            capabilities: vec![capability.clone()],
        };
        catalog.validate().unwrap();
        catalog.capabilities.push(capability);
        assert!(matches!(
            catalog.validate(),
            Err(crate::Error::ContractMismatch)
        ));
    }

    #[test]
    fn capability_catalog_allows_parallel_versions_but_not_ambiguous_records() {
        let entry = |version: &str| CapabilityEntry {
            id: "infer.example".into(),
            schema_version: version.into(),
            stability: Stability::Experimental,
            schema: CapabilitySchemaReference {
                format: "openapi-3.1".into(),
                url: format!("/infer/v1/capability-schemas/infer.example/{version}/openapi.json"),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            },
            routes: vec![CapabilityRoute {
                method: "POST".into(),
                path: "/infer/v1/example".into(),
                execution_modes: vec!["unary".into()],
            }],
        };
        let mut catalog = CapabilityCatalog {
            schema: CAPABILITY_CATALOG_SCHEMA.into(),
            schema_version: CAPABILITY_CATALOG_VERSION.into(),
            core_contract: CONSUMER_CORE.into(),
            capabilities: vec![entry("20260812.1"), entry("20260813.1")],
        };
        catalog.validate().unwrap();

        catalog.capabilities[0].schema_version.clear();
        assert!(matches!(
            catalog.validate(),
            Err(crate::Error::ContractMismatch)
        ));
    }

    #[test]
    fn capability_selection_is_an_exact_identity_intersection() {
        let catalog = CapabilityCatalog {
            schema: CAPABILITY_CATALOG_SCHEMA.into(),
            schema_version: CAPABILITY_CATALOG_VERSION.into(),
            core_contract: CONSUMER_CORE.into(),
            capabilities: vec![CapabilityEntry {
                id: "infer.responses".into(),
                schema_version: "20260812.1".into(),
                stability: Stability::Experimental,
                schema: CapabilitySchemaReference {
                    format: "openapi-3.1".into(),
                    url: "/infer/v1/capability-schemas/infer.responses/20260812.1/openapi.json"
                        .into(),
                    sha256: "abfb3b4b9a3c5d3831d56bb877ecfdd43d62b4442ba101a5ef071ec2740adbd5"
                        .into(),
                },
                routes: vec![CapabilityRoute {
                    method: "POST".into(),
                    path: "/infer/v1/example".into(),
                    execution_modes: vec!["unary".into()],
                }],
            }],
        };
        catalog.require_exact("infer.responses@20260812.1").unwrap();
        assert!(catalog.require_exact("infer.responses@20260811.1").is_err());
        assert!(catalog.require_exact("infer.other@20260812.1").is_err());
        assert_eq!(
            catalog
                .select_preferred(
                    "infer.responses",
                    &["infer.responses@20260813.1", "infer.responses@20260812.1"]
                )
                .unwrap(),
            "infer.responses@20260812.1"
        );
    }
}
