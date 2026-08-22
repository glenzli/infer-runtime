//! Machine-readable Consumer Core identity and additive Capability Catalog.

use serde::Serialize;

pub const CORE_PROTOCOL: &str = infer_core::CONSUMER_CORE_PROTOCOL;
pub const CORE_VERSION: &str = infer_core::CONSUMER_CORE_VERSION;
pub const CORE_CONTRACT: &str = infer_core::CONSUMER_CORE_CONTRACT;
pub const SUPPORTED_CORE_CONTRACTS: &[&str] = &[CORE_CONTRACT];
pub const CONSUMER_CORE_HEADER: &str = infer_core::CONSUMER_CORE_HEADER;
pub const CAPABILITY_CONTRACT_HEADER: &str = infer_core::CAPABILITY_CONTRACT_HEADER;

pub fn published_versions_match_contract(versions: &[String]) -> bool {
    versions == [CORE_VERSION]
}
pub const CAPABILITY_CATALOG_SCHEMA: &str = "infer-runtime.capability-catalog";
pub const CAPABILITY_CATALOG_VERSION: &str = "20260813.1";
pub const OPENAPI_PATH: &str = "/infer/v1/openapi.json";
pub const OPENAPI_JSON: &str =
    include_str!("../../../contracts/consumer-core/20260813.1/openapi.json");
// Checked by a focused test against the embedded bytes. A schema edit must
// intentionally update both this digest and the dated contract artifact.
pub const OPENAPI_SHA256: &str = "08dcb1cc800b3eb5a8f63d45e8767404a40e8d36d7be391f5e7dd73c085f5801";

pub mod error_code {
    pub const INVALID_API_KEY: &str = "invalid_api_key";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID_REQUEST: &str = "invalid_request_error";
    pub const CONSUMER_CORE_UNSUPPORTED: &str = "consumer_core_unsupported";
    pub const CAPABILITY_CONTRACT_UNSUPPORTED: &str = "capability_contract_unsupported";
    pub const INTERNAL: &str = "internal_error";
    pub const OBSERVER_CREDENTIAL_RESTRICTED: &str = "observer_credential_restricted";
    pub const OBSERVER_ACCESS_REQUIRED: &str = "observer_access_required";
    pub const POLICY_VIOLATION: &str = "policy_violation";
    pub const INTENT_FORBIDDEN: &str = "intent_forbidden";
    pub const ROUTE_TARGET_FORBIDDEN: &str = "route_target_forbidden";
    pub const RESOURCE_ADMIN_REQUIRED: &str = "resource_admin_required";
    pub const NO_CANDIDATE: &str = "no_candidate";
    pub const CANCELLED: &str = "cancelled";
    pub const QUEUE_FULL: &str = "queue_full";
    pub const APP_QUEUE_FULL: &str = "app_queue_full";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const DEADLINE_EXCEEDED: &str = "deadline_exceeded";
    pub const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";
    pub const PROVIDER_PROBE_MODEL_MISSING: &str = "provider_probe_model_missing";
    pub const BACKGROUND_UNAVAILABLE: &str = "background_unavailable";
    pub const BACKGROUND_PAYLOAD_TOO_LARGE: &str = "background_payload_too_large";
    pub const BACKGROUND_PERSISTENCE_ERROR: &str = "background_persistence_error";
    pub const PERSISTENCE_ERROR: &str = "persistence_error";
    pub const ARTIFACT_STORE_ERROR: &str = "artifact_store_error";
    pub const UPSTREAM_AUTHENTICATION: &str = "upstream_authentication";
    pub const UPSTREAM_RATE_LIMITED: &str = "upstream_rate_limited";
    pub const UPSTREAM_TIMEOUT: &str = "upstream_timeout";
    pub const UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";
    pub const UPSTREAM_INVALID_REQUEST: &str = "upstream_invalid_request";
    pub const UPSTREAM_PROTOCOL: &str = "upstream_protocol";
    pub const VISION_PAYLOAD_TOO_LARGE: &str = "vision_payload_too_large";
    pub const RAW_JOB_NOT_FOUND: &str = "raw_job_not_found";
    pub const RAW_DESCRIPTOR_INVALID: &str = "raw_descriptor_invalid";
    pub const RAW_EXECUTION_FAILED: &str = "raw_execution_failed";
    pub const ARTIFACT_LEASE_SCOPE_MISMATCH: &str = "artifact_lease_scope_mismatch";
    pub const DAEMON_GENERATION_CHANGED: &str = "daemon_generation_changed";
    pub const ARTIFACT_LEASE_INVALID: &str = "artifact_lease_invalid";
    pub const ARTIFACT_DESCRIPTOR_INVALID: &str = "artifact_descriptor_invalid";
    pub const ARTIFACT_LEASE_ERROR: &str = "artifact_lease_error";
    // Operator-only codes share the envelope, but are intentionally excluded
    // from the Consumer Core manifest and Consumer OpenAPI enum.
    pub const INVALID_EVICTION_PLAN: &str = "invalid_eviction_plan";
    pub const RESOURCE_TRANSITION_CONFLICT: &str = "resource_transition_conflict";
    pub const INVALID_RELOAD_BENCHMARK: &str = "invalid_reload_benchmark";
    pub const INVALID_EVICTION_APPROVAL: &str = "invalid_eviction_approval";
    pub const EVICTION_RECOMMENDATION_CHANGED: &str = "eviction_recommendation_changed";
    pub const NATIVE_CONTROL_UNAVAILABLE: &str = "native_control_unavailable";
    pub const INVALID_MAINTENANCE_LEASE: &str = "invalid_maintenance_lease";
    pub const MAINTENANCE_LEASE_CONFLICT: &str = "maintenance_lease_conflict";
}

