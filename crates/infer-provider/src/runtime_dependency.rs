//! Trusted, startup-time resolution of Provider host executables.
//!
//! Discovery is deliberately bounded to absolute inherited PATH entries plus
//! well-known Homebrew and system binary directories. Resolved executables are
//! canonical absolute paths and are reused for the Provider lifetime.

use std::{
    collections::BTreeSet,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use infer_core::{LocalWorkerAdapterKind, ProviderConfig, RuntimeConfig};
use serde::Serialize;

const EXTRA_EXECUTABLE_DIRECTORIES: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_VERSION_OUTPUT_BYTES: u64 = 16 * 1024;
const WORKER_PROBE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_WORKER_PROBE_OUTPUT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReadinessStatus {
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeDependencyCheck {
    pub name: String,
    pub requested: String,
    pub status: ProviderReadinessStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub searched_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRuntimeReadiness {
    pub provider: String,
    pub status: ProviderReadinessStatus,
    pub summary: String,
    pub checks: Vec<RuntimeDependencyCheck>,
}

impl ProviderRuntimeReadiness {
    pub fn is_ready(&self) -> bool {
        self.status == ProviderReadinessStatus::Ready
    }

    pub fn mark_unavailable(&mut self, name: impl Into<String>, message: impl Into<String>) {
        let message = message.into();
        self.status = ProviderReadinessStatus::Unavailable;
        self.summary = message.clone();
        self.checks.push(RuntimeDependencyCheck {
            name: name.into(),
            requested: "runtime-owned".into(),
            status: ProviderReadinessStatus::Unavailable,
            resolved_path: None,
            canonical_target: None,
            version: None,
            searched_paths: Vec::new(),
            message: Some(message),
        });
    }

    pub fn mark_ready(
        &mut self,
        name: impl Into<String>,
        resolved_path: impl Into<String>,
        version: Option<String>,
    ) {
        let resolved_path = resolved_path.into();
        self.checks.push(RuntimeDependencyCheck {
            name: name.into(),
            requested: "runtime-owned".into(),
            status: ProviderReadinessStatus::Ready,
            resolved_path: Some(resolved_path),
            canonical_target: None,
            version,
            searched_paths: Vec::new(),
            message: None,
        });
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProviderProcess {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub readiness: ProviderRuntimeReadiness,
}

/// Resolves command and typed runtime dependencies without loading a model.
/// This is safe for Console status refresh and daemon startup.
pub fn resolve_provider_process(
    provider_id: &str,
    provider: &ProviderConfig,
    requires_ffmpeg: bool,
) -> ResolvedProviderProcess {
    let mut readiness = ProviderRuntimeReadiness {
        provider: provider_id.into(),
        status: ProviderReadinessStatus::Ready,
        summary: "runtime dependencies are ready".into(),
        checks: Vec::new(),
    };
    let mut args = provider.args.clone();
    let command = provider.command.as_deref().and_then(|requested| {
        match resolve_executable("command", requested, false, true) {
            Ok((resolved, check)) => {
                readiness.checks.push(check);
                Some(resolved)
            }
            Err(check) => {
                readiness.status = ProviderReadinessStatus::Unavailable;
                readiness.summary = check
                    .message
                    .clone()
                    .unwrap_or_else(|| "provider command is unavailable".into());
                readiness.checks.push(*check);
                None
            }
        }
    });

    if requires_ffmpeg {
        let legacy_index = args.iter().position(|argument| argument == "--ffmpeg");
        let requested = provider
            .runtime_dependencies
            .ffmpeg
            .as_deref()
            .map(str::to_owned)
            .or_else(|| legacy_index.and_then(|index| args.get(index + 1).cloned()))
            .unwrap_or_else(|| "ffmpeg".into());
        match resolve_executable("ffmpeg", &requested, true, false) {
            Ok((resolved, check)) => {
                if let Some(index) = legacy_index {
                    if let Some(argument) = args.get_mut(index + 1) {
                        *argument = resolved;
                    } else {
                        readiness.status = ProviderReadinessStatus::Unavailable;
                        readiness.summary =
                            "audio worker --ffmpeg argument has no executable value".into();
                    }
                } else {
                    args.extend(["--ffmpeg".into(), resolved]);
                }
                readiness.checks.push(check);
            }
            Err(check) => {
                readiness.status = ProviderReadinessStatus::Unavailable;
                readiness.summary = check
                    .message
                    .clone()
                    .unwrap_or_else(|| "ffmpeg is unavailable".into());
                readiness.checks.push(*check);
            }
        }
    }

    ResolvedProviderProcess {
        command,
        args,
        readiness,
    }
}

/// Lightweight operator preflight for every configured Provider. Artifact and
/// model verification remains owned by daemon assembly.
pub fn preflight_provider_runtime(config: &RuntimeConfig) -> Vec<ProviderRuntimeReadiness> {
    config
        .providers
        .iter()
        .map(|(provider_id, provider)| {
            resolve_provider_process(
                provider_id,
                provider,
                provider_requires_ffmpeg(config, provider_id),
            )
            .readiness
        })
        .collect()
}

pub fn provider_requires_ffmpeg(config: &RuntimeConfig, provider_id: &str) -> bool {
    config.deployments.values().any(|deployment| {
        deployment.provider == provider_id
            && config
                .model_builds
                .get(&deployment.build)
                .and_then(|build| build.local_worker.as_ref())
                .is_some_and(|worker| {
                    matches!(
                        worker.adapter,
                        LocalWorkerAdapterKind::YamnetAudioEvents
                            | LocalWorkerAdapterKind::ClapAudioTextEmbedding
                    )
                })
    })
}

/// Loads the exact admitted YAMNet artifact without audio input and verifies
/// the worker runtime plus decoder. Diagnostics are intentionally stable and
/// never include the model path or worker stderr.
pub fn verify_yamnet_worker(
    command: &str,
    args: &[String],
    model_path: &str,
) -> Result<RuntimeDependencyCheck, Box<RuntimeDependencyCheck>> {
    let mut child = Command::new(command)
        .args(args)
        .args(["--verify-model", model_path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            Box::new(worker_probe_failure(
                "YAMNet readiness probe could not start",
            ))
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < WORKER_PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(worker_probe_failure(
                    "YAMNet readiness probe timed out",
                )));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(worker_probe_failure(
                    "YAMNet readiness probe failed",
                )));
            }
        }
    };
    if !status.success() {
        return Err(Box::new(worker_probe_failure(
            "YAMNet readiness probe reported an unavailable runtime, model, or decoder",
        )));
    }
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_WORKER_PROBE_OUTPUT_BYTES)
            .read_to_string(&mut output)
            .map_err(|_| {
                Box::new(worker_probe_failure(
                    "YAMNet readiness response was unreadable",
                ))
            })?;
    }
    let response: serde_json::Value = serde_json::from_str(&output).map_err(|_| {
        Box::new(worker_probe_failure(
            "YAMNet readiness response was malformed",
        ))
    })?;
    let classes = response.get("classes").and_then(serde_json::Value::as_u64);
    let runtime_version = response
        .get("runtime_version")
        .and_then(serde_json::Value::as_str);
    let decoder_version = response
        .get("decoder_version")
        .and_then(serde_json::Value::as_str);
    if classes != Some(521)
        || runtime_version.is_none_or(str::is_empty)
        || decoder_version.is_none_or(str::is_empty)
    {
        return Err(Box::new(worker_probe_failure(
            "YAMNet readiness response did not match the admitted runtime contract",
        )));
    }
    Ok(RuntimeDependencyCheck {
        name: "yamnet_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Ready,
        resolved_path: None,
        canonical_target: None,
        version: Some(format!(
            "tensorflow {}; {}",
            runtime_version.unwrap_or_default(),
            decoder_version.unwrap_or_default()
        )),
        searched_paths: Vec::new(),
        message: None,
    })
}

