use std::{
    collections::BTreeSet,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{STATUS_PROTOCOL, STATUS_PROTOCOL_VERSION};

pub const DISCOVERY_SCHEMA: &str = "infra.discovery.registration";
pub const DISCOVERY_SCHEMA_VERSION: &str = "20260812.1";
pub const UNIX_SOCKET_BINDING: &str = "infra.local.unix-socket";
pub const UNIX_SOCKET_OPAQUE_MAX_BYTES: usize = 16;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 103;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("Infra Discovery runtime root is unavailable on this platform")]
    RuntimeRootUnavailable,
    #[error("Infra Discovery runtime root `{0}` must be absolute")]
    RelativeRuntimeRoot(PathBuf),
    #[error("discovery path component `{0}` is invalid")]
    InvalidComponent(String),
    #[error("discovery protocol offer is invalid: {0}")]
    InvalidOffer(String),
    #[error("discovery directory `{0}` must not be a symbolic link")]
    SymlinkDirectory(PathBuf),
    #[error("discovery directory `{0}` is not a directory")]
    NotDirectory(PathBuf),
    #[error("discovery path `{path}` is owned by uid {actual}; expected uid {expected}")]
    WrongOwner {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
    #[error("discovery directory `{path}` has mode {actual:o}; expected 700")]
    UnsafeDirectoryMode { path: PathBuf, actual: u32 },
    #[error("registration manifest `{0}` must be a regular non-symlink file")]
    UnsafeManifest(PathBuf),
    #[error("registration manifest `{0}` exceeds 64 KiB")]
    ManifestTooLarge(PathBuf),
    #[error("registration manifest `{0}` is invalid: {1}")]
    InvalidManifest(PathBuf, String),
    #[error("publication authority for `{0}` is already held")]
    PublicationAuthorityHeld(String),
    #[error(
        "discovery publication authority or manifest changed at `{0}`; restart the owning daemon"
    )]
    PublicationChanged(PathBuf),
    #[error("Unix socket endpoint `{0}` is invalid")]
    InvalidSocketEndpoint(String),
    #[error("Unix socket path `{path}` is {actual} bytes; maximum is {maximum}")]
    SocketPathTooLong {
        path: PathBuf,
        actual: usize,
        maximum: usize,
    },
    #[error("discovery I/O failed for `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("discovery JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryService {
    pub kind: String,
    pub instance_id: String,
    pub generation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryOffer {
    pub protocol: String,
    pub protocol_versions: Vec<String>,
    pub binding: String,
    pub endpoint: String,
}

