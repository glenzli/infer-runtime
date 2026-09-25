//! Local Echo-style sound generation smoke: credential_file output.wav

use std::{collections::BTreeMap, path::PathBuf};

use infer_runtime_client::{Client, SoundGenerationRequest};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let credential = PathBuf::from(
        args.next()
            .ok_or("usage: sound_effect CREDENTIAL_FILE OUTPUT.wav")?,
    );
    let output = PathBuf::from(
        args.next()
            .ok_or("usage: sound_effect CREDENTIAL_FILE OUTPUT.wav")?,
    );
    if args.next().is_some() || !output.is_absolute() {
        return Err("output path must be absolute and only two arguments are accepted".into());
    }
    let client = Client::builder().credential_file(credential).build()?;
    let metadata = BTreeMap::from([
        ("infer.policy".into(), "local-first".into()),
        ("infer.priority".into(), "background".into()),
        ("infer.placement".into(), "local_only".into()),
        ("infer.prefer".into(), "local".into()),
        ("infer.offline_required".into(), "true".into()),
        ("infer.capability_floor".into(), "foundational".into()),
        ("infer.latency".into(), "throughput".into()),
        ("infer.fallback".into(), "none".into()),
        ("infer.max_cost_usd".into(), "0".into()),
    ]);
    let request = SoundGenerationRequest {
        model: "audio.generate_sound".into(),
        prompt: "Light rain falling on a window, no speech or music".into(),
        duration_seconds: 5,
        seed: Some(4200),
        metadata,
    };
    let artifact = client.generate_sound_effect(&request).await?;
    let job = client.job(&artifact.job_id).await?;
    if job.state != "succeeded"
        || job.app_id != "echo"
        || job.intent != request.model
        || job.placement != "local"
        || job.provider != artifact.provider
        || job.deployment != artifact.deployment
        || job.model_build != artifact.model_build
        || job.physical_model != artifact.physical_model
        || job.routing.capability_floor != "foundational"
        || job.constraints["policy"] != "local-first"
        || job.constraints["priority"] != "background"
        || job.constraints["placement"] != "local_only"
        || job.constraints["prefer"] != "local"
        || job.constraints["offline_required"] != true
        || job.constraints["capability_floor"] != "foundational"
        || job.constraints["latency"] != "throughput"
        || job.constraints["fallback"] != "none"
        || job.constraints["max_cost_usd"] != 0.0
        || job
            .attempts
            .iter()
            .any(|attempt| attempt.trigger == "fallback")
    {
        return Err("Job provenance does not match the local sound artifact".into());
    }
    std::fs::write(&output, &artifact.wav)?;
    let evidence = json!({
        "artifact": output,
        "sha256": artifact.sha256,
        "bytes": artifact.wav.len(),
        "duration_seconds": artifact.duration_seconds,
        "seed": artifact.seed,
        "job": job,
    });
    let sidecar = output.with_extension("json");
    std::fs::write(&sidecar, serde_json::to_vec_pretty(&evidence)?)?;
    println!("{}", sidecar.display());
    Ok(())
}
