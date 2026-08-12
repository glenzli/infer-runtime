use std::{collections::BTreeMap, env, fs::File, path::PathBuf};

use infer_artifact::{ArtifactStore, LocalWorkerBuildManifest};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRequest {
    artifact_root: PathBuf,
    manifest: LocalWorkerBuildManifest,
    sources: BTreeMap<String, PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request_path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: publish_local_worker_build <private-request.json>")?;
    let request: PublicationRequest = serde_json::from_reader(File::open(request_path)?)?;
    let store = ArtifactStore::at(request.artifact_root)?;
    let runtime_root = store.publish_local_worker_build(&request.manifest, &request.sources)?;
    // This operator tool returns only the private managed execution root. It
    // never prints credentials, model contents or Consumer payloads.
    println!("{}", runtime_root.display());
    Ok(())
}
