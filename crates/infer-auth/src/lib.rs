//! Runtime-managed App credentials.
//!
//! This crate owns secret generation, at-rest file permissions, environment
//! resolution for externally managed consumers, and bearer-token matching.
//! Config and control-plane owners only see credential source declarations or
//! resolved App identities; plaintext tokens never enter config snapshots.

use std::{
    env, fs,
    fs::OpenOptions,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use infer_core::{AppCredentialConfig, RuntimeConfig};
use ring::{hmac, rand::SecureRandom};
use thiserror::Error;
use zeroize::Zeroizing;

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_LENGTH: usize = TOKEN_BYTES * 2;
const CONCURRENT_CREATE_RETRIES: usize = 5;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("{operation} `{path}`: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("managed credential path `{0}` must not be a symbolic link")]
    Symlink(PathBuf),
    #[error("managed credential path `{0}` is not a regular file")]
    NotAFile(PathBuf),
    #[error("managed credential file `{0}` must be readable only by its owner")]
    InsecurePermissions(PathBuf),
    #[error("managed credential file `{0}` does not contain one 256-bit hexadecimal token")]
    InvalidManagedToken(PathBuf),
    #[error("credential for App `{0}` is empty")]
    EmptyToken(String),
    #[error("the same bearer credential is assigned to more than one App")]
    DuplicateToken,
    #[error("App `{0}` is not configured or its credential is unavailable")]
    AppUnavailable(String),
    #[error("secure random generation failed")]
    RandomGeneration,
    #[error("App `{0}` is not a path-safe credential identity")]
    InvalidAppId(String),
    #[error("managed credential for App `{0}` already exists")]
    ManagedCredentialExists(String),
    #[error("managed credential for App `{0}` does not exist")]
    ManagedCredentialMissing(String),
}

/// Non-secret identity for a managed credential. The fingerprint is derived
/// from SHA-256 and is safe to display in operator surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCredentialSummary {
    pub fingerprint: String,
}

/// A newly provisioned secret. Callers must display it at most once and must
/// never persist it in config, logs, browser storage, or runtime metadata.
pub struct ProvisionedManagedCredential {
    token: Zeroizing<String>,
    pub fingerprint: String,
}

impl ProvisionedManagedCredential {
    pub fn expose_token(&self) -> &str {
        self.token.as_str()
    }
}

/// Owns explicit operator lifecycle operations for runtime-managed App
/// credentials. Mutation callers are responsible for serializing concurrent
/// updates with their App registry transaction.
#[derive(Debug, Clone)]
pub struct ManagedCredentialStore {
    directory: PathBuf,
}

