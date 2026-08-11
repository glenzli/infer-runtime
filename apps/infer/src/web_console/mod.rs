//! Browser-based local operator console.
//!
//! The web console is a loopback-only presentation and process supervisor. It
//! keeps the runtime credential server-side, exposes a narrow same-origin API
//! to its bundled UI, and delegates control-plane behavior to `OperatorClient`.

mod access;
mod config_file;

use std::{
    collections::{BTreeSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as RoutePath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use infer_auth::AppCredentials;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, mpsc::Receiver},
};
use uuid::Uuid;

use crate::{
    daemon_supervisor::{DaemonSupervisor, LogLine},
    operator_client::OperatorClient,
};

use self::{access::AccessManager, config_file::RuntimeConfigFile};

const INDEX_HTML: &str = include_str!("index.html");
const APP_CSS: &str = include_str!("app.css");
const APP_JS: &str = include_str!("app.js");
const ACCESS_JS: &str = include_str!("access.js");

#[derive(Debug, Clone)]
pub(crate) struct WebConsoleOptions {
    pub(crate) runtime_url: String,
    pub(crate) api_key: String,
    pub(crate) config: PathBuf,
    pub(crate) daemon_bin: Option<PathBuf>,
    pub(crate) spawn: bool,
    pub(crate) bind: SocketAddr,
    pub(crate) open_browser: bool,
    pub(crate) max_logs: usize,
}

#[derive(Clone)]
struct WebState {
    client: OperatorClient,
    supervisor: Arc<Mutex<DaemonSupervisor>>,
    logs: Arc<RwLock<VecDeque<LogLine>>>,
    config_file: RuntimeConfigFile,
    access: AccessManager,
    config_write_lock: Arc<Mutex<()>>,
    pending_access_restart: Arc<RwLock<BTreeSet<String>>>,
    access_admin: bool,
    runtime_url: String,
    csrf: String,
    generation: Arc<AtomicU64>,
    max_logs: usize,
}

#[derive(Debug, Serialize)]
struct DaemonStatus {
    reachable: bool,
    ownership: &'static str,
    pid: Option<u32>,
    uptime_seconds: Option<u64>,
    config_valid: bool,
    config_message: String,
    config_path: String,
    runtime_url: String,
}

#[derive(Debug, Serialize)]
struct LogRecord<'a> {
    recorded_at_unix_ms: u64,
    source: &'a str,
    level: &'a str,
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct ConfigUpdate {
    source: String,
}

pub(crate) async fn run(options: WebConsoleOptions) -> anyhow::Result<()> {
    if !is_loopback(options.bind.ip()) {
        bail!(
            "the operator Web Console may only bind to loopback; received {}",
            options.bind
        );
    }

    let config_file = RuntimeConfigFile::new(options.config.clone());
    let access_admin = local_resource_admin(&config_file, &options.api_key);
    let client = OperatorClient::new(options.runtime_url.clone(), options.api_key)?;
    let access = AccessManager::new(config_file.clone());
    let (supervisor, logs) = DaemonSupervisor::new(options.daemon_bin, options.config.clone());
    let state = WebState {
        client,
        supervisor: Arc::new(Mutex::new(supervisor)),
        logs: Arc::new(RwLock::new(VecDeque::new())),
        config_file,
        access,
        config_write_lock: Arc::new(Mutex::new(())),
        pending_access_restart: Arc::new(RwLock::new(BTreeSet::new())),
        access_admin,
        runtime_url: options.runtime_url,
        csrf: Uuid::new_v4().to_string(),
        generation: Arc::new(AtomicU64::new(0)),
        max_logs: options.max_logs.max(50),
    };
    spawn_log_collector(logs, state.logs.clone(), state.max_logs);

    if options.spawn && !state.client.health_reachable().await {
        state.config_file.validate()?;
        state
            .supervisor
            .lock()
            .await
            .start()
            .await
            .context("start inferd for Web Console")?;
    }

    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("bind Web Console to {}", options.bind))?;
    let address = listener.local_addr().context("read Web Console address")?;
    let url = format!("http://{address}/");
    println!("infer Web Console: {url}");
    println!("Press Ctrl+C to stop the Console.");
    if options.open_browser
        && let Err(error) = open_browser(&url)
    {
        eprintln!("could not open the browser automatically: {error}");
    }

    let serve_result = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    let stop_result = {
        let mut supervisor = state.supervisor.lock().await;
        if supervisor.owns_running_process() {
            supervisor.stop().await
        } else {
            Ok(())
        }
    };
    serve_result.context("serve Web Console")?;
    stop_result
}

fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(styles))
        .route("/app.js", get(script))
        .route("/access.js", get(access_script))
        .route("/api/status", get(status))
        .route("/api/snapshot", get(snapshot))
        .route("/api/logs", get(logs))
        .route("/api/config", get(get_config).put(save_config))
        .merge(access::routes())
        .route("/api/daemon/{action}", post(daemon_action))
        .route("/api/jobs/{job_id}/explain", get(explain_job))
        .route("/api/jobs/{job_id}/cancel", post(cancel_job))
        .route("/api/resources/refresh", post(refresh_resources))
        .route(
            "/api/resources/{provider}/{deployment}/{action}",
            post(resource_action),
        )
        .route("/api/providers/{provider}/probe", post(probe_provider))
        .route("/api/providers/{provider}/models", get(provider_models))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn index(State(state): State<WebState>) -> impl IntoResponse {
    let body = INDEX_HTML.replace("__INFER_CSRF__", &state.csrf);
    Html(body)
}

async fn styles() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn access_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        ACCESS_JS,
    )
}

async fn status(State(state): State<WebState>) -> Json<Value> {
    Json(json!(status_snapshot(&state).await))
}

async fn snapshot(State(state): State<WebState>) -> Json<Value> {
    let generation = state.generation.fetch_add(1, Ordering::Relaxed) + 1;
    Json(json!({
        "daemon": status_snapshot(&state).await,
        "runtime": state.client.snapshot(generation).await,
    }))
}

async fn logs(State(state): State<WebState>) -> Json<Value> {
    let logs = state.logs.read().await;
    let records = logs
        .iter()
        .map(|line| LogRecord {
            recorded_at_unix_ms: line.recorded_at_unix_ms,
            source: line.source.label(),
            level: line.level.label(),
            text: &line.text,
        })
        .collect::<Vec<_>>();
    Json(json!({"logs": records, "capacity": state.max_logs}))
}

