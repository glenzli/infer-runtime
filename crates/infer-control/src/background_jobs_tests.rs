use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use infer_auth::AppCredentials;
use infer_core::{
    AttemptOutcome, AttemptTrigger, DurablePayloadKind, DurablePayloadRef, JobState,
    ResponsesRequest, RuntimeConfig,
};
use infer_payload::EncryptedPayloadSpool;
use infer_provider::{DynProvider, Provider, ProviderByteStream, ProviderError};
use infer_store::{ConfigSnapshot, Store};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::*;

const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const WRONG_KEY: &str = "f00102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

struct CompletingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CompletingProvider {
    fn id(&self) -> &str {
        "local"
    }

    async fn execute(&self, _request: ResponsesRequest) -> Result<Value, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "closed-loop result"}]
            }]
        }))
    }

    async fn execute_stream(
        &self,
        _request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        unreachable!("durable background execution is non-streaming")
    }
}

fn config(database: &Path) -> RuntimeConfig {
    let mut config: RuntimeConfig = toml::from_str(
        r#"
        [server]
        bind = "127.0.0.1:8787"
        [defaults]
        policy = "balanced"
        [providers.local]
        kind = "responses"
        base_url = "http://127.0.0.1:11434/v1"
        placement = "local"
        [providers.local.capability_profile]
        version = 1
        protocol = "responses"
        capabilities = ["responses"]
        [profiles.balanced]
        order = ["cost"]
        [intents."text.summarize"]
        input_modalities = ["text"]
        output_modalities = ["text"]
        default_quality_floor = "basic"
        [model_profiles.qwen]
        family = "qwen"
        [model_profiles.qwen.ratings."text.summarize"]
        grade = "basic"
        status = "benchmarked"
        eval_profile = "summary-v1"
        score = 0.8
        [model_builds.qwen_local]
        profile = "qwen"
        model_id = "qwen"
        input_modalities = ["text"]
        output_modalities = ["text"]
        [deployments.qwen_local]
        provider = "local"
        build = "qwen_local"
        estimated_cost_usd = 0.0
        [apps.test-app]
        credential = { source = "environment", variable = "INFER_TEST_TOKEN" }
        max_pending_jobs = 16
        allowed_policies = ["balanced"]
        [apps.test-app.request_overrides]
        priority = ["background"]
        placement = ["local_only"]
    "#,
    )
    .unwrap();
    config.persistence.path = database.to_string_lossy().into_owned();
    config
}

fn request() -> ResponsesRequest {
    let mut request = ResponsesRequest {
        model: "text.summarize".into(),
        input: Value::String("durable input".into()),
        instructions: None,
        stream: false,
        background: true,
        metadata: BTreeMap::new(),
        tools: Vec::new(),
        reasoning: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        truncation: None,
        store: None,
        previous_response_id: None,
        conversation: None,
    };
    request
        .metadata
        .insert("infer.priority".into(), "background".into());
    request
        .metadata
        .insert("infer.placement".into(), "local_only".into());
    request
}

fn provider(calls: Arc<AtomicUsize>) -> BTreeMap<String, DynProvider> {
    BTreeMap::from([(
        "local".into(),
        Arc::new(CompletingProvider { calls }) as DynProvider,
    )])
}

