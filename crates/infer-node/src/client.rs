use crate::{tls, wire::*};
use infer_core::{NodePeerConfig, ResponsesRequest};
use rustls::pki_types::ServerName;
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::Instant};
use tokio_rustls::TlsConnector;

#[derive(Clone)]
pub struct NodeClient {
    config: NodePeerConfig,
    tls: Arc<rustls::ClientConfig>,
}

impl NodeClient {
    /// Leave room for the Dispatch envelope and attempt identity in the wire frame.
    pub fn request_fits_wire(request: &ResponsesRequest) -> bool {
        serde_json::to_vec(request).is_ok_and(|bytes| bytes.len() <= MAX_FRAME - 1024)
    }

    pub fn new(config: NodePeerConfig) -> Result<Self, NodeError> {
        if config.address.parse::<std::net::SocketAddr>().is_err()
            || !infer_core::valid_node_digest(&config.certificate_sha256)
        {
            return Err(NodeError::Protocol);
        }
        Ok(Self {
            tls: tls::client(&config.tls)?,
            config,
        })
    }

    async fn rpc(&self, generation: Option<&str>, command: Command) -> Result<Response, NodeError> {
        let operation = async {
            let socket = TcpStream::connect(&self.config.address)
                .await
                .map_err(|_| NodeError::Unavailable)?;
            let name = ServerName::try_from(self.config.server_name.clone())
                .map_err(|_| NodeError::Protocol)?;
            let mut socket = TlsConnector::from(Arc::clone(&self.tls))
                .connect(name, socket)
                .await
                .map_err(|_| NodeError::Forbidden)?;
            if tls::fingerprint(socket.get_ref().1.peer_certificates())?
                != self.config.certificate_sha256
                || socket.get_ref().1.alpn_protocol() != Some(PROTOCOL.as_bytes())
            {
                return Err(NodeError::Forbidden);
            }
            write_frame(
                &mut socket,
                &Request {
                    protocol: PROTOCOL.into(),
                    generation: generation.map(str::to_owned),
                    command,
                },
            )
            .await?;
            let response: Response = read_frame(&mut socket).await?;
            if response.protocol != PROTOCOL
                || response.node_id != self.config.node_id
                || generation.is_some_and(|expected| response.generation != expected)
            {
                return Err(NodeError::Protocol);
            }
            if let Reply::Error(error) = response.reply {
                return Err(error);
            }
            Ok(response)
        };
        tokio::time::timeout(Duration::from_secs(2), operation)
            .await
            .map_err(|_| NodeError::Unavailable)?
    }

    pub async fn catalog(&self) -> Result<(String, Catalog), NodeError> {
        let response = self.rpc(None, Command::Catalog).await?;
        let Reply::Catalog(catalog) = response.reply else {
            return Err(NodeError::Protocol);
        };
        if catalog.lease_ms != LEASE_MS || catalog.offers.len() > 64 {
            return Err(NodeError::Protocol);
        }
        Ok((response.generation, catalog))
    }

    pub async fn execute(
        &self,
        attempt: NodeAttempt,
        deployment: String,
        mut request: ResponsesRequest,
        ttl: Duration,
    ) -> Result<serde_json::Value, NodeError> {
        request.validate().map_err(|_| NodeError::Protocol)?;
        if request.stream
            || request.background
            || !request.tools.is_empty()
            || request.execution_requirements().input_modalities
                != std::collections::BTreeSet::from([infer_core::Modality::Text])
        {
            return Err(NodeError::Protocol);
        }
        if !Self::request_fits_wire(&request) {
            return Err(NodeError::Protocol);
        }
        let deadline = Instant::now() + ttl.min(Duration::from_millis(MAX_TASK_MS));
        let NodeAttempt {
            key,
            app_id,
            intent,
        } = attempt;
        let (generation, catalog) = self.catalog().await?;
        let contract = self
            .config
            .imports
            .get(&deployment)
            .ok_or(NodeError::Forbidden)?;
        let offer = catalog
            .offers
            .iter()
            .find(|offer| offer.deployment_id == deployment && &offer.contract_digest == contract)
            .ok_or(NodeError::Protocol)?;
        if offer.intent != intent {
            return Err(NodeError::Protocol);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(NodeError::Cancelled);
        }
        let ttl_ms = remaining.as_millis().clamp(1, u128::from(MAX_TASK_MS)) as u64;
        // Physical model on A names the paired deployment; the wire carries its declared Intent.
        request.model = offer.intent.clone();
        request.metadata.clear();
        let guard = CancelOnDrop {
            client: self.clone(),
            generation: generation.clone(),
            key: key.clone(),
            armed: true,
        };
        self.rpc(
            Some(&generation),
            Command::Reserve {
                key: key.clone(),
                app_id,
                deployment,
                contract_digest: contract.clone(),
                ttl_ms,
            },
        )
        .await?;
        // After dispatch is attempted, transport failure is ambiguous. Never dispatch again.
        let dispatched = self
            .rpc(
                Some(&generation),
                Command::Dispatch {
                    key: key.clone(),
                    request: Box::new(request),
                },
            )
            .await;
        let mut disconnected_since = None;
        let mut response = dispatched;
        loop {
            match response {
                Ok(Response {
                    reply: Reply::Task(TaskState::Succeeded { result }),
                    ..
                }) => {
                    let mut guard = guard;
                    guard.armed = false;
                    return Ok(result);
                }
                Ok(Response {
                    reply: Reply::Task(TaskState::Failed { error }),
                    ..
                }) => return Err(error),
                Ok(Response {
                    reply: Reply::Task(TaskState::Running),
                    ..
                }) => {
                    disconnected_since = None;
                }
                Ok(_) => return Err(NodeError::OutcomeUnknown),
                Err(_) => {
                    let since = disconnected_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(LEASE_MS) {
                        return Err(NodeError::OutcomeUnknown);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(NodeError::OutcomeUnknown);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            response = self
                .rpc(Some(&generation), Command::Status { key: key.clone() })
                .await;
        }
    }
}

struct CancelOnDrop {
    client: NodeClient,
    generation: String,
    key: TaskKey,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let client = self.client.clone();
        let generation = self.generation.clone();
        let key = self.key.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = client.rpc(Some(&generation), Command::Cancel { key }).await;
            });
        }
    }
}
