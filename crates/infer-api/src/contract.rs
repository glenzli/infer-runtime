//! Machine-readable identity for the external v0.1 consumer contract.

use serde::Serialize;

pub const CONTRACT_VERSION: &str = "0.1.0-candidate.2";
pub const CONTRACT_STABILITY: &str = "candidate";
pub const OPENAPI_PATH: &str = "/infer/v1/openapi.json";
pub const OPENAPI_JSON: &str = include_str!("../../../contracts/v0.1/openapi.json");

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
        method: "POST",
        path: "/v1/responses",
    },
    ContractRoute {
        method: "GET",
        path: "/v1/responses/{response_id}",
    },
    ContractRoute {
        method: "POST",
        path: "/v1/responses/{response_id}/cancel",
    },
    ContractRoute {
        method: "POST",
        path: "/v1/audio/transcriptions",
    },
    ContractRoute {
        method: "POST",
        path: "/v1/audio/alignments",
    },
    ContractRoute {
        method: "POST",
        path: "/v1/audio/speech",
    },
    ContractRoute {
        method: "POST",
        path: "/v1/audio/voice-clones",
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
        path: OPENAPI_PATH,
    },
];

pub const EXPERIMENTAL_ROUTES: &[ContractRoute] = &[
    ContractRoute {
        method: "GET",
        path: "/v1/audio/transcriptions/stream",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/face-detections",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/face-embeddings",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/image-embeddings",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/text-embeddings",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/image-descriptions",
    },
    ContractRoute {
        method: "POST",
        path: "/infer/v1/vision/classification-reviews",
    },
];

#[derive(Debug, Serialize)]
pub struct ContractManifest {
    pub contract_version: &'static str,
    pub stability: &'static str,
    pub compatibility: &'static str,
    pub openapi_url: &'static str,
    pub consumer_routes: &'static [ContractRoute],
    pub experimental_routes: &'static [ContractRoute],
    pub operator_routes: OperatorRouteStatus,
}

#[derive(Debug, Serialize)]
pub struct OperatorRouteStatus {
    pub stability: &'static str,
    pub note: &'static str,
}

impl ContractManifest {
    pub fn current() -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            stability: CONTRACT_STABILITY,
            compatibility: "this revision is immutable; later candidate revisions are additive unless accompanied by migration notes",
            openapi_url: OPENAPI_PATH,
            consumer_routes: CONSUMER_ROUTES,
            experimental_routes: EXPERIMENTAL_ROUTES,
            operator_routes: OperatorRouteStatus {
                stability: "experimental",
                note: "metrics, budget, provider probe/model inventory, and resource lifecycle routes are not part of the consumer compatibility promise",
            },
        }
    }
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
        JobListPage, JobState, Placement, Priority, QualityGrade, RatingStatus, ResourceClass,
        ResponsesRequest,
    };
    use serde_json::Value;

    use super::*;

    #[test]
    fn embedded_openapi_matches_the_runtime_contract_identity() {
        let document: Value = serde_json::from_str(OPENAPI_JSON).expect("OpenAPI JSON is valid");
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["info"]["version"], CONTRACT_VERSION);
        assert_eq!(
            document["components"]["schemas"]["ContractManifest"]["properties"]["contract_version"]
                ["const"],
            CONTRACT_VERSION
        );
        assert!(
            document["components"]["schemas"]["ContractManifest"]["required"]
                .as_array()
                .expect("manifest required fields")
                .iter()
                .any(|field| field == "experimental_routes")
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
    }

    #[test]
    fn checked_in_examples_are_valid_json() {
        for (name, fixture) in [
            (
                "responses-request",
                include_str!("../../../contracts/v0.1/fixtures/responses-request.json"),
            ),
            (
                "responses-response",
                include_str!("../../../contracts/v0.1/fixtures/responses-response.json"),
            ),
            (
                "background-queued",
                include_str!("../../../contracts/v0.1/fixtures/background-queued.json"),
            ),
            (
                "error",
                include_str!("../../../contracts/v0.1/fixtures/error.json"),
            ),
            (
                "job-list",
                include_str!("../../../contracts/v0.1/fixtures/job-list.json"),
            ),
        ] {
            serde_json::from_str::<Value>(fixture)
                .unwrap_or_else(|error| panic!("{name} fixture is invalid: {error}"));
        }
    }

    #[test]
    fn typed_fixtures_and_public_enums_match_the_openapi() {
        let request: ResponsesRequest = serde_json::from_str(include_str!(
            "../../../contracts/v0.1/fixtures/responses-request.json"
        ))
        .expect("request fixture follows the typed request");
        request.validate().expect("request fixture is admissible");
        serde_json::from_str::<JobListPage>(include_str!(
            "../../../contracts/v0.1/fixtures/job-list.json"
        ))
        .expect("Job list fixture follows the typed projection");

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
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["quality_grade"]["enum"],
            &[
                QualityGrade::Basic,
                QualityGrade::General,
                QualityGrade::Advanced,
                QualityGrade::Frontier,
            ],
        );
        assert_enum(
            &document["components"]["schemas"]["JobSnapshot"]["properties"]["rating_status"]["enum"],
            &[RatingStatus::Provisional, RatingStatus::Benchmarked],
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
