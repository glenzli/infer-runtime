use std::{
    collections::BTreeSet,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{
    Error, Result,
    contract::{CONSUMER_CORE_PROTOCOL, SUPPORTED_CONSUMER_CORE_VERSIONS},
};

pub const CONSUMER_HTTP_LOOPBACK_BINDING: &str = "infer-runtime.http-loopback";
const DISCOVERY_SCHEMA: &str = "infra.discovery.registration";
const DISCOVERY_SCHEMA_VERSION: &str = "20260812.1";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    pub endpoint: String,
    pub instance_id: String,
    pub generation: String,
    pub core_version: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveryResolver {
    runtime_root: Option<PathBuf>,
    instance_id: String,
    explicit_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    schema: String,
    schema_version: String,
    service: Service,
    offers: Vec<Offer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    kind: String,
    instance_id: String,
    generation: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    protocol: String,
    protocol_versions: Vec<String>,
    binding: String,
    endpoint: String,
}

impl DiscoveryResolver {
    pub fn local() -> Self {
        Self {
            runtime_root: None,
            instance_id: "local".into(),
            explicit_endpoint: None,
        }
    }

    pub fn with_runtime_root(runtime_root: PathBuf) -> Self {
        Self {
            runtime_root: Some(runtime_root),
            instance_id: "local".into(),
            explicit_endpoint: None,
        }
    }

    pub fn with_instance_id(mut self, instance_id: impl Into<String>) -> Self {
        self.instance_id = instance_id.into();
        self
    }

    /// Explicit development/diagnostic override. Product integrations should
    /// use Discovery and must not turn this into a fixed-port fallback.
    pub fn with_explicit_endpoint(mut self, endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        validate_loopback_endpoint(&endpoint)?;
        self.explicit_endpoint = Some(endpoint);
        Ok(self)
    }

    pub fn resolve(&self) -> Result<ResolvedEndpoint> {
        if let Some(endpoint) = &self.explicit_endpoint {
            return Ok(ResolvedEndpoint {
                endpoint: endpoint.clone(),
                instance_id: "explicit".into(),
                generation: "explicit".into(),
                core_version: SUPPORTED_CONSUMER_CORE_VERSIONS[0].into(),
            });
        }
        validate_file_token(&self.instance_id, 96)?;
        let root = match &self.runtime_root {
            Some(root) => root.clone(),
            None => runtime_root()?,
        };
        if !root.is_absolute() {
            return Err(Error::Discovery("runtime root must be absolute".into()));
        }
        ensure_local_filesystem(&root)?;
        let registrations = root.join("registrations");
        let sockets = root.join("sockets");
        validate_owner_only_directory(&root)?;
        validate_owner_only_directory(&registrations)?;
        #[cfg(unix)]
        validate_owner_only_directory(&sockets)?;
        let manifest = registrations.join(format!("infer-runtime--{}.json", self.instance_id));
        let registration = read_registration(&manifest)?;
        validate_registration(&registration)?;
        if registration.schema != DISCOVERY_SCHEMA
            || registration.schema_version != DISCOVERY_SCHEMA_VERSION
            || registration.service.kind != "infer-runtime"
            || registration.service.instance_id != self.instance_id
        {
            return Err(Error::Discovery(
                "registration identity is incompatible".into(),
            ));
        }
        validate_file_token(&registration.service.generation, 96)?;
        let offer = registration
            .offers
            .iter()
            .find(|offer| {
                offer.protocol == CONSUMER_CORE_PROTOCOL
                    && offer.binding == CONSUMER_HTTP_LOOPBACK_BINDING
            })
            .ok_or_else(|| Error::Discovery("no compatible Consumer Core offer".into()))?;
        let core_version = SUPPORTED_CONSUMER_CORE_VERSIONS
            .iter()
            .find(|supported| {
                offer
                    .protocol_versions
                    .iter()
                    .any(|offered| offered == **supported)
            })
            .ok_or_else(|| Error::Discovery("no compatible Consumer Core offer".into()))?;
        validate_loopback_endpoint(&offer.endpoint)?;
        Ok(ResolvedEndpoint {
            endpoint: offer.endpoint.clone(),
            instance_id: registration.service.instance_id,
            generation: registration.service.generation,
            core_version: (*core_version).into(),
        })
    }
}

fn validate_registration(registration: &Registration) -> Result<()> {
    if registration.schema != DISCOVERY_SCHEMA
        || registration.schema_version != DISCOVERY_SCHEMA_VERSION
        || !valid_service_kind(&registration.service.kind)
        || registration.offers.is_empty()
        || registration.offers.len() > 64
    {
        return Err(Error::Discovery(
            "registration does not follow the discovery schema".into(),
        ));
    }
    validate_file_token(&registration.service.instance_id, 96)?;
    validate_file_token(&registration.service.generation, 96)?;
    for offer in &registration.offers {
        if !valid_contract_id(&offer.protocol)
            || !valid_contract_id(&offer.binding)
            || offer.endpoint.is_empty()
            || offer.endpoint.len() > 512
            || offer.protocol_versions.is_empty()
            || offer.protocol_versions.len() > 16
        {
            return Err(Error::Discovery(
                "registration offer does not follow the discovery schema".into(),
            ));
        }
        let mut versions = BTreeSet::new();
        for version in &offer.protocol_versions {
            if !valid_contract_version(version) || !versions.insert(version.as_str()) {
                return Err(Error::Discovery(
                    "registration offer versions are invalid".into(),
                ));
            }
        }
    }
    Ok(())
}

fn valid_service_kind(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut separator = false;
    for byte in &bytes[1..] {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => separator = false,
            b'.' | b'-' if !separator => separator = true,
            _ => return false,
        }
    }
    !separator
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

fn read_registration(path: &Path) -> Result<Registration> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| Error::Discovery(format!("{}: {error}", path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::Discovery(format!("{}: {error}", path.display())))?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err(Error::Discovery("registration manifest is unsafe".into()));
    }
    validate_owner_and_mode(path, &metadata, 0o600)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Discovery(format!("{}: {error}", path.display())))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::Discovery("registration manifest is unsafe".into()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::Discovery(format!("invalid registration: {error}")))
}

