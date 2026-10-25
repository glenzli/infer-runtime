//! Native Consumer smoke for a separately managed local Runtime.
//! Usage: shadow_vision <credential-file> <discovery-root> [connect|faces|semantic|describe|all] [image.jpg]
//! Prints contract and execution provenance, never credentials or embedding values.

use std::{collections::BTreeMap, path::PathBuf};

use infer_runtime_client::{Client, DiscoveryResolver, TextEmbeddingRequest, VisionProvenance};
use serde_json::json;
use sha2::{Digest, Sha256};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 2 {
        return Err(
            "usage: shadow_vision <credential-file> <discovery-root> [mode] [image.jpg]".into(),
        );
    }
    let resolver = DiscoveryResolver::with_runtime_root(PathBuf::from(&args[1]));
    let endpoint = resolver.resolve()?;
    let client = Client::with_discovery(resolver)
        .credential_file(PathBuf::from(&args[0]))
        .build()?;
    client
        .contract()
        .await
        .map_err(|error| format!("Core negotiation: {error}"))?;
    client
        .capabilities()
        .await
        .map_err(|error| format!("Capability negotiation: {error}"))?;
    client
        .jobs(&[("limit", "1")])
        .await
        .map_err(|error| format!("Consumer authentication: {error}"))?;
    println!(
        "{}",
        json!({"connection":"ok","endpoint":endpoint.endpoint,
        "generation":endpoint.generation,"authenticated_job_listing":"ok"})
    );
    let mode = args
        .get(2)
        .and_then(|value| value.to_str())
        .unwrap_or("connect");
    if mode == "connect" {
        return Ok(());
    }
    if !matches!(mode, "faces" | "semantic" | "describe" | "all") {
        return Err("unknown smoke mode".into());
    }
    let image = args
        .get(3)
        .map(PathBuf::from)
        .ok_or("image.jpg is required")?;
    let bytes = std::fs::read(&image)?;
    if bytes.len() > 20 * 1024 * 1024 {
        return Err("smoke image exceeds 20 MiB".into());
    }
    let revision = format!("shadow-sdk-smoke:sha256:{:x}", Sha256::digest(&bytes));
    let metadata = BTreeMap::from([
        ("infer.priority".into(), "interactive".into()),
        ("infer.placement".into(), "local_only".into()),
        ("infer.offline_required".into(), "true".into()),
        ("infer.fallback".into(), "none".into()),
        ("infer.capability_floor".into(), "foundational".into()),
    ]);
    if matches!(mode, "faces" | "all") {
        let detected = client
            .detect_faces(&image, "image/jpeg", &revision, &metadata)
            .await?;
        require_cuda(&detected.provenance)?;
        let face = detected
            .detections
            .first()
            .ok_or("fixture has no detected face")?;
        let embedded = client
            .embed_face(&image, "image/jpeg", &revision, face.landmarks, &metadata)
            .await?;
        require_cuda(&embedded.provenance)?;
        if embedded.embedding.dimensions != 128 || !embedded.eligibility.eligible {
            return Err("SFace output is not an eligible 128-dimensional embedding".into());
        }
        println!(
            "{}",
            json!({"faces":detected.detections.len(),"sface_dimensions":embedded.embedding.dimensions,
            "detection_provenance":detected.provenance,"embedding_provenance":embedded.provenance})
        );
    }
    if matches!(mode, "semantic" | "all") {
        let visual = client
            .embed_image(&image, "image/jpeg", &revision, &metadata)
            .await?;
        let text = client
            .embed_text(&TextEmbeddingRequest {
                model: "semantic.embed_text".into(),
                text: "a portrait of a person".into(),
                query_revision: "shadow-sdk-smoke:query:v1".into(),
                language: Some("en".into()),
                metadata: metadata.clone(),
            })
            .await?;
        require_cuda(&visual.provenance)?;
        require_cuda(&text.provenance)?;
        if visual.embedding.dimensions != 768
            || text.embedding.dimensions != 768
            || visual.embedding.space != text.embedding.space
        {
            return Err("SigLIP image and text do not share the expected embedding space".into());
        }
        println!(
            "{}",
            json!({"siglip_dimensions":visual.embedding.dimensions,
            "image_provenance":visual.provenance,"text_provenance":text.provenance})
        );
    }
    if matches!(mode, "describe" | "all") {
        let result = client
            .describe_image(&image, "image/jpeg", &revision, "en", &metadata)
            .await?;
        println!(
            "{}",
            json!({"image_description":"ok","provenance":result.provenance})
        );
    }
    Ok(())
}

fn require_cuda(provenance: &VisionProvenance) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("INFER_SMOKE_REQUIRE_CUDA").is_some()
        && (provenance.actual_execution_provider != "cuda"
            || provenance.execution_provider_fallback_reason.is_some())
    {
        return Err(format!(
            "expected CUDA without fallback: {}",
            provenance.actual_execution_provider
        )
        .into());
    }
    Ok(())
}
