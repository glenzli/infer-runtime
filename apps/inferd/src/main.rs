//! Composition root for the local infer-runtime daemon.

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::Parser;
use infer_control::Runtime;
use infer_core::RuntimeConfig;
use infer_observer::{
    DiscoveryOffer, DiscoveryRuntime, DiscoveryService, RegistrationLease, RegistrationSpec,
    SnapshotProvider, UnixJsonObserverServer, consumer_http_offer,
};
use tracing::info;
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
    let runtime = Runtime::from_config(config)
        .await
        .map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind inferd to {bind}"))?;
    let consumer_address = listener
        .local_addr()
        .context("read bound inferd Consumer address")?;
    let (observer_socket, registration) = if observer_config.enabled {
        let discovery_runtime = DiscoveryRuntime::from_environment()?;
        let identity = runtime.observer_identity();
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
        let registration = RegistrationLease::start(RegistrationSpec {
            runtime: discovery_runtime,
            service: DiscoveryService::from(&identity.service),
            offers: vec![
                DiscoveryOffer::infer_status_unix(socket_endpoint),
                consumer_http_offer(consumer_address, infer_api::contract::CONTRACT_VERSION)
                    .context("build Infer Runtime Consumer discovery offer")?,
            ],
        })
        .await
        .context("start Infra Discovery registration lease")?;
        (Some(observer_socket), Some(registration))
    } else {
        (None, None)
    };
    info!(address = %bind, "inferd started");
    let serve_result = axum::serve(listener, infer_api::router(runtime))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(registration) = registration {
        registration
            .shutdown()
            .await
            .context("stop Infra Discovery registration lease")?;
    }
    if let Some(observer_socket) = observer_socket {
        observer_socket
            .shutdown()
            .await
            .context("stop infer-runtime.status Unix socket")?;
    }
    serve_result?;
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
