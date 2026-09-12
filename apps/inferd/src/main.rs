//! Composition root for the local infer-runtime daemon.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use clap::Parser;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
use infer_observer::{
    DiscoveryOffer, DiscoveryRuntime, DiscoveryService, RegistrationPublication, RegistrationSpec,
    SnapshotProvider, UnixJsonObserverServer, consumer_http_offer,
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "inferd", about = "Local AI inference control-plane daemon")]
struct Args {
    #[arg(short, long, default_value = "config/infer.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("inferd=info".parse()?)
                .add_directive("infer_api=info".parse()?),
        )
        .init();
    let args = Args::parse();
    let config = RuntimeConfig::load(&args.config)
        .map_err(anyhow::Error::msg)
        .context("load inferd configuration")?;
    let bind = config.server.bind.clone();
    let observer_config = config.observer.clone();
    let raw_config = config.raw_foundation.clone();
    let runtime = Runtime::from_config(config)
        .await
        .map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind inferd to {bind}"))?;
    let consumer_address = listener
        .local_addr()
        .context("read bound inferd Consumer address")?;
    #[cfg(unix)]
    let (raw_control, raw_socket) = if raw_config.enabled {
        let graph = required_path(raw_config.graph.as_deref(), "raw_foundation.graph")?;
        verify_sha256(
            &graph,
            raw_config.graph_sha256.as_deref().unwrap_or_default(),
        )?;
        let runtime_library = required_path(
            raw_config.runtime_library.as_deref(),
            "raw_foundation.runtime_library",
        )?;
        let socket_directory = required_path(
            raw_config.socket_directory.as_deref(),
            "raw_foundation.socket_directory",
        )?;
        prepare_owner_only_directory(&socket_directory)?;
        let generation = runtime.observer_identity().service.generation;
        let socket_path = socket_directory.join(format!("rawnind-{generation}.sock"));
        let registry = Arc::new(infer_artifact_lease::ArtifactLeaseRegistry::new(
            unsafe { libc::geteuid() },
            generation,
        )?);
        let control = infer_control::RawFoundationControl::new(
            Arc::clone(&runtime),
            Arc::clone(&registry),
            socket_path.clone(),
            &graph,
            &runtime_library,
        )?;
        let service = infer_artifact_lease::ArtifactLeaseUnixService::start(socket_path, registry)?;
        (Some(control), Some(service))
    } else {
        (None, None)
    };
    #[cfg(not(unix))]
    let raw_control: Option<Arc<infer_control::RawFoundationControl>> = {
        if raw_config.enabled {
            anyhow::bail!(
                "raw_foundation requires Unix SCM_RIGHTS artifact leases on this release"
            );
        }
        None
    };
    // Consumer discovery is part of the data-plane lifecycle, not the optional
    // observer lifecycle. Disabling Sentinel/status observation must never make
    // an otherwise healthy Consumer API undiscoverable.
    let discovery_runtime = DiscoveryRuntime::from_environment()?;
    let identity = runtime.observer_identity();
    let (observer_endpoint, observer_socket) = if observer_config.enabled {
        let socket_runtime = Arc::clone(&runtime);
        let snapshot_provider: SnapshotProvider = Arc::new(move || {
            let runtime = Arc::clone(&socket_runtime);
            Box::pin(async move { runtime.observer_snapshot().await.map_err(|_| ()) })
        });
        let (socket_endpoint, observer_socket) = UnixJsonObserverServer::start_unique(
            &discovery_runtime,
            &identity.service.generation,
            snapshot_provider,
        )
        .await
        .context("start infer-runtime.status Unix socket")?;
        (Some(socket_endpoint), Some(observer_socket))
    } else {
        (None, None)
    };
    let offers = discovery_offers(consumer_address, observer_endpoint)?;
    let registration = RegistrationPublication::publish(RegistrationSpec {
        runtime: discovery_runtime,
        service: DiscoveryService::from(&identity.service),
        offers,
    })
    .context("publish Infra Discovery registration")?;
    info!(address = %bind, "inferd started");
    let api = raw_control.map_or_else(
        || infer_api::router(Arc::clone(&runtime)),
        |raw| infer_api::router_with_raw(Arc::clone(&runtime), raw),
    );
    let serve = axum::serve(listener, api).with_graceful_shutdown(shutdown_signal());
    let serve = std::future::IntoFuture::into_future(serve);
    tokio::pin!(serve);
    let mut discovery_check = tokio::time::interval(std::time::Duration::from_secs(30));
    discovery_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut discovery_error = None;
    let serve_result = loop {
        tokio::select! {
            result = &mut serve => break result,
            _ = discovery_check.tick() => {
                match registration.ensure_published() {
                    Ok(repaired) => {
                        if repaired { warn!("restored missing Infra Discovery registration"); }
                        if discovery_error.take().is_some() { info!("Infra Discovery publication is healthy again"); }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if discovery_error.as_ref() != Some(&message) {
                            warn!(error = %message, "Infra Discovery publication unavailable; serving endpoints remain active");
                        }
                        discovery_error = Some(message);
                    }
                }
            }
        }
    };
    registration.shutdown();
    if let Some(observer_socket) = observer_socket {
        observer_socket
            .shutdown()
            .await
            .context("stop infer-runtime.status Unix socket")?;
    }
    #[cfg(unix)]
    if let Some(raw_socket) = raw_socket {
        raw_socket.shutdown();
    }
    serve_result?;
    Ok(())
}

fn discovery_offers(
    consumer_address: std::net::SocketAddr,
    observer_endpoint: Option<String>,
) -> anyhow::Result<Vec<DiscoveryOffer>> {
    let consumer_offer =
        consumer_http_offer(consumer_address, &[infer_api::contract::CORE_VERSION])
            .context("build Infer Runtime Consumer discovery offer")?;
    if !infer_api::contract::published_versions_match_contract(&consumer_offer.protocol_versions) {
        anyhow::bail!("Consumer Discovery offer diverges from the HTTP contract set");
    }
    let mut offers = Vec::with_capacity(1 + usize::from(observer_endpoint.is_some()));
    if let Some(endpoint) = observer_endpoint {
        offers.push(DiscoveryOffer::infer_status_unix(endpoint));
    }
    offers.push(consumer_offer);
    Ok(offers)
}

fn required_path(value: Option<&str>, field: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value.context(format!("{field} is required"))?);
    if !path.is_absolute() {
        anyhow::bail!("{field} must be absolute");
    }
    Ok(path)
}

fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        anyhow::bail!("raw_foundation graph digest mismatch");
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_owner_only_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        anyhow::bail!("raw_foundation socket directory is not owner-only");
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_discovery_does_not_depend_on_observer() {
        let offers = discovery_offers("127.0.0.1:8787".parse().unwrap(), None).unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].protocol, infer_observer::CONSUMER_PROTOCOL);
        assert_eq!(
            offers[0].protocol_versions,
            vec![infer_api::contract::CORE_VERSION.to_owned()]
        );
    }

    #[test]
    fn observer_adds_status_offer_without_replacing_consumer_discovery() {
        let offers = discovery_offers(
            "127.0.0.1:8787".parse().unwrap(),
            Some("sockets/infer-runtime--local.sock".into()),
        )
        .unwrap();
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].protocol, "infer-runtime.status");
        assert_eq!(offers[1].protocol, infer_observer::CONSUMER_PROTOCOL);
    }
}
