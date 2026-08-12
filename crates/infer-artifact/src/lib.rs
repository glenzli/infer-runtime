//! Content-addressed artifact storage for native model runtimes.
//!
//! Consumer requests never carry filesystem paths. Only version-controlled
//! Build identities reach this owner, which verifies content before returning
//! a path to a native runtime.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use infer_core::{ArtifactIdentityConfig, ArtifactStoreConfig, OnnxModelBuildConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact manifest error: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("invalid artifact identity: {0}")]
    InvalidIdentity(String),
    #[error("artifact digest mismatch for build {build}: expected {expected}, got {actual}")]
    DigestMismatch {
        build: String,
        expected: String,
        actual: String,
    },
    #[error("artifact size mismatch for build {build}: expected {expected}, got {actual}")]
    SizeMismatch {
        build: String,
        expected: u64,
        actual: u64,
    },
    #[error("artifact manifest for build {0} does not match the configured Build identity")]
    ManifestDrift(String),
    #[error("unable to determine a platform application-data directory")]
    MissingApplicationDataDirectory,
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoredBuildManifest {
    pub schema_version: u16,
    pub build_id: String,
    pub onnx: OnnxModelBuildConfig,
}

/// Exact multi-file identity for a trusted local worker Build.
///
/// The artifact store owns only immutable bytes and their stable names. Typed
/// Providers continue to own preprocessing, instruction templates and output
/// interpretation; this is not a generic execution API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkerBuildManifest {
    pub schema_version: u16,
    pub build_id: String,
    pub adapter: String,
    pub artifacts: std::collections::BTreeMap<String, ArtifactIdentityConfig>,
}

#[derive(Debug, Clone)]
pub struct ResolvedLocalWorkerBuild {
    pub runtime_root: PathBuf,
    pub manifest: LocalWorkerBuildManifest,
}

impl ArtifactStore {
    pub fn from_config(config: &ArtifactStoreConfig) -> Result<Self, ArtifactError> {
        let root = match config.root.as_deref() {
            Some(root) if !root.trim().is_empty() => PathBuf::from(root),
            _ => default_root()?,
        };
        Self::at(root)
    }

