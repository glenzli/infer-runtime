use crate::{contract_digest, tls, wire::*};
use async_trait::async_trait;
use infer_core::{NodeServerConfig, ResponsesRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Semaphore},
    time::Instant,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerGrant {
    pub node_id: String,
    /// Origin App -> destination App. Identity comes from the paired certificate.
    pub apps: BTreeMap<String, String>,
    pub exports: BTreeSet<String>,
}
/// Keys are SHA-256 fingerprints of paired client leaf certificates.
pub type PeerGrants = BTreeMap<String, PeerGrant>;

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute_image(
        &self,
        _app: &str,
        _export: &str,
        _request: AppleNodeRequest,
        _cancellation: CancellationToken,
    ) -> Result<Value, NodeError> {
        Err(NodeError::Protocol)
    }
    async fn execute(
        &self,
        app: &str,
        export: &str,
        request: ResponsesRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, NodeError>;
}

enum DispatchPayload {
    Text(Box<ResponsesRequest>),
    Image(Box<AppleNodeRequest>),
}

struct Task {
    protocol: String,
    app: String,
    local_app: String,
    deployment: String,
    contract: String,
    state: TaskState,
    payload_digest: Option<String>,
    cancellation: CancellationToken,
    deadline: Instant,
    lease: Instant,
    retain_until: Instant,
}
impl Task {
    fn active(&self) -> bool {
        matches!(self.state, TaskState::Reserved | TaskState::Running)
    }
    fn cancel(&mut self) {
        if self.active() {
            self.cancellation.cancel();
            self.state = TaskState::Failed {
                error: NodeError::Cancelled,
            };
        }
    }
}

struct State {
    config: NodeServerConfig,
    generation: String,
    offers: BTreeMap<String, Offer>,
    tasks: Mutex<BTreeMap<(String, TaskKey), Task>>,
    executor: Arc<dyn NodeExecutor>,
}

pub struct NodeServer {
    shutdown: CancellationToken,
    pub address: std::net::SocketAddr,
}
impl Drop for NodeServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl NodeServer {
    pub async fn start(
        config: NodeServerConfig,
        offers: Vec<Offer>,
        executor: Arc<dyn NodeExecutor>,
    ) -> Result<Self, NodeError> {
        if !(1..=64).contains(&config.max_active) || offers.len() > 64 {
            return Err(NodeError::Protocol);
        }
        let acceptor = TlsAcceptor::from(tls::server(&config.tls)?);
        read_grants(&config)?;
        let listener = TcpListener::bind(&config.bind)
            .await
            .map_err(|_| NodeError::Unavailable)?;
        let address = listener.local_addr().map_err(|_| NodeError::Unavailable)?;
        let state = Arc::new(State {
            config,
            generation: uuid::Uuid::new_v4().to_string(),
            offers: offers
                .into_iter()
                .map(|offer| (offer.deployment_id.clone(), offer))
                .collect(),
            tasks: Mutex::new(BTreeMap::new()),
            executor,
        });
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let connections = Arc::new(Semaphore::new(32));
            let mut sweep = tokio::time::interval(Duration::from_millis(500));
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = sweep.tick() => state.sweep().await,
                    connection = listener.accept() => {
                        let Ok((socket, _)) = connection else { break; };
                        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else { continue; };
                        let state = Arc::clone(&state);
                        let acceptor = acceptor.clone();
                        let stop = stop.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let serve = async {
                                let mut socket = acceptor.accept(socket).await.map_err(|_| NodeError::Forbidden)?;
                                let peer = tls::fingerprint(socket.get_ref().1.peer_certificates())?;
                                let protocol = match socket.get_ref().1.alpn_protocol() { Some(p) if p==PROTOCOL.as_bytes()=>PROTOCOL, Some(p) if p==IMAGE_PROTOCOL.as_bytes()=>IMAGE_PROTOCOL, _=>return Err(NodeError::Protocol) };
                                let grants = read_grants(&state.config)?;
                                if !grants.contains_key(&peer) { return Err(NodeError::Forbidden); }
                                let request: Request = read_frame(&mut socket).await?;
                                let reply = if request.protocol != protocol {
                                    Reply::Error(NodeError::Protocol)
                                } else { match state.handle(&peer, request).await {
                                    Ok(reply) => reply, Err(error) => Reply::Error(error),
                                }};
                                write_frame(&mut socket, &Response {
                                    protocol: protocol.into(), node_id: state.config.node_id.clone(), generation: state.generation.clone(), reply,
                                }).await
                            };
                            tokio::select! {
                                _ = stop.cancelled() => {},
                                _ = tokio::time::timeout(Duration::from_secs(3), serve) => {},
                            }
                        });
                    }
                }
            }
            for task in state.tasks.lock().await.values_mut() {
                task.cancel();
            }
        });
        Ok(Self { shutdown, address })
    }
}

