use std::collections::BTreeMap;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use infer_auth::AppCredentials;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
use serde_json::Value;
use tower::ServiceExt;

use crate::router;

const OBSERVER_TOKEN: &str = "observer-test-token";
const OPERATOR_TOKEN: &str = "operator-test-token";

fn observer_router() -> axum::Router {
    let config: RuntimeConfig = toml::from_str(include_str!("../../../config/infer.example.toml"))
        .expect("checked-in config parses");
    let credentials = AppCredentials::from_pairs([
        ("infra-sentinel", OBSERVER_TOKEN),
        ("local-operator", OPERATOR_TOKEN),
    ])
    .unwrap();
    router(Runtime::with_providers_and_credentials(
        config,
        BTreeMap::new(),
        credentials,
    ))
}

fn authenticated(
    request: axum::http::request::Builder,
    token: &str,
) -> axum::http::request::Builder {
    request.header(header::AUTHORIZATION, format!("Bearer {token}"))
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body is readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("response is JSON")
}

#[tokio::test]
async fn observer_snapshot_is_versioned_bounded_and_redacted() {
    let response = observer_router()
        .oneshot(
            authenticated(
                Request::builder().uri("/infer/v1/observer/snapshot"),
                OBSERVER_TOKEN,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let snapshot = body_json(response).await;
    assert_eq!(snapshot["schema"], "infer-runtime.status.snapshot");
    assert_eq!(snapshot["schema_version"], "20260810.1");
    assert_eq!(snapshot["service"]["kind"], "infer-runtime");
    assert!(
        snapshot["service"]["generation"]
            .as_str()
            .is_some_and(|generation| generation.starts_with("gen_"))
    );
    assert_eq!(snapshot["links"]["console_url"], "http://127.0.0.1:8790/");
    assert_eq!(snapshot["status"]["state"], "starting");
    assert!(
        !snapshot["status"]["reason_codes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|code| code == "infer.resource.pressure_unknown")
    );
    assert!(snapshot["sequence"].as_u64().unwrap() > 0);
    let headline_metrics = snapshot["headline_metrics"].as_array().unwrap();
    assert!(headline_metrics.len() <= 3);
    let metrics = snapshot["metrics"].as_array().unwrap();
    for headline_id in headline_metrics {
        let headline_id = headline_id.as_str().expect("headline is a metric ID");
        assert!(
            metrics
                .iter()
                .any(|metric| metric["id"].as_str() == Some(headline_id)),
            "headline metric {headline_id} must exist in metrics"
        );
    }
    assert!(snapshot["extensions"].get("infer-runtime").is_some());

    let serialized = serde_json::to_string(&snapshot).unwrap();
    for forbidden_key in [
        "job_id",
        "last_error",
        "api_key",
        "credential_path",
        "payload_directory",
    ] {
        assert!(
            !serialized.contains(&format!("\"{forbidden_key}\"")),
            "leaked field {forbidden_key}"
        );
    }
    let excluded = snapshot["redaction"]["excluded"].as_array().unwrap();
    assert!(excluded.iter().any(|value| value == "payloads"));
    assert!(excluded.iter().any(|value| value == "usage_ledger"));
    for metric in metrics {
        assert!(!metric["value"].is_null());
    }
    for issue in snapshot["issues"].as_array().unwrap() {
        assert!(matches!(
            issue["severity"].as_str(),
            Some("info" | "warning" | "critical")
        ));
    }
}

#[tokio::test]
async fn observer_credential_is_rejected_from_every_existing_surface_class() {
    for (method, path, body) in [
        ("GET", "/infer/v1/metrics", ""),
        ("GET", "/infer/v1/providers", ""),
        ("GET", "/infer/v1/resources", ""),
        ("GET", "/infer/v1/budget", ""),
        ("GET", "/infer/v1/jobs", ""),
        ("POST", "/infer/v1/resources", ""),
        (
            "POST",
            "/v1/responses",
            r#"{"model":"text.summarize","input":"should never run"}"#,
        ),
    ] {
        let response = observer_router()
            .oneshot(
                authenticated(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::CONTENT_TYPE, "application/json"),
                    OBSERVER_TOKEN,
                )
                .body(Body::from(body))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        assert_eq!(
            body_json(response).await["error"]["code"],
            "observer_credential_restricted",
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn operator_credential_cannot_reuse_the_observer_endpoint() {
    let response = observer_router()
        .oneshot(
            authenticated(
                Request::builder().uri("/infer/v1/observer/snapshot"),
                OPERATOR_TOKEN,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "observer_access_required"
    );
}

#[tokio::test]
async fn health_and_contract_remain_public_bootstrap_routes() {
    for path in ["/health", "/infer/v1/contract"] {
        let response = observer_router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}