pub const CORE_ERROR_CODES: &[&str] = &[
    error_code::INVALID_API_KEY,
    error_code::NOT_FOUND,
    error_code::INVALID_REQUEST,
    error_code::CONSUMER_CORE_UNSUPPORTED,
    error_code::CAPABILITY_CONTRACT_UNSUPPORTED,
    error_code::INTERNAL,
    error_code::OBSERVER_CREDENTIAL_RESTRICTED,
    error_code::OBSERVER_ACCESS_REQUIRED,
    error_code::POLICY_VIOLATION,
    error_code::INTENT_FORBIDDEN,
    error_code::ROUTE_TARGET_FORBIDDEN,
    error_code::RESOURCE_ADMIN_REQUIRED,
];

pub const CAPABILITY_ERROR_CODES: &[&str] = &[
    error_code::NO_CANDIDATE,
    error_code::CANCELLED,
    error_code::QUEUE_FULL,
    error_code::APP_QUEUE_FULL,
    error_code::QUOTA_EXCEEDED,
    error_code::DEADLINE_EXCEEDED,
    error_code::PROVIDER_UNAVAILABLE,
    error_code::PROVIDER_PROBE_MODEL_MISSING,
    error_code::BACKGROUND_UNAVAILABLE,
    error_code::BACKGROUND_PAYLOAD_TOO_LARGE,
    error_code::BACKGROUND_PERSISTENCE_ERROR,
    error_code::PERSISTENCE_ERROR,
    error_code::ARTIFACT_STORE_ERROR,
    error_code::UPSTREAM_AUTHENTICATION,
    error_code::UPSTREAM_RATE_LIMITED,
    error_code::UPSTREAM_TIMEOUT,
    error_code::UPSTREAM_UNAVAILABLE,
    error_code::UPSTREAM_INVALID_REQUEST,
    error_code::UPSTREAM_PROTOCOL,
    error_code::VISION_PAYLOAD_TOO_LARGE,
    error_code::RAW_JOB_NOT_FOUND,
    error_code::RAW_DESCRIPTOR_INVALID,
    error_code::RAW_EXECUTION_FAILED,
    error_code::ARTIFACT_LEASE_SCOPE_MISMATCH,
    error_code::DAEMON_GENERATION_CHANGED,
    error_code::ARTIFACT_LEASE_INVALID,
    error_code::ARTIFACT_DESCRIPTOR_INVALID,
    error_code::ARTIFACT_LEASE_ERROR,
];

pub const OPERATOR_ERROR_CODES: &[&str] = &[
    error_code::INVALID_EVICTION_PLAN,
    error_code::RESOURCE_TRANSITION_CONFLICT,
    error_code::INVALID_RELOAD_BENCHMARK,
    error_code::INVALID_EVICTION_APPROVAL,
    error_code::EVICTION_RECOMMENDATION_CHANGED,
    error_code::NATIVE_CONTROL_UNAVAILABLE,
    error_code::INVALID_MAINTENANCE_LEASE,
    error_code::MAINTENANCE_LEASE_CONFLICT,
];

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ContractRoute {
    pub method: &'static str,
    pub path: &'static str,
}