async fn admit(
    runtime: &Runtime,
    spool: &EncryptedPayloadSpool,
) -> (PreparedRun, ResponsesRequest, DurablePayloadRef) {
    let request = runtime.prepare_responses_request(request()).unwrap();
    let request_ref = spool
        .put(
            "test-app",
            DurablePayloadKind::ResponsesRequest,
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
    let prepared = runtime
        .prepare_job(
            "test-app",
            JobPreparation {
                logical_model: &request.model,
                constraints: request.constraints().unwrap(),
                execution_requirements: request.execution_requirements(),
                reasoning_effort: request.reasoning_effort(),
                estimated_tokens: estimate_response_tokens(&request),
                id_prefix: "resp",
                expected_data_plane: "responses",
                durable_payload: Some(&request_ref),
            },
        )
        .await
        .unwrap();
    (prepared, request, request_ref)
}

fn blob_ids(directory: &Path) -> BTreeSet<String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn pending_validation_with_the_wrong_key_preserves_ciphertext() {
    let directory = tempfile::tempdir().unwrap();
    let writer = EncryptedPayloadSpool::open(directory.path(), KEY, 1024).unwrap();
    let reference = writer
        .put(
            "test-app",
            DurablePayloadKind::ResponsesRequest,
            b"recover me",
        )
        .unwrap();
    drop(writer);
    let reader = Arc::new(EncryptedPayloadSpool::open(directory.path(), WRONG_KEY, 1024).unwrap());
    let jobs = BackgroundJobs::for_test(reader, 60_000);

    assert!(matches!(
        jobs.validate_pending(vec![("test-app".into(), reference)])
            .await,
        Err(RuntimeError::Payload(
            infer_payload::PayloadError::Authentication
        ))
    ));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn failed_result_metadata_publication_keeps_input_for_restart_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runtime.sqlite3");
    let payloads = directory.path().join("payloads");
    let config = config(&database);
    let store = Arc::new(
        Store::open(
            &database,
            ConfigSnapshot::from_serializable(&config).unwrap(),
        )
        .unwrap(),
    );
    let spool = Arc::new(EncryptedPayloadSpool::open(&payloads, KEY, 4096).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::with_components(
        config.clone(),
        provider(calls.clone()),
        Some(store.clone()),
        BackgroundJobs::for_test(spool.clone(), 60_000),
        AppCredentials::empty(),
    );
    let (prepared, request, request_ref) = admit(&runtime, &spool).await;
    let job_id = prepared.job_id.clone();

    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_background_result_publication
             BEFORE UPDATE OF result_ref_json ON durable_background_jobs
             WHEN NEW.result_ref_json IS NOT NULL
             BEGIN SELECT RAISE(FAIL, 'injected result publication failure'); END;",
        )
        .unwrap();
    runtime
        .clone()
        .run_background(prepared, request, request_ref.clone())
        .await;

    let interrupted = store.load_job(&job_id).unwrap().unwrap();
    assert_eq!(interrupted.state, JobState::Running);
    assert_eq!(interrupted.attempts.len(), 1);
    assert_eq!(interrupted.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(store.pending_background_jobs().unwrap(), 1);
    assert_eq!(
        store.pending_background_payloads().unwrap(),
        vec![("test-app".into(), request_ref.clone())]
    );
    assert_eq!(blob_ids(&payloads).len(), 1);

    connection
        .execute_batch("DROP TRIGGER fail_background_result_publication;")
        .unwrap();
    drop(connection);
    drop(runtime);

    let mut recovery = store.recover_background_jobs(Some(2)).unwrap();
    assert_eq!(recovery.runnable.len(), 1);
    let record = recovery.runnable.pop().unwrap();
    assert_eq!(record.snapshot.id, job_id);
    let recovered_runtime = Runtime::with_components(
        config,
        provider(calls.clone()),
        Some(store.clone()),
        BackgroundJobs::for_test(spool, 60_000),
        AppCredentials::empty(),
    );
    let (prepared, request) = recovered_runtime.restore_prepared(&record).await.unwrap();
    recovered_runtime
        .clone()
        .run_background(prepared, request, record.request)
        .await;

    let completed = store.load_job(&job_id).unwrap().unwrap();
    assert_eq!(completed.state, JobState::Succeeded);
    assert_eq!(completed.attempts.len(), 2);
    assert_eq!(completed.attempts[1].trigger, AttemptTrigger::Recovery);
    assert_eq!(completed.attempts[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.pending_background_jobs().unwrap(), 0);
    assert_eq!(blob_ids(&payloads).len(), 1);
}

#[tokio::test]
async fn failed_request_retirement_never_deletes_a_still_referenced_blob() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runtime.sqlite3");
    let payloads = directory.path().join("payloads");
    let config = config(&database);
    let store = Arc::new(
        Store::open(
            &database,
            ConfigSnapshot::from_serializable(&config).unwrap(),
        )
        .unwrap(),
    );
    let spool = Arc::new(EncryptedPayloadSpool::open(&payloads, KEY, 4096).unwrap());
    let runtime = Runtime::with_components(
        config,
        BTreeMap::new(),
        Some(store.clone()),
        BackgroundJobs::for_test(spool.clone(), 60_000),
        AppCredentials::empty(),
    );
    let (prepared, _request, request_ref) = admit(&runtime, &spool).await;
    let job_id = prepared.job_id;
    runtime
        .mark(&job_id, JobState::Failed, Some("terminal test".into()))
        .await
        .unwrap();

    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_background_request_retirement
             BEFORE UPDATE OF request_ref_json ON durable_background_jobs
             WHEN OLD.request_ref_json IS NOT NULL AND NEW.request_ref_json IS NULL
             BEGIN SELECT RAISE(FAIL, 'injected request retirement failure'); END;",
        )
        .unwrap();
    runtime
        .retire_background_input(&job_id, request_ref.clone())
        .await;
    assert_eq!(
        store.background_payload_refs().unwrap(),
        vec![request_ref.clone()]
    );
    assert_eq!(blob_ids(&payloads).len(), 1);

    connection
        .execute_batch("DROP TRIGGER fail_background_request_retirement;")
        .unwrap();
    drop(connection);
    runtime.retire_background_input(&job_id, request_ref).await;
    assert!(store.background_payload_refs().unwrap().is_empty());
    assert!(blob_ids(&payloads).is_empty());
}
