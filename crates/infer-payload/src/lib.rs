//! Authenticated, encrypted filesystem ownership for durable inference payloads.
//!
//! SQLite stores only [`DurablePayloadRef`]. This owner controls key parsing,
//! file naming, atomic publication, authenticated encryption, size bounds, and
//! deletion without learning anything about Job scheduling or provider APIs.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use infer_core::{DurablePayloadKind, DurablePayloadRef};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const MAGIC: &[u8; 8] = b"INFPAY01";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("background payload key must contain exactly 64 hexadecimal characters")]
    InvalidKey,
    #[error("background payload exceeds the configured {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("background payload reference is invalid")]
    InvalidReference,
    #[error("background payload is missing")]
    Missing,
    #[error("background payload authentication failed")]
    Authentication,
    #[error("background payload random generation failed")]
    Random,
    #[error("background payload I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub struct EncryptedPayloadSpool {
    root: PathBuf,
    aead_key: Zeroizing<[u8; 32]>,
    digest_key: Zeroizing<[u8; 32]>,
    max_payload_bytes: usize,
}

impl EncryptedPayloadSpool {
    pub fn open(
        root: impl AsRef<Path>,
        key_hex: &str,
        max_payload_bytes: usize,
    ) -> Result<Self, PayloadError> {
        let root = root.as_ref().to_path_buf();
        let mut master_key = Zeroizing::new(parse_key(key_hex)?);
        let aead_key = derive_key(&master_key, b"infer-runtime payload aead v1");
        let digest_key = derive_key(&master_key, b"infer-runtime payload digest v1");
        master_key.zeroize();
        fs::create_dir_all(&root)?;
        set_directory_permissions(&root)?;
        Ok(Self {
            root,
            aead_key: Zeroizing::new(aead_key),
            digest_key: Zeroizing::new(digest_key),
            max_payload_bytes,
        })
    }