impl DiscoveryOffer {
    pub fn infer_status_unix(endpoint: impl Into<String>) -> Self {
        Self {
            protocol: STATUS_PROTOCOL.into(),
            protocol_versions: vec![STATUS_PROTOCOL_VERSION.into()],
            binding: UNIX_SOCKET_BINDING.into(),
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRegistration {
    pub schema: String,
    pub schema_version: String,
    pub service: DiscoveryService,
    pub offers: Vec<DiscoveryOffer>,
}

#[derive(Debug, Clone)]
pub struct DiscoveryRuntime {
    root: PathBuf,
    registrations: PathBuf,
    sockets: PathBuf,
}

impl DiscoveryRuntime {
    pub fn from_environment() -> Result<Self, DiscoveryError> {
        let root = match env::var_os("INFRA_PROTOCOL_RUNTIME_DIR") {
            Some(root) => PathBuf::from(root),
            None => platform_runtime_root()?,
        };
        Self::prepare(root)
    }

    pub fn prepare(root: PathBuf) -> Result<Self, DiscoveryError> {
        if !root.is_absolute() {
            return Err(DiscoveryError::RelativeRuntimeRoot(root));
        }
        prepare_owned_directory(&root)?;
        ensure_local_filesystem(&root)?;
        let registrations = root.join("registrations");
        let sockets = root.join("sockets");
        prepare_owned_directory(&registrations)?;
        prepare_owned_directory(&sockets)?;
        Ok(Self {
            root,
            registrations,
            sockets,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registrations(&self) -> &Path {
        &self.registrations
    }

    pub fn sockets(&self) -> &Path {
        &self.sockets
    }

    pub fn resolve_unix_socket(&self, endpoint: &str) -> Result<PathBuf, DiscoveryError> {
        let opaque = endpoint
            .strip_prefix("sockets/")
            .and_then(|name| name.strip_suffix(".sock"))
            .filter(|name| valid_file_token(name, UNIX_SOCKET_OPAQUE_MAX_BYTES))
            .ok_or_else(|| DiscoveryError::InvalidSocketEndpoint(endpoint.into()))?;
        let path = self.sockets.join(format!("{opaque}.sock"));
        validate_socket_path_length(&path)?;
        Ok(path)
    }
}

#[derive(Debug, Clone)]
pub struct RegistrationSpec {
    pub runtime: DiscoveryRuntime,
    pub service: DiscoveryService,
    pub offers: Vec<DiscoveryOffer>,
}

pub struct RegistrationPublication {
    path: PathBuf,
    authority: PublicationAuthority,
    spec: RegistrationSpec,
}

impl RegistrationPublication {
    /// Publishes one immutable declaration for this process generation.
    ///
    /// The caller must bind every advertised endpoint before calling this
    /// function and retain the returned publication authority for as long as
    /// it may serve the stable service identity. The stable manifest is left
    /// in place when this value is dropped; a successor atomically replaces it.
    pub fn publish(spec: RegistrationSpec) -> Result<Self, DiscoveryError> {
        validate_spec(&spec)?;
        let path = spec.runtime.registrations.join(format!(
            "{}--{}.json",
            spec.service.kind, spec.service.instance_id
        ));
        let authority = PublicationAuthority::acquire(&spec.runtime.registrations, &spec.service)?;
        write_manifest(&path, &spec)?;
        Ok(Self {
            path,
            authority,
            spec,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Restore a removed manifest without changing this generation or overwriting
    /// another publisher. An intact declaration is checked but never rewritten.
    pub fn ensure_published(&self) -> Result<bool, DiscoveryError> {
        for directory in [&self.spec.runtime.root, &self.spec.runtime.registrations] {
            validate_owned_directory(directory)?;
        }
        self.authority.ensure_current()?;
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {
                validate_manifest_file(&self.path)?;
                let bytes = fs::read(&self.path).map_err(|source| DiscoveryError::Io {
                    path: self.path.clone(),
                    source,
                })?;
                let current: DiscoveryRegistration =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        DiscoveryError::InvalidManifest(self.path.clone(), error.to_string())
                    })?;
                let expected = manifest(&self.spec);
                if current.service != expected.service
                    || current.offers != expected.offers
                    || current.schema != expected.schema
                    || current.schema_version != expected.schema_version
                {
                    return Err(DiscoveryError::PublicationChanged(self.path.clone()));
                }
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_manifest_inner(&self.path, &self.spec, false)
            }
            Err(source) => Err(DiscoveryError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Releases publication authority without changing the stable manifest.
    pub fn shutdown(self) {}
}

struct PublicationAuthority {
    path: PathBuf,
    #[cfg(unix)]
    file: fs::File,
}

impl PublicationAuthority {
    fn ensure_current(&self) -> Result<(), DiscoveryError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let held = self.file.metadata().map_err(|source| DiscoveryError::Io {
                path: self.path.clone(),
                source,
            })?;
            let current =
                fs::symlink_metadata(&self.path).map_err(|source| DiscoveryError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            if !current.is_file()
                || current.file_type().is_symlink()
                || current.dev() != held.dev()
                || current.ino() != held.ino()
                || current.uid() != held.uid()
                || current.permissions().mode() & 0o777 != 0o600
            {
                return Err(DiscoveryError::PublicationChanged(self.path.clone()));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err(DiscoveryError::RuntimeRootUnavailable)
    }

    fn acquire(root: &Path, service: &DiscoveryService) -> Result<Self, DiscoveryError> {
        let path = root.join(format!(
            ".{}--{}.publisher.lock",
            service.kind, service.instance_id
        ));
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

            let mut options = fs::OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let file = options.open(&path).map_err(|source| DiscoveryError::Io {
                path: path.clone(),
                source,
            })?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|source| {
                DiscoveryError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
            // SAFETY: flock operates on this live file descriptor and does not
            // dereference application memory.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error
                    .raw_os_error()
                    .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
                {
                    return Err(DiscoveryError::PublicationAuthorityHeld(format!(
                        "{}--{}",
                        service.kind, service.instance_id
                    )));
                }
                return Err(DiscoveryError::Io {
                    path,
                    source: error,
                });
            }
            Ok(Self { file, path })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, service);
            Err(DiscoveryError::RuntimeRootUnavailable)
        }
    }
}

#[cfg(unix)]
impl Drop for PublicationAuthority {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: flock operates on this live file descriptor.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn unique_status_socket_endpoint(generation: &str) -> Result<String, DiscoveryError> {
    if !valid_file_token(generation, 96) {
        return Err(DiscoveryError::InvalidComponent(generation.into()));
    }
    // Darwin's sockaddr_un path is short. A 48-bit random process suffix keeps
    // the endpoint below that limit even under the canonical user temp root.
    let random = Uuid::new_v4().simple().to_string();
    Ok(format!("sockets/ir-{}.sock", &random[..12]))
}

fn validate_spec(spec: &RegistrationSpec) -> Result<(), DiscoveryError> {
    if !valid_service_kind(&spec.service.kind) {
        return Err(DiscoveryError::InvalidComponent(spec.service.kind.clone()));
    }
    for value in [&spec.service.instance_id, &spec.service.generation] {
        if !valid_file_token(value, 96) {
            return Err(DiscoveryError::InvalidComponent(value.clone()));
        }
    }
    if spec.offers.is_empty() || spec.offers.len() > 64 {
        return Err(DiscoveryError::InvalidOffer(
            "offers must contain between 1 and 64 entries".into(),
        ));
    }
    for offer in &spec.offers {
        validate_offer(offer)?;
        if offer.binding == UNIX_SOCKET_BINDING {
            spec.runtime.resolve_unix_socket(&offer.endpoint)?;
        }
    }
    Ok(())
}

fn validate_offer(offer: &DiscoveryOffer) -> Result<(), DiscoveryError> {
    if !valid_contract_id(&offer.protocol)
        || !valid_contract_id(&offer.binding)
        || offer.protocol_versions.is_empty()
        || offer.protocol_versions.len() > 16
        || offer
            .protocol_versions
            .iter()
            .any(|version| !valid_contract_version(version))
        || offer
            .protocol_versions
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != offer.protocol_versions.len()
    {
        return Err(DiscoveryError::InvalidOffer(offer.protocol.clone()));
    }
    if offer.binding == UNIX_SOCKET_BINDING {
        let opaque = offer
            .endpoint
            .strip_prefix("sockets/")
            .and_then(|name| name.strip_suffix(".sock"));
        if opaque.is_none_or(|name| !valid_file_token(name, UNIX_SOCKET_OPAQUE_MAX_BYTES)) {
            return Err(DiscoveryError::InvalidSocketEndpoint(
                offer.endpoint.clone(),
            ));
        }
    }
    Ok(())
}

fn valid_service_kind(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 || !value.as_bytes()[0].is_ascii_lowercase() {
        return false;
    }
    let mut previous_separator = false;
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_separator = false,
            b'.' | b'-' if !previous_separator => previous_separator = true,
            _ => return false,
        }
    }
    !previous_separator
}

fn valid_file_token(value: &str, maximum: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= maximum
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_contract_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'+' | b'/' | b'@' | b'%' | b'-')
        })
}

fn valid_contract_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'+' | b'-')
        })
}