impl ManagedCredentialStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn inspect(
        &self,
        app_id: &str,
    ) -> Result<Option<ManagedCredentialSummary>, CredentialError> {
        let path = self.path_for(app_id)?;
        match read_managed_token(&path) {
            Ok(token) => Ok(Some(ManagedCredentialSummary {
                fingerprint: token_fingerprint(&token),
            })),
            Err(CredentialError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn provision(&self, app_id: &str) -> Result<ProvisionedManagedCredential, CredentialError> {
        let path = self.path_for(app_id)?;
        prepare_directory(&self.directory)?;
        let token = create_new_managed_token(&path, app_id)?;
        Ok(provisioned(token))
    }

    pub fn rotate(&self, app_id: &str) -> Result<ProvisionedManagedCredential, CredentialError> {
        let path = self.path_for(app_id)?;
        prepare_directory(&self.directory)?;
        match read_managed_token(&path) {
            Ok(_) => {}
            Err(CredentialError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                return Err(CredentialError::ManagedCredentialMissing(app_id.to_owned()));
            }
            Err(error) => return Err(error),
        }
        let token = generate_token()?;
        replace_managed_token(&path, &token)?;
        Ok(provisioned(token))
    }

    pub fn remove(&self, app_id: &str) -> Result<bool, CredentialError> {
        let path = self.path_for(app_id)?;
        match read_managed_token(&path) {
            Ok(_) => fs::remove_file(&path)
                .map(|()| true)
                .map_err(|source| io_error("remove managed credential", &path, source)),
            Err(CredentialError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn path_for(&self, app_id: &str) -> Result<PathBuf, CredentialError> {
        if !is_path_safe_app_id(app_id) {
            return Err(CredentialError::InvalidAppId(app_id.to_owned()));
        }
        Ok(self.directory.join(format!("{app_id}.token")))
    }
}

struct Credential {
    app_id: String,
    token: Zeroizing<String>,
}

/// Resolved, in-memory bearer credentials. Debug output is intentionally
/// unavailable so secrets cannot be included accidentally in diagnostics.
pub struct AppCredentials {
    entries: Vec<Credential>,
}

impl AppCredentials {
    /// Resolves environment-backed consumers and creates any missing managed
    /// credential files. Missing environment variables leave that external App
    /// unavailable without preventing the local runtime from starting.
    pub fn load_or_create(config: &RuntimeConfig) -> Result<Self, CredentialError> {
        let directory = Path::new(&config.auth.managed_credentials_directory);
        let mut pairs = Vec::new();
        for (app_id, app) in &config.apps {
            let token = match &app.credential {
                AppCredentialConfig::Managed => {
                    Some(load_or_create_managed_token(directory, app_id)?)
                }
                AppCredentialConfig::Environment { variable } => env::var(variable)
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty()),
            };
            if let Some(token) = token {
                pairs.push((app_id.clone(), token));
            }
        }
        Self::from_pairs(pairs)
    }

    /// Constructs an explicit credential set for embedded runtimes and tests.
    pub fn from_pairs<I, A, T>(pairs: I) -> Result<Self, CredentialError>
    where
        I: IntoIterator<Item = (A, T)>,
        A: Into<String>,
        T: Into<String>,
    {
        let mut entries = Vec::new();
        for (app_id, token) in pairs {
            let app_id = app_id.into();
            let token = token.into();
            if token.trim().is_empty() {
                return Err(CredentialError::EmptyToken(app_id));
            }
            if entries
                .iter()
                .any(|entry: &Credential| entry.token.as_str() == token)
            {
                return Err(CredentialError::DuplicateToken);
            }
            entries.push(Credential {
                app_id,
                token: Zeroizing::new(token),
            });
        }
        Ok(Self { entries })
    }

    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn authenticate(&self, bearer_token: &str) -> Option<&str> {
        let presented_key = hmac::Key::new(hmac::HMAC_SHA256, bearer_token.as_bytes());
        self.entries.iter().find_map(|entry| {
            let expected_key = hmac::Key::new(hmac::HMAC_SHA256, entry.token.as_bytes());
            let expected_tag = hmac::sign(&expected_key, b"infer-runtime/app-credential/v1");
            hmac::verify(
                &presented_key,
                b"infer-runtime/app-credential/v1",
                expected_tag.as_ref(),
            )
            .is_ok()
            .then_some(entry.app_id.as_str())
        })
    }

    pub fn token_for(&self, app_id: &str) -> Result<&str, CredentialError> {
        self.entries
            .iter()
            .find(|entry| entry.app_id == app_id)
            .map(|entry| entry.token.as_str())
            .ok_or_else(|| CredentialError::AppUnavailable(app_id.to_owned()))
    }
}

fn load_or_create_managed_token(directory: &Path, app_id: &str) -> Result<String, CredentialError> {
    prepare_directory(directory)?;
    let path = directory.join(format!("{app_id}.token"));
    match read_managed_token(&path) {
        Ok(token) => Ok(token),
        Err(CredentialError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            create_managed_token(&path)
        }
        Err(error) => Err(error),
    }
}

fn prepare_directory(path: &Path) -> Result<(), CredentialError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(CredentialError::Symlink(path.to_owned()));
    }
    fs::create_dir_all(path)
        .map_err(|source| io_error("create credential directory", path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error("secure credential directory", path, source))?;
    }
    Ok(())
}

fn create_managed_token(path: &Path) -> Result<String, CredentialError> {
    let token = generate_token()?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.sync_all())
                .map_err(|source| io_error("write managed credential", path, source))?;
            Ok(token)
        }
        Err(source) if source.kind() == ErrorKind::AlreadyExists => {
            for _ in 0..CONCURRENT_CREATE_RETRIES {
                match read_managed_token(path) {
                    Ok(token) => return Ok(token),
                    Err(CredentialError::InvalidManagedToken(_)) => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => return Err(error),
                }
            }
            read_managed_token(path)
        }
        Err(source) => Err(io_error("create managed credential", path, source)),
    }
}

