//! Infer Runtime's read-only status protocol and Infra Discovery publisher.
//!
//! Discovery owns only service offers and process-generation publication. The status
//! protocol independently owns request framing and the redacted snapshot.

mod consumer_offer;
mod discovery;
mod socket;

use std::collections::BTreeMap;

pub use consumer_offer::{
    CONSUMER_HTTP_LOOPBACK_BINDING, CONSUMER_PROTOCOL, ConsumerOfferError, consumer_http_offer,
    validate_consumer_http_endpoint,
};
pub use discovery::{
    DISCOVERY_SCHEMA, DISCOVERY_SCHEMA_VERSION, DiscoveryError, DiscoveryOffer,
    DiscoveryRegistration, DiscoveryRuntime, DiscoveryService, RegistrationPublication,
    RegistrationSpec, UNIX_SOCKET_OPAQUE_MAX_BYTES, unique_status_socket_endpoint,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use socket::{
    MAX_STATUS_REQUEST_BYTES, MAX_STATUS_RESPONSE_BYTES, SNAPSHOT_REQUEST_LINE,
    STATUS_REQUEST_SCHEMA, SnapshotFuture, SnapshotProvider, UnixJsonObserverServer,
    UnixJsonServerError,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

pub const STATUS_PROTOCOL: &str = "infer-runtime.status";
pub const STATUS_PROTOCOL_VERSION: &str = "20260810.1";
pub const SNAPSHOT_SCHEMA: &str = "infer-runtime.status.snapshot";
pub const OBSERVER_ERROR_SCHEMA: &str = "infer-runtime.status.error";

#[derive(Debug, Clone)]
pub struct ObserverIdentity {
    pub service: SnapshotService,
    pub links: Option<ObserverLinks>,
}

impl ObserverIdentity {
    pub fn new(
        service_kind: impl Into<String>,
        instance_id: impl Into<String>,
        console_url: Option<String>,
    ) -> Self {
        Self {
            service: SnapshotService {
                kind: service_kind.into(),
                instance_id: instance_id.into(),
                generation: format!("gen_{}", Uuid::new_v4().simple()),
            },
            links: console_url.map(|console_url| ObserverLinks { console_url }),
        }
    }

    pub fn snapshot_service(&self) -> SnapshotService {
        SnapshotService {
            kind: self.service.kind.clone(),
            instance_id: self.service.instance_id.clone(),
            generation: self.service.generation.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotService {
    pub kind: String,
    pub instance_id: String,
    pub generation: String,
}

impl From<&SnapshotService> for DiscoveryService {
    fn from(service: &SnapshotService) -> Self {
        Self {
            kind: service.kind.clone(),
            instance_id: service.instance_id.clone(),
            generation: service.generation.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObserverLinks {
    pub console_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverStatusState {
    Starting,
    Healthy,
    Degraded,
    Unavailable,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverStatus {
    pub state: ObserverStatusState,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Gauge,
    Counter,
    State,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverMetric {
    pub id: String,
    pub kind: MetricKind,
    #[serde(deserialize_with = "deserialize_non_null_value")]
    value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dimensions: BTreeMap<String, String>,
}

impl ObserverMetric {
    pub fn new(id: impl Into<String>, kind: MetricKind, value: impl Into<Value>) -> Self {
        let value = value.into();
        assert!(!value.is_null(), "observer metric values must not be null");
        Self {
            id: id.into(),
            kind,
            value,
            unit: None,
            window_seconds: None,
            dimensions: BTreeMap::new(),
        }
    }

    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

fn deserialize_non_null_value<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom(
            "observer metric values must not be null",
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverIssue {
    pub code: String,
    pub severity: IssueSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverSnapshot {
    pub schema: String,
    pub schema_version: String,
    pub service: SnapshotService,
    pub sequence: u64,
    pub captured_at: String,
    pub status: ObserverStatus,
    pub headline_metrics: Vec<String>,
    pub metrics: Vec<ObserverMetric>,
    pub issues: Vec<ObserverIssue>,
    pub extensions: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<ObserverLinks>,
    pub redaction: ObserverRedaction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverRedaction {
    pub excluded: Vec<String>,
}

impl Default for ObserverRedaction {
    fn default() -> Self {
        Self {
            excluded: vec![
                "credentials".into(),
                "filesystem_paths".into(),
                "job_identifiers".into(),
                "job_metadata".into(),
                "payloads".into(),
                "raw_errors".into(),
                "usage_ledger".into(),
            ],
        }
    }
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("system time is representable as RFC 3339")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_contains_no_secret_material_and_uses_stable_schema_values() {
        let identity = ObserverIdentity::new(
            "infer-runtime",
            "local",
            Some("http://127.0.0.1:8790/".into()),
        );
        assert_eq!(STATUS_PROTOCOL_VERSION, "20260810.1");
        assert!(identity.service.generation.starts_with("gen_"));
        assert_eq!(identity.snapshot_service().kind, "infer-runtime");
        let discovery = DiscoveryService::from(&identity.service);
        assert_eq!(discovery.kind, identity.service.kind);
        assert_eq!(discovery.instance_id, identity.service.instance_id);
        assert_eq!(discovery.generation, identity.service.generation);
        assert_eq!(
            identity.links.unwrap().console_url,
            "http://127.0.0.1:8790/"
        );
    }

    #[test]
    fn metric_values_reject_null_during_construction_and_deserialization() {
        assert!(
            std::panic::catch_unwind(|| {
                ObserverMetric::new("infer.test", MetricKind::Gauge, Value::Null)
            })
            .is_err()
        );
        let encoded = r#"{"id":"infer.test","kind":"gauge","value":null}"#;
        assert!(serde_json::from_str::<ObserverMetric>(encoded).is_err());
    }

    #[test]
    fn redaction_matches_the_frozen_contract() {
        assert_eq!(
            serde_json::to_value(ObserverRedaction::default()).unwrap(),
            serde_json::json!({
                "excluded": [
                    "credentials",
                    "filesystem_paths",
                    "job_identifiers",
                    "job_metadata",
                    "payloads",
                    "raw_errors",
                    "usage_ledger"
                ]
            })
        );
    }
}