fn manifest(spec: &RegistrationSpec) -> DiscoveryRegistration {
    DiscoveryRegistration {
        schema: DISCOVERY_SCHEMA.into(),
        schema_version: DISCOVERY_SCHEMA_VERSION.into(),
        service: spec.service.clone(),
        offers: spec.offers.clone(),
    }
}

fn write_manifest(path: &Path, spec: &RegistrationSpec) -> Result<(), DiscoveryError> {
    write_manifest_inner(path, spec, true).map(|_| ())
}

fn write_manifest_inner(
    path: &Path,
    spec: &RegistrationSpec,
    replace: bool,
) -> Result<bool, DiscoveryError> {
    let mut bytes = serde_json::to_vec_pretty(&manifest(spec))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(DiscoveryError::ManifestTooLarge(path.to_owned()));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated registration path has a UTF-8 filename");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options
            .open(&temporary)
            .map_err(|source| DiscoveryError::Io {
                path: temporary.clone(),
                source,
            })?;
        write_sync_and_close(file, &bytes, |file| file.sync_all()).map_err(|source| {
            DiscoveryError::Io {
                path: temporary.clone(),
                source,
            }
        })?;
        if replace {
            fs::rename(&temporary, path).map_err(|source| DiscoveryError::Io {
                path: path.to_owned(),
                source,
            })?;
        } else {
            // Publish the complete file only if the stable name is still absent.
            // A concurrent declaration must never be overwritten by repair.
            match fs::hard_link(&temporary, path) {
                Ok(()) => {
                    fs::remove_file(&temporary).map_err(|source| DiscoveryError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temporary);
                    return Ok(false);
                }
                Err(source) => {
                    return Err(DiscoveryError::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
        }
        validate_manifest_file(path)?;
        if let Some(directory) = path.parent()
            && let Ok(directory) = fs::File::open(directory)
        {
            let _ = directory.sync_all();
        }
        Ok(true)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_sync_and_close<W>(
    mut writer: W,
    bytes: &[u8],
    sync: impl FnOnce(&mut W) -> std::io::Result<()>,
) -> std::io::Result<()>
where
    W: Write,
{
    writer.write_all(bytes)?;
    sync(&mut writer)?;
    drop(writer);
    Ok(())
}

fn validate_manifest_file(path: &Path) -> Result<(), DiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| DiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DiscoveryError::UnsafeManifest(path.to_owned()));
    }
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(DiscoveryError::ManifestTooLarge(path.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid {
            return Err(DiscoveryError::WrongOwner {
                path: path.to_owned(),
                actual: metadata.uid(),
                expected: expected_uid,
            });
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(DiscoveryError::UnsafeManifest(path.to_owned()));
        }
    }
    Ok(())
}

#[cfg(test)]
fn read_manifest(path: &Path) -> Result<Option<DiscoveryRegistration>, DiscoveryError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DiscoveryError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DiscoveryError::UnsafeManifest(path.to_owned()));
    }
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(DiscoveryError::ManifestTooLarge(path.to_owned()));
    }
    let bytes = fs::read(path).map_err(|source| DiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| DiscoveryError::InvalidManifest(path.to_owned(), error.to_string()))
}