pub const CONSUMER_ROUTES: &[ContractRoute] = &[
    ContractRoute {
        method: "GET",
        path: "/health",
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/jobs",
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/jobs/{response_id}",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/jobs/{response_id}/cancel",
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/explain/{response_id}",
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/contract",
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/capabilities",
    },
    ContractRoute {
        method: "GET",
        path: OPENAPI_PATH,
    },
    ContractRoute {
        method: "GET",
        path: "/infer/v1/capability-schemas/{capability_id}/{version}/openapi.json",
    },
];

#[derive(Debug, Serialize)]
pub struct ContractManifest {
    pub schema: &'static str,
    pub schema_version: &'static str,
    pub core_contract: &'static str,
    pub supported_core_contracts: &'static [&'static str],
    pub capability_catalog: CapabilityCatalogReference,
    pub openapi_url: &'static str,
    pub openapi_sha256: &'static str,
    pub error_codes: &'static [&'static str],
    pub consumer_routes: &'static [ContractRoute],
}

#[derive(Debug, Serialize)]
pub struct CapabilityCatalogReference {
    pub schema: &'static str,
    pub schema_version: &'static str,
    pub url: &'static str,
}

impl ContractManifest {
    pub fn current() -> Self {
        Self {
            schema: "infer-runtime.consumer-core",
            schema_version: CORE_VERSION,
            core_contract: CORE_CONTRACT,
            supported_core_contracts: SUPPORTED_CORE_CONTRACTS,
            capability_catalog: CapabilityCatalogReference {
                schema: CAPABILITY_CATALOG_SCHEMA,
                schema_version: CAPABILITY_CATALOG_VERSION,
                url: "/infer/v1/capabilities",
            },
            openapi_url: OPENAPI_PATH,
            openapi_sha256: OPENAPI_SHA256,
            error_codes: CORE_ERROR_CODES,
            consumer_routes: CONSUMER_ROUTES,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CapabilityCatalog {
    pub schema: &'static str,
    pub schema_version: &'static str,
    pub core_contract: &'static str,
    pub capabilities: &'static [CapabilityEntry],
}

#[derive(Debug, Serialize)]
pub struct CapabilityEntry {
    pub id: &'static str,
    #[serde(skip)]
    pub identity: &'static str,
    pub schema_version: &'static str,
    pub stability: &'static str,
    pub schema: CapabilitySchemaReference,
    pub routes: &'static [CapabilityRoute],
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CapabilitySchemaReference {
    pub format: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    #[serde(skip)]
    pub document: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CapabilityRoute {
    pub method: &'static str,
    pub path: &'static str,
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub execution_modes: &'static [&'static str],
}

fn slice_is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

macro_rules! route {
    ($method:literal, $path:literal) => {
        CapabilityRoute {
            method: $method,
            path: $path,
            execution_modes: &[],
        }
    };
    ($method:literal, $path:literal, $modes:expr) => {
        CapabilityRoute {
            method: $method,
            path: $path,
            execution_modes: $modes,
        }
    };
}

macro_rules! capability {
    ($id:tt, $version:literal, $stability:literal, [$($route:expr),+ $(,)?]) => {
        CapabilityEntry {
            id: $id,
            identity: concat!($id, "@", $version),
            schema_version: $version,
            stability: $stability,
            schema: CapabilitySchemaReference {
                format: "openapi-3.1",
                url: concat!("/infer/v1/capability-schemas/", $id, "/", $version, "/openapi.json"),
                sha256: capability_schema_digest!($id),
                document: include_str!(concat!(
                    "../../../contracts/capabilities/",
                    $id,
                    "/",
                    $version,
                    "/openapi.json"
                )),
            },
            routes: &[$($route),+],
        }
    };
}

macro_rules! capability_schema_digest {
    ("infer.responses") => {
        "abfb3b4b9a3c5d3831d56bb877ecfdd43d62b4442ba101a5ef071ec2740adbd5"
    };
    ("infer.audio.transcription") => {
        "ece4a288a01e8a72cd67a4242896f11f8751ed4a82e99f029f283bbc9de6c580"
    };
    ("infer.audio.alignment") => {
        "76c7f4ab7d5e6333808aceec558822d9deceb2918bc478e326593d302dcb96e8"
    };
    ("infer.audio.event-detection") => {
        "a7179c88c03a768299835bd84c4c6f8d68e47f3c51a96ed4fe01387cf6fb8613"
    };
    ("infer.audio.embedding") => {
        "5cbb2e22487f98a68fef49b9a81f0a8cdb51f45c8c8fa77e78420c8122948c2c"
    };
    ("infer.audio.speech") => {
        "19d29d6799a6cee1a6d24a63f9a9aab73ab925dd2e79f7181fbfe922f6906c68"
    };
    ("infer.audio.voice-clone") => {
        "ac182b38d13a91dd5bb3d69f8f4715807357fc9a75e8cdbdaacf2b15931b0ef8"
    };
    ("infer.audio.transcription-stream") => {
        "855d7a886689e442d5aa6260271061bc1a317bf81fbb28d6257071d4e2f81726"
    };
    ("infer.vision.face-detection") => {
        "3f5c9be22302de8c243c3133704282fa21e6d74e12990c0d1f32deb086986d33"
    };
    ("infer.vision.face-embedding") => {
        "e225a4e06bcadb7274074d3af5aec4d0b96e476f81cac1b4f5e10e33a7d02ccc"
    };
    ("infer.vision.subject-segmentation") => {
        "4df56eefaa7ced43ef3c823f933e9bea689bee23f60d3279c3d84f75414bdb1d"
    };
    ("infer.vision.subject-segmentation-soft-mask") => {
        "8ae50dcde6459072bc09a0f32b7a391df62cb0eae27d45879a3441aa203330d4"
    };
    ("infer.vision.face-parsing") => {
        "663e549c77528811a2c78406ef87b34b6d0add5c0702eec042a49bf7c8968b39"
    };
    ("infer.vision.image-embedding") => {
        "1f30793c1c7866f1cd4239e35dcdb10842b609f021731882810d4de143df42e9"
    };
    ("infer.vision.text-embedding") => {
        "b1bc536f7ded5f1397c739c2b1a282d460ff4b59923fb4f6fd3c564a8627be71"
    };
    ("infer.vision.image-description") => {
        "e655e711e8101ed5f3c11c52fff1eb5fee1ebebb5f51cdd265d0a983d8aa5793"
    };
    ("infer.vision.classification-review") => {
        "c191467016d770f86c0e8df132127293543a2edef8d336dea358a8256bc6157b"
    };
    ("infer.text.embedding") => {
        "93beba6b0076f757c982dc7a9cfb4029e9c585461dc4596f154f138309993040"
    };
    ("infer.text.rerank") => {
        "9de8641a5dd9e575ac7d88c0b12a32964b6a4a4d7c4fca7ba2fa6cfbac0f6979"
    };
    ("infer.document.ocr") => {
        "bad84843af128390030b40d845cca93072ef3ee96fede280773399ad35893aa9"
    };
    ("infer.raw-foundation") => {
        "bb897cf44c58aec6c7173dda5ea43bba78993d0e4233aca7ce3168835d6c3597"
    };
}

pub const CAPABILITIES: &[CapabilityEntry] = &[
    capability!(
        "infer.responses",
        "20260812.1",
        "stable",
        [
            route!("POST", "/v1/responses", &["unary", "server_stream"]),
            route!("GET", "/v1/responses/{response_id}"),
            route!("POST", "/v1/responses/{response_id}/cancel"),
        ]
    ),
    capability!(
        "infer.audio.transcription",
        "20260814.1",
        "stable",
        [route!("POST", "/v1/audio/transcriptions", &["unary"]),]
    ),
    capability!(
        "infer.audio.event-detection",
        "20260813.2",
        "stable",
        [route!("POST", "/v1/audio/event-detections", &["unary"]),]
    ),
    capability!(
        "infer.audio.embedding",
        "20260815.2",
        "experimental",
        [
            route!("POST", "/v1/audio/embeddings", &["unary"]),
            route!("POST", "/v1/audio/text-embeddings", &["unary"]),
        ]
    ),
    capability!(
        "infer.audio.alignment",
        "20260811.1",
        "stable",
        [route!("POST", "/v1/audio/alignments", &["unary"]),]
    ),
    capability!(
        "infer.audio.speech",
        "20260811.1",
        "stable",
        [route!(
            "POST",
            "/v1/audio/speech",
            &["unary", "server_stream"]
        ),]
    ),
    capability!(
        "infer.audio.voice-clone",
        "20260811.1",
        "experimental",
        [route!("POST", "/v1/audio/voice-clones", &["unary"]),]
    ),
    capability!(
        "infer.audio.transcription-stream",
        "20260811.1",
        "experimental",
        [route!(
            "GET",
            "/v1/audio/transcriptions/stream",
            &["duplex_stream"]
        ),]
    ),
    capability!(
        "infer.vision.face-detection",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/face-detections",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.face-embedding",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/face-embeddings",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.subject-segmentation",
        "20260813.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/subject-segmentations",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.subject-segmentation-soft-mask",
        "20260814.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/subject-segmentations/soft-mask",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.face-parsing",
        "20260813.1",
        "experimental",
        [route!("POST", "/infer/v1/vision/face-parsings", &["unary"]),]
    ),
    capability!(
        "infer.vision.image-embedding",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/image-embeddings",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.text-embedding",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/text-embeddings",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.image-description",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/image-descriptions",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.vision.classification-review",
        "20260811.1",
        "experimental",
        [route!(
            "POST",
            "/infer/v1/vision/classification-reviews",
            &["unary"]
        ),]
    ),
    capability!(
        "infer.text.embedding",
        "20260812.1",
        "experimental",
        [
            route!("POST", "/infer/v1/text/query-embeddings", &["unary"]),
            route!("POST", "/infer/v1/text/document-embeddings", &["unary"]),
        ]
    ),
    capability!(
        "infer.text.rerank",
        "20260812.1",
        "experimental",
        [route!("POST", "/infer/v1/text/rerank", &["unary"]),]
    ),
    capability!(
        "infer.document.ocr",
        "20260812.1",
        "experimental",
        [route!("POST", "/infer/v1/documents/ocr", &["unary"]),]
    ),
    capability!(
        "infer.raw-foundation",
        "20260811.1",
        "experimental",
        [
            route!("POST", "/infer/v1/raw/foundations/leases"),
            route!("POST", "/infer/v1/raw/foundations"),
            route!("POST", "/infer/v1/raw/foundations/{job_id}/cancel"),
        ]
    ),
];

pub fn required_capability_id(path: &str) -> Option<&'static str> {
    if path == "/v1/responses" || path.starts_with("/v1/responses/") {
        Some("infer.responses")
    } else if path == "/v1/audio/transcriptions" {
        Some("infer.audio.transcription")
    } else if path == "/v1/audio/event-detections" {
        Some("infer.audio.event-detection")
    } else if path == "/v1/audio/embeddings" || path == "/v1/audio/text-embeddings" {
        Some("infer.audio.embedding")
    } else if path == "/v1/audio/alignments" {
        Some("infer.audio.alignment")
    } else if path == "/v1/audio/speech" {
        Some("infer.audio.speech")
    } else if path == "/v1/audio/voice-clones" {
        Some("infer.audio.voice-clone")
    } else if path == "/v1/audio/transcriptions/stream" {
        Some("infer.audio.transcription-stream")
    } else if path == "/infer/v1/vision/face-detections" {
        Some("infer.vision.face-detection")
    } else if path == "/infer/v1/vision/face-embeddings" {
        Some("infer.vision.face-embedding")
    } else if path == "/infer/v1/vision/subject-segmentations" {
        Some("infer.vision.subject-segmentation")
    } else if path == "/infer/v1/vision/subject-segmentations/soft-mask" {
        Some("infer.vision.subject-segmentation-soft-mask")
    } else if path == "/infer/v1/vision/face-parsings" {
        Some("infer.vision.face-parsing")
    } else if path == "/infer/v1/vision/image-embeddings" {
        Some("infer.vision.image-embedding")
    } else if path == "/infer/v1/vision/text-embeddings" {
        Some("infer.vision.text-embedding")
    } else if path == "/infer/v1/vision/image-descriptions" {
        Some("infer.vision.image-description")
    } else if path == "/infer/v1/vision/classification-reviews" {
        Some("infer.vision.classification-review")
    } else if path == "/infer/v1/text/query-embeddings"
        || path == "/infer/v1/text/document-embeddings"
    {
        Some("infer.text.embedding")
    } else if path == "/infer/v1/text/rerank" {
        Some("infer.text.rerank")
    } else if path == "/infer/v1/documents/ocr" {
        Some("infer.document.ocr")
    } else if path.starts_with("/infer/v1/raw/foundations") {
        Some("infer.raw-foundation")
    } else {
        None
    }
}

pub fn supported_capability_contract(capability_id: &str, identity: &str) -> Option<&'static str> {
    let (id, version) = identity.rsplit_once('@')?;
    if id != capability_id {
        return None;
    }
    CAPABILITIES
        .iter()
        .find(|capability| capability.id == id && capability.schema_version == version)
        .map(|capability| capability.identity)
}

impl CapabilityCatalog {
    pub fn current() -> Self {
        Self {
            schema: CAPABILITY_CATALOG_SCHEMA,
            schema_version: CAPABILITY_CATALOG_VERSION,
            core_contract: CORE_CONTRACT,
            capabilities: CAPABILITIES,
        }
    }
}

pub fn capability_schema_document(id: &str, version: &str) -> Option<&'static str> {
    CAPABILITIES
        .iter()
        .find(|capability| capability.id == id && capability.schema_version == version)
        .map(|capability| capability.schema.document)
}

#[derive(Debug, Serialize)]
pub struct PublicErrorEnvelope {
    pub error: PublicError,
}

#[derive(Debug, Serialize)]
pub struct PublicError {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub code: &'static str,
}

impl PublicErrorEnvelope {
    pub fn new(code: &'static str, message: String) -> Self {
        Self {
            error: PublicError {
                message,
                // Kept compatible with OpenAI-shaped SDK error decoding. The
                // stable, machine-actionable discriminator is `code`.
                kind: "invalid_request_error",
                code,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use infer_core::{
        CapabilityLevel, EvaluationStatus, JobListPage, JobState, Placement, Priority,
        ResourceClass, ResponsesRequest,
    };
    use serde_json::Value;

    use super::*;

    #[test]
    fn embedded_openapi_matches_the_runtime_contract_identity() {
        use sha2::{Digest, Sha256};

        let document: Value = serde_json::from_str(OPENAPI_JSON).expect("OpenAPI JSON is valid");
        let aggregate_document: Value = serde_json::from_str(include_str!(
            "../../../contracts/schema-source/consumer-api-20260813.1.json"
        ))
        .expect("aggregate schema source is valid");
        assert_eq!(
            format!("{:x}", Sha256::digest(OPENAPI_JSON.as_bytes())),
            OPENAPI_SHA256
        );
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["info"]["version"], CORE_VERSION);
        assert_eq!(
            document["components"]["schemas"]["ContractManifest"]["properties"]["core_contract"]["const"],
            CORE_CONTRACT
        );
        assert_eq!(
            document["components"]["schemas"]["ContractManifest"]["properties"]["supported_core_contracts"]
                ["prefixItems"][0]["const"],
            CORE_CONTRACT
        );
        assert_eq!(
            document["components"]["schemas"]["CapabilityCatalog"]["properties"]["schema_version"]
                ["const"],
            CAPABILITY_CATALOG_VERSION
        );
        assert_eq!(
            document["components"]["schemas"]["CapabilityCatalog"]["properties"]["capabilities"]["items"]
                ["required"],
            serde_json::json!(["id", "schema_version", "stability", "schema", "routes"]),
            "one Catalog record must identify exactly one immutable schema"
        );
        assert_eq!(
            document["components"]["parameters"]["ConsumerContract"]["required"],
            true
        );
        assert_eq!(
            document["components"]["parameters"]["ConsumerContract"]["schema"]["const"],
            CORE_CONTRACT
        );
        assert_eq!(
            document["components"]["schemas"]["PublicErrorCode"]["enum"],
            serde_json::json!(CORE_ERROR_CODES),
            "Core OpenAPI must not absorb capability or operator-only error codes"
        );

        for route in CONSUMER_ROUTES {
            let method = route.method.to_ascii_lowercase();
            assert!(
                !document["paths"][route.path][method].is_null(),
                "OpenAPI is missing {} {}",
                route.method,
                route.path
            );
        }

        for capability in CAPABILITIES {
            let identity = format!("{}@{}", capability.id, capability.schema_version);
            let capability_document: Value = serde_json::from_str(capability.schema.document)
                .expect("capability OpenAPI JSON is valid");
            assert_eq!(capability.schema.format, "openapi-3.1");
            assert_eq!(
                format!(
                    "{:x}",
                    Sha256::digest(capability.schema.document.as_bytes())
                ),
                capability.schema.sha256
            );
            assert_eq!(
                capability.schema.url,
                format!(
                    "/infer/v1/capability-schemas/{}/{}/openapi.json",
                    capability.id, capability.schema_version
                )
            );
            assert_eq!(capability_document["x-infer-capability-contract"], identity);
            assert_eq!(
                capability_document["paths"]
                    .as_object()
                    .map(|paths| paths.len()),
                Some(capability.routes.len()),
                "capability artifact must not absorb unrelated routes"
            );
            for (section, components) in capability_document["components"]
                .as_object()
                .expect("capability components are an object")
            {
                for (name, component) in components
                    .as_object()
                    .expect("component section is an object")
                {
                    assert_eq!(
                        component, &aggregate_document["components"][section][name],
                        "capability artifact has stale component {section}/{name}"
                    );
                }
            }
            for route in capability.routes {
                let operation = capability_document["paths"]
                    .get(route.path)
                    .and_then(|path| path.get(route.method.to_ascii_lowercase()))
                    .unwrap_or_else(|| {
                        panic!("OpenAPI is missing {} {}", route.method, route.path)
                    });
                let mut aggregate_operation = aggregate_document["paths"][route.path]
                    [route.method.to_ascii_lowercase()]
                .clone();
                if let Some(responses) = aggregate_operation["responses"].as_object_mut() {
                    responses
                        .retain(|_, response| response["$ref"] != "#/components/responses/Error");
                }
                assert_eq!(
                    operation, &aggregate_operation,
                    "capability artifact has stale operation {} {}",
                    route.method, route.path
                );
                let capability_parameter = operation["parameters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|parameter| parameter["$ref"].as_str())
                    .filter_map(|reference| reference.strip_prefix("#/components/parameters/"))
                    .filter_map(|name| capability_document["components"]["parameters"].get(name))
                    .find(|parameter| parameter["name"] == CAPABILITY_CONTRACT_HEADER);
                assert_eq!(
                    capability_parameter
                        .and_then(|parameter| parameter["schema"]["const"].as_str()),
                    Some(identity.as_str()),
                    "OpenAPI capability identity diverges for {} {}",
                    route.method,
                    route.path
                );
            }
        }
    }

    #[test]
    fn core_response_schemas_are_additive_and_match_current_optional_fields() {
        let document: Value = serde_json::from_str(OPENAPI_JSON).unwrap();
        for schema in [
            "Health",
            "ContractManifest",
            "CapabilityCatalog",
            "JobListItem",
            "JobListPage",
            "JobSnapshot",
            "RoutingDecision",
            "Attempt",
            "CancelResult",
            "ErrorEnvelope",
        ] {
            assert_eq!(
                document["components"]["schemas"][schema]["additionalProperties"], true,
                "response schema {schema} must permit additive fields"
            );
        }
        assert!(
            !document["components"]["schemas"]["JobSnapshot"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "capability_contract"),
            "historical and Core-owned Jobs may omit capability_contract"
        );
        assert_eq!(
            document["components"]["schemas"]["RoutingDecision"]["properties"]["named_route"]["properties"]
                ["kind"]["enum"],
            serde_json::json!(["deployment", "model_profile"])
        );
    }

    #[test]
    fn discovery_offer_and_http_manifest_publish_the_same_exact_version_set() {
        let exact = vec![CORE_VERSION.into()];
        assert!(published_versions_match_contract(&exact));
        assert!(!published_versions_match_contract(&[]));
        assert!(!published_versions_match_contract(&[
            CORE_VERSION.into(),
            "0.1.0-candidate.4".into(),
        ]));
    }

    #[test]
    fn checked_in_examples_are_valid_json() {
        for (name, fixture) in [
            (
                "responses-request",
                include_str!(
                    "../../../contracts/consumer-core/20260813.1/fixtures/responses-request.json"
                ),
            ),
            (
                "responses-response",
                include_str!(
                    "../../../contracts/consumer-core/20260813.1/fixtures/responses-response.json"
                ),
            ),
            (
                "background-queued",
                include_str!(
                    "../../../contracts/consumer-core/20260813.1/fixtures/background-queued.json"
                ),
            ),
            (
                "error",
                include_str!("../../../contracts/consumer-core/20260813.1/fixtures/error.json"),
            ),
            (
                "job-list",
                include_str!("../../../contracts/consumer-core/20260813.1/fixtures/job-list.json"),
            ),
            (
                "job-snapshot",
                include_str!(
                    "../../../contracts/consumer-core/20260813.1/fixtures/job-snapshot.json"
                ),
            ),
        ] {
            serde_json::from_str::<Value>(fixture)
                .unwrap_or_else(|error| panic!("{name} fixture is invalid: {error}"));
        }
    }

    #[test]
    fn generated_contract_artifacts_are_reproducible() {
        let status = std::process::Command::new("python3")
            .arg("tools/generate_capability_schemas.py")
            .arg("--check")
            .current_dir(
                env!("CARGO_MANIFEST_DIR")
                    .strip_suffix("/crates/infer-api")
                    .unwrap(),
            )
            .status()
            .expect("schema generator is runnable");
        assert!(status.success(), "checked-in contract artifacts are stale");
    }

    #[test]
    fn typed_fixtures_and_public_enums_match_the_openapi() {
        let request: ResponsesRequest = serde_json::from_str(include_str!(
            "../../../contracts/consumer-core/20260813.1/fixtures/responses-request.json"
        ))
        .expect("request fixture follows the typed request");
        request.validate().expect("request fixture is admissible");
        serde_json::from_str::<JobListPage>(include_str!(
            "../../../contracts/consumer-core/20260813.1/fixtures/job-list.json"
        ))
        .expect("Job list fixture follows the typed projection");
        let job: infer_core::JobSnapshot = serde_json::from_str(include_str!(
            "../../../contracts/consumer-core/20260813.1/fixtures/job-snapshot.json"
        ))
        .expect("Job snapshot fixture follows the typed projection");
        assert_eq!(job.consumer_core_contract, CORE_CONTRACT);
        assert_eq!(
            job.capability_contract.as_deref(),
            Some("infer.responses@20260812.1")
        );

        let document: Value = serde_json::from_str(OPENAPI_JSON).unwrap();
        assert_enum(
            &document["components"]["schemas"]["Priority"]["enum"],
            &[
                Priority::Interactive,
                Priority::Normal,
                Priority::Background,
            ],
        );
        assert_enum(
            &document["components"]["schemas"]["JobState"]["enum"],
            &[
                JobState::Queued,
                JobState::Running,
                JobState::Succeeded,
                JobState::Failed,
                JobState::Cancelled,
                JobState::Expired,
            ],
        );
        assert_enum(
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["placement"]["enum"],
            &[Placement::Local, Placement::TrustedNode, Placement::Cloud],
        );
        assert_enum(
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["capability_level"]["enum"],
            &[
                CapabilityLevel::Foundational,
                CapabilityLevel::Capable,
                CapabilityLevel::Advanced,
                CapabilityLevel::Expert,
                CapabilityLevel::Exceptional,
            ],
        );
        assert_enum(
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["evaluation_status"]["enum"],
            &[EvaluationStatus::Provisional, EvaluationStatus::Benchmarked],
        );
        assert_enum(
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["resource_class"]["enum"],
            &[
                ResourceClass::Light,
                ResourceClass::Standard,
                ResourceClass::Heavy,
                ResourceClass::Extreme,
            ],
        );
    }

    #[test]
    fn every_openapi_reference_resolves_and_operation_ids_are_unique() {
        let document: Value = serde_json::from_str(OPENAPI_JSON).unwrap();
        let mut references = Vec::new();
        collect_references(&document, &mut references);
        for reference in references {
            let pointer = reference
                .strip_prefix('#')
                .expect("external OpenAPI references are not allowed");
            assert!(
                document.pointer(pointer).is_some(),
                "unresolved OpenAPI reference: {reference}"
            );
        }

        let mut operation_ids = std::collections::BTreeSet::new();
        for path in document["paths"].as_object().unwrap().values() {
            for operation in path.as_object().unwrap().values() {
                let Some(operation_id) = operation.get("operationId").and_then(Value::as_str)
                else {
                    continue;
                };
                assert!(
                    operation_ids.insert(operation_id),
                    "duplicate OpenAPI operationId: {operation_id}"
                );
            }
        }
    }

    fn assert_enum<T: serde::Serialize>(actual: &Value, expected: &[T]) {
        assert_eq!(actual, &serde_json::to_value(expected).unwrap());
    }

    fn collect_references<'a>(value: &'a Value, output: &mut Vec<&'a str>) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    output.push(reference);
                }
                for child in object.values() {
                    collect_references(child, output);
                }
            }
            Value::Array(array) => {
                for child in array {
                    collect_references(child, output);
                }
            }
            _ => {}
        }
    }
}