/// Loads the exact admitted CLAP artifact without Consumer content and verifies
/// the MPS-only local worker contract.  This is intentionally distinct from
/// the YAMNet probe: both use ffmpeg for bounded decode, but their model and
/// output contracts are unrelated.
pub fn verify_clap_worker(
    command: &str,
    args: &[String],
    model_path: &str,
) -> Result<RuntimeDependencyCheck, Box<RuntimeDependencyCheck>> {
    let failure = |message: &str| RuntimeDependencyCheck {
        name: "clap_audio_embedding_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Unavailable,
        resolved_path: None,
        canonical_target: None,
        version: None,
        searched_paths: Vec::new(),
        message: Some(message.into()),
    };
    let mut child = Command::new(command)
        .args(args)
        .args(["--verify-model", model_path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Box::new(failure("CLAP readiness probe could not start")))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < WORKER_PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(failure("CLAP readiness probe timed out")));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(failure("CLAP readiness probe failed")));
            }
        }
    };
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_WORKER_PROBE_OUTPUT_BYTES)
            .read_to_string(&mut output)
            .map_err(|_| Box::new(failure("CLAP readiness response was unreadable")))?;
    }
    if !status.success() {
        return Err(Box::new(failure(
            "CLAP readiness probe reported an unavailable MPS runtime or model",
        )));
    }
    let response: serde_json::Value = serde_json::from_str(&output)
        .map_err(|_| Box::new(failure("CLAP readiness response was malformed")))?;
    if response
        .get("dimensions")
        .and_then(serde_json::Value::as_u64)
        != Some(512)
        || response.get("runtime").and_then(serde_json::Value::as_str) != Some("pytorch-mps")
    {
        return Err(Box::new(failure(
            "CLAP readiness response did not match the admitted runtime contract",
        )));
    }
    Ok(RuntimeDependencyCheck {
        name: "clap_audio_embedding_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Ready,
        resolved_path: None,
        canonical_target: None,
        version: Some("pytorch-mps; dimensions=512".into()),
        searched_paths: Vec::new(),
        message: None,
    })
}