fn validate_owned_directory(directory: &Path) -> Result<(), DiscoveryError> {
    let metadata = fs::symlink_metadata(directory).map_err(|source| DiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DiscoveryError::SymlinkDirectory(directory.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(DiscoveryError::NotDirectory(directory.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let expected = unsafe { libc::geteuid() };
        if metadata.uid() != expected {
            return Err(DiscoveryError::WrongOwner {
                path: directory.to_owned(),
                actual: metadata.uid(),
                expected,
            });
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(DiscoveryError::UnsafeDirectoryMode {
                path: directory.to_owned(),
                actual: metadata.permissions().mode() & 0o777,
            });
        }
    }
    Ok(())
}

fn prepare_owned_directory(directory: &Path) -> Result<(), DiscoveryError> {
    fs::create_dir_all(directory).map_err(|source| DiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })?;
    let metadata = fs::symlink_metadata(directory).map_err(|source| DiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DiscoveryError::SymlinkDirectory(directory.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(DiscoveryError::NotDirectory(directory.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid {
            return Err(DiscoveryError::WrongOwner {
                path: directory.to_owned(),
                actual: metadata.uid(),
                expected: expected_uid,
            });
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|source| {
            DiscoveryError::Io {
                path: directory.to_owned(),
                source,
            }
        })?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_runtime_root() -> Result<PathBuf, DiscoveryError> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    // SAFETY: confstr with a null buffer reports the required size.
    let length = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if length == 0 {
        return Err(DiscoveryError::RuntimeRootUnavailable);
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: the buffer is allocated to the exact size reported by confstr.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if written == 0 || written > buffer.len() {
        return Err(DiscoveryError::RuntimeRootUnavailable);
    }
    buffer.truncate(written.saturating_sub(1));
    Ok(PathBuf::from(OsString::from_vec(buffer)).join("infra-protocol"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_runtime_root() -> Result<PathBuf, DiscoveryError> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(DiscoveryError::RuntimeRootUnavailable)?;
    if !base.is_absolute() {
        return Err(DiscoveryError::RuntimeRootUnavailable);
    }
    validate_login_runtime_directory(&base)?;
    Ok(base.join("infra-protocol"))
}

#[cfg(not(unix))]
fn platform_runtime_root() -> Result<PathBuf, DiscoveryError> {
    Err(DiscoveryError::RuntimeRootUnavailable)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn validate_login_runtime_directory(path: &Path) -> Result<(), DiscoveryError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path).map_err(|source| DiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DiscoveryError::SymlinkDirectory(path.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(DiscoveryError::NotDirectory(path.to_owned()));
    }
    // SAFETY: geteuid has no preconditions.
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid {
        return Err(DiscoveryError::WrongOwner {
            path: path.to_owned(),
            actual: metadata.uid(),
            expected: expected_uid,
        });
    }
    let actual = metadata.permissions().mode() & 0o777;
    if actual != 0o700 {
        return Err(DiscoveryError::UnsafeDirectoryMode {
            path: path.to_owned(),
            actual,
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_local_filesystem(path: &Path) -> Result<(), DiscoveryError> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DiscoveryError::InvalidComponent(path.display().to_string()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stats points to writable storage.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(DiscoveryError::Io {
            path: PathBuf::from(path.to_string_lossy().as_ref()),
            source: std::io::Error::last_os_error(),
        });
    }
    // SAFETY: statfs initialized the structure on success.
    let stats = unsafe { stats.assume_init() };
    if stats.f_flags as i32 & libc::MNT_LOCAL == 0 {
        return Err(DiscoveryError::RuntimeRootUnavailable);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_local_filesystem(_path: &Path) -> Result<(), DiscoveryError> {
    Ok(())
}

#[cfg(unix)]
fn validate_socket_path_length(path: &Path) -> Result<(), DiscoveryError> {
    use std::os::unix::ffi::OsStrExt;
    let actual = path.as_os_str().as_bytes().len();
    if actual > UNIX_SOCKET_PATH_MAX_BYTES {
        return Err(DiscoveryError::SocketPathTooLong {
            path: path.to_owned(),
            actual,
            maximum: UNIX_SOCKET_PATH_MAX_BYTES,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_socket_path_length(path: &Path) -> Result<(), DiscoveryError> {
    Err(DiscoveryError::SocketPathTooLong {
        path: path.to_owned(),
        actual: 0,
        maximum: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;

    fn spec(runtime: DiscoveryRuntime, generation: &str) -> RegistrationSpec {
        let endpoint = unique_status_socket_endpoint(generation).unwrap();
        RegistrationSpec {
            runtime,
            service: DiscoveryService {
                kind: "infer-runtime".into(),
                instance_id: "local".into(),
                generation: generation.into(),
            },
            offers: vec![DiscoveryOffer::infer_status_unix(endpoint)],
        }
    }

    #[test]
    #[cfg(unix)]
    fn missing_manifest_is_restored_without_rewriting_healthy_publication() {
        use std::os::unix::fs::MetadataExt;
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let publication = RegistrationPublication::publish(spec(runtime, "gen_first")).unwrap();
        let before = fs::read(publication.path()).unwrap();
        let original = fs::metadata(publication.path()).unwrap();
        assert!(!publication.ensure_published().unwrap());
        let unchanged = fs::metadata(publication.path()).unwrap();
        assert_eq!(original.ino(), unchanged.ino());
        assert_eq!(original.modified().unwrap(), unchanged.modified().unwrap());
        fs::remove_file(publication.path()).unwrap();
        assert!(publication.ensure_published().unwrap());
        assert_eq!(fs::read(publication.path()).unwrap(), before);
        validate_manifest_file(publication.path()).unwrap();
        assert!(!publication.ensure_published().unwrap());
    }

    #[test]
    fn replaced_publisher_lock_cannot_republish_an_old_generation() {
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let publication =
            RegistrationPublication::publish(spec(runtime.clone(), "gen_first")).unwrap();
        fs::remove_file(&publication.authority.path).unwrap();
        let successor = RegistrationPublication::publish(spec(runtime, "gen_second")).unwrap();
        fs::remove_file(successor.path()).unwrap();
        assert!(matches!(
            publication.ensure_published(),
            Err(DiscoveryError::PublicationChanged(_))
        ));
        assert!(!publication.path().exists());
        assert!(successor.ensure_published().unwrap());
        assert_eq!(
            read_manifest(successor.path())
                .unwrap()
                .unwrap()
                .service
                .generation,
            "gen_second"
        );
    }

    #[test]
    fn repair_preserves_foreign_or_concurrently_published_manifest() {
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let publication =
            RegistrationPublication::publish(spec(runtime.clone(), "gen_first")).unwrap();
        let replacement = spec(runtime, "gen_second");
        write_manifest(publication.path(), &replacement).unwrap();
        let before = fs::read(publication.path()).unwrap();
        assert!(matches!(
            publication.ensure_published(),
            Err(DiscoveryError::PublicationChanged(_))
        ));
        assert!(!write_manifest_inner(publication.path(), &publication.spec, false).unwrap());
        assert_eq!(fs::read(publication.path()).unwrap(), before);
    }

    #[test]
    #[cfg(unix)]
    fn repair_rejects_missing_authority_unsafe_directory_and_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let publication =
            RegistrationPublication::publish(spec(runtime.clone(), "gen_first")).unwrap();
        fs::remove_file(publication.path()).unwrap();
        fs::set_permissions(runtime.registrations(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            publication.ensure_published(),
            Err(DiscoveryError::UnsafeDirectoryMode { .. })
        ));
        assert!(!publication.path().exists());
        fs::set_permissions(runtime.registrations(), fs::Permissions::from_mode(0o700)).unwrap();
        let unrelated = temporary.path().join("unrelated");
        fs::write(&unrelated, "retain").unwrap();
        symlink(&unrelated, publication.path()).unwrap();
        assert!(matches!(
            publication.ensure_published(),
            Err(DiscoveryError::UnsafeManifest(_))
        ));
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "retain");
        fs::remove_file(publication.path()).unwrap();
        fs::remove_file(&publication.authority.path).unwrap();
        assert!(publication.ensure_published().is_err());
        assert!(!publication.path().exists());
    }

    #[test]
    fn temporary_writer_is_closed_before_atomic_replace_step() {
        struct LifecycleWriter {
            events: Arc<Mutex<Vec<&'static str>>>,
        }

        impl Write for LifecycleWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.events.lock().unwrap().push("write");
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl Drop for LifecycleWriter {
            fn drop(&mut self) {
                self.events.lock().unwrap().push("close");
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let writer = LifecycleWriter {
            events: Arc::clone(&events),
        };
        write_sync_and_close(writer, b"manifest", |writer| {
            writer.events.lock().unwrap().push("sync");
            Ok(())
        })
        .unwrap();
        events.lock().unwrap().push("replace");

        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["write", "sync", "close", "replace"]
        );
    }

    #[test]
    fn manifest_is_canonical_atomic_owner_only_published_once_and_retained() {
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("infra-protocol")).unwrap();
        let publication = RegistrationPublication::publish(spec(runtime, "gen_first")).unwrap();
        let path = publication.path().to_owned();
        assert_eq!(path.file_name().unwrap(), "infer-runtime--local.json");
        let initial_bytes = fs::read(&path).unwrap();
        std::thread::sleep(Duration::from_millis(55));
        let manifest = read_manifest(&path).unwrap().unwrap();
        assert_eq!(manifest.schema, DISCOVERY_SCHEMA);
        assert_eq!(manifest.schema_version, DISCOVERY_SCHEMA_VERSION);
        assert_eq!(manifest.service.generation, "gen_first");
        assert_eq!(manifest.offers.len(), 1);
        assert_eq!(manifest.offers[0].protocol, STATUS_PROTOCOL);
        assert_eq!(manifest.offers[0].binding, UNIX_SOCKET_BINDING);
        assert!(manifest.offers[0].endpoint.starts_with("sockets/"));
        assert!(!Path::new(&manifest.offers[0].endpoint).is_absolute());
        assert_eq!(fs::read(&path).unwrap(), initial_bytes);
        let encoded = serde_json::to_value(&manifest).unwrap();
        for legacy in [
            "lease", "process", "observer", "links", "security", "sequence",
        ] {
            assert!(
                encoded.get(legacy).is_none(),
                "legacy field {legacy} leaked"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(spec_runtime_root(&path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(spec_runtime_root(&path).join("sockets"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        publication.shutdown();
        assert!(
            path.exists(),
            "stable manifest must be retained for atomic successor replacement"
        );
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn exclusive_publisher_handoff_replaces_stable_manifest_only_after_release() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("infra-protocol");
        let first = RegistrationPublication::publish(spec(
            DiscoveryRuntime::prepare(root.clone()).unwrap(),
            "gen_first",
        ))
        .unwrap();
        let denied = RegistrationPublication::publish(spec(
            DiscoveryRuntime::prepare(root.clone()).unwrap(),
            "gen_second",
        ));
        assert!(matches!(
            denied,
            Err(DiscoveryError::PublicationAuthorityHeld(_))
        ));

        let path = first.path().to_owned();
        first.shutdown();
        let second = RegistrationPublication::publish(spec(
            DiscoveryRuntime::prepare(root).unwrap(),
            "gen_second",
        ))
        .unwrap();
        assert_eq!(
            read_manifest(&path).unwrap().unwrap().service.generation,
            "gen_second"
        );
        second.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_layout_rejects_symlinks_and_long_socket_paths() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let actual = temporary.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let link = temporary.path().join("infra-protocol");
        symlink(&actual, &link).unwrap();
        assert!(matches!(
            DiscoveryRuntime::prepare(link),
            Err(DiscoveryError::SymlinkDirectory(_))
        ));

        let long_root = temporary.path().join("a".repeat(100));
        let runtime = DiscoveryRuntime::prepare(long_root).unwrap();
        assert!(matches!(
            runtime.resolve_unix_socket("sockets/ir-0123456789.sock"),
            Err(DiscoveryError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn unix_socket_opaque_name_enforces_the_canonical_sixteen_byte_limit() {
        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("root")).unwrap();
        let accepted = format!("sockets/{}.sock", "a".repeat(UNIX_SOCKET_OPAQUE_MAX_BYTES));
        let rejected = format!(
            "sockets/{}.sock",
            "a".repeat(UNIX_SOCKET_OPAQUE_MAX_BYTES + 1)
        );

        assert!(runtime.resolve_unix_socket(&accepted).is_ok());
        assert!(validate_offer(&DiscoveryOffer::infer_status_unix(accepted)).is_ok());
        assert!(matches!(
            runtime.resolve_unix_socket(&rejected),
            Err(DiscoveryError::InvalidSocketEndpoint(_))
        ));
        assert!(matches!(
            validate_offer(&DiscoveryOffer::infer_status_unix(rejected)),
            Err(DiscoveryError::InvalidSocketEndpoint(_))
        ));

        for _ in 0..32 {
            let endpoint = unique_status_socket_endpoint("gen_test").unwrap();
            let opaque = endpoint
                .strip_prefix("sockets/")
                .and_then(|name| name.strip_suffix(".sock"))
                .unwrap();
            assert!(valid_file_token(opaque, UNIX_SOCKET_OPAQUE_MAX_BYTES));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_path_capacity_includes_space_for_the_terminating_nul() {
        use std::os::unix::ffi::OsStrExt;

        let at_limit = PathBuf::from(format!("/{}", "a".repeat(UNIX_SOCKET_PATH_MAX_BYTES - 1)));
        assert_eq!(
            at_limit.as_os_str().as_bytes().len(),
            UNIX_SOCKET_PATH_MAX_BYTES
        );
        validate_socket_path_length(&at_limit).unwrap();

        let over_limit = PathBuf::from(format!("/{}", "a".repeat(UNIX_SOCKET_PATH_MAX_BYTES)));
        assert!(matches!(
            validate_socket_path_length(&over_limit),
            Err(DiscoveryError::SocketPathTooLong {
                actual,
                maximum,
                ..
            }) if actual == UNIX_SOCKET_PATH_MAX_BYTES + 1
                && maximum == UNIX_SOCKET_PATH_MAX_BYTES
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn canonical_macos_runtime_root_fits_a_generated_socket_endpoint() {
        use std::os::unix::ffi::OsStrExt;

        let root = platform_runtime_root().unwrap();
        let runtime = DiscoveryRuntime {
            registrations: root.join("registrations"),
            sockets: root.join("sockets"),
            root,
        };
        let endpoint = unique_status_socket_endpoint("gen_test").unwrap();
        let path = runtime.resolve_unix_socket(&endpoint).unwrap();

        assert!(path.starts_with(runtime.sockets()));
        assert!(path.as_os_str().as_bytes().len() < UNIX_SOCKET_PATH_MAX_BYTES);
    }

    #[test]
    fn registration_rejects_unknown_fields_and_invalid_relative_endpoint() {
        let json = r#"{
            "schema":"infra.discovery.registration",
            "schema_version":"20260812.1",
            "service":{"kind":"infer-runtime","instance_id":"local","generation":"gen_a"},
            "lease":{"renewed_at":"2026-08-10T00:00:00Z","expires_at":"2026-08-10T00:00:45Z"},
            "offers":[{
                "protocol":"infer-runtime.status",
                "protocol_versions":["20260810.1"],
                "binding":"infra.local.unix-socket",
                "endpoint":"sockets/ir-test.sock"
            }]
        }"#;
        assert!(serde_json::from_str::<DiscoveryRegistration>(json).is_err());

        let temporary = tempdir().unwrap();
        let runtime = DiscoveryRuntime::prepare(temporary.path().join("root")).unwrap();
        assert!(matches!(
            runtime.resolve_unix_socket("../outside.sock"),
            Err(DiscoveryError::InvalidSocketEndpoint(_))
        ));
    }

    fn spec_runtime_root(manifest_path: &Path) -> PathBuf {
        manifest_path
            .parent()
            .and_then(Path::parent)
            .expect("manifest is under <root>/registrations")
            .to_owned()
    }
}