    pub fn put(
        &self,
        app_id: &str,
        kind: DurablePayloadKind,
        plaintext: &[u8],
    ) -> Result<DurablePayloadRef, PayloadError> {
        if plaintext.len() > self.max_payload_bytes {
            return Err(PayloadError::TooLarge {
                limit: self.max_payload_bytes,
            });
        }
        let reference = DurablePayloadRef {
            blob_id: format!("pay_{}", Uuid::new_v4().simple()),
            kind,
            digest: self.digest(app_id, kind, plaintext),
            plaintext_bytes: plaintext.len(),
        };
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| PayloadError::Random)?;
        let mut encrypted = Zeroizing::new(plaintext.to_vec());
        self.key()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad(app_id, &reference)),
                &mut *encrypted,
            )
            .map_err(|_| PayloadError::Authentication)?;

        let path = self.path(&reference.blob_id)?;
        let temporary = self.root.join(format!(".{}.tmp", reference.blob_id));
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            set_file_permissions(&file)?;
            file.write_all(MAGIC)?;
            file.write_all(&nonce_bytes)?;
            file.write_all(&encrypted)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            sync_directory(&self.root)?;
            Ok::<_, std::io::Error>(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result?;
        Ok(reference)
    }

    pub fn get(
        &self,
        app_id: &str,
        reference: &DurablePayloadRef,
    ) -> Result<Zeroizing<Vec<u8>>, PayloadError> {
        self.validate_reference(reference)?;
        let path = self.path(&reference.blob_id)?;
        let file = OpenOptions::new().read(true).open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PayloadError::Missing
            } else {
                PayloadError::Io(error)
            }
        })?;
        let maximum = MAGIC.len() + NONCE_BYTES + reference.plaintext_bytes + TAG_BYTES + 1;
        let mut contents = Vec::with_capacity(maximum.min(self.max_payload_bytes + 64));
        file.take(maximum as u64).read_to_end(&mut contents)?;
        if contents.len() != MAGIC.len() + NONCE_BYTES + reference.plaintext_bytes + TAG_BYTES
            || contents.get(..MAGIC.len()) != Some(MAGIC)
        {
            return Err(PayloadError::InvalidReference);
        }
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        nonce_bytes.copy_from_slice(&contents[MAGIC.len()..MAGIC.len() + NONCE_BYTES]);
        let mut encrypted = Zeroizing::new(contents.split_off(MAGIC.len() + NONCE_BYTES));
        let plaintext = self
            .key()?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad(app_id, reference)),
                &mut encrypted,
            )
            .map_err(|_| PayloadError::Authentication)?;
        if plaintext.len() != reference.plaintext_bytes
            || self.digest(app_id, reference.kind, plaintext) != reference.digest
        {
            return Err(PayloadError::Authentication);
        }
        Ok(Zeroizing::new(plaintext.to_vec()))
    }

    pub fn delete(&self, reference: &DurablePayloadRef) -> Result<(), PayloadError> {
        self.validate_reference(reference)?;
        match fs::remove_file(self.path(&reference.blob_id)?) {
            Ok(()) => {
                sync_directory(&self.root)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Removes only this owner's unpublished blob and temporary files. Other
    /// files in the configured directory are never touched.
    pub fn remove_orphans(&self, live_blob_ids: &BTreeSet<String>) -> Result<usize, PayloadError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let orphan_blob = name
                .strip_suffix(".blob")
                .filter(|blob_id| self.path(blob_id).is_ok())
                .is_some_and(|blob_id| !live_blob_ids.contains(blob_id));
            let stale_temporary = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".tmp"))
                .is_some_and(|blob_id| self.path(blob_id).is_ok());
            if orphan_blob || stale_temporary {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        if removed > 0 {
            sync_directory(&self.root)?;
        }
        Ok(removed)
    }

    fn key(&self) -> Result<LessSafeKey, PayloadError> {
        UnboundKey::new(&aead::AES_256_GCM, self.aead_key.as_ref())
            .map(LessSafeKey::new)
            .map_err(|_| PayloadError::InvalidKey)
    }

    fn digest(&self, app_id: &str, kind: DurablePayloadKind, plaintext: &[u8]) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, self.digest_key.as_ref());
        let mut context = hmac::Context::with_key(&key);
        context.update(b"infer-runtime payload identity v1");
        update_hmac_field(&mut context, app_id.as_bytes());
        update_hmac_field(&mut context, kind_code(kind).as_bytes());
        context.update(plaintext);
        format!("hmac-sha256:{}", hex(context.sign().as_ref()))
    }

    fn path(&self, blob_id: &str) -> Result<PathBuf, PayloadError> {
        if !blob_id.strip_prefix("pay_").is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(PayloadError::InvalidReference);
        }
        Ok(self.root.join(format!("{blob_id}.blob")))
    }

    fn validate_reference(&self, reference: &DurablePayloadRef) -> Result<(), PayloadError> {
        self.path(&reference.blob_id)?;
        let digest = reference
            .digest
            .strip_prefix("hmac-sha256:")
            .ok_or(PayloadError::InvalidReference)?;
        if digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || reference.plaintext_bytes > self.max_payload_bytes
        {
            return Err(PayloadError::InvalidReference);
        }
        Ok(())
    }
}

fn aad(app_id: &str, reference: &DurablePayloadRef) -> Vec<u8> {
    let mut aad = b"infer-runtime payload aad v1".to_vec();
    push_field(&mut aad, app_id.as_bytes());
    push_field(&mut aad, kind_code(reference.kind).as_bytes());
    push_field(&mut aad, reference.blob_id.as_bytes());
    push_field(&mut aad, reference.digest.as_bytes());
    aad.extend_from_slice(&(reference.plaintext_bytes as u64).to_be_bytes());
    aad
}

fn push_field(buffer: &mut Vec<u8>, field: &[u8]) {
    buffer.extend_from_slice(&(field.len() as u64).to_be_bytes());
    buffer.extend_from_slice(field);
}

fn update_hmac_field(context: &mut hmac::Context, field: &[u8]) {
    context.update(&(field.len() as u64).to_be_bytes());
    context.update(field);
}

fn parse_key(value: &str) -> Result<[u8; 32], PayloadError> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(PayloadError::InvalidKey);
    }
    let mut key = [0_u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| PayloadError::InvalidKey)?;
    }
    Ok(key)
}

fn derive_key(master_key: &[u8; 32], domain: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, master_key);
    let tag = hmac::sign(&key, domain);
    let mut derived = [0_u8; 32];
    derived.copy_from_slice(tag.as_ref());
    derived
}