    pub fn at(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let store = Self { root: root.into() };
        store.ensure_layout()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf, ArtifactError> {
        validate_sha256(digest)?;
        Ok(self.root.join("blobs").join("sha256").join(digest))
    }

    pub fn manifest_path(&self, build_id: &str) -> Result<PathBuf, ArtifactError> {
        validate_build_id(build_id)?;
        Ok(self
            .root
            .join("builds")
            .join(build_id)
            .join("manifest.json"))
    }

    /// Copy into private staging, verify, then atomically publish by digest.
    pub fn publish_onnx(
        &self,
        build_id: &str,
        source: impl AsRef<Path>,
        onnx: &OnnxModelBuildConfig,
    ) -> Result<PathBuf, ArtifactError> {
        validate_build_id(build_id)?;
        validate_sha256(&onnx.artifact.sha256)?;
        let blob = self.publish_file(build_id, source.as_ref(), &onnx.artifact)?;

        let manifest = StoredBuildManifest {
            schema_version: 1,
            build_id: build_id.into(),
            onnx: onnx.clone(),
        };
        self.write_manifest(&manifest)?;
        Ok(blob)
    }

    /// Publish one configured non-graph dependency, such as tokenizer.json.
    /// The executable graph must already have established the exact Build
    /// manifest, preventing an auxiliary file from drifting independently.
    pub fn publish_onnx_auxiliary(
        &self,
        build_id: &str,
        name: &str,
        source: impl AsRef<Path>,
        onnx: &OnnxModelBuildConfig,
    ) -> Result<PathBuf, ArtifactError> {
        validate_build_id(name)?;
        let identity = onnx.auxiliary_artifacts.get(name).ok_or_else(|| {
            ArtifactError::InvalidIdentity(format!(
                "ONNX Build {build_id} has no auxiliary artifact {name:?}"
            ))
        })?;
        self.verify_manifest(build_id, onnx)?;
        self.publish_file(&format!("{build_id}:{name}"), source.as_ref(), identity)
    }

    /// Publish and verify every file needed by one typed local worker Build,
    /// then atomically publish its immutable manifest.
    pub fn publish_local_worker_build(
        &self,
        manifest: &LocalWorkerBuildManifest,
        sources: &std::collections::BTreeMap<String, PathBuf>,
    ) -> Result<PathBuf, ArtifactError> {
        validate_local_worker_manifest(manifest)?;
        if sources.keys().ne(manifest.artifacts.keys()) {
            return Err(ArtifactError::InvalidIdentity(format!(
                "local worker Build {} sources do not match its artifact manifest",
                manifest.build_id
            )));
        }
        for (name, identity) in &manifest.artifacts {
            let source = sources.get(name).expect("validated source identity");
            self.publish_file(&format!("{}:{name}", manifest.build_id), source, identity)?;
        }
        self.write_local_worker_manifest(manifest)?;
        self.materialize_local_worker_build(manifest)
    }

    /// Resolve a typed local worker Build, rejecting manifest drift or any
    /// modified artifact before the Provider process is started.
    pub fn resolve_local_worker_build(
        &self,
        expected: &LocalWorkerBuildManifest,
    ) -> Result<PathBuf, ArtifactError> {
        validate_local_worker_manifest(expected)?;
        let path = self.local_worker_manifest_path(&expected.build_id)?;
        let actual: LocalWorkerBuildManifest = serde_json::from_reader(File::open(path)?)?;
        if actual != *expected {
            return Err(ArtifactError::ManifestDrift(expected.build_id.clone()));
        }
        self.materialize_local_worker_build(expected)
    }

    /// Resolve from the compact artifact-set identity carried by versioned
    /// config. Per-file receipts remain private in the managed manifest.
    pub fn resolve_local_worker_build_identity(
        &self,
        build_id: &str,
        adapter: &str,
        artifact_set_sha256: &str,
    ) -> Result<ResolvedLocalWorkerBuild, ArtifactError> {
        validate_build_id(build_id)?;
        validate_build_id(adapter)?;
        validate_sha256(artifact_set_sha256)?;
        let path = self.local_worker_manifest_path(build_id)?;
        let manifest: LocalWorkerBuildManifest = serde_json::from_reader(File::open(path)?)?;
        if manifest.build_id != build_id
            || manifest.adapter != adapter
            || local_worker_artifact_set_sha256(&manifest) != artifact_set_sha256
        {
            return Err(ArtifactError::ManifestDrift(build_id.into()));
        }
        let runtime_root = self.materialize_local_worker_build(&manifest)?;
        Ok(ResolvedLocalWorkerBuild {
            runtime_root,
            manifest,
        })
    }

    fn materialize_local_worker_build(
        &self,
        manifest: &LocalWorkerBuildManifest,
    ) -> Result<PathBuf, ArtifactError> {
        let root = self
            .root
            .join("builds")
            .join(&manifest.build_id)
            .join("runtime");
        private_directory(&root)?;
        for (relative, identity) in &manifest.artifacts {
            let blob = self.blob_path(&identity.sha256)?;
            verify_file(
                &format!("{}:{relative}", manifest.build_id),
                &blob,
                identity,
            )?;
            let target = root.join(relative);
            private_directory(target.parent().expect("artifact has a runtime parent"))?;
            if target.exists() {
                if !same_file(&blob, &target)? {
                    verify_file(
                        &format!("{}:{relative}", manifest.build_id),
                        &target,
                        identity,
                    )?;
                }
            } else if fs::hard_link(&blob, &target).is_err() {
                let mut input = File::open(&blob)?;
                let mut output = private_file(&target)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
            }
        }
        Ok(root)
    }

    fn local_worker_manifest_path(&self, build_id: &str) -> Result<PathBuf, ArtifactError> {
        validate_build_id(build_id)?;
        Ok(self
            .root
            .join("builds")
            .join(build_id)
            .join("worker-manifest.json"))
    }

    pub fn resolve_onnx_auxiliary(
        &self,
        build_id: &str,
        name: &str,
        onnx: &OnnxModelBuildConfig,
    ) -> Result<PathBuf, ArtifactError> {
        let identity = onnx.auxiliary_artifacts.get(name).ok_or_else(|| {
            ArtifactError::InvalidIdentity(format!(
                "ONNX Build {build_id} has no auxiliary artifact {name:?}"
            ))
        })?;
        self.verify_manifest(build_id, onnx)?;
        let blob = self.blob_path(&identity.sha256)?;
        verify_file(&format!("{build_id}:{name}"), &blob, identity)?;
        Ok(blob)
    }

    fn publish_file(
        &self,
        identity_name: &str,
        source: &Path,
        identity: &ArtifactIdentityConfig,
    ) -> Result<PathBuf, ArtifactError> {
        validate_sha256(&identity.sha256)?;
        let staged = self
            .root
            .join("staging")
            .join(format!("{}.partial", Uuid::new_v4()));
        let mut input = File::open(source)?;
        let mut output = private_file(&staged)?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        output.sync_all()?;
        drop(output);

        let actual = format!("{:x}", hasher.finalize());
        if actual != identity.sha256 {
            let _ = fs::remove_file(&staged);
            return Err(ArtifactError::DigestMismatch {
                build: identity_name.into(),
                expected: identity.sha256.clone(),
                actual,
            });
        }
        if size != identity.size_bytes {
            let _ = fs::remove_file(&staged);
            return Err(ArtifactError::SizeMismatch {
                build: identity_name.into(),
                expected: identity.size_bytes,
                actual: size,
            });
        }

        let blob = self.blob_path(&identity.sha256)?;
        if blob.exists() {
            verify_file(identity_name, &blob, identity)?;
            fs::remove_file(&staged)?;
        } else if let Err(error) = fs::rename(&staged, &blob) {
            // Another publisher may have won the race. Accept only a fully
            // verified winner; otherwise preserve the original error.
            if blob.exists() {
                verify_file(identity_name, &blob, identity)?;
                let _ = fs::remove_file(&staged);
            } else {
                return Err(error.into());
            }
        }

        Ok(blob)
    }

    /// Resolve for native execution, re-verifying manifest identity and bytes.
    pub fn resolve_onnx(
        &self,
        build_id: &str,
        onnx: &OnnxModelBuildConfig,
    ) -> Result<PathBuf, ArtifactError> {
        self.verify_manifest(build_id, onnx)?;
        let blob = self.blob_path(&onnx.artifact.sha256)?;
        verify_file(build_id, &blob, &onnx.artifact)?;
        for (name, identity) in &onnx.auxiliary_artifacts {
            let auxiliary = self.blob_path(&identity.sha256)?;
            verify_file(&format!("{build_id}:{name}"), &auxiliary, identity)?;
        }
        Ok(blob)
    }

    fn verify_manifest(
        &self,
        build_id: &str,
        onnx: &OnnxModelBuildConfig,
    ) -> Result<(), ArtifactError> {
        let manifest_path = self.manifest_path(build_id)?;
        let manifest: StoredBuildManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
        if manifest.schema_version != 1 || manifest.build_id != build_id || manifest.onnx != *onnx {
            return Err(ArtifactError::ManifestDrift(build_id.into()));
        }
        Ok(())
    }

    fn ensure_layout(&self) -> Result<(), ArtifactError> {
        for path in [
            self.root.clone(),
            self.root.join("blobs"),
            self.root.join("blobs").join("sha256"),
            self.root.join("builds"),
            self.root.join("staging"),
        ] {
            private_directory(&path)?;
        }
        Ok(())
    }

    fn write_manifest(&self, manifest: &StoredBuildManifest) -> Result<(), ArtifactError> {
        let path = self.manifest_path(&manifest.build_id)?;
        let directory = path.parent().expect("manifest has a build directory");
        private_directory(directory)?;

        if path.exists() {
            let existing: StoredBuildManifest = serde_json::from_reader(File::open(&path)?)?;
            return if existing == *manifest {
                Ok(())
            } else {
                Err(ArtifactError::ManifestDrift(manifest.build_id.clone()))
            };
        }

        let temporary = directory.join(format!("manifest.{}.partial", Uuid::new_v4()));
        let mut file = private_file(&temporary)?;
        serde_json::to_writer_pretty(&mut file, manifest)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, &path) {
            if path.exists() {
                let existing: StoredBuildManifest = serde_json::from_reader(File::open(&path)?)?;
                let _ = fs::remove_file(&temporary);
                if existing == *manifest {
                    return Ok(());
                }
                return Err(ArtifactError::ManifestDrift(manifest.build_id.clone()));
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn write_local_worker_manifest(
        &self,
        manifest: &LocalWorkerBuildManifest,
    ) -> Result<(), ArtifactError> {
        let path = self.local_worker_manifest_path(&manifest.build_id)?;
        let directory = path.parent().expect("manifest has a build directory");
        private_directory(directory)?;
        if path.exists() {
            let existing: LocalWorkerBuildManifest = serde_json::from_reader(File::open(&path)?)?;
            return if existing == *manifest {
                Ok(())
            } else {
                Err(ArtifactError::ManifestDrift(manifest.build_id.clone()))
            };
        }
        let temporary = directory.join(format!("worker-manifest.{}.partial", Uuid::new_v4()));
        let mut file = private_file(&temporary)?;
        serde_json::to_writer_pretty(&mut file, manifest)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, &path) {
            if path.exists() {
                let existing: LocalWorkerBuildManifest =
                    serde_json::from_reader(File::open(&path)?)?;
                let _ = fs::remove_file(&temporary);
                if existing == *manifest {
                    return Ok(());
                }
                return Err(ArtifactError::ManifestDrift(manifest.build_id.clone()));
            }
            return Err(error.into());
        }
        Ok(())
    }
}

fn validate_local_worker_manifest(
    manifest: &LocalWorkerBuildManifest,
) -> Result<(), ArtifactError> {
    validate_build_id(&manifest.build_id)?;
    validate_build_id(&manifest.adapter)?;
    if manifest.schema_version != 1 || manifest.artifacts.is_empty() {
        return Err(ArtifactError::InvalidIdentity(format!(
            "local worker Build {} needs schema version 1 and at least one artifact",
            manifest.build_id
        )));
    }
    for (name, identity) in &manifest.artifacts {
        validate_artifact_relative_path(name)?;
        validate_sha256(&identity.sha256)?;
        if identity.size_bytes == 0
            || identity.source_url.trim().is_empty()
            || identity.source_revision.trim().is_empty()
            || identity.license_spdx.trim().is_empty()
        {
            return Err(ArtifactError::InvalidIdentity(format!(
                "local worker Build {} artifact {name} has incomplete supply-chain identity",
                manifest.build_id
            )));
        }
    }
    Ok(())
}

fn local_worker_artifact_set_sha256(manifest: &LocalWorkerBuildManifest) -> String {
    let mut hasher = Sha256::new();
    for (name, identity) in &manifest.artifacts {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(identity.sha256.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

fn validate_artifact_relative_path(value: &str) -> Result<(), ArtifactError> {
    let path = Path::new(value);
    let valid = !value.is_empty()
        && !value.contains('\\')
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, std::path::Component::Normal(segment) if !segment.is_empty())
        })
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
        });
    if valid {
        Ok(())
    } else {
        Err(ArtifactError::InvalidIdentity(format!(
            "invalid local worker artifact path {value:?}"
        )))
    }
}

