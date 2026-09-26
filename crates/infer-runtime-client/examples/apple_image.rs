//! Native image consumer: uses Discovery unless a diagnostic endpoint is explicit.
use infer_runtime_client::{Client, DiscoveryResolver, apple_image::*};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    let mut resolver = DiscoveryResolver::local();
    if args.peek().map(String::as_str) == Some("--endpoint") {
        args.next();
        resolver = resolver.with_explicit_endpoint(args.next().ok_or("missing endpoint")?)?;
    }
    let mut metadata = BTreeMap::new();
    if args.peek().map(String::as_str) == Some("--node-deployment") {
        args.next();
        metadata.insert("infer.placement".into(), "private".into());
        metadata.insert(
            "infer.deployment_ids".into(),
            args.next().ok_or("missing deployment")?,
        );
    }
    let credential = args.next().ok_or("missing credential file")?;
    let image = PathBuf::from(args.next().ok_or("missing input image")?);
    let operation = args.next().ok_or("missing operation")?;
    let options = match operation.as_str() {
        "ocr" => AppleImageOperation::Ocr {},
        "aesthetics" => AppleImageOperation::Aesthetics {},
        "segment" => AppleImageOperation::Segment {
            points: vec![AppleImagePoint {
                x: 0.3,
                y: 0.5,
                include: true,
            }],
            box_prompt: None,
        },
        "raw_render" => AppleImageOperation::RawRender {
            exposure: 0.0,
            noise_reduction: 1.0,
        },
        _ => return Err("unsupported operation".into()),
    };
    let output = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let parameters = AppleImageParameters {
        model: format!("apple.{operation}"),
        source_revision: format!("sha256:{:x}", Sha256::digest(std::fs::read(&image)?)),
        options,
        metadata,
    };
    let client = Client::with_discovery(resolver)
        .credential_file(credential)
        .build()?;
    client.contract().await?;
    let response = client.apple_image(&image, &parameters).await?;
    let job = client.job(&response.id).await?;
    if job.state != "succeeded"
        || job.intent != parameters.model
        || job.provider != response.provider
        || job.deployment != response.deployment
        || job.model_build != response.model_build
        || job.attempts.iter().any(|a| a.trigger == "fallback")
    {
        return Err("native artifact and job provenance mismatch".into());
    }
    let mut receipt = serde_json::to_value(&response)?;
    match &response.result {
        AppleImageResult::Segment { raster, .. } | AppleImageResult::RawRender { raster, .. } => {
            if let Some(output) = output {
                std::fs::write(output, raster.png_bytes()?)?;
            }
            receipt["result"]["raster"]
                .as_object_mut()
                .unwrap()
                .remove("data_base64");
        }
        _ => {}
    }
    receipt["job_state"] = json!(job.state);
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}