fn validate_owner_only_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::Discovery(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Discovery(format!(
            "{} is not a safe directory",
            path.display()
        )));
    }
    validate_owner_and_mode(path, &metadata, 0o700)
}

#[cfg(unix)]
fn validate_owner_and_mode(path: &Path, metadata: &fs::Metadata, expected_mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions.
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o777 != expected_mode {
        return Err(Error::Discovery(format!(
            "{} is not owner-only",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner_and_mode(
    _path: &Path,
    _metadata: &fs::Metadata,
    _expected_mode: u32,
) -> Result<()> {
    Err(Error::Discovery(
        "this SDK build does not implement platform ACL verification".into(),
    ))
}

fn validate_file_token(value: &str, maximum: usize) -> Result<()> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= maximum
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(Error::Discovery("invalid discovery file token".into()))
    }
}

pub(crate) fn validate_loopback_endpoint(endpoint: &str) -> Result<()> {
    let address = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| Error::Discovery("Consumer endpoint must use HTTP loopback".into()))?
        .parse::<std::net::SocketAddr>()
        .map_err(|_| {
            Error::Discovery("Consumer endpoint is not a numeric socket address".into())
        })?;
    if !address.ip().is_loopback() || address.port() == 0 || format!("http://{address}") != endpoint
    {
        return Err(Error::Discovery(
            "Consumer endpoint is not canonical loopback".into(),
        ));
    }
    Ok(())
}

fn runtime_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("INFRA_PROTOCOL_RUNTIME_DIR") {
        return Ok(PathBuf::from(root));
    }
    platform_runtime_root()
}

#[cfg(target_os = "macos")]
fn ensure_local_filesystem(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Discovery("runtime root contains a NUL byte".into()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stats points to writable storage.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(Error::Discovery(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: statfs initialized the structure on success.
    let stats = unsafe { stats.assume_init() };
    if stats.f_flags as i32 & libc::MNT_LOCAL == 0 {
        return Err(Error::Discovery(
            "runtime root is not on a local filesystem".into(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_local_filesystem(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_runtime_root() -> Result<PathBuf> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    // SAFETY: confstr with a null buffer reports the required size.
    let length = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if length == 0 {
        return Err(Error::Discovery(
            "Darwin user runtime directory is unavailable".into(),
        ));
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: the buffer is allocated to the size returned by confstr.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if written == 0 || written > buffer.len() {
        return Err(Error::Discovery(
            "Darwin user runtime directory is unavailable".into(),
        ));
    }
    buffer.truncate(written.saturating_sub(1));
    Ok(PathBuf::from(OsString::from_vec(buffer)).join("infra-protocol"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_runtime_root() -> Result<PathBuf> {
    let root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Discovery("XDG_RUNTIME_DIR is unavailable".into()))?;
    Ok(root.join("infra-protocol"))
}

#[cfg(not(unix))]
fn platform_runtime_root() -> Result<PathBuf> {
    Err(Error::Discovery(
        "a verified INFRA_PROTOCOL_RUNTIME_DIR is required on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    fn fixture() -> (tempfile::TempDir, DiscoveryResolver, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("infra-protocol");
        let registrations = root.join("registrations");
        let sockets = root.join("sockets");
        fs::create_dir_all(&registrations).unwrap();
        fs::create_dir_all(&sockets).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&registrations, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = registrations.join("infer-runtime--local.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&manifest)
            .unwrap();
        write!(
            file,
            r#"{{
          "schema":"infra.discovery.registration",
          "schema_version":"20260812.1",
          "service":{{"kind":"infer-runtime","instance_id":"local","generation":"gen_test"}},
          "offers":[{{
            "protocol":"infer-runtime.consumer-core",
            "protocol_versions":["20260813.1"],
            "binding":"infer-runtime.http-loopback",
            "endpoint":"http://127.0.0.1:18787"
          }}]
        }}"#
        )
        .unwrap();
        drop(file);
        (
            temporary,
            DiscoveryResolver::with_runtime_root(root),
            manifest,
        )
    }

    #[cfg(unix)]
    #[test]
    fn resolves_only_the_exact_dated_core_offer() {
        let (_temporary, resolver, _) = fixture();
        assert_eq!(
            resolver.resolve().unwrap(),
            ResolvedEndpoint {
                endpoint: "http://127.0.0.1:18787".into(),
                instance_id: "local".into(),
                generation: "gen_test".into(),
                core_version: "20260813.1".into(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_legacy_schema_unsafe_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_temporary, resolver, manifest) = fixture();
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolver.resolve().is_err());

        let (temporary, resolver, manifest) = fixture();
        let target = temporary.path().join("target.json");
        fs::rename(&manifest, &target).unwrap();
        symlink(&target, &manifest).unwrap();
        assert!(resolver.resolve().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_missing_sockets_directory_and_schema_invalid_offers() {
        let (temporary, resolver, _manifest) = fixture();
        let sockets = temporary.path().join("infra-protocol/sockets");
        fs::remove_dir(&sockets).unwrap();
        assert!(resolver.resolve().is_err());

        let (_temporary, resolver, manifest) = fixture();
        let bytes = fs::read(&manifest).unwrap();
        let mut registration: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        registration["offers"][0]["protocol_versions"] =
            serde_json::json!(["20260812.1", "20260812.1"]);
        fs::write(&manifest, serde_json::to_vec(&registration).unwrap()).unwrap();
        assert!(resolver.resolve().is_err());
    }

    #[test]
    fn rejects_noncanonical_or_nonloopback_http_origins() {
        for endpoint in [
            "http://localhost:8787",
            "http://127.0.0.1:8787/",
            "https://127.0.0.1:8787",
            "http://0.0.0.0:8787",
            "http://127.0.0.1:0",
        ] {
            assert!(validate_loopback_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }
}