fn verify_file(
    build_id: &str,
    path: &Path,
    identity: &ArtifactIdentityConfig,
) -> Result<(), ArtifactError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    if size != identity.size_bytes {
        return Err(ArtifactError::SizeMismatch {
            build: build_id.into(),
            expected: identity.size_bytes,
            actual: size,
        });
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != identity.sha256 {
        return Err(ArtifactError::DigestMismatch {
            build: build_id.into(),
            expected: identity.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

fn validate_sha256(digest: &str) -> Result<(), ArtifactError> {
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ArtifactError::InvalidIdentity(
            "sha256 must contain exactly 64 hexadecimal characters".into(),
        ))
    }
}

fn validate_build_id(build_id: &str) -> Result<(), ArtifactError> {
    if !build_id.is_empty()
        && build_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(ArtifactError::InvalidIdentity(format!(
            "invalid build id {build_id:?}"
        )))
    }
}

fn default_root() -> Result<PathBuf, ArtifactError> {
    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Application Support/infer-runtime/artifacts"))
            .ok_or(ArtifactError::MissingApplicationDataDirectory)
    }
    #[cfg(target_os = "windows")]
    {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("infer-runtime/artifacts"))
            .ok_or(ArtifactError::MissingApplicationDataDirectory)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(root) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(root).join("infer-runtime/artifacts"));
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".local/share/infer-runtime/artifacts"))
            .ok_or(ArtifactError::MissingApplicationDataDirectory)
    }
}