async fn get_config(State(state): State<WebState>) -> ApiResult {
    match state.config_file.read_source().await {
        Ok(source) => ok(json!({
            "source": source,
            "path": state.config_file.path().display().to_string(),
            "validation": state.config_file.validation_message(),
        })),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn save_config(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(update): Json<ConfigUpdate>,
) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    let _write_guard = state.config_write_lock.lock().await;
    let previous = state.config_file.load().ok();
    let parsed = RuntimeConfigFile::validate_source(&update.source)
        .map_err(|error| api_failure(StatusCode::BAD_REQUEST, error.to_string()))?;
    state
        .config_file
        .write_validated(&update.source)
        .await
        .map_err(|error| api_failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if previous.is_some_and(|previous| {
        serde_json::to_value(previous.apps).ok() != serde_json::to_value(parsed.apps).ok()
    }) {
        state
            .pending_access_restart
            .write()
            .await
            .insert("*".into());
    }
    ok(json!({
        "saved": true,
        "path": state.config_file.path().display().to_string(),
        "requires_restart": true,
    }))
}

async fn daemon_action(
    State(state): State<WebState>,
    RoutePath(action): RoutePath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    match action.as_str() {
        "start" => {
            state
                .config_file
                .validate()
                .map_err(|error| api_failure(StatusCode::BAD_REQUEST, error.to_string()))?;
            if state.client.health_reachable().await {
                return api_error(
                    StatusCode::CONFLICT,
                    "inferd is already reachable; the Console will not start a competing daemon",
                );
            }
            state
                .supervisor
                .lock()
                .await
                .start()
                .await
                .map_err(|error| api_failure(StatusCode::CONFLICT, error.to_string()))?;
            state.pending_access_restart.write().await.clear();
            ok(json!({"started": true}))
        }
        "stop" => {
            let mut supervisor = state.supervisor.lock().await;
            if !supervisor.owns_running_process() {
                return api_error(
                    StatusCode::CONFLICT,
                    "the Console can only stop an inferd process it started",
                );
            }
            supervisor.stop().await.map_err(|error| {
                api_failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            })?;
            ok(json!({"stopped": true}))
        }
        "restart" => {
            state
                .config_file
                .validate()
                .map_err(|error| api_failure(StatusCode::BAD_REQUEST, error.to_string()))?;
            let mut supervisor = state.supervisor.lock().await;
            if !supervisor.owns_running_process() {
                return api_error(
                    StatusCode::CONFLICT,
                    "the Console can only restart an inferd process it started",
                );
            }
            supervisor.restart().await.map_err(|error| {
                api_failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            })?;
            state.pending_access_restart.write().await.clear();
            ok(json!({"restarted": true}))
        }
        _ => api_error(StatusCode::NOT_FOUND, "unknown daemon action"),
    }
}

async fn explain_job(
    State(state): State<WebState>,
    RoutePath(job_id): RoutePath<String>,
) -> ApiResult {
    from_operator(state.client.explain_job(&job_id).await)
}

async fn cancel_job(
    State(state): State<WebState>,
    RoutePath(job_id): RoutePath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    from_operator(state.client.cancel_job(&job_id).await)
}

async fn refresh_resources(State(state): State<WebState>, headers: HeaderMap) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    from_operator(state.client.refresh_resources().await)
}

async fn resource_action(
    State(state): State<WebState>,
    RoutePath((provider, deployment, action)): RoutePath<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    let result = match action.as_str() {
        "load" => state.client.load_resource(&provider, &deployment).await,
        "unload" => state.client.unload_resource(&provider, &deployment).await,
        _ => return api_error(StatusCode::NOT_FOUND, "unknown resource action"),
    };
    from_operator(result)
}

async fn probe_provider(
    State(state): State<WebState>,
    RoutePath(provider): RoutePath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_mutation(&state, &headers)?;
    from_operator(state.client.probe_provider(&provider).await)
}

async fn provider_models(
    State(state): State<WebState>,
    RoutePath(provider): RoutePath<String>,
) -> ApiResult {
    from_operator(state.client.provider_models(&provider).await)
}

async fn status_snapshot(state: &WebState) -> DaemonStatus {
    let reachable = state.client.health_reachable().await;
    let (ownership, pid, uptime_seconds) = {
        let mut supervisor = state.supervisor.lock().await;
        let _ = supervisor.poll_exit();
        if supervisor.owns_running_process() {
            ("console", supervisor.pid(), supervisor.uptime_seconds())
        } else if reachable {
            ("external", None, None)
        } else {
            ("none", None, None)
        }
    };
    let (config_valid, config_message) = match state.config_file.validate() {
        Ok(()) => (true, "Configuration is valid".to_owned()),
        Err(error) => (false, error.to_string()),
    };
    DaemonStatus {
        reachable,
        ownership,
        pid,
        uptime_seconds,
        config_valid,
        config_message,
        config_path: state.config_file.path().display().to_string(),
        runtime_url: state.runtime_url.clone(),
    }
}

fn authorize_mutation(state: &WebState, headers: &HeaderMap) -> Result<(), ApiFailure> {
    let supplied = headers
        .get("x-infer-console-session")
        .and_then(|value| value.to_str().ok());
    if supplied == Some(state.csrf.as_str()) {
        Ok(())
    } else {
        Err(api_failure(
            StatusCode::FORBIDDEN,
            "missing or invalid Console session proof",
        ))
    }
}

fn authorize_access_mutation(state: &WebState, headers: &HeaderMap) -> Result<(), ApiFailure> {
    authorize_mutation(state, headers)?;
    authorize_access_read(state)
}

fn authorize_access_read(state: &WebState) -> Result<(), ApiFailure> {
    if state.access_admin {
        Ok(())
    } else {
        Err(api_failure(
            StatusCode::FORBIDDEN,
            "Apps & Access requires a local resource-admin credential",
        ))
    }
}

fn local_resource_admin(config_file: &RuntimeConfigFile, api_key: &str) -> bool {
    let Ok(config) = config_file.load() else {
        return false;
    };
    let Ok(credentials) = AppCredentials::load_or_create(&config) else {
        return false;
    };
    credentials
        .authenticate(api_key)
        .and_then(|app_id| config.apps.get(app_id))
        .is_some_and(|app| app.resource_admin)
}

async fn mark_access_pending(state: &WebState, app_id: &str) {
    state
        .pending_access_restart
        .write()
        .await
        .insert(app_id.to_owned());
}

async fn record_console_event(state: &WebState, message: String) {
    let mut logs = state.logs.write().await;
    logs.push_back(LogLine::console_audit(message));
    while logs.len() > state.max_logs {
        logs.pop_front();
    }
}

fn spawn_log_collector(
    mut receiver: Receiver<LogLine>,
    logs: Arc<RwLock<VecDeque<LogLine>>>,
    max_logs: usize,
) {
    tokio::spawn(async move {
        while let Some(line) = receiver.recv().await {
            let mut logs = logs.write().await;
            logs.push_back(line);
            while logs.len() > max_logs {
                logs.pop_front();
            }
        }
    });
}

fn open_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(url)
        .spawn()
        .context("launch the default browser")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let trusted_host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_host);
    if !trusted_host {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "Infer Console only accepts loopback Host headers",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
        ),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}

type ApiResult = Result<Json<Value>, ApiFailure>;
type ApiFailure = (StatusCode, Json<Value>);

fn ok(value: Value) -> ApiResult {
    Ok(Json(json!({"ok": true, "result": value})))
}

