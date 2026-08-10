use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    DiscoveryError, DiscoveryRuntime, OBSERVER_ERROR_SCHEMA, ObserverSnapshot,
    STATUS_PROTOCOL_VERSION, unique_status_socket_endpoint,
};

pub const STATUS_REQUEST_SCHEMA: &str = "infer-runtime.status.request";
pub const SNAPSHOT_REQUEST_LINE: &[u8] =
    br#"{"schema":"infer-runtime.status.request","schema_version":"20260810.1","operation":"snapshot"}
"#;

pub const MAX_STATUS_REQUEST_BYTES: usize = 512;
pub const MAX_STATUS_RESPONSE_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const UNIQUE_SOCKET_BIND_ATTEMPTS: usize = 8;

pub type SnapshotFuture =
    Pin<Box<dyn Future<Output = Result<ObserverSnapshot, ()>> + Send + 'static>>;
pub type SnapshotProvider = Arc<dyn Fn() -> SnapshotFuture + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum UnixJsonServerError {
    #[error("Unix JSON observer transport is not supported on this platform")]
    Unsupported,
    #[error("observer socket parent `{0}` must not be a symbolic link")]
    SymlinkDirectory(PathBuf),
    #[error("observer socket parent `{0}` is not a directory")]
    NotDirectory(PathBuf),
    #[error("observer socket path `{0}` is already occupied")]
    ActiveSocket(PathBuf),
    #[error("observer socket path `{0}` is not a socket")]
    UnsafeSocket(PathBuf),
    #[error("observer socket path `{path}` is owned by uid {actual}; expected uid {expected}")]
    WrongOwner {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
    #[error("observer socket I/O failed for `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not allocate a unique observer socket after {attempts} address collisions")]
    EndpointCollisions { attempts: usize },
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
}

pub struct UnixJsonObserverServer {
    path: PathBuf,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
    #[cfg(unix)]
    socket_identity: (u64, u64),
}

impl UnixJsonObserverServer {
    #[cfg(unix)]
    pub async fn start_unique(
        runtime: &DiscoveryRuntime,
        generation: &str,
        snapshot_provider: SnapshotProvider,
    ) -> Result<(String, Self), UnixJsonServerError> {
        Self::start_unique_with(runtime, snapshot_provider, || {
            unique_status_socket_endpoint(generation)
        })
        .await
    }

    #[cfg(not(unix))]
    pub async fn start_unique(
        _runtime: &DiscoveryRuntime,
        _generation: &str,
        _snapshot_provider: SnapshotProvider,
    ) -> Result<(String, Self), UnixJsonServerError> {
        Err(UnixJsonServerError::Unsupported)
    }

    #[cfg(unix)]
    async fn start_unique_with<F>(
        runtime: &DiscoveryRuntime,
        snapshot_provider: SnapshotProvider,
        mut next_endpoint: F,
    ) -> Result<(String, Self), UnixJsonServerError>
    where
        F: FnMut() -> Result<String, DiscoveryError>,
    {
        for _ in 0..UNIQUE_SOCKET_BIND_ATTEMPTS {
            let endpoint = next_endpoint()?;
            let path = runtime.resolve_unix_socket(&endpoint)?;
            match Self::start(path, Arc::clone(&snapshot_provider)).await {
                Ok(server) => return Ok((endpoint, server)),
                Err(UnixJsonServerError::ActiveSocket(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(UnixJsonServerError::EndpointCollisions {
            attempts: UNIQUE_SOCKET_BIND_ATTEMPTS,
        })
    }

    #[cfg(unix)]
    pub async fn start(
        path: PathBuf,
        snapshot_provider: SnapshotProvider,
    ) -> Result<Self, UnixJsonServerError> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        use tokio::net::UnixListener;

        let parent = path
            .parent()
            .ok_or_else(|| UnixJsonServerError::NotDirectory(path.clone()))?;
        prepare_parent(parent)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                    return Err(UnixJsonServerError::UnsafeSocket(path));
                }
                return Err(UnixJsonServerError::ActiveSocket(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(UnixJsonServerError::Io {
                    path: path.clone(),
                    source,
                });
            }
        }

        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(source) if source.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(UnixJsonServerError::ActiveSocket(path));
            }
            Err(source) => {
                return Err(UnixJsonServerError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| UnixJsonServerError::Io {
                path: path.clone(),
                source,
            },
        )?;
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| UnixJsonServerError::Io {
                path: path.clone(),
                source,
            })?;
        // SAFETY: geteuid has no preconditions and does not dereference memory.
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid {
            return Err(UnixJsonServerError::WrongOwner {
                path,
                actual: metadata.uid(),
                expected: expected_uid,
            });
        }
        let socket_identity = (metadata.dev(), metadata.ino());
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_path = path.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancellation.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            serve_connection(stream, &snapshot_provider).await;
                        }
                        Err(error) => {
                            tracing::warn!(path = %task_path.display(), %error, "observer Unix socket accept failed");
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            path,
            cancellation,
            task: Some(task),
            socket_identity,
        })
    }

    #[cfg(not(unix))]
    pub async fn start(
        _path: PathBuf,
        _snapshot_provider: SnapshotProvider,
    ) -> Result<Self, UnixJsonServerError> {
        Err(UnixJsonServerError::Unsupported)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn shutdown(mut self) -> Result<(), UnixJsonServerError> {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.remove_if_owned()
    }

    fn remove_if_owned(&self) -> Result<(), UnixJsonServerError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};

            let metadata = match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(source) => {
                    return Err(UnixJsonServerError::Io {
                        path: self.path.clone(),
                        source,
                    });
                }
            };
            if metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino()) == self.socket_identity
            {
                std::fs::remove_file(&self.path).map_err(|source| UnixJsonServerError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            }
        }
        Ok(())
    }
}