fn private_directory(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn same_file(left: &Path, right: &Path) -> Result<bool, std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = fs::metadata(left)?;
        let right = fs::metadata(right)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use infer_core::{
        ArtifactIdentityConfig, ImagePreprocessConfig, OnnxAdapterKind, OnnxExecutionProvider,
        TensorContractConfig,
    };
    use tempfile::tempdir;

    use super::*;

    fn build(bytes: &[u8]) -> OnnxModelBuildConfig {
        OnnxModelBuildConfig {
            adapter: OnnxAdapterKind::YunetFaceDetection,
            artifact: ArtifactIdentityConfig {
                sha256: format!("{:x}", Sha256::digest(bytes)),
                size_bytes: bytes.len() as u64,
                source_url: "https://example.invalid/model.onnx".into(),
                source_revision: "revision".into(),
                license_spdx: "MIT".into(),
            },
            auxiliary_artifacts: BTreeMap::new(),
            opset: 17,
            inputs: vec![TensorContractConfig {
                name: "input".into(),
                dtype: "float32".into(),
                shape: vec!["1".into(), "3".into(), "height".into(), "width".into()],
            }],
            outputs: vec![],
            preprocessing: Some(ImagePreprocessConfig {
                identity: "test".into(),
                orientation: "decoded".into(),
                resize: "none".into(),
                color_space: "srgb".into(),
                channel_order: "bgr".into(),
                layout: "nchw".into(),
                dtype: "float32".into(),
                mean: vec![0.0, 0.0, 0.0],
                scale: vec![1.0],
            }),
            text_preprocessing: None,
            embedding_space: None,
            postprocessing_identity: "test".into(),
            postprocessing_parameters: BTreeMap::new(),
            allowed_execution_providers: vec![OnnxExecutionProvider::Cpu],
            precision: "fp32".into(),
        }
    }

    #[test]
    fn publishes_and_reverifies_content_addressed_artifact() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.onnx");
        fs::write(&source, b"verified model").unwrap();
        let store = ArtifactStore::at(directory.path().join("store")).unwrap();
        let build = build(b"verified model");
        let path = store.publish_onnx("yunet", &source, &build).unwrap();
        assert_eq!(path, store.resolve_onnx("yunet", &build).unwrap());
    }

    #[test]
    fn digest_mismatch_is_never_published() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.onnx");
        fs::write(&source, b"tampered").unwrap();
        let store = ArtifactStore::at(directory.path().join("store")).unwrap();
        let build = build(b"expected");
        assert!(matches!(
            store.publish_onnx("yunet", &source, &build),
            Err(ArtifactError::DigestMismatch { .. })
        ));
        assert!(!store.blob_path(&build.artifact.sha256).unwrap().exists());
    }

    #[test]
    fn changed_preprocessing_cannot_reuse_a_manifest_identity() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.onnx");
        fs::write(&source, b"verified model").unwrap();
        let store = ArtifactStore::at(directory.path().join("store")).unwrap();
        let original = build(b"verified model");
        store.publish_onnx("yunet", &source, &original).unwrap();
        let mut changed = original;
        changed.preprocessing.as_mut().unwrap().identity = "different".into();
        assert!(matches!(
            store.resolve_onnx("yunet", &changed),
            Err(ArtifactError::ManifestDrift(_))
        ));
    }

    #[test]
    fn auxiliary_artifact_is_part_of_resolvable_build_identity() {
        let directory = tempdir().unwrap();
        let graph = directory.path().join("model.onnx");
        let tokenizer = directory.path().join("tokenizer.json");
        fs::write(&graph, b"verified model").unwrap();
        fs::write(&tokenizer, b"verified tokenizer").unwrap();
        let store = ArtifactStore::at(directory.path().join("store")).unwrap();
        let mut build = build(b"verified model");
        build.auxiliary_artifacts.insert(
            "tokenizer".into(),
            ArtifactIdentityConfig {
                sha256: format!("{:x}", Sha256::digest(b"verified tokenizer")),
                size_bytes: b"verified tokenizer".len() as u64,
                source_url: "https://example.invalid/tokenizer.json".into(),
                source_revision: "revision".into(),
                license_spdx: "Apache-2.0".into(),
            },
        );

        store.publish_onnx("siglip-text", &graph, &build).unwrap();
        assert!(store.resolve_onnx("siglip-text", &build).is_err());
        store
            .publish_onnx_auxiliary("siglip-text", "tokenizer", &tokenizer, &build)
            .unwrap();
        store.resolve_onnx("siglip-text", &build).unwrap();

        fs::write(
            store
                .blob_path(&build.auxiliary_artifacts["tokenizer"].sha256)
                .unwrap(),
            b"tampered tokenizer",
        )
        .unwrap();
        assert!(matches!(
            store.resolve_onnx("siglip-text", &build),
            Err(ArtifactError::SizeMismatch { .. }) | Err(ArtifactError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn publishes_and_reverifies_typed_local_worker_build() {
        let directory = tempdir().unwrap();
        let model = directory.path().join("model.safetensors");
        let tokenizer = directory.path().join("tokenizer.json");
        fs::write(&model, b"model bytes").unwrap();
        fs::write(&tokenizer, b"tokenizer bytes").unwrap();
        let manifest = LocalWorkerBuildManifest {
            schema_version: 1,
            build_id: "qwen3_embedding_0_6b_mlx".into(),
            adapter: "qwen3_embedding".into(),
            artifacts: BTreeMap::from([
                (
                    "model".into(),
                    ArtifactIdentityConfig {
                        sha256: format!("{:x}", Sha256::digest(b"model bytes")),
                        size_bytes: b"model bytes".len() as u64,
                        source_url: "https://example.invalid/model".into(),
                        source_revision: "revision".into(),
                        license_spdx: "Apache-2.0".into(),
                    },
                ),
                (
                    "tokenizer".into(),
                    ArtifactIdentityConfig {
                        sha256: format!("{:x}", Sha256::digest(b"tokenizer bytes")),
                        size_bytes: b"tokenizer bytes".len() as u64,
                        source_url: "https://example.invalid/tokenizer".into(),
                        source_revision: "revision".into(),
                        license_spdx: "Apache-2.0".into(),
                    },
                ),
            ]),
        };
        let store = ArtifactStore::at(directory.path().join("store")).unwrap();
        let published = store
            .publish_local_worker_build(
                &manifest,
                &BTreeMap::from([("model".into(), model), ("tokenizer".into(), tokenizer)]),
            )
            .unwrap();
        assert_eq!(
            published,
            store.resolve_local_worker_build(&manifest).unwrap()
        );

        fs::write(published.join("model"), b"tampered").unwrap();
        assert!(matches!(
            store.resolve_local_worker_build(&manifest),
            Err(ArtifactError::SizeMismatch { .. }) | Err(ArtifactError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn local_worker_manifest_rejects_incomplete_supply_chain_identity() {
        let manifest = LocalWorkerBuildManifest {
            schema_version: 1,
            build_id: "ocr".into(),
            adapter: "pp_ocrv6".into(),
            artifacts: BTreeMap::from([(
                "detection".into(),
                ArtifactIdentityConfig {
                    sha256: "0".repeat(64),
                    size_bytes: 1,
                    source_url: String::new(),
                    source_revision: "revision".into(),
                    license_spdx: "Apache-2.0".into(),
                },
            )]),
        };
        assert!(validate_local_worker_manifest(&manifest).is_err());
    }

    #[test]
    fn local_worker_artifact_paths_cannot_escape_the_build_root() {
        assert!(validate_artifact_relative_path("1_Pooling/config.json").is_ok());
        assert!(validate_artifact_relative_path("../model").is_err());
        assert!(validate_artifact_relative_path("/tmp/model").is_err());
    }
}