fn serialize_ok(value: impl Serialize) -> ApiResult {
    match serde_json::to_value(value) {
        Ok(value) => ok(value),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

fn from_operator(result: Result<Value, String>) -> ApiResult {
    match result {
        Ok(value) => ok(value),
        Err(error) => api_error(StatusCode::BAD_GATEWAY, error),
    }
}

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiResult {
    Err(api_failure(status, message))
}

fn api_failure(status: StatusCode, message: impl Into<String>) -> ApiFailure {
    (
        status,
        Json(json!({"ok": false, "error": {"message": message.into()}})),
    )
}

fn is_loopback(address: IpAddr) -> bool {
    address.is_loopback()
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost")
        || host
            .strip_prefix("localhost:")
            .is_some_and(|port| port.parse::<u16>().is_ok())
    {
        return true;
    }
    if let Ok(address) = host.parse::<SocketAddr>() {
        return address.ip().is_loopback();
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn test_state() -> WebState {
        let config =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config_file = RuntimeConfigFile::new(config.clone());
        let client =
            OperatorClient::new("http://127.0.0.1:9".into(), "test-runtime-token".into()).unwrap();
        let (supervisor, _receiver) = DaemonSupervisor::new(None, config.clone());
        WebState {
            client,
            supervisor: Arc::new(Mutex::new(supervisor)),
            logs: Arc::new(RwLock::new(VecDeque::new())),
            access: AccessManager::new(config_file.clone()),
            config_file,
            config_write_lock: Arc::new(Mutex::new(())),
            pending_access_restart: Arc::new(RwLock::new(BTreeSet::new())),
            access_admin: true,
            runtime_url: "http://127.0.0.1:9".into(),
            csrf: "test-console-session".into(),
            generation: Arc::new(AtomicU64::new(0)),
            max_logs: 50,
        }
    }

    #[tokio::test]
    async fn index_bootstraps_the_private_console_session() {
        let response = router(test_state())
            .oneshot(
                Request::get("/")
                    .header(header::HOST, "127.0.0.1:8790")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("test-console-session"));
        assert!(body.contains("name=\"color-scheme\" content=\"light dark\""));
        assert!(body.contains("Apps 与访问"));
        assert!(body.contains("/access.js"));
        assert!(!body.contains("__INFER_CSRF__"));
    }

    #[test]
    fn bundled_styles_follow_the_system_color_scheme() {
        assert!(APP_CSS.contains("@media (prefers-color-scheme: light)"));
        assert!(APP_CSS.contains("color-scheme: light dark"));
        assert!(APP_CSS.contains("--log-bg: #f7f7f6"));
        assert!(APP_CSS.contains("--accent: #d5d7d8"));
        assert!(APP_CSS.contains("--accent: #3f4245"));
        assert!(!APP_CSS.contains("#82f0c5"));
        assert!(!APP_CSS.contains("#167d60"));
        assert!(!APP_CSS.contains("gradient("));
        assert!(!ACCESS_JS.contains("localStorage"));
        assert!(!ACCESS_JS.contains("sessionStorage"));
        assert!(ACCESS_JS.contains("全部 Intent（本机 Operator）"));
        assert!(ACCESS_JS.contains("全部 Intent（兼容配置）"));
        assert!(!ACCESS_JS.contains("全部 Intent（兼容模式）"));
    }

    #[test]
    fn bundled_model_catalog_exposes_intent_and_capability_filters() {
        assert!(INDEX_HTML.contains("id=\"model-intent-filter\""));
        assert!(INDEX_HTML.contains("id=\"model-capability-filter\""));
        assert!(INDEX_HTML.contains("id=\"model-placement-filter\""));
        assert!(APP_JS.contains("function deploymentMatchesModelFilters"));
        assert!(APP_JS.contains("deployment.ratings"));
        assert!(APP_JS.contains("model-filter-count"));
    }

    #[tokio::test]
    async fn mutating_routes_reject_cross_origin_requests_without_session_proof() {
        let response = router(test_state())
            .oneshot(
                Request::post("/api/daemon/start")
                    .header(header::HOST, "127.0.0.1:8790")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let access_response = router(test_state())
            .oneshot(
                Request::post("/api/access/apps/sample-consumer/rotate")
                    .header(header::HOST, "127.0.0.1:8790")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(access_response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn access_inventory_requires_a_local_resource_admin() {
        let mut state = test_state();
        state.access_admin = false;
        let response = router(state)
            .oneshot(
                Request::get("/api/access/apps")
                    .header(header::HOST, "127.0.0.1:8790")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn public_console_binding_is_rejected_by_policy() {
        assert!(is_loopback("127.0.0.1".parse().unwrap()));
        assert!(!is_loopback("0.0.0.0".parse().unwrap()));
        assert!(is_loopback_host("127.0.0.1:8790"));
        assert!(is_loopback_host("[::1]:8790"));
        assert!(is_loopback_host("localhost:8790"));
        assert!(!is_loopback_host("attacker.example:8790"));
    }
}
