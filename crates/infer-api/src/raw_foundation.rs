//! Feature-gated HTTP assembly for the authenticated RawNIND contract.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use infer_control::{RawFoundationControl, RawFoundationControlError};
use infer_raw_foundation::{RawFoundationExecuteRequest, RawFoundationLeaseRequest};

use crate::{ApiError, ApiState, authenticate, strict_json};

#[derive(Clone)]
struct RawState {
    api: ApiState,
    raw: Arc<RawFoundationControl>,
}

/// Typed RAW router, mounted by `inferd` only after the exact configured graph,
/// runtime, socket owner, Deployment, and App ACL pass startup validation.
pub fn router(runtime: Arc<infer_control::Runtime>, raw: Arc<RawFoundationControl>) -> Router {
    Router::new()
        .route("/infer/v1/raw/foundations/leases", post(create_lease))
        .route("/infer/v1/raw/foundations", post(execute))
        .route("/infer/v1/raw/foundations/{job_id}/cancel", post(cancel))
        .with_state(RawState {
            api: ApiState { runtime },
            raw,
        })
}

async fn cancel(
    State(state): State<RawState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<infer_control::RawFoundationCancellation>, ApiError> {
    let app_id = authenticate(&state.api, &headers)?;
    Ok(Json(
        state
            .raw
            .cancel(&app_id, &job_id)
            .await
            .map_err(raw_error)?,
    ))
}

async fn create_lease(
    State(state): State<RawState>,
    headers: HeaderMap,
    request: Result<Json<RawFoundationLeaseRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<infer_control::RawFoundationLeaseGrant>), ApiError> {
    let app_id = authenticate(&state.api, &headers)?;
    let request = strict_json(request)?;
    let grant = state
        .raw
        .create_lease(&app_id, request)
        .await
        .map_err(raw_error)?;
    Ok((StatusCode::CREATED, Json(grant)))
}

async fn execute(
    State(state): State<RawState>,
    headers: HeaderMap,
    request: Result<Json<RawFoundationExecuteRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<infer_control::RawFoundationResponse>, ApiError> {
    let app_id = authenticate(&state.api, &headers)?;
    let request = strict_json(request)?;
    let job_id = request.job_id.clone();
    Ok(Json(
        state
            .raw
            .execute(&app_id, &job_id, request)
            .await
            .map_err(raw_error)?,
    ))
}

fn raw_error(error: RawFoundationControlError) -> ApiError {
    match error {
        RawFoundationControlError::Runtime(error) => error.into(),
        RawFoundationControlError::Raw(error) => raw_execution_error(error),
        RawFoundationControlError::UnknownJob => ApiError {
            status: StatusCode::NOT_FOUND,
            code: "raw_job_not_found",
            message: error.to_string(),
        },
        RawFoundationControlError::Lease(error) => artifact_lease_error(error),
        RawFoundationControlError::Worker => ApiError::internal("RawNIND worker failed"),
    }
}

fn raw_execution_error(error: infer_raw_foundation::RawFoundationError) -> ApiError {
    use infer_raw_foundation::RawFoundationError;
    match error {
        RawFoundationError::InvalidRequest(_) | RawFoundationError::SampleIdentityMismatch => {
            ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "raw_descriptor_invalid",
                message: error.to_string(),
            }
        }
        RawFoundationError::Lease(error) => artifact_lease_error(error),
        RawFoundationError::Io(_)
        | RawFoundationError::ModelContract(_)
        | RawFoundationError::NativeExecution(_) => ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "raw_execution_failed",
            message: "RawNIND execution failed".into(),
        },
    }
}

