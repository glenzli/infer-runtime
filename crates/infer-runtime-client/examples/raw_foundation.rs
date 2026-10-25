//! Real typed RAW Foundation smoke. Files and descriptors stay private; only
//! receipts/provenance and bounded cancellation evidence are printed.
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use infer_runtime_client::{Client, DiscoveryResolver, RawFoundationLeaseRequest};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        return Err("usage: raw_foundation <credential> <discovery-root> <descriptor.json> <samples> <new-output.partial> [execute|cancel|cancel-running] [WSL-distribution installed-bridge.py]".into());
    }
    let mode = args.get(5).map(String::as_str).unwrap_or("execute");
    if !matches!(mode, "execute" | "cancel" | "cancel-running") {
        return Err("invalid smoke mode".into());
    }
    let builder = Client::with_discovery(DiscoveryResolver::with_runtime_root(PathBuf::from(
        &args[1],
    )))
    .credential_file(&args[0]);
    #[cfg(windows)]
    let builder = match (args.get(6), args.get(7)) {
        (Some(distribution), Some(script)) => builder.raw_foundation_wsl_bridge(
            infer_runtime_client::RawFoundationWslBridge::new(distribution, script)?,
        ),
        (None, None) => builder,
        _ => {
            return Err(
                "WSL diagnostic override needs both distribution and installed bridge path".into(),
            );
        }
    };
    let client = Arc::new(builder.build()?);
    let request: RawFoundationLeaseRequest = serde_json::from_reader(File::open(&args[2])?)?;
    let input = File::open(&args[3])?;
    let output_path = PathBuf::from(&args[4]);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let output = options.open(&output_path)?;
    let result = async {
        let started = Instant::now();
        let grant = client.create_raw_foundation_lease(&request).await?;
        let lease = client.register_raw_foundation_handles(&grant, &input, &output)?;
        if mode == "cancel" {
            let cancellation = client.cancel_raw_foundation(&grant.job_id).await?;
            assert_eq!(output.metadata()?.len(), 0);
            println!("{}", json!({"status":cancellation.status,"output_bytes":0,"elapsed_ms":started.elapsed().as_millis()}));
        } else if mode == "cancel-running" {
            let worker = Arc::clone(&client);
            let job = grant.job_id.clone();
            let in_flight = tokio::spawn(async move { worker.execute_raw_foundation(&job, &lease).await });
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let cancellation_started = Instant::now();
            let cancellation = client.cancel_raw_foundation(&grant.job_id).await?;
            let result = in_flight.await?;
            assert!(result.is_err(), "a cancelled request must not publish a result");
            assert_eq!(output.metadata()?.len(), 0);
            println!("{}", json!({"status":cancellation.status,"output_bytes":0,"cancel_ms":cancellation_started.elapsed().as_millis()}));
        } else {
            let receipt = client.execute_raw_foundation(&grant.job_id, &lease).await?;
            assert_eq!(receipt.source_revision, request.source_revision);
            assert_eq!(output.metadata()?.len(), receipt.artifact.artifact_file_bytes);
            println!("{}", json!({"receipt":receipt,"elapsed_ms":started.elapsed().as_millis()}));
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }.await;
    drop(client);
    drop(output);
    if result.is_err() || mode != "execute" {
        std::fs::remove_file(output_path)?;
    }
    result
}