/// Verifies that the configured Python runtime can import Core ML Tools and
/// that the exact admitted SAM artifact set exposes the frozen three-model
/// interface. The probe never receives image input and never emits paths.
pub fn verify_coreml_sam_worker(
    command: &str,
    args: &[String],
    model_path: &str,
    artifact_sha256: &str,
) -> Result<RuntimeDependencyCheck, Box<RuntimeDependencyCheck>> {
    let failure = |message: &str| RuntimeDependencyCheck {
        name: "coreml_sam_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Unavailable,
        resolved_path: None,
        canonical_target: None,
        version: None,
        searched_paths: Vec::new(),
        message: Some(message.into()),
    };
    let mut child = Command::new(command)
        .args(args)
        .args([
            "--verify-model",
            model_path,
            "--artifact-sha256",
            artifact_sha256,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Box::new(failure("SAM readiness probe could not start")))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < WORKER_PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(failure("SAM readiness probe timed out")));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Box::new(failure("SAM readiness probe failed")));
            }
        }
    };
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_WORKER_PROBE_OUTPUT_BYTES)
            .read_to_string(&mut output)
            .map_err(|_| Box::new(failure("SAM readiness response was unreadable")))?;
    }
    if !status.success() {
        let message = serde_json::from_str::<serde_json::Value>(&output)
            .ok()
            .and_then(|response| response.get("error")?.as_str().map(str::to_owned))
            .map_or(
                "SAM readiness probe reported an unavailable runtime or model",
                |code| {
                    if code == "sam_model_not_prepared" {
                        "SAM Core ML compiled cache is missing or incompatible; prepare this Build before restarting inferd"
                    } else {
                        "SAM readiness probe reported an unavailable runtime or model"
                    }
                },
            );
        return Err(Box::new(failure(message)));
    }
    let response: serde_json::Value = serde_json::from_str(&output)
        .map_err(|_| Box::new(failure("SAM readiness response was malformed")))?;
    let version = response
        .get("runtime_version")
        .and_then(serde_json::Value::as_str);
    let components = response
        .get("model_components")
        .and_then(serde_json::Value::as_array);
    let input_size = response
        .get("input_size")
        .and_then(serde_json::Value::as_u64);
    let max_prompts = response
        .get("max_prompts")
        .and_then(serde_json::Value::as_u64);
    if version.is_none_or(str::is_empty)
        || components.is_none_or(|items| items.len() != 3)
        || input_size != Some(1024)
        || max_prompts != Some(16)
    {
        return Err(Box::new(failure(
            "SAM readiness response did not match the admitted runtime contract",
        )));
    }
    Ok(RuntimeDependencyCheck {
        name: "coreml_sam_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Ready,
        resolved_path: None,
        canonical_target: None,
        version: Some(format!("coremltools {}", version.unwrap_or_default())),
        searched_paths: Vec::new(),
        message: None,
    })
}

fn worker_probe_failure(message: &str) -> RuntimeDependencyCheck {
    RuntimeDependencyCheck {
        name: "yamnet_worker".into(),
        requested: "admitted_build".into(),
        status: ProviderReadinessStatus::Unavailable,
        resolved_path: None,
        canonical_target: None,
        version: None,
        searched_paths: Vec::new(),
        message: Some(message.into()),
    }
}

