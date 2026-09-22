use crate::{NodeError, PROTOCOL};
use infer_core::NodeTlsConfig;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path, sync::Arc};

#[cfg(unix)]
pub(crate) fn owner_file(path: &str) -> Result<Vec<u8>, NodeError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| NodeError::Forbidden)?;
    let metadata = file.metadata().map_err(|_| NodeError::Forbidden)?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(NodeError::Forbidden);
    }
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(NodeError::Forbidden);
    }
    // Never include paths, PEM bytes, or decoder details in protocol errors.
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| NodeError::Forbidden)?;
    if bytes.len() > 64 * 1024 {
        return Err(NodeError::Forbidden);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
pub(crate) fn owner_file(_path: &str) -> Result<Vec<u8>, NodeError> {
    // Windows needs an explicit owner/DACL check before enabling node credentials.
    Err(NodeError::Forbidden)
}

fn certificates(path: &str) -> Result<Vec<CertificateDer<'static>>, NodeError> {
    let certs: Result<Vec<_>, _> = CertificateDer::pem_file_iter(Path::new(path))
        .map_err(|_| NodeError::Forbidden)?
        .collect();
    let certs = certs.map_err(|_| NodeError::Forbidden)?;
    if certs.is_empty() {
        return Err(NodeError::Forbidden);
    }
    Ok(certs)
}

fn roots(config: &NodeTlsConfig) -> Result<RootCertStore, NodeError> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates(&config.ca_certificate)? {
        roots.add(certificate).map_err(|_| NodeError::Forbidden)?;
    }
    Ok(roots)
}

fn key(config: &NodeTlsConfig) -> Result<PrivateKeyDer<'static>, NodeError> {
    let mut bytes = owner_file(&config.private_key)?;
    let key = PrivateKeyDer::from_pem_slice(&bytes).map_err(|_| NodeError::Forbidden);
    bytes.fill(0);
    key
}

pub(crate) fn client(config: &NodeTlsConfig) -> Result<Arc<ClientConfig>, NodeError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| NodeError::Protocol)?
        .with_root_certificates(roots(config)?)
        .with_client_auth_cert(certificates(&config.certificate)?, key(config)?)
        .map_err(|_| NodeError::Forbidden)?;
    tls.alpn_protocols = vec![PROTOCOL.as_bytes().to_vec()];
    Ok(Arc::new(tls))
}

pub(crate) fn server(config: &NodeTlsConfig) -> Result<Arc<ServerConfig>, NodeError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots(config)?),
        Arc::clone(&provider),
    )
    .build()
    .map_err(|_| NodeError::Forbidden)?;
    let mut tls = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| NodeError::Protocol)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates(&config.certificate)?, key(config)?)
        .map_err(|_| NodeError::Forbidden)?;
    tls.alpn_protocols = vec![PROTOCOL.as_bytes().to_vec()];
    Ok(Arc::new(tls))
}

pub(crate) fn fingerprint(certs: Option<&[CertificateDer<'_>]>) -> Result<String, NodeError> {
    let certificate = certs
        .and_then(|chain| chain.first())
        .ok_or(NodeError::Forbidden)?;
    Ok(format!("{:x}", Sha256::digest(certificate.as_ref())))
}