fn create_new_managed_token(path: &Path, app_id: &str) -> Result<String, CredentialError> {
    let token = generate_token()?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| {
        if source.kind() == ErrorKind::AlreadyExists {
            CredentialError::ManagedCredentialExists(app_id.to_owned())
        } else {
            io_error("create managed credential", path, source)
        }
    })?;
    let write_result = file
        .write_all(token.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all());
    if let Err(source) = write_result {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(io_error("write managed credential", path, source));
    }
    Ok(token)
}

fn replace_managed_token(path: &Path, token: &str) -> Result<(), CredentialError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("credential.token");
    let temporary = parent.join(format!(".{filename}.{}.tmp", random_suffix()?));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| {
        let mut file = options
            .open(&temporary)
            .map_err(|source| io_error("create replacement credential", &temporary, source))?;
        file.write_all(token.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|source| io_error("write replacement credential", &temporary, source))?;
        replace_file(&temporary, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(unix)]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), CredentialError> {
    fs::rename(temporary, path)
        .map_err(|source| io_error("replace managed credential", path, source))
}

#[cfg(not(unix))]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), CredentialError> {
    let backup = path.with_extension("token.rotation-backup");
    fs::rename(path, &backup)
        .map_err(|source| io_error("stage managed credential rotation", path, source))?;
    match fs::rename(temporary, path) {
        Ok(()) => {
            fs::remove_file(&backup)
                .map_err(|source| io_error("remove credential rotation backup", &backup, source))?;
            Ok(())
        }
        Err(source) => {
            let _ = fs::rename(&backup, path);
            Err(io_error("replace managed credential", path, source))
        }
    }
}

fn read_managed_token(path: &Path) -> Result<String, CredentialError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_error("inspect managed credential", path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(CredentialError::Symlink(path.to_owned()));
    }
    if !metadata.is_file() {
        return Err(CredentialError::NotAFile(path.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(CredentialError::InsecurePermissions(path.to_owned()));
        }
    }
    let mut source = String::new();
    fs::File::open(path)
        .and_then(|file| file.take(129).read_to_string(&mut source))
        .map_err(|error| io_error("read managed credential", path, error))?;
    let token = source.trim();
    if token.len() != TOKEN_HEX_LENGTH
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CredentialError::InvalidManagedToken(path.to_owned()));
    }
    Ok(token.to_owned())
}

fn generate_token() -> Result<String, CredentialError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| CredentialError::RandomGeneration)?;
    let mut token = String::with_capacity(TOKEN_HEX_LENGTH);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

fn random_suffix() -> Result<String, CredentialError> {
    let mut bytes = [0_u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| CredentialError::RandomGeneration)?;
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(suffix)
}

fn provisioned(token: String) -> ProvisionedManagedCredential {
    let fingerprint = token_fingerprint(&token);
    ProvisionedManagedCredential {
        token: Zeroizing::new(token),
        fingerprint,
    }
}