fn resolve_executable(
    name: &str,
    requested: &str,
    probe_ffmpeg_version: bool,
    preserve_entry_path: bool,
) -> Result<(String, RuntimeDependencyCheck), Box<RuntimeDependencyCheck>> {
    let directories = executable_search_directories();
    let candidates = if Path::new(requested).components().count() > 1 {
        let requested = PathBuf::from(requested);
        vec![if requested.is_absolute() {
            requested
        } else {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join(requested)
        }]
    } else {
        directories
            .iter()
            .map(|directory| directory.join(requested))
            .collect()
    };
    for candidate in candidates {
        let Ok(resolved) = fs::canonicalize(&candidate) else {
            continue;
        };
        if !is_executable_file(&resolved) {
            continue;
        }
        let launch_path = if preserve_entry_path {
            candidate
        } else {
            resolved.clone()
        };
        let version = if probe_ffmpeg_version {
            match ffmpeg_version(&launch_path) {
                Ok(version) => Some(version),
                Err(message) => {
                    return Err(Box::new(RuntimeDependencyCheck {
                        name: name.into(),
                        requested: requested.into(),
                        status: ProviderReadinessStatus::Unavailable,
                        resolved_path: Some(launch_path.display().to_string()),
                        canonical_target: Some(resolved.display().to_string()),
                        version: None,
                        searched_paths: directories
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect(),
                        message: Some(message),
                    }));
                }
            }
        } else {
            None
        };
        let launch_path = launch_path.display().to_string();
        let canonical_target = resolved.display().to_string();
        return Ok((
            launch_path.clone(),
            RuntimeDependencyCheck {
                name: name.into(),
                requested: requested.into(),
                status: ProviderReadinessStatus::Ready,
                resolved_path: Some(launch_path),
                canonical_target: Some(canonical_target),
                version,
                searched_paths: directories
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
                message: None,
            },
        ));
    }
    Err(Box::new(RuntimeDependencyCheck {
        name: name.into(),
        requested: requested.into(),
        status: ProviderReadinessStatus::Unavailable,
        resolved_path: None,
        canonical_target: None,
        version: None,
        searched_paths: directories
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        message: Some(format!(
            "{name} executable `{requested}` was not found in inherited PATH, Homebrew, or system binary directories"
        )),
    }))
}

fn executable_search_directories() -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut directories = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path).filter(|path| path.is_absolute()) {
            if seen.insert(directory.clone()) {
                directories.push(directory);
            }
        }
    }
    for directory in EXTRA_EXECUTABLE_DIRECTORIES.map(PathBuf::from) {
        if seen.insert(directory.clone()) {
            directories.push(directory);
        }
    }
    directories
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn ffmpeg_version(path: &Path) -> Result<String, String> {
    let mut child = Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "ffmpeg version probe could not start".to_owned())?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < VERSION_PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ffmpeg version probe timed out".into());
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ffmpeg version probe failed".into());
            }
        }
    };
    if !status.success() {
        return Err("ffmpeg version probe returned a failure status".into());
    }
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_VERSION_OUTPUT_BYTES)
            .read_to_string(&mut output)
            .map_err(|_| "ffmpeg version output was unreadable".to_owned())?;
    }
    output
        .lines()
        .next()
        .filter(|line| line.starts_with("ffmpeg version "))
        .map(str::to_owned)
        .ok_or_else(|| "ffmpeg version probe returned an unexpected response".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_search_includes_homebrew_even_when_path_does_not() {
        let directories = executable_search_directories();
        assert!(directories.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(directories.contains(&PathBuf::from("/usr/local/bin")));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_non_executable_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("ffmpeg");
        fs::write(&candidate, "not executable").unwrap();
        let check =
            resolve_executable("ffmpeg", candidate.to_str().unwrap(), true, false).unwrap_err();
        assert_eq!(check.status, ProviderReadinessStatus::Unavailable);
        assert!(check.resolved_path.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn worker_command_preserves_a_virtual_environment_entry_path() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("python-target");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&target, permissions).unwrap();
        let entry = temp.path().join("venv-python");
        symlink(&target, &entry).unwrap();

        let (launch, check) =
            resolve_executable("command", entry.to_str().unwrap(), false, true).unwrap();
        assert_eq!(launch, entry.display().to_string());
        assert_eq!(check.resolved_path.as_deref(), Some(launch.as_str()));
        let canonical_target = fs::canonicalize(&target).unwrap().display().to_string();
        assert_eq!(
            check.canonical_target.as_deref(),
            Some(canonical_target.as_str())
        );
    }

    #[test]
    fn preflight_reports_a_missing_decoder_without_invalidating_other_providers() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        let yamnet = config.providers.get_mut("yamnet-local").unwrap();
        yamnet.command = Some("/usr/bin/true".into());
        yamnet.runtime_dependencies.ffmpeg = Some("definitely-missing-infer-ffmpeg".into());
        let readiness = preflight_provider_runtime(&config);
        let yamnet = readiness
            .iter()
            .find(|readiness| readiness.provider == "yamnet-local")
            .unwrap();
        assert_eq!(yamnet.status, ProviderReadinessStatus::Unavailable);
        let ffmpeg = yamnet
            .checks
            .iter()
            .find(|check| check.name == "ffmpeg")
            .unwrap();
        assert!(ffmpeg.searched_paths.contains(&"/opt/homebrew/bin".into()));
        assert!(
            readiness
                .iter()
                .any(|readiness| readiness.provider != "yamnet-local")
        );
    }
}