impl Drop for UnixJsonObserverServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let _ = self.remove_if_owned();
    }
}

#[cfg(unix)]
fn prepare_parent(parent: &Path) -> Result<(), UnixJsonServerError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::create_dir_all(parent).map_err(|source| UnixJsonServerError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|source| UnixJsonServerError::Io {
        path: parent.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(UnixJsonServerError::SymlinkDirectory(parent.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(UnixJsonServerError::NotDirectory(parent.to_owned()));
    }
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid {
        return Err(UnixJsonServerError::WrongOwner {
            path: parent.to_owned(),
            actual: metadata.uid(),
            expected: expected_uid,
        });
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        UnixJsonServerError::Io {
            path: parent.to_owned(),
            source,
        }
    })
}

#[cfg(unix)]
async fn serve_connection(mut stream: tokio::net::UnixStream, provider: &SnapshotProvider) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if !peer_is_current_user(&stream) {
        let _ = stream.shutdown().await;
        return;
    }

    let valid_request = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut request = Vec::with_capacity(SNAPSHOT_REQUEST_LINE.len());
        let mut buffer = [0_u8; 128];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<bool, std::io::Error>(false);
            }
            request.extend_from_slice(&buffer[..read]);
            if request.len() > MAX_STATUS_REQUEST_BYTES {
                return Ok(false);
            }
            if let Some(newline) = request.iter().position(|byte| *byte == b'\n') {
                if newline + 1 != request.len() {
                    return Ok(false);
                }
                return Ok(parse_request(&request[..newline]));
            }
        }
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or(false);

    let response = if valid_request {
        match provider().await {
            Ok(snapshot) => bounded_response(&snapshot),
            Err(()) => serde_json::to_vec(&ErrorEnvelope::new("snapshot_unavailable")),
        }
    } else {
        serde_json::to_vec(&ErrorEnvelope::new("invalid_request"))
    };
    if let Ok(mut response) = response {
        response.push(b'\n');
        debug_assert!(response.len() <= MAX_STATUS_RESPONSE_BYTES);
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    }
}

#[cfg(unix)]
fn peer_is_current_user(stream: &tokio::net::UnixStream) -> bool {
    // SAFETY: geteuid has no preconditions.
    let expected_uid = unsafe { libc::geteuid() };
    stream
        .peer_cred()
        .is_ok_and(|credentials| peer_uid_is_allowed(credentials.uid(), expected_uid))
}

fn peer_uid_is_allowed(actual: u32, expected: u32) -> bool {
    actual == expected
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    schema: String,
    schema_version: String,
    operation: String,
}

fn parse_request(bytes: &[u8]) -> bool {
    serde_json::from_slice::<StatusRequest>(bytes).is_ok_and(|request| {
        request.schema == STATUS_REQUEST_SCHEMA
            && request.schema_version == STATUS_PROTOCOL_VERSION
            && request.operation == "snapshot"
    })
}