fn artifact_lease_error(error: infer_artifact_lease::ArtifactLeaseError) -> ApiError {
    use infer_artifact_lease::ArtifactLeaseError;
    match error {
        ArtifactLeaseError::ScopeMismatch => ApiError {
            status: StatusCode::FORBIDDEN,
            code: "artifact_lease_scope_mismatch",
            message: error.to_string(),
        },
        ArtifactLeaseError::GenerationMismatch => ApiError {
            status: StatusCode::CONFLICT,
            code: "daemon_generation_changed",
            message: error.to_string(),
        },
        ArtifactLeaseError::InvalidLease | ArtifactLeaseError::InvalidTicket => ApiError {
            status: StatusCode::CONFLICT,
            code: "artifact_lease_invalid",
            message: error.to_string(),
        },
        ArtifactLeaseError::Cancelled => ApiError {
            status: StatusCode::CONFLICT,
            code: "cancelled",
            message: error.to_string(),
        },
        ArtifactLeaseError::InvalidDescriptor(_) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "artifact_descriptor_invalid",
            message: error.to_string(),
        },
        ArtifactLeaseError::InvalidIdentity | ArtifactLeaseError::InvalidTtl => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "artifact_lease_invalid",
            message: error.to_string(),
        },
        ArtifactLeaseError::StateUnavailable | ArtifactLeaseError::Io(_) => ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "artifact_lease_error",
            message: "artifact lease subsystem failed".into(),
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File, OpenOptions},
        os::{fd::AsRawFd, unix::fs::PermissionsExt},
        path::PathBuf,
        sync::Arc,
    };

    use axum::{
        body::Body,
        http::{Request, header},
    };
    use http_body_util::BodyExt;
    use infer_artifact_lease::{
        ArtifactLeaseRegistry, ArtifactLeaseUnixServer, register_unix_handles,
    };
    use infer_auth::AppCredentials;
    use infer_control::Runtime;
    use infer_core::RuntimeConfig;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tower::ServiceExt;

    #[tokio::test]
    #[ignore = "requires pinned RawNIND graph and ORT 1.27"]
    async fn authenticated_http_uds_job_and_real_model_e2e() {
        let graph = required_path("INFER_TEST_RAWNIND_GRAPH");
        let library = required_path("INFER_TEST_ORT_LIBRARY");
        let temp = TempDir::new().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let input_path = temp.path().join("input.u16le");
        let output_path = temp.path().join("output.shadowrawf");
        let mut bytes = Vec::with_capacity(2048 * 2048 * 2);
        for index in 0..(2048 * 2048) {
            let sample = 64_u16 + u16::try_from(index % 16_000).unwrap();
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(&input_path, &bytes).unwrap();
        fs::set_permissions(&input_path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = raw_config();
        let credentials = AppCredentials::from_pairs([
            ("local-operator", "raw-test-token"),
            ("example-local-consumer", "other-test-token"),
        ])
        .unwrap();
        let runtime = Runtime::with_providers_and_credentials(config, BTreeMap::new(), credentials);
        let registry = Arc::new(
            ArtifactLeaseRegistry::new(unsafe { libc::geteuid() }, "gen_raw_e2e").unwrap(),
        );
        let socket_path = temp.path().join("lease.sock");
        let server =
            Arc::new(ArtifactLeaseUnixServer::bind(&socket_path, Arc::clone(&registry)).unwrap());
        let control = infer_control::RawFoundationControl::new(
            Arc::clone(&runtime),
            Arc::clone(&registry),
            socket_path.clone(),
            &graph,
            &library,
        )
        .unwrap();
        let app = crate::router_with_raw(Arc::clone(&runtime), control);
        let lease_body = json!({
            "model":"raw.materialize_foundation", "priority":"background", "source_revision":"synthetic-phase0-v1",
            "source":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size_bytes":1,"pixel_contract_sha256":"e1998069001c14d01251cc3d6e2bc2aa66b807f3f17d246e7ee7270528302f7f"},
            "staging":{"schema":"infer.raw-foundation-staging@20260811.1","width":2048,"height":2048,"cfa":"RGGB","black_levels":[64,64,64,64],"white_levels":[16383,16383,16383,16383],"sample_format":"uint16-le-row-major-active-bayer","sample_bytes":bytes.len(),"decoded_samples_sha256":format!("{:x}", Sha256::digest(&bytes)),"decoder_provider_id":"shadow.synthetic","decoder_provider_version":"phase-0-v1"}
        });
        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/infer/v1/raw/foundations/leases")
                    .header(
                        crate::contract::CONSUMER_CORE_HEADER,
                        crate::contract::CORE_CONTRACT,
                    )
                    .header(
                        crate::contract::CAPABILITY_CONTRACT_HEADER,
                        "infer.raw-foundation@20260811.1",
                    )
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(lease_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), 401);
        let mut invalid = lease_body.clone();
        invalid["app_id"] = "shadow".into();
        let invalid = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations/leases"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(invalid.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), 400);
        assert_eq!(
            json_body(invalid).await["error"]["code"],
            "invalid_request_error"
        );
        let grant_response = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations/leases"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(lease_body.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grant_response.status(), 201);
        let grant = json_body(grant_response).await;
        assert_eq!(grant["daemon_generation"], "gen_raw_e2e");
        assert_eq!(grant["binding"]["transport"], "uds-scm-rights");
        let job_id = grant["job_id"].as_str().unwrap().to_owned();
        let ticket_id = grant["ticket_id"].as_str().unwrap().to_owned();
        let queued = app
            .clone()
            .oneshot(
                auth(Request::builder().uri(format!("/infer/v1/jobs/{job_id}")))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(queued).await["state"], "queued");
        let input = File::open(&input_path).unwrap();
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .unwrap();
        fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600)).unwrap();
        let server_for_registration = Arc::clone(&server);
        let registration =
            std::thread::spawn(move || server_for_registration.accept_once(now_ms()).unwrap());
        let (lease_id, _) = register_unix_handles(
            &socket_path,
            &ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        registration.join().unwrap();
        let wrong_app = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/infer/v1/raw/foundations")
                    .header(
                        crate::contract::CONSUMER_CORE_HEADER,
                        crate::contract::CORE_CONTRACT,
                    )
                    .header(
                        crate::contract::CAPABILITY_CONTRACT_HEADER,
                        "infer.raw-foundation@20260811.1",
                    )
                    .header(header::AUTHORIZATION, "Bearer other-test-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"job_id":job_id,"lease_id":lease_id}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_app.status(), 404);
        let execute = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"job_id":job_id,"lease_id":lease_id}).to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(execute.status(), 200);
        let response = json_body(execute).await;
        assert_eq!(response["provenance"]["provider"], "raw-foundation-local");
        assert_eq!(response["provenance"]["model_build"], "rawnind_ort127_exp1");
        assert_eq!(
            response["provenance"]["exact_revision"],
            "release-5.6.0@5454d7aa6d89a67054fd4a83343b09e69acaf76a"
        );
        assert_eq!(
            response["provenance"]["graph_sha256"],
            "da27509dab6a2915da67e988acd86cf71f9d5bbc8d1aa0ed32933578a887b901"
        );
        assert_eq!(
            response["artifact"]["artifact_file_sha256"],
            format!("{:x}", Sha256::digest(fs::read(&output_path).unwrap()))
        );
        let job = app
            .clone()
            .oneshot(
                auth(Request::builder().uri(format!("/infer/v1/jobs/{job_id}")))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(job.status(), 200);
        let job = json_body(job).await;
        assert_eq!(job["app_id"], "local-operator");
        assert_eq!(job["state"], "succeeded");
        assert_eq!(job["attempts"][0]["outcome"], "succeeded");
        let repeated = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"job_id":job_id,"lease_id":"lease_reuse"}).to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(repeated.status(), 404);
        assert_eq!(registry.active_counts().unwrap(), (0, 0));

        let cancel_grant = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations/leases"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(lease_body.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        let cancel_grant = json_body(cancel_grant).await;
        let cancel_job = cancel_grant["job_id"].as_str().unwrap().to_owned();
        let cancel_ticket = cancel_grant["ticket_id"].as_str().unwrap().to_owned();
        let cancel_output_path = temp.path().join("cancelled.shadowrawf");
        let input = File::open(&input_path).unwrap();
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cancel_output_path)
            .unwrap();
        fs::set_permissions(&cancel_output_path, fs::Permissions::from_mode(0o600)).unwrap();
        let server_for_registration = Arc::clone(&server);
        let registration =
            std::thread::spawn(move || server_for_registration.accept_once(now_ms()).unwrap());
        let _ = register_unix_handles(
            &socket_path,
            &cancel_ticket,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        registration.join().unwrap();
        let cancelled = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/infer/v1/raw/foundations/{cancel_job}/cancel")),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled.status(), 200);
        assert_eq!(fs::metadata(cancel_output_path).unwrap().len(), 0);
        assert_eq!(registry.active_counts().unwrap(), (0, 0));
        let cancelled_job = app
            .clone()
            .oneshot(
                auth(Request::builder().uri(format!("/infer/v1/jobs/{cancel_job}")))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(cancelled_job).await["state"], "cancelled");

        let running_grant = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri("/infer/v1/raw/foundations/leases"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(lease_body.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        let running_grant = json_body(running_grant).await;
        let running_job = running_grant["job_id"].as_str().unwrap().to_owned();
        let running_ticket = running_grant["ticket_id"].as_str().unwrap().to_owned();
        let running_output_path = temp.path().join("running-cancelled.shadowrawf");
        let input = File::open(&input_path).unwrap();
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&running_output_path)
            .unwrap();
        fs::set_permissions(&running_output_path, fs::Permissions::from_mode(0o600)).unwrap();
        let server_for_registration = Arc::clone(&server);
        let registration =
            std::thread::spawn(move || server_for_registration.accept_once(now_ms()).unwrap());
        let (running_lease, _) = register_unix_handles(
            &socket_path,
            &running_ticket,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        registration.join().unwrap();
        let executing_app = app.clone();
        let executing_job = running_job.clone();
        let execution = tokio::spawn(async move {
            executing_app
                .oneshot(
                    auth(
                        Request::builder()
                            .method("POST")
                            .uri("/infer/v1/raw/foundations"),
                    )
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"job_id":executing_job,"lease_id":running_lease}).to_string(),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap()
        });
        let mut observed_running = false;
        for _ in 0..500 {
            let snapshot = app
                .clone()
                .oneshot(
                    auth(Request::builder().uri(format!("/infer/v1/jobs/{running_job}")))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if json_body(snapshot).await["state"] == "running" {
                observed_running = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(observed_running, "RawNIND Job never entered running state");
        let cancelled = app
            .clone()
            .oneshot(
                auth(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/infer/v1/raw/foundations/{running_job}/cancel")),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled.status(), 200);
        let execution = execution.await.unwrap();
        assert_eq!(execution.status(), 409);
        assert_eq!(json_body(execution).await["error"]["code"], "cancelled");
        let cancelled_job = app
            .oneshot(
                auth(Request::builder().uri(format!("/infer/v1/jobs/{running_job}")))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(cancelled_job).await["state"], "cancelled");
        assert_eq!(registry.active_counts().unwrap(), (0, 0));
    }

    fn raw_config() -> RuntimeConfig {
        let source = format!(
            "{}\n{}",
            include_str!("../../../config/infer.example.toml"),
            r#"
[intents."raw.materialize_foundation"]
data_plane = "raw.foundation"
input_modalities = ["image"]
output_modalities = ["image"]
required_features = ["rawnind_foundation_ort127_exp1"]
default_capability_floor = "foundational"
default_policy = "local-first"
[model_profiles.rawnind]
family = "rawnind"
[model_profiles.rawnind.ratings."raw.materialize_foundation"]
level = "foundational"
status = "benchmarked"
eval_profile = "rawnind-phase2-v1"
score = 1.0
[model_builds.rawnind_ort127_exp1]
profile = "rawnind"
model_id = "darktable-ai/rawnind-public-bayer"
input_modalities = ["image"]
output_modalities = ["image"]
features = ["rawnind_foundation_ort127_exp1"]
[deployments.rawnind_ort127_exp1]
provider = "raw-foundation-local"
build = "rawnind_ort127_exp1"
resource_class = "heavy"
estimated_cost_usd = 0.0
"#
        );
        toml::from_str(&source).unwrap()
    }

    fn auth(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(
                crate::contract::CONSUMER_CORE_HEADER,
                crate::contract::CORE_CONTRACT,
            )
            .header(
                crate::contract::CAPABILITY_CONTRACT_HEADER,
                "infer.raw-foundation@20260811.1",
            )
            .header(header::AUTHORIZATION, "Bearer raw-test-token")
    }
    async fn json_body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
    fn required_path(name: &str) -> PathBuf {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
            .unwrap_or_else(|| panic!("{name} must be an existing absolute file"))
    }
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
}