fn kind_code(kind: DurablePayloadKind) -> &'static str {
    match kind {
        DurablePayloadKind::ResponsesRequest => "responses_request",
        DurablePayloadKind::ResponsesResult => "responses_result",
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(file: &fs::File) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_permissions(_file: &fs::File) -> Result<(), std::io::Error> {
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn round_trips_and_deletes_an_authenticated_payload() {
        let directory = tempfile::tempdir().unwrap();
        let spool = EncryptedPayloadSpool::open(directory.path(), KEY, 1024).unwrap();
        let reference = spool
            .put(
                "test-app",
                DurablePayloadKind::ResponsesRequest,
                b"private prompt",
            )
            .unwrap();
        let raw = fs::read(spool.path(&reference.blob_id).unwrap()).unwrap();
        assert!(!raw.windows(14).any(|window| window == b"private prompt"));
        assert_eq!(
            spool.get("test-app", &reference).unwrap().as_slice(),
            b"private prompt"
        );
        spool.delete(&reference).unwrap();
        assert!(matches!(
            spool.get("test-app", &reference),
            Err(PayloadError::Missing)
        ));
    }

    #[test]
    fn app_identity_reference_and_ciphertext_are_authenticated() {
        let directory = tempfile::tempdir().unwrap();
        let spool = EncryptedPayloadSpool::open(directory.path(), KEY, 1024).unwrap();
        let reference = spool
            .put("test-app", DurablePayloadKind::ResponsesRequest, b"payload")
            .unwrap();
        let other_reference = spool
            .put("other", DurablePayloadKind::ResponsesRequest, b"payload")
            .unwrap();
        assert_ne!(reference.digest, other_reference.digest);
        assert!(matches!(
            spool.get("other", &reference),
            Err(PayloadError::Authentication)
        ));
        let path = spool.path(&reference.blob_id).unwrap();
        let mut raw = fs::read(&path).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        fs::write(path, raw).unwrap();
        assert!(matches!(
            spool.get("test-app", &reference),
            Err(PayloadError::Authentication)
        ));
    }

    #[test]
    fn rejects_bad_keys_oversized_payloads_and_unsafe_ids() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_root = directory.path().join("invalid-key-spool");
        assert!(matches!(
            EncryptedPayloadSpool::open(&invalid_root, "short", 4),
            Err(PayloadError::InvalidKey)
        ));
        assert!(!invalid_root.exists());
        let spool = EncryptedPayloadSpool::open(directory.path(), KEY, 4).unwrap();
        assert!(matches!(
            spool.put("test-app", DurablePayloadKind::ResponsesRequest, b"12345"),
            Err(PayloadError::TooLarge { .. })
        ));
        let unsafe_reference = DurablePayloadRef {
            blob_id: "../../secret".into(),
            kind: DurablePayloadKind::ResponsesRequest,
            digest: "hmac-sha256:bad".into(),
            plaintext_bytes: 1,
        };
        assert!(matches!(
            spool.get("test-app", &unsafe_reference),
            Err(PayloadError::InvalidReference)
        ));
        let invalid_digest = DurablePayloadRef {
            blob_id: "pay_00000000000000000000000000000000".into(),
            kind: DurablePayloadKind::ResponsesRequest,
            digest: "hmac-sha256:short".into(),
            plaintext_bytes: 1,
        };
        assert!(matches!(
            spool.get("test-app", &invalid_digest),
            Err(PayloadError::InvalidReference)
        ));
    }

    #[test]
    fn orphan_cleanup_preserves_live_blobs_and_unowned_files() {
        let directory = tempfile::tempdir().unwrap();
        let spool = EncryptedPayloadSpool::open(directory.path(), KEY, 1024).unwrap();
        let live = spool
            .put("test-app", DurablePayloadKind::ResponsesRequest, b"live")
            .unwrap();
        let orphan = spool
            .put("test-app", DurablePayloadKind::ResponsesRequest, b"orphan")
            .unwrap();
        fs::write(directory.path().join("operator-note"), b"keep").unwrap();
        fs::write(
            directory
                .path()
                .join(".pay_11111111111111111111111111111111.tmp"),
            b"partial",
        )
        .unwrap();
        let removed = spool
            .remove_orphans(&BTreeSet::from([live.blob_id.clone()]))
            .unwrap();
        assert_eq!(removed, 2);
        assert!(spool.get("test-app", &live).is_ok());
        assert!(matches!(
            spool.get("test-app", &orphan),
            Err(PayloadError::Missing)
        ));
        assert!(directory.path().join("operator-note").exists());
    }
}