fn read_grants(config: &NodeServerConfig) -> Result<PeerGrants, NodeError> {
    let bytes = tls::owner_file(&config.peers_file)?;
    let grants: PeerGrants = serde_json::from_slice(&bytes).map_err(|_| NodeError::Forbidden)?;
    if grants.len() > 64
        || grants.iter().any(|(digest, grant)| {
            !infer_core::valid_node_digest(digest)
                || grant.node_id.is_empty()
                || grant.apps.len() > 64
                || grant.exports.len() > 64
        })
    {
        return Err(NodeError::Forbidden);
    }
    Ok(grants)
}

impl State {
    async fn sweep(&self) {
        let grants = read_grants(&self.config).unwrap_or_default();
        let now = Instant::now();
        let mut tasks = self.tasks.lock().await;
        for ((peer, _), task) in tasks.iter_mut() {
            let authorized = grants.get(peer).is_some_and(|grant| {
                grant.apps.get(&task.app) == Some(&task.local_app)
                    && grant.exports.contains(&task.deployment)
            });
            if now >= task.deadline || now >= task.lease || !authorized {
                task.cancel();
            }
        }
        tasks.retain(|_, task| task.active() || now < task.retain_until);
    }

    async fn handle(self: &Arc<Self>, peer: &str, request: Request) -> Result<Reply, NodeError> {
        if request.protocol != PROTOCOL && request.protocol != IMAGE_PROTOCOL {
            return Err(NodeError::Protocol);
        }
        let grants = read_grants(&self.config)?;
        let grant = grants.get(peer).ok_or(NodeError::Forbidden)?;
        self.sweep().await;
        if matches!(request.command, Command::Catalog) {
            let tasks = self.tasks.lock().await;
            let active = tasks.values().filter(|task| task.active()).count();
            let available_admissions = self
                .config
                .max_active
                .saturating_sub(active)
                .min(64_usize.saturating_sub(tasks.len()));
            drop(tasks);
            return Ok(Reply::Catalog(Catalog {
                offers: self
                    .offers
                    .values()
                    .filter(|offer| grant.exports.contains(&offer.deployment_id))
                    .cloned()
                    .collect(),
                available_admissions,
                lease_ms: LEASE_MS,
            }));
        }
        if request.generation.as_deref() != Some(&self.generation) {
            return Err(NodeError::Protocol);
        }
        let protocol = request.protocol.clone();
        let is_cancel = matches!(request.command, Command::Cancel { .. });
        match request.command {
            Command::Catalog => unreachable!(),
            Command::Reserve {
                key,
                app_id,
                deployment,
                contract_digest,
                ttl_ms,
            } => {
                if key.job_id.is_empty()
                    || key.job_id.len() > 128
                    || key.attempt == 0
                    || !(1..=MAX_TASK_MS).contains(&ttl_ms)
                {
                    return Err(NodeError::Protocol);
                }
                let app = grant.apps.get(&app_id).ok_or(NodeError::Forbidden)?;
                if !grant.exports.contains(&deployment) {
                    return Err(NodeError::Forbidden);
                }
                let offer = self.offers.get(&deployment).ok_or(NodeError::Protocol)?;
                if offer.contract_digest != contract_digest {
                    return Err(NodeError::Protocol);
                }
                let mut tasks = self.tasks.lock().await;
                let id = (peer.to_string(), key);
                if let Some(task) = tasks.get(&id) {
                    if task.protocol != protocol
                        || task.app != app_id
                        || task.deployment != deployment
                        || task.contract != contract_digest
                    {
                        return Err(NodeError::Protocol);
                    }
                    return Ok(Reply::Task(task.state.clone()));
                }
                if tasks.len() >= 64
                    || tasks.values().filter(|task| task.active()).count() >= self.config.max_active
                {
                    return Err(NodeError::Busy);
                }
                let now = Instant::now();
                tasks.insert(
                    id,
                    Task {
                        protocol: protocol.clone(),
                        app: app_id,
                        local_app: app.clone(),
                        deployment,
                        contract: contract_digest,
                        state: TaskState::Reserved,
                        payload_digest: None,
                        cancellation: CancellationToken::new(),
                        deadline: now + Duration::from_millis(ttl_ms),
                        lease: now + Duration::from_millis(LEASE_MS),
                        // Keep tombstones/results beyond the maximum client reconciliation window.
                        retain_until: now + Duration::from_millis(MAX_TASK_MS + 60_000),
                    },
                );
                Ok(Reply::Task(TaskState::Reserved))
            }
            Command::Dispatch { key, request } => {
                if protocol != PROTOCOL {
                    return Err(NodeError::Protocol);
                }
                request.validate().map_err(|_| NodeError::Protocol)?;
                if request.stream
                    || request.background
                    || !request.tools.is_empty()
                    || request.execution_requirements().input_modalities
                        != BTreeSet::from([infer_core::Modality::Text])
                {
                    return Err(NodeError::Protocol);
                }
                self.dispatch(peer, key, &protocol, DispatchPayload::Text(request))
                    .await
            }
            Command::DispatchImage { key, request } => {
                if protocol != IMAGE_PROTOCOL {
                    return Err(NodeError::Protocol);
                }
                request.validate()?;
                self.dispatch(peer, key, &protocol, DispatchPayload::Image(request))
                    .await
            }
            Command::Status { key } | Command::Cancel { key } => {
                let cancel = is_cancel;
                let mut tasks = self.tasks.lock().await;
                let task = tasks
                    .get_mut(&(peer.to_string(), key))
                    .ok_or(NodeError::Missing)?;
                if task.protocol != protocol {
                    return Err(NodeError::Protocol);
                }
                if cancel {
                    task.cancel();
                } else if task.active() {
                    task.lease = Instant::now() + Duration::from_millis(LEASE_MS);
                }
                Ok(Reply::Task(task.state.clone()))
            }
        }
    }
    async fn dispatch(
        self: &Arc<Self>,
        peer: &str,
        key: TaskKey,
        protocol: &str,
        payload: DispatchPayload,
    ) -> Result<Reply, NodeError> {
        let (intent, digest) = match &payload {
            DispatchPayload::Text(request) => (request.model.clone(), contract_digest(request)?),
            DispatchPayload::Image(request) => (
                request.parameters.model.clone(),
                contract_digest(&("image", request))?,
            ),
        };
        let id = (peer.to_string(), key);
        let mut tasks = self.tasks.lock().await;
        let task = tasks.get_mut(&id).ok_or(NodeError::Missing)?;
        if task.protocol != protocol {
            return Err(NodeError::Protocol);
        }
        if task
            .payload_digest
            .as_ref()
            .is_some_and(|old| old != &digest)
        {
            return Err(NodeError::Protocol);
        }
        if task.state != TaskState::Reserved {
            return Ok(Reply::Task(task.state.clone()));
        }
        let offer = &self.offers[&task.deployment];
        if intent != offer.intent {
            return Err(NodeError::Protocol);
        }
        task.payload_digest = Some(digest);
        task.state = TaskState::Running;
        let app = task.local_app.clone();
        let deployment = task.deployment.clone();
        let cancel = task.cancellation.clone();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let result = match payload {
                DispatchPayload::Text(request) => {
                    state
                        .executor
                        .execute(&app, &deployment, *request, cancel)
                        .await
                }
                DispatchPayload::Image(request) => {
                    state
                        .executor
                        .execute_image(&app, &deployment, *request, cancel)
                        .await
                }
            };
            let mut tasks = state.tasks.lock().await;
            if let Some(task) = tasks.get_mut(&id) {
                if task.state != TaskState::Running {
                    return;
                }
                if Instant::now() >= task.deadline || Instant::now() >= task.lease {
                    task.cancel();
                    return;
                }
                task.state = match result {
                    Ok(result)
                        if serde_json::to_vec(&result)
                            .is_ok_and(|bytes| bytes.len() < MAX_FRAME / 2) =>
                    {
                        TaskState::Succeeded { result }
                    }
                    Ok(_) => TaskState::Failed {
                        error: NodeError::Protocol,
                    },
                    Err(error) => TaskState::Failed { error },
                };
            }
        });
        Ok(Reply::Task(TaskState::Running))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct ControlledExecutor {
        calls: AtomicUsize,
        started: Notify,
        release: Notify,
    }
    #[async_trait]
    impl NodeExecutor for ControlledExecutor {
        async fn execute_image(
            &self,
            _: &str,
            _: &str,
            _: AppleNodeRequest,
            _: CancellationToken,
        ) -> Result<Value, NodeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(serde_json::json!({"image":true}))
        }

        async fn execute(
            &self,
            _: &str,
            _: &str,
            _: ResponsesRequest,
            _: CancellationToken,
        ) -> Result<Value, NodeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            // Deliberately uncooperative: a late executor success must be fenced.
            self.release.notified().await;
            Ok(serde_json::json!({"output": []}))
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<State>,
        Arc<ControlledExecutor>,
        String,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let peer = "a".repeat(64);
        let file = directory.path().join("peers.json");
        let grants = BTreeMap::from([(
            peer.clone(),
            PeerGrant {
                node_id: "a".into(),
                apps: BTreeMap::from([("app".into(), "local-app".into())]),
                exports: BTreeSet::from(["shared".into()]),
            },
        )]);
        std::fs::write(&file, serde_json::to_vec(&grants).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let executor = Arc::new(ControlledExecutor {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        });
        let config = NodeServerConfig {
            node_id: "b".into(),
            bind: "127.0.0.1:0".into(),
            tls: infer_core::NodeTlsConfig {
                certificate: "unused".into(),
                private_key: "unused".into(),
                ca_certificate: "unused".into(),
            },
            peers_file: file.to_string_lossy().into_owned(),
            max_active: 1,
            exports: BTreeMap::new(),
        };
        let offer = Offer {
            deployment_id: "shared".into(),
            intent: "text.summarize".into(),
            contract_digest: "b".repeat(64),
            build_id: "build".into(),
            resource_estimate: Default::default(),
        };
        let state = Arc::new(State {
            config,
            generation: "generation".into(),
            offers: BTreeMap::from([("shared".into(), offer)]),
            tasks: Mutex::new(BTreeMap::new()),
            executor: executor.clone(),
        });
        (directory, state, executor, peer)
    }
    fn key(id: &str) -> TaskKey {
        TaskKey {
            job_id: id.into(),
            attempt: 1,
        }
    }
    fn reserve(id: &str) -> Command {
        Command::Reserve {
            key: key(id),
            app_id: "app".into(),
            deployment: "shared".into(),
            contract_digest: "b".repeat(64),
            ttl_ms: 30_000,
        }
    }
    fn dispatch(id: &str, text: &str) -> Command {
        Command::Dispatch {
            key: key(id),
            request: Box::new(
                serde_json::from_value(serde_json::json!({"model":"text.summarize", "input":text}))
                    .unwrap(),
            ),
        }
    }
    async fn command(state: &Arc<State>, peer: &str, command: Command) -> Result<Reply, NodeError> {
        state
            .handle(
                peer,
                Request {
                    protocol: PROTOCOL.into(),
                    generation: Some(state.generation.clone()),
                    command,
                },
            )
            .await
    }

    async fn image_command(
        state: &Arc<State>,
        peer: &str,
        command: Command,
    ) -> Result<Reply, NodeError> {
        state
            .handle(
                peer,
                Request {
                    protocol: IMAGE_PROTOCOL.into(),
                    generation: Some(state.generation.clone()),
                    command,
                },
            )
            .await
    }
    fn image_dispatch(id: &str, bytes: Vec<u8>) -> Command {
        Command::DispatchImage {
            key: key(id),
            request: Box::new(AppleNodeRequest {
                parameters: infer_core::AppleImageParameters {
                    model: "text.summarize".into(),
                    source_revision: "r1".into(),
                    options: infer_core::AppleImageOperation::Ocr {},
                    metadata: BTreeMap::from([
                        ("infer.placement".into(), "local_only".into()),
                        ("infer.offline_required".into(), "true".into()),
                        ("infer.fallback".into(), "none".into()),
                    ]),
                },
                bytes,
            }),
        }
    }
    #[tokio::test]
    async fn image_protocol_binds_reservation_and_rejects_replay_changes() {
        let (_dir, state, executor, peer) = fixture();
        image_command(&state, &peer, reserve("image"))
            .await
            .unwrap();
        assert!(matches!(
            command(&state, &peer, reserve("image")).await,
            Err(NodeError::Protocol)
        ));
        assert!(matches!(
            command(&state, &peer, image_dispatch("image", vec![1])).await,
            Err(NodeError::Protocol)
        ));
        assert!(matches!(
            image_command(&state, &peer, dispatch("image", "text")).await,
            Err(NodeError::Protocol)
        ));
        image_command(&state, &peer, image_dispatch("image", vec![1]))
            .await
            .unwrap();
        executor.started.notified().await;
        image_command(&state, &peer, image_dispatch("image", vec![1]))
            .await
            .unwrap();
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            image_command(&state, &peer, image_dispatch("image", vec![2])).await,
            Err(NodeError::Protocol)
        ));
        image_command(&state, &peer, Command::Cancel { key: key("image") })
            .await
            .unwrap();
        executor.release.notify_one();
        tokio::task::yield_now().await;
        assert!(matches!(
            image_command(&state, &peer, Command::Status { key: key("image") }).await,
            Ok(Reply::Task(TaskState::Failed {
                error: NodeError::Cancelled
            }))
        ));
    }
    #[tokio::test]
    async fn oversized_image_is_rejected_without_execution() {
        let (_dir, state, executor, peer) = fixture();
        image_command(&state, &peer, reserve("big")).await.unwrap();
        assert!(matches!(
            image_command(
                &state,
                &peer,
                image_dispatch("big", vec![0; MAX_NODE_IMAGE_BYTES + 1])
            )
            .await,
            Err(NodeError::Protocol)
        ));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reservation_and_dispatch_are_idempotent_but_payload_changes_are_rejected() {
        let (_dir, state, executor, peer) = fixture();
        command(&state, &peer, reserve("one")).await.unwrap();
        command(&state, &peer, reserve("one")).await.unwrap();
        assert!(matches!(
            command(&state, &peer, reserve("two")).await,
            Err(NodeError::Busy)
        ));
        command(&state, &peer, dispatch("one", "same"))
            .await
            .unwrap();
        executor.started.notified().await;
        command(&state, &peer, dispatch("one", "same"))
            .await
            .unwrap();
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            command(&state, &peer, dispatch("one", "different")).await,
            Err(NodeError::Protocol)
        ));
        executor.release.notify_one();
    }

    #[tokio::test]
    async fn expired_lease_releases_admission_and_fences_late_success() {
        let (_dir, state, executor, peer) = fixture();
        command(&state, &peer, reserve("one")).await.unwrap();
        command(&state, &peer, dispatch("one", "text"))
            .await
            .unwrap();
        executor.started.notified().await;
        {
            let mut tasks = state.tasks.lock().await;
            tasks.get_mut(&(peer.clone(), key("one"))).unwrap().lease = Instant::now();
        }
        state.sweep().await;
        command(&state, &peer, reserve("two")).await.unwrap();
        executor.release.notify_one();
        tokio::task::yield_now().await;
        assert!(matches!(
            command(&state, &peer, Command::Status { key: key("one") }).await,
            Ok(Reply::Task(TaskState::Failed {
                error: NodeError::Cancelled
            }))
        ));
    }

    #[tokio::test]
    async fn revocation_invalidates_live_leases_and_old_generation_is_rejected() {
        let (_dir, state, _executor, peer) = fixture();
        let old = Request {
            protocol: PROTOCOL.into(),
            generation: Some("old".into()),
            command: reserve("one"),
        };
        assert!(matches!(
            state.handle(&peer, old).await,
            Err(NodeError::Protocol)
        ));
        command(&state, &peer, reserve("one")).await.unwrap();
        std::fs::write(&state.config.peers_file, b"{}").unwrap();
        state.sweep().await;
        assert!(matches!(
            command(&state, &peer, Command::Status { key: key("one") }).await,
            Err(NodeError::Forbidden)
        ));
        let tasks = state.tasks.lock().await;
        assert!(
            tasks[&(peer.clone(), key("one"))]
                .cancellation
                .is_cancelled()
        );
        assert!(!tasks[&(peer, key("one"))].active());
    }

    #[tokio::test]
    async fn retained_record_limit_is_reflected_in_catalog_admission() {
        let (_dir, state, _executor, peer) = fixture();
        for i in 0..64 {
            let id = format!("finished-{i}");
            command(&state, &peer, reserve(&id)).await.unwrap();
            command(&state, &peer, Command::Cancel { key: key(&id) })
                .await
                .unwrap();
        }
        assert!(matches!(
            command(&state, &peer, Command::Catalog).await,
            Ok(Reply::Catalog(Catalog {
                available_admissions: 0,
                ..
            }))
        ));
        assert!(matches!(
            command(&state, &peer, reserve("overflow")).await,
            Err(NodeError::Busy)
        ));
        state
            .tasks
            .lock()
            .await
            .get_mut(&(peer.clone(), key("finished-0")))
            .unwrap()
            .retain_until = Instant::now();
        assert!(matches!(
            command(&state, &peer, Command::Catalog).await,
            Ok(Reply::Catalog(Catalog {
                available_admissions: 1,
                ..
            }))
        ));
        command(&state, &peer, reserve("after-expiry"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn grants_and_contract_identity_are_checked_before_accepting_work() {
        let (_dir, state, executor, peer) = fixture();
        let mut request = reserve("one");
        if let Command::Reserve { app_id, .. } = &mut request {
            *app_id = "not-granted".into();
        }
        assert!(matches!(
            command(&state, &peer, request).await,
            Err(NodeError::Forbidden)
        ));
        let mut request = reserve("one");
        if let Command::Reserve {
            contract_digest, ..
        } = &mut request
        {
            *contract_digest = "c".repeat(64);
        }
        assert!(matches!(
            command(&state, &peer, request).await,
            Err(NodeError::Protocol)
        ));
        assert!(state.tasks.lock().await.is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
}