fn token_fingerprint(token: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut fingerprint = String::from("sha256:");
    for byte in &digest.as_ref()[..6] {
        use std::fmt::Write as _;
        write!(&mut fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    fingerprint
}

fn is_path_safe_app_id(app_id: &str) -> bool {
    !app_id.is_empty()
        && app_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> CredentialError {
    CredentialError::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use infer_core::{AppConfig, AppCredentialConfig, RuntimeConfig};
    use tempfile::tempdir;

    use super::{AppCredentials, CredentialError, ManagedCredentialStore};

    fn managed_config() -> (tempfile::TempDir, RuntimeConfig) {
        let temp = tempdir().unwrap();
        let mut config = RuntimeConfig::load(format!(
            "{}/../../config/infer.example.toml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        config.auth.managed_credentials_directory =
            temp.path().join("credentials").display().to_string();
        config.apps = BTreeMap::from([(
            "local-operator".into(),
            AppConfig {
                credential: AppCredentialConfig::Managed,
                ..config.apps["local-operator"].clone()
            },
        )]);
        (temp, config)
    }

    #[test]
    fn managed_token_is_generated_once_and_authenticates() {
        let (_temp, config) = managed_config();
        let first = AppCredentials::load_or_create(&config).unwrap();
        let token = first.token_for("local-operator").unwrap().to_owned();
        assert_eq!(token.len(), 64);
        assert_eq!(first.authenticate(&token), Some("local-operator"));

        let second = AppCredentials::load_or_create(&config).unwrap();
        assert_eq!(second.token_for("local-operator").unwrap(), token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = std::path::Path::new(&config.auth.managed_credentials_directory)
                .join("local-operator.token");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn duplicate_and_empty_credentials_fail_closed() {
        assert!(matches!(
            AppCredentials::from_pairs([("a", "same"), ("b", "same")]),
            Err(CredentialError::DuplicateToken)
        ));
        assert!(matches!(
            AppCredentials::from_pairs([("a", "")]),
            Err(CredentialError::EmptyToken(_))
        ));
    }

    #[test]
    fn environment_consumer_is_resolved_without_becoming_an_operator() {
        let (_temp, mut config) = managed_config();
        let mut consumer = config.apps["local-operator"].clone();
        consumer.credential = AppCredentialConfig::Environment {
            variable: "PATH".into(),
        };
        consumer.resource_admin = false;
        config.apps = BTreeMap::from([("sample-consumer".into(), consumer)]);

        let credentials = AppCredentials::load_or_create(&config).unwrap();
        let token = std::env::var("PATH").unwrap();
        assert_eq!(credentials.authenticate(&token), Some("sample-consumer"));
        assert!(!config.apps["sample-consumer"].resource_admin);
    }

    #[cfg(unix)]
    #[test]
    fn permissive_managed_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, config) = managed_config();
        let credentials = AppCredentials::load_or_create(&config).unwrap();
        drop(credentials);
        let path = std::path::Path::new(&config.auth.managed_credentials_directory)
            .join("local-operator.token");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            AppCredentials::load_or_create(&config),
            Err(CredentialError::InsecurePermissions(actual)) if actual == path
        ));
    }

    #[cfg(unix)]
    #[test]
    fn managed_token_symlink_is_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (temp, config) = managed_config();
        let directory = std::path::Path::new(&config.auth.managed_credentials_directory);
        fs::create_dir_all(directory).unwrap();
        let target = temp.path().join("target.token");
        fs::write(
            &target,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n",
        )
        .unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.join("local-operator.token");
        symlink(target, &link).unwrap();
        assert!(matches!(
            AppCredentials::load_or_create(&config),
            Err(CredentialError::Symlink(actual)) if actual == link
        ));
    }

    #[test]
    fn explicit_managed_lifecycle_never_reveals_an_existing_token() {
        let temp = tempdir().unwrap();
        let store = ManagedCredentialStore::new(temp.path().join("credentials"));

        let created = store.provision("sample-consumer").unwrap();
        let first = created.expose_token().to_owned();
        assert_eq!(first.len(), 64);
        assert_eq!(
            store
                .inspect("sample-consumer")
                .unwrap()
                .unwrap()
                .fingerprint,
            created.fingerprint
        );
        assert!(matches!(
            store.provision("sample-consumer"),
            Err(CredentialError::ManagedCredentialExists(app)) if app == "sample-consumer"
        ));

        let rotated = store.rotate("sample-consumer").unwrap();
        assert_ne!(rotated.expose_token(), first);
        assert_ne!(rotated.fingerprint, created.fingerprint);
        assert!(store.remove("sample-consumer").unwrap());
        assert!(store.inspect("sample-consumer").unwrap().is_none());
        assert!(!store.remove("sample-consumer").unwrap());
    }

    #[test]
    fn explicit_managed_lifecycle_rejects_path_traversal() {
        let temp = tempdir().unwrap();
        let store = ManagedCredentialStore::new(temp.path().join("credentials"));
        assert!(matches!(
            store.provision("../consumer"),
            Err(CredentialError::InvalidAppId(app)) if app == "../consumer"
        ));
    }
}
