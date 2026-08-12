//! App identity and managed consumer credential lifecycle for the Web Console.
//!
//! This owner keeps secret-file mutation, comment-preserving TOML edits, and
//! App policy validation in one serialized transaction. Plaintext tokens only
//! leave this module in create/rotate responses and are never listed again.

use std::collections::BTreeSet;

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use infer_auth::{ManagedCredentialStore, ProvisionedManagedCredential};
use infer_core::{
    AppConfig, AppCredentialConfig, BuiltinTool, Modality, ObserverAccess, ProviderAccessClass,
    RequestOverrideConfig, RuntimeConfig,
};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use super::config_file::RuntimeConfigFile;
use super::{
    ApiResult, WebState, api_error, authorize_access_mutation, authorize_access_read,
    mark_access_pending, record_console_event, serialize_ok,
};

const PROTECTED_OPERATOR: &str = "local-operator";

pub(super) fn routes() -> Router<WebState> {
    Router::new()
        .route(
            "/api/access/apps",
            get(list_access_apps).post(create_access_app),
        )
        .route(
            "/api/access/apps/{app_id}",
            put(update_access_app).delete(revoke_access_app),
        )
        .route("/api/access/apps/{app_id}/rotate", post(rotate_access_app))
}

async fn list_access_apps(State(state): State<WebState>) -> ApiResult {
    authorize_access_read(&state)?;
    let pending = state.pending_access_restart.read().await.clone();
    match state.access.list(&pending).await {
        Ok(overview) => serialize_ok(overview),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn create_access_app(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<AppAccessInput>,
) -> ApiResult {
    authorize_access_mutation(&state, &headers)?;
    let app_id = input.app_id.clone();
    let _write_guard = state.config_write_lock.lock().await;
    match state.access.create(input).await {
        Ok(credential) => {
            mark_access_pending(&state, &app_id).await;
            record_console_event(&state, format!("access app_created app_id={app_id}")).await;
            serialize_ok(credential)
        }
        Err(error) => api_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn update_access_app(
    State(state): State<WebState>,
    RoutePath(app_id): RoutePath<String>,
    headers: HeaderMap,
    Json(input): Json<AppAccessInput>,
) -> ApiResult {
    authorize_access_mutation(&state, &headers)?;
    if input.app_id != app_id {
        return api_error(
            StatusCode::BAD_REQUEST,
            "route and body App ids do not match",
        );
    }
    let _write_guard = state.config_write_lock.lock().await;
    match state.access.update(input).await {
        Ok(result) => {
            mark_access_pending(&state, &app_id).await;
            record_console_event(&state, format!("access app_updated app_id={app_id}")).await;
            serialize_ok(result)
        }
        Err(error) => api_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn rotate_access_app(
    State(state): State<WebState>,
    RoutePath(app_id): RoutePath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_access_mutation(&state, &headers)?;
    let _write_guard = state.config_write_lock.lock().await;
    match state.access.rotate(&app_id).await {
        Ok(credential) => {
            mark_access_pending(&state, &app_id).await;
            record_console_event(&state, format!("access credential_rotated app_id={app_id}"))
                .await;
            serialize_ok(credential)
        }
        Err(error) => api_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn revoke_access_app(
    State(state): State<WebState>,
    RoutePath(app_id): RoutePath<String>,
    headers: HeaderMap,
) -> ApiResult {
    authorize_access_mutation(&state, &headers)?;
    let _write_guard = state.config_write_lock.lock().await;
    match state.access.revoke(&app_id).await {
        Ok(result) => {
            mark_access_pending(&state, &app_id).await;
            record_console_event(&state, format!("access app_revoked app_id={app_id}")).await;
            serialize_ok(result)
        }
        Err(error) => api_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

#[derive(Debug, Clone)]
pub(super) struct AccessManager {
    config_file: RuntimeConfigFile,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppAccessInput {
    pub app_id: String,
    /// An explicit empty list denies inference. For defensive compatibility,
    /// a missing field creates a deny-all App and preserves the current ACL on
    /// update; the Console always sends an explicit list.
    #[serde(default)]
    pub allowed_intents: Option<Vec<String>>,
    /// Omission preserves the existing value on update and creates a
    /// deny-all hosted-tool ACL. Web Search is never implied by subscription
    /// provider access.
    #[serde(default)]
    pub allowed_builtin_tools: Option<BTreeSet<BuiltinTool>>,
    /// Omission preserves the existing value on update and creates a
    /// standard-only Consumer. The Console exposes subscription access as an
    /// explicit operator-controlled permission, never as part of a preset.
    #[serde(default)]
    pub allowed_provider_access_classes: Option<BTreeSet<ProviderAccessClass>>,
    /// Independent cloud payload egress permission. Omission preserves the
    /// existing value on update and creates a Consumer with no cloud egress.
    #[serde(default)]
    pub allowed_cloud_input_modalities: Option<BTreeSet<Modality>>,
    pub max_pending_jobs: usize,
    pub default_policy: Option<String>,
    #[serde(default)]
    pub allowed_policies: Vec<String>,
    #[serde(default)]
    pub request_overrides: RequestOverrideConfig,
}

#[derive(Debug, Serialize)]
pub(super) struct AccessOverview {
    apps: Vec<AppAccessView>,
    intents: Vec<String>,
    restart_required: bool,
}

#[derive(Debug, Serialize)]
struct AppAccessView {
    app_id: String,
    credential_source: &'static str,
    credential_state: &'static str,
    credential_message: Option<String>,
    fingerprint: Option<String>,
    environment_variable: Option<String>,
    resource_admin: bool,
    observer_access: ObserverAccess,
    protected: bool,
    allowed_intents: Option<Vec<String>>,
    allowed_builtin_tools: BTreeSet<BuiltinTool>,
    allowed_provider_access_classes: BTreeSet<ProviderAccessClass>,
    allowed_cloud_input_modalities: BTreeSet<Modality>,
    max_pending_jobs: usize,
    default_policy: Option<String>,
    allowed_policies: Vec<String>,
    request_overrides: RequestOverrideConfig,
    pending_restart: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct OneTimeCredential {
    pub app_id: String,
    pub token: String,
    pub fingerprint: String,
    pub displayed_once: bool,
    pub requires_restart: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct AccessMutation {
    pub app_id: String,
    pub requires_restart: bool,
    pub credential_removed: Option<bool>,
    pub cleanup_warning: Option<String>,
}

impl AccessManager {
    pub(super) fn new(config_file: RuntimeConfigFile) -> Self {
        Self { config_file }
    }

    pub(super) async fn list(
        &self,
        pending_apps: &BTreeSet<String>,
    ) -> anyhow::Result<AccessOverview> {
        let config = self.config_file.load()?;
        let store = credential_store(&config);
        let intents = config.intents.keys().cloned().collect();
        let apps = config
            .apps
            .into_iter()
            .map(|(app_id, app)| app_view(&store, app_id, app, pending_apps))
            .collect();
        Ok(AccessOverview {
            apps,
            intents,
            restart_required: !pending_apps.is_empty(),
        })
    }

    pub(super) async fn create(
        &self,
        mut input: AppAccessInput,
    ) -> anyhow::Result<OneTimeCredential> {
        if input.allowed_intents.is_none() {
            input.allowed_intents = Some(Vec::new());
        }
        if input.allowed_provider_access_classes.is_none() {
            input.allowed_provider_access_classes =
                Some(BTreeSet::from([ProviderAccessClass::Standard]));
        }
        if input.allowed_builtin_tools.is_none() {
            input.allowed_builtin_tools = Some(BTreeSet::new());
        }
        if input.allowed_cloud_input_modalities.is_none() {
            input.allowed_cloud_input_modalities = Some(BTreeSet::new());
        }
        validate_console_input(&input)?;
        let source = self.config_file.read_source().await?;
        let mut document = source
            .parse::<DocumentMut>()
            .context("parse runtime configuration for App creation")?;
        let mut config = toml::from_str::<RuntimeConfig>(&source)
            .context("parse runtime configuration for App creation")?;
        if config.apps.contains_key(&input.app_id) {
            bail!("App `{}` already exists", input.app_id);
        }
        config.apps.insert(
            input.app_id.clone(),
            app_config(
                &input,
                AppCredentialConfig::Managed,
                ObserverAccess::None,
                None,
            ),
        );
        config.validate().map_err(anyhow::Error::from)?;
        insert_app(&mut document, &input)?;
        let updated_source = document.to_string();
        let store = credential_store(&config);
        RuntimeConfigFile::validate_source(&updated_source)
            .context("validate App configuration before credential creation")?;
        let credential = store.provision(&input.app_id)?;
        if let Err(error) = self.config_file.write_validated(&updated_source).await {
            let cleanup = store.remove(&input.app_id);
            if let Err(cleanup_error) = cleanup {
                return Err(
                    error.context(format!("credential rollback also failed: {cleanup_error}"))
                );
            }
            return Err(error);
        }
        Ok(one_time_credential(input.app_id, credential))
    }

    pub(super) async fn update(&self, mut input: AppAccessInput) -> anyhow::Result<AccessMutation> {
        validate_console_input(&input)?;
        ensure_mutable(&input.app_id)?;
        let source = self.config_file.read_source().await?;
        let mut document = source
            .parse::<DocumentMut>()
            .context("parse runtime configuration for App update")?;
        let mut config = toml::from_str::<RuntimeConfig>(&source)
            .context("parse runtime configuration for App update")?;
        let existing = config
            .apps
            .get(&input.app_id)
            .cloned()
            .with_context(|| format!("App `{}` does not exist", input.app_id))?;
        ensure_not_resource_admin(&input.app_id, &existing)?;
        if input.allowed_intents.is_none() {
            input.allowed_intents = existing.allowed_intents.clone();
        }
        if input.allowed_provider_access_classes.is_none() {
            input.allowed_provider_access_classes =
                Some(existing.allowed_provider_access_classes.clone());
        }
        if input.allowed_builtin_tools.is_none() {
            input.allowed_builtin_tools = Some(existing.allowed_builtin_tools.clone());
        }
        if input.allowed_cloud_input_modalities.is_none() {
            input.allowed_cloud_input_modalities =
                Some(existing.allowed_cloud_input_modalities.clone());
        }
        config.apps.insert(
            input.app_id.clone(),
            app_config(
                &input,
                existing.credential,
                existing.observer_access,
                existing.routing,
            ),
        );
        config.validate().map_err(anyhow::Error::from)?;
        update_app(&mut document, &input)?;
        self.config_file
            .write_validated(&document.to_string())
            .await?;
        Ok(AccessMutation {
            app_id: input.app_id,
            requires_restart: true,
            credential_removed: None,
            cleanup_warning: None,
        })
    }

    pub(super) async fn rotate(&self, app_id: &str) -> anyhow::Result<OneTimeCredential> {
        ensure_mutable(app_id)?;
        let config = self.config_file.load()?;
        let app = config
            .apps
            .get(app_id)
            .with_context(|| format!("App `{app_id}` does not exist"))?;
        ensure_not_resource_admin(app_id, app)?;
        if !matches!(app.credential, AppCredentialConfig::Managed) {
            bail!("App `{app_id}` uses an externally managed environment credential");
        }
        let store = credential_store(&config);
        let credential = match store.inspect(app_id)? {
            Some(_) => store.rotate(app_id)?,
            None => store.provision(app_id)?,
        };
        Ok(one_time_credential(app_id.to_owned(), credential))
    }

    pub(super) async fn revoke(&self, app_id: &str) -> anyhow::Result<AccessMutation> {
        ensure_mutable(app_id)?;
        let source = self.config_file.read_source().await?;
        let mut document = source
            .parse::<DocumentMut>()
            .context("parse runtime configuration for App revocation")?;
        let mut config = toml::from_str::<RuntimeConfig>(&source)
            .context("parse runtime configuration for App revocation")?;
        let app = config
            .apps
            .get(app_id)
            .cloned()
            .with_context(|| format!("App `{app_id}` does not exist"))?;
        ensure_not_resource_admin(app_id, &app)?;
        config
            .apps
            .remove(app_id)
            .with_context(|| format!("App `{app_id}` does not exist"))?;
        remove_app(&mut document, app_id)?;
        config.validate().map_err(anyhow::Error::from)?;
        self.config_file
            .write_validated(&document.to_string())
            .await?;

        let (credential_removed, cleanup_warning) = match app.credential {
            AppCredentialConfig::Managed => match credential_store(&config).remove(app_id) {
                Ok(removed) => (Some(removed), None),
                Err(error) => (None, Some(error.to_string())),
            },
            AppCredentialConfig::Environment { .. } => (None, None),
        };
        Ok(AccessMutation {
            app_id: app_id.to_owned(),
            requires_restart: true,
            credential_removed,
            cleanup_warning,
        })
    }
}

fn app_view(
    store: &ManagedCredentialStore,
    app_id: String,
    app: AppConfig,
    pending_apps: &BTreeSet<String>,
) -> AppAccessView {
    let (credential_source, credential_state, credential_message, fingerprint, variable) =
        match &app.credential {
            AppCredentialConfig::Managed => match store.inspect(&app_id) {
                Ok(Some(summary)) => ("managed", "ready", None, Some(summary.fingerprint), None),
                Ok(None) => (
                    "managed",
                    "missing",
                    Some("credential file will be created when inferd starts".into()),
                    None,
                    None,
                ),
                Err(error) => ("managed", "invalid", Some(error.to_string()), None, None),
            },
            AppCredentialConfig::Environment { variable } => (
                "environment",
                "external",
                None,
                None,
                Some(variable.clone()),
            ),
        };
    AppAccessView {
        protected: app_id == PROTECTED_OPERATOR || app.resource_admin,
        pending_restart: pending_apps.contains("*") || pending_apps.contains(&app_id),
        app_id,
        credential_source,
        credential_state,
        credential_message,
        fingerprint,
        environment_variable: variable,
        resource_admin: app.resource_admin,
        observer_access: app.observer_access,
        allowed_intents: app.allowed_intents,
        allowed_builtin_tools: app.allowed_builtin_tools,
        allowed_provider_access_classes: app.allowed_provider_access_classes,
        allowed_cloud_input_modalities: app.allowed_cloud_input_modalities,
        max_pending_jobs: app.max_pending_jobs,
        default_policy: app.default_policy,
        allowed_policies: app.allowed_policies,
        request_overrides: app.request_overrides,
    }
}

fn validate_console_input(input: &AppAccessInput) -> anyhow::Result<()> {
    if input.max_pending_jobs == 0 || input.max_pending_jobs > 100_000 {
        bail!("max_pending_jobs must be between 1 and 100000");
    }
    if let Some(default) = &input.default_policy
        && !input.allowed_policies.contains(default)
    {
        bail!("default_policy must also appear in allowed_policies");
    }
    Ok(())
}

fn ensure_mutable(app_id: &str) -> anyhow::Result<()> {
    if app_id == PROTECTED_OPERATOR {
        bail!("the protected local operator cannot be changed from Apps & Access");
    }
    Ok(())
}

fn ensure_not_resource_admin(app_id: &str, app: &AppConfig) -> anyhow::Result<()> {
    if app.resource_admin {
        bail!("resource-admin App `{app_id}` is protected from Apps & Access mutations");
    }
    Ok(())
}

fn app_config(
    input: &AppAccessInput,
    credential: AppCredentialConfig,
    observer_access: ObserverAccess,
    routing: Option<infer_core::AppRoutingConfig>,
) -> AppConfig {
    AppConfig {
        credential,
        observer_access,
        resource_admin: false,
        allow_all_intents: false,
        allowed_intents: input.allowed_intents.clone(),
        // This Console surface does not edit named routing; create denies it
        // and update preserves the existing independently managed contract.
        routing,
        allowed_builtin_tools: input.allowed_builtin_tools.clone().unwrap_or_default(),
        allowed_speech_voice_aliases: None,
        allow_all_speech_voice_aliases: false,
        allowed_provider_access_classes: input
            .allowed_provider_access_classes
            .clone()
            .unwrap_or_else(|| BTreeSet::from([ProviderAccessClass::Standard])),
        allowed_cloud_input_modalities: input
            .allowed_cloud_input_modalities
            .clone()
            .unwrap_or_default(),
        max_pending_jobs: input.max_pending_jobs,
        default_policy: input.default_policy.clone(),
        allowed_policies: input.allowed_policies.clone(),
        request_overrides: input.request_overrides.clone(),
    }
}

fn credential_store(config: &RuntimeConfig) -> ManagedCredentialStore {
    ManagedCredentialStore::new(&config.auth.managed_credentials_directory)
}

fn one_time_credential(
    app_id: String,
    credential: ProvisionedManagedCredential,
) -> OneTimeCredential {
    OneTimeCredential {
        app_id,
        token: credential.expose_token().to_owned(),
        fingerprint: credential.fingerprint,
        displayed_once: true,
        requires_restart: true,
    }
}

fn insert_app(document: &mut DocumentMut, input: &AppAccessInput) -> anyhow::Result<()> {
    let apps = ensure_apps_table(document)?;
    if apps.contains_key(&input.app_id) {
        bail!("App `{}` already exists", input.app_id);
    }
    let mut app = Table::new();
    let mut credential = InlineTable::new();
    credential.insert("source", Value::from("managed"));
    app["credential"] = Item::Value(Value::InlineTable(credential));
    app["resource_admin"] = value(false);
    write_app_policy(&mut app, input);
    apps.insert(&input.app_id, Item::Table(app));
    Ok(())
}

fn update_app(document: &mut DocumentMut, input: &AppAccessInput) -> anyhow::Result<()> {
    let apps = ensure_apps_table(document)?;
    let app = apps
        .get_mut(&input.app_id)
        .and_then(Item::as_table_mut)
        .with_context(|| format!("App `{}` is not a mutable TOML table", input.app_id))?;
    write_app_policy(app, input);
    Ok(())
}

fn remove_app(document: &mut DocumentMut, app_id: &str) -> anyhow::Result<()> {
    let apps = ensure_apps_table(document)?;
    apps.remove(app_id)
        .with_context(|| format!("App `{app_id}` is not present in the TOML document"))?;
    Ok(())
}

fn ensure_apps_table(document: &mut DocumentMut) -> anyhow::Result<&mut Table> {
    if document.get("apps").is_none() {
        document["apps"] = Item::Table(Table::new());
    }
    document["apps"]
        .as_table_mut()
        .context("`apps` must be a TOML table")
}

fn write_app_policy(table: &mut Table, input: &AppAccessInput) {
    match &input.allowed_intents {
        Some(intents) => table["allowed_intents"] = value(string_array(intents)),
        None => {
            table.remove("allowed_intents");
        }
    }
    if let Some(tools) = &input.allowed_builtin_tools {
        table["allowed_builtin_tools"] =
            value(enum_array(&tools.iter().copied().collect::<Vec<_>>()));
    }
    if let Some(classes) = &input.allowed_provider_access_classes {
        table["allowed_provider_access_classes"] =
            value(enum_array(&classes.iter().copied().collect::<Vec<_>>()));
    }
    if let Some(modalities) = &input.allowed_cloud_input_modalities {
        table["allowed_cloud_input_modalities"] =
            value(enum_array(&modalities.iter().copied().collect::<Vec<_>>()));
    }
    table["max_pending_jobs"] = value(input.max_pending_jobs as i64);
    match &input.default_policy {
        Some(policy) => table["default_policy"] = value(policy),
        None => {
            table.remove("default_policy");
        }
    }
    table["allowed_policies"] = value(string_array(&input.allowed_policies));

    let mut overrides = Table::new();
    overrides["priority"] = value(enum_array(&input.request_overrides.priority));
    overrides["placement"] = value(enum_array(&input.request_overrides.placement));
    overrides["prefer"] = value(enum_array(&input.request_overrides.prefer));
    overrides["offline_required"] = value(input.request_overrides.offline_required);
    overrides["capability_floor"] = value(enum_array(&input.request_overrides.capability_floor));
    overrides["latency"] = value(enum_array(&input.request_overrides.latency));
    overrides["fallback"] = value(enum_array(&input.request_overrides.fallback));
    if let Some(range) = &input.request_overrides.max_cost_usd {
        let mut inline = InlineTable::new();
        inline.insert("min", Value::from(range.min));
        inline.insert("max", Value::from(range.max));
        overrides["max_cost_usd"] = Item::Value(Value::InlineTable(inline));
    }
    table["request_overrides"] = Item::Table(overrides);
}

fn string_array(values: &[String]) -> Array {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    array
}

fn enum_array<T: Serialize>(values: &[T]) -> Array {
    let mut array = Array::new();
    for value in values {
        let encoded = serde_json::to_value(value).expect("runtime enums serialize as strings");
        let string = encoded
            .as_str()
            .expect("runtime enums serialize as strings");
        array.push(string);
    }
    array
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> (tempfile::TempDir, AccessManager) {
        let temp = tempfile::tempdir().unwrap();
        let credential_directory = temp.path().join("credentials");
        let source = include_str!("../../../../config/infer.example.toml").replace(
            "managed_credentials_directory = \".infer-runtime/credentials\"",
            &format!(
                "managed_credentials_directory = {:?}",
                credential_directory.display().to_string()
            ),
        );
        let config_path = temp.path().join("infer.toml");
        std::fs::write(&config_path, source).unwrap();
        let config_file = RuntimeConfigFile::new(config_path);
        (temp, AccessManager::new(config_file))
    }

    fn input(app_id: &str) -> AppAccessInput {
        AppAccessInput {
            app_id: app_id.into(),
            allowed_intents: Some(vec!["text.summarize".into()]),
            allowed_builtin_tools: Some(BTreeSet::new()),
            allowed_provider_access_classes: Some(BTreeSet::from([ProviderAccessClass::Standard])),
            allowed_cloud_input_modalities: Some(BTreeSet::from([Modality::Text])),
            max_pending_jobs: 16,
            default_policy: Some("local-first".into()),
            allowed_policies: vec!["local-first".into()],
            request_overrides: RequestOverrideConfig::default(),
        }
    }

    #[tokio::test]
    async fn create_list_rotate_update_and_revoke_are_one_way() {
        let (_temp, manager) = test_manager();
        let created = manager.create(input("sample-consumer")).await.unwrap();
        assert_eq!(created.token.len(), 64);
        assert!(created.displayed_once);
        assert!(
            manager
                .config_file
                .read_source()
                .await
                .unwrap()
                .contains("# Ordinary Consumers receive their own least-privilege identity")
        );

        let overview = manager.list(&BTreeSet::new()).await.unwrap();
        let consumer = overview
            .apps
            .iter()
            .find(|app| app.app_id == "sample-consumer")
            .unwrap();
        assert_eq!(
            consumer.fingerprint.as_deref(),
            Some(created.fingerprint.as_str())
        );
        assert_eq!(consumer.credential_state, "ready");
        assert_eq!(
            consumer.allowed_intents.as_ref().unwrap(),
            &vec!["text.summarize".to_owned()]
        );
        assert!(
            !serde_json::to_string(&overview)
                .unwrap()
                .contains(&created.token)
        );

        let rotated = manager.rotate("sample-consumer").await.unwrap();
        assert_ne!(rotated.token, created.token);

        let mut updated = input("sample-consumer");
        updated.max_pending_jobs = 24;
        manager.update(updated).await.unwrap();
        let overview = manager.list(&BTreeSet::new()).await.unwrap();
        assert_eq!(
            overview
                .apps
                .iter()
                .find(|app| app.app_id == "sample-consumer")
                .unwrap()
                .max_pending_jobs,
            24
        );

        manager.revoke("sample-consumer").await.unwrap();
        let overview = manager.list(&BTreeSet::new()).await.unwrap();
        assert!(
            !overview
                .apps
                .iter()
                .any(|app| app.app_id == "sample-consumer")
        );
    }

    #[tokio::test]
    async fn protected_operator_cannot_be_mutated() {
        let (_temp, manager) = test_manager();
        assert!(manager.update(input(PROTECTED_OPERATOR)).await.is_err());
        assert!(manager.rotate(PROTECTED_OPERATOR).await.is_err());
        assert!(manager.revoke(PROTECTED_OPERATOR).await.is_err());
    }

    #[tokio::test]
    async fn unknown_intent_acl_is_rejected_without_publishing_config() {
        let (_temp, manager) = test_manager();
        let before = manager.config_file.read_source().await.unwrap();
        let mut candidate = input("bad-intent-consumer");
        candidate.allowed_intents = Some(vec!["missing.intent".into()]);
        assert!(manager.create(candidate).await.is_err());
        assert_eq!(manager.config_file.read_source().await.unwrap(), before);
    }

    #[tokio::test]
    async fn omitted_console_acl_never_widens_inference_access() {
        let (_temp, manager) = test_manager();
        let mut created_input = input("deny-by-default");
        created_input.allowed_intents = None;
        manager.create(created_input).await.unwrap();
        let created = manager.list(&BTreeSet::new()).await.unwrap();
        assert!(
            created
                .apps
                .iter()
                .find(|app| app.app_id == "deny-by-default")
                .unwrap()
                .allowed_intents
                .as_ref()
                .is_some_and(Vec::is_empty)
        );

        let mut update = input("example-local-consumer");
        update.allowed_intents = None;
        manager.update(update).await.unwrap();
        let updated = manager.list(&BTreeSet::new()).await.unwrap();
        assert_eq!(
            updated
                .apps
                .iter()
                .find(|app| app.app_id == "example-local-consumer")
                .unwrap()
                .allowed_intents
                .as_ref()
                .unwrap(),
            &vec![
                "audio.transcribe".to_owned(),
                "audio.align".to_owned(),
                "text.summarize".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn omitted_hosted_tool_acl_preserves_existing_explicit_grant() {
        let (_temp, manager) = test_manager();
        let mut created = input("web-search-consumer");
        created.allowed_builtin_tools = Some(BTreeSet::from([BuiltinTool::WebSearch]));
        manager.create(created).await.unwrap();

        let mut update = input("web-search-consumer");
        update.allowed_builtin_tools = None;
        update.max_pending_jobs = 20;
        manager.update(update).await.unwrap();

        let overview = manager.list(&BTreeSet::new()).await.unwrap();
        let app = overview
            .apps
            .iter()
            .find(|app| app.app_id == "web-search-consumer")
            .unwrap();
        assert_eq!(
            app.allowed_builtin_tools,
            BTreeSet::from([BuiltinTool::WebSearch])
        );
    }

    #[tokio::test]
    async fn managed_consumer_is_visible_without_exposing_a_token() {
        let (_temp, manager) = test_manager();
        let overview = manager.list(&BTreeSet::new()).await.unwrap();
        let consumer = overview
            .apps
            .iter()
            .find(|app| app.app_id == "example-local-consumer")
            .expect("checked-in example consumer registration");
        assert_eq!(consumer.credential_source, "managed");
        assert_eq!(consumer.credential_state, "missing");
        assert!(consumer.environment_variable.is_none());
        assert!(consumer.fingerprint.is_none());
    }

    #[tokio::test]
    async fn observer_app_updates_preserve_read_only_identity_and_allow_token_rotation() {
        let (_temp, manager) = test_manager();
        let mut update = input("infra-sentinel");
        update.allowed_intents = Some(Vec::new());
        update.max_pending_jobs = 1;
        update.default_policy = None;
        update.allowed_policies.clear();
        manager.update(update).await.unwrap();

        let config = manager.config_file.load().unwrap();
        let observer = &config.apps["infra-sentinel"];
        assert_eq!(observer.observer_access, ObserverAccess::Summary);
        assert_eq!(observer.allowed_intents, Some(Vec::new()));
        assert!(!observer.resource_admin);

        let credential = manager.rotate("infra-sentinel").await.unwrap();
        assert_eq!(credential.token.len(), 64);
        assert!(manager.revoke("infra-sentinel").await.is_err());
    }
}