fn bounded_response(snapshot: &ObserverSnapshot) -> Result<Vec<u8>, serde_json::Error> {
    let response = serde_json::to_vec(snapshot)?;
    if response.len() < MAX_STATUS_RESPONSE_BYTES {
        Ok(response)
    } else {
        serde_json::to_vec(&ErrorEnvelope::new("snapshot_unavailable"))
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema: &'static str,
    schema_version: &'static str,
    error: ErrorBody<'a>,
}

impl<'a> ErrorEnvelope<'a> {
    fn new(code: &'a str) -> Self {
        Self {
            schema: OBSERVER_ERROR_SCHEMA,
            schema_version: STATUS_PROTOCOL_VERSION,
            error: ErrorBody { code },
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::{fs::PermissionsExt, net::UnixListener as StdUnixListener};

    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use super::*;
    use crate::{
        ObserverRedaction, ObserverStatus, ObserverStatusState, SNAPSHOT_SCHEMA, SnapshotService,
    };

    fn snapshot() -> ObserverSnapshot {
        ObserverSnapshot {
            schema: SNAPSHOT_SCHEMA.into(),
            schema_version: STATUS_PROTOCOL_VERSION.into(),
            service: SnapshotService {
                kind: "infer-runtime".into(),
                instance_id: "local".into(),
                generation: "gen_test".into(),
            },
            sequence: 1,
            captured_at: "2026-08-10T00:00:00Z".into(),
            status: ObserverStatus {
                state: ObserverStatusState::Healthy,
                reason_codes: Vec::new(),
            },
            headline_metrics: Vec::new(),
            metrics: Vec::new(),
            issues: Vec::new(),
            extensions: BTreeMap::new(),
            links: None,
            redaction: ObserverRedaction::default(),
        }
    }

    async fn request(path: &Path, request: &[u8]) -> serde_json::Value {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        serde_json::from_slice(&response).unwrap()
    }

    #[tokio::test]
    async fn unique_start_retries_without_deleting_an_unknown_colliding_socket() {
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let collision_endpoint = "sockets/collision.sock".to_owned();
        let selected_endpoint = "sockets/fresh.sock".to_owned();
        let collision_path = runtime.resolve_unix_socket(&collision_endpoint).unwrap();
        let collision_listener = StdUnixListener::bind(&collision_path).unwrap();
        let mut endpoints = VecDeque::from([collision_endpoint.clone(), selected_endpoint.clone()]);

        let (selected, server) = UnixJsonObserverServer::start_unique_with(
            &runtime,
            Arc::new(|| Box::pin(async { Ok(snapshot()) })),
            || Ok(endpoints.pop_front().expect("two bind attempts")),
        )
        .await
        .unwrap();

        assert_eq!(selected, selected_endpoint);
        assert!(collision_path.exists(), "collision must remain untouched");
        assert_eq!(
            std::fs::metadata(server.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let selected_path = server.path().to_owned();
        server.shutdown().await.unwrap();
        assert!(!selected_path.exists());
        assert!(collision_path.exists());
        drop(collision_listener);
        std::fs::remove_file(collision_path).unwrap();
    }

    #[tokio::test]
    async fn semantic_snapshot_request_returns_one_frame_eof_and_socket_is_owner_only() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("private").join("observer.sock");
        let server = UnixJsonObserverServer::start(
            path.clone(),
            Arc::new(|| Box::pin(async { Ok(snapshot()) })),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let response = request(&path, SNAPSHOT_REQUEST_LINE).await;
        assert_eq!(response["schema"], SNAPSHOT_SCHEMA);
        assert_eq!(response["schema_version"], STATUS_PROTOCOL_VERSION);

        let reordered = br#" { "operation" : "snapshot", "schema_version" : "20260810.1", "schema" : "infer-runtime.status.request" }
"#;
        let response = request(&path, reordered).await;
        assert_eq!(response["schema"], SNAPSHOT_SCHEMA);
        server.shutdown().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn wrong_operation_unknown_fields_and_trailing_frames_are_rejected() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("observer.sock");
        let server = UnixJsonObserverServer::start(
            path.clone(),
            Arc::new(|| Box::pin(async { panic!("invalid requests must not collect a snapshot") })),
        )
        .await
        .unwrap();
        let wrong_operation = br#"{"schema":"infer-runtime.status.request","schema_version":"20260810.1","operation":"mutate"}
"#;
        let response = request(&path, wrong_operation).await;
        assert_eq!(response["schema"], OBSERVER_ERROR_SCHEMA);
        assert_eq!(response["error"]["code"], "invalid_request");
        let unknown = br#"{"schema":"infer-runtime.status.request","schema_version":"20260810.1","operation":"snapshot","extra":true}
"#;
        let response = request(&path, unknown).await;
        assert_eq!(response["error"]["code"], "invalid_request");
        let trailing = [SNAPSHOT_REQUEST_LINE, SNAPSHOT_REQUEST_LINE].concat();
        let response = request(&path, &trailing).await;
        assert_eq!(response["error"]["code"], "invalid_request");
        let oversized = [vec![b' '; MAX_STATUS_REQUEST_BYTES], vec![b'\n']].concat();
        let response = request(&path, &oversized).await;
        assert_eq!(response["error"]["code"], "invalid_request");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_failure_is_redacted_and_stale_socket_is_cleaned_up() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("observer.sock");
        let server =
            UnixJsonObserverServer::start(path.clone(), Arc::new(|| Box::pin(async { Err(()) })))
                .await
                .unwrap();
        let response = request(&path, SNAPSHOT_REQUEST_LINE).await;
        assert_eq!(response["error"]["code"], "snapshot_unavailable");
        server.shutdown().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn response_is_bounded_and_terminated_by_exactly_one_lf_then_eof() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("observer.sock");
        let server = UnixJsonObserverServer::start(
            path.clone(),
            Arc::new(|| {
                Box::pin(async {
                    let mut value = snapshot();
                    value.extensions.insert(
                        "oversized".into(),
                        serde_json::Value::String("x".repeat(MAX_STATUS_RESPONSE_BYTES)),
                    );
                    Ok(value)
                })
            }),
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(SNAPSHOT_REQUEST_LINE).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.len() <= MAX_STATUS_RESPONSE_BYTES);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["error"]["code"], "snapshot_unavailable");
        server.shutdown().await.unwrap();
    }

    #[test]
    fn peer_uid_gate_rejects_a_different_effective_user() {
        assert!(peer_uid_is_allowed(501, 501));
        assert!(!peer_uid_is_allowed(502, 501));
    }
}
