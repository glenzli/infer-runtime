use std::{
    collections::BTreeSet,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
};

use reqwest::{Method, RequestBuilder, Response};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::contract::{CAPABILITY_CONTRACT_HEADER, expected_capability_schema};
use crate::{
    CancelResult, CapabilityCatalog, ContractManifest, DiscoveryResolver, Error, ExplainResult,
    JobListPage, JobSnapshot, PublicErrorEnvelope, ResolvedEndpoint, Result,
    contract::{CONSUMER_CORE_HEADER, CONSUMER_CORE_PROTOCOL},
};

pub(crate) const MAX_JSON_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AUDIO_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_AUDIO_INPUT_BYTES: u64 = 25 * 1024 * 1024;
pub(crate) const MAX_IMAGE_INPUT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
struct CatalogCache {
    endpoint: ResolvedEndpoint,
    catalog: CapabilityCatalog,
    verified_capabilities: BTreeSet<String>,
}

#[derive(Clone)]
pub enum CredentialSource {
    File(PathBuf),
}

impl fmt::Debug for CredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
        }
    }
}

impl CredentialSource {
    fn load(&self) -> Result<Zeroizing<String>> {
        match self {
            Self::File(path) => read_owner_only_token(path),
        }
    }
}

#[derive(Debug)]
pub struct ClientBuilder {
    resolver: DiscoveryResolver,
    credential: Option<CredentialSource>,
}

impl ClientBuilder {
    pub fn discovery(resolver: DiscoveryResolver) -> Self {
        Self {
            resolver,
            credential: None,
        }
    }

    pub fn credential_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.credential = Some(CredentialSource::File(path.into()));
        self
    }

    pub fn build(self) -> Result<Client> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Client {
            resolver: self.resolver,
            credential: self.credential,
            http,
            capability_catalog: tokio::sync::RwLock::new(None),
        })
    }
}

#[derive(Debug)]
pub struct Client {
    resolver: DiscoveryResolver,
    credential: Option<CredentialSource>,
    http: reqwest::Client,
    capability_catalog: tokio::sync::RwLock<Option<CatalogCache>>,
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::discovery(DiscoveryResolver::local())
    }

    pub fn with_discovery(resolver: DiscoveryResolver) -> ClientBuilder {
        ClientBuilder::discovery(resolver)
    }

    pub async fn contract(&self) -> Result<ContractManifest> {
        let endpoint = self.resolver.resolve()?;
        let response = self
            .send_once(&endpoint, None, false, &|http, base| {
                http.get(format!("{base}/infer/v1/contract"))
            })
            .await?;
        let manifest: ContractManifest = decode(response).await?;
        manifest.validate()?;
        self.verify_schema_at(&endpoint, &manifest.openapi_url, &manifest.openapi_sha256)
            .await?;
        Ok(manifest)
    }

    pub async fn capabilities(&self) -> Result<CapabilityCatalog> {
        let catalog: CapabilityCatalog = self
            .send_core_json(
                Method::GET,
                "/infer/v1/capabilities",
                Option::<&()>::None,
                false,
            )
            .await?;
        catalog.validate()?;
        Ok(catalog)
    }

    async fn require_capability_at(
        &self,
        endpoint: &ResolvedEndpoint,
        capability_id: &str,
        supported: &[&'static str],
    ) -> Result<&'static str> {
        let cached_match = {
            let cache = self.capability_catalog.read().await;
            cache
                .as_ref()
                .filter(|cache| cache.endpoint == *endpoint)
                .map(|cache| cache.catalog.select_preferred(capability_id, supported))
        };
        if let Some(catalog_result) = cached_match {
            let identity = catalog_result?;
            let already_verified =
                self.capability_catalog
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|cache| {
                        cache.endpoint == *endpoint
                            && cache.verified_capabilities.contains(identity)
                    });
            if already_verified {
                return Ok(identity);
            }

            let (schema_url, schema_sha256) =
                expected_capability_schema(identity).ok_or(Error::ContractMismatch)?;
            self.verify_schema_at(endpoint, schema_url, schema_sha256)
                .await?;
            let mut cache = self.capability_catalog.write().await;
            if let Some(cache) = cache.as_mut()
                && cache.endpoint == *endpoint
            {
                cache.verified_capabilities.insert(identity.to_owned());
                return Ok(identity);
            }
            // Discovery changed while the schema was being verified. Reload the
            // Catalog at the caller's pinned endpoint rather than publishing a
            // verification result into another generation's cache.
        }
        let response = self
            .send_once(endpoint, None, false, &|http, base| {
                http.get(format!("{base}/infer/v1/capabilities"))
            })
            .await?;
        let catalog: CapabilityCatalog = decode(response).await?;
        catalog.validate()?;
        let identity = catalog.select_preferred(capability_id, supported)?;
        let (schema_url, schema_sha256) =
            expected_capability_schema(identity).ok_or(Error::ContractMismatch)?;
        self.verify_schema_at(endpoint, schema_url, schema_sha256)
            .await?;
        *self.capability_catalog.write().await = Some(CatalogCache {
            endpoint: endpoint.clone(),
            catalog,
            verified_capabilities: BTreeSet::from([identity.to_owned()]),
        });
        Ok(identity)
    }

    async fn verify_schema_at(
        &self,
        endpoint: &ResolvedEndpoint,
        path: &str,
        expected_sha256: &str,
    ) -> Result<()> {
        if !path.starts_with("/infer/v1/") {
            return Err(Error::ContractMismatch);
        }
        let response = self
            .send_once(endpoint, None, false, &|http, base| {
                http.get(format!("{base}{path}"))
            })
            .await?;
        if !response.status().is_success() {
            return Err(Error::ContractMismatch);
        }
        let bytes = read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != expected_sha256 {
            return Err(Error::ContractMismatch);
        }
        Ok(())
    }

    pub async fn job(&self, job_id: &str) -> Result<JobSnapshot> {
        validate_job_id(job_id)?;
        self.send_core_json(
            Method::GET,
            &format!("/infer/v1/jobs/{job_id}"),
            Option::<&()>::None,
            true,
        )
        .await
    }

    pub async fn jobs(&self, query: &[(&str, &str)]) -> Result<JobListPage> {
        let response = self
            .send_with_contract(None, true, |http, endpoint| {
                http.get(format!("{endpoint}/infer/v1/jobs")).query(query)
            })
            .await?;
        decode(response).await
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<CancelResult> {
        validate_job_id(job_id)?;
        self.send_core_json(
            Method::POST,
            &format!("/infer/v1/jobs/{job_id}/cancel"),
            Option::<&()>::None,
            true,
        )
        .await
    }

    pub async fn explain(&self, job_id: &str) -> Result<ExplainResult> {
        validate_job_id(job_id)?;
        self.send_core_json(
            Method::GET,
            &format!("/infer/v1/explain/{job_id}"),
            Option::<&()>::None,
            true,
        )
        .await
    }

    pub(crate) async fn send_core_json<T, B>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        authenticated: bool,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_with_contract(None, authenticated, |http, endpoint| {
                let request = http.request(method.clone(), format!("{endpoint}{path}"));
                match body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await?;
        decode(response).await
    }

    pub(crate) async fn send_capability_json<T, B>(
        &self,
        supported_contracts: &'static [&'static str],
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_capability_bound(supported_contracts, |http, endpoint| {
                let request = http.request(method.clone(), format!("{endpoint}{path}"));
                match body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await?;
        decode(response).await
    }

    pub(crate) async fn send_capability_with<F>(
        &self,
        supported_contracts: &'static [&'static str],
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        self.send_capability_bound(supported_contracts, build).await
    }

    async fn send_capability_bound<F>(
        &self,
        supported_contracts: &'static [&'static str],
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let first = self.resolver.resolve()?;
        let preferred_contract = supported_contracts
            .first()
            .copied()
            .ok_or(Error::ContractMismatch)?;
        let (capability_id, _) = preferred_contract
            .rsplit_once('@')
            .ok_or(Error::ContractMismatch)?;
        if supported_contracts.iter().any(|identity| {
            identity
                .rsplit_once('@')
                .map(|(id, _)| id != capability_id)
                .unwrap_or(true)
        }) {
            return Err(Error::ContractMismatch);
        }
        let first_contract = match self
            .require_capability_at(&first, capability_id, supported_contracts)
            .await
        {
            Ok(identity) => identity,
            Err(error) => {
                if !matches!(&error, Error::Transport(transport) if transport.is_connect()) {
                    return Err(error);
                }
                let second = self.resolver.resolve()?;
                if second == first {
                    return Err(error);
                }
                let second_contract = self
                    .require_capability_at(&second, capability_id, supported_contracts)
                    .await?;
                return self
                    .send_once(&second, Some(second_contract), true, &build)
                    .await;
            }
        };
        match self
            .send_once(&first, Some(first_contract), true, &build)
            .await
        {
            Ok(response) => Ok(response),
            Err(Error::Transport(error)) if error.is_connect() => {
                let second = self.resolver.resolve()?;
                if second == first {
                    return Err(Error::Transport(error));
                }
                let second_contract = self
                    .require_capability_at(&second, capability_id, supported_contracts)
                    .await?;
                self.send_once(&second, Some(second_contract), true, &build)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn send_with_contract<F>(
        &self,
        capability_contract: Option<&'static str>,
        authenticated: bool,
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let first = self.resolver.resolve()?;
        match self
            .send_once(&first, capability_contract, authenticated, &build)
            .await
        {
            Ok(response) => Ok(response),
            Err(Error::Transport(error)) if error.is_connect() => {
                let second = self.resolver.resolve()?;
                if second == first {
                    return Err(Error::Transport(error));
                }
                self.send_once(&second, capability_contract, authenticated, &build)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn send_once<F>(
        &self,
        endpoint: &ResolvedEndpoint,
        capability_contract: Option<&'static str>,
        authenticated: bool,
        build: &F,
    ) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let negotiated_core = format!("{CONSUMER_CORE_PROTOCOL}@{}", endpoint.core_version);
        let mut request =
            build(&self.http, &endpoint.endpoint).header(CONSUMER_CORE_HEADER, negotiated_core);
        if let Some(capability_contract) = capability_contract {
            request = request.header(CAPABILITY_CONTRACT_HEADER, capability_contract);
        }
        if authenticated {
            let token = self
                .credential
                .as_ref()
                .ok_or_else(|| Error::Credential("credential file was not configured".into()))?
                .load()?;
            request = request.bearer_auth(token.as_str());
        }
        Ok(request.send().await?)
    }
}

fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty() || job_id.contains('/') {
        Err(Error::MalformedResponse("invalid Job id".into()))
    } else {
        Ok(())
    }
}

pub(crate) async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let maximum = if status.is_success() {
        MAX_JSON_RESPONSE_BYTES
    } else {
        MAX_ERROR_RESPONSE_BYTES
    };
    let bytes = read_bounded(response, maximum).await?;
    if status.is_success() {
        return serde_json::from_slice(&bytes)
            .map_err(|error| Error::MalformedResponse(error.to_string()));
    }
    let envelope = serde_json::from_slice::<PublicErrorEnvelope>(&bytes).unwrap_or_else(|_| {
        PublicErrorEnvelope {
            error: crate::PublicError {
                message: status.canonical_reason().unwrap_or("request failed").into(),
                kind: "invalid_response_error".into(),
                code: "unparseable_error".into(),
            },
        }
    });
    Err(Error::Api {
        status,
        code: envelope.error.code,
        message: envelope.error.message,
    })
}

pub(crate) async fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let bytes = read_bounded(response, MAX_ERROR_RESPONSE_BYTES).await?;
    let envelope = serde_json::from_slice::<PublicErrorEnvelope>(&bytes).unwrap_or_else(|_| {
        PublicErrorEnvelope {
            error: crate::PublicError {
                message: status.canonical_reason().unwrap_or("request failed").into(),
                kind: "invalid_response_error".into(),
                code: "unparseable_error".into(),
            },
        }
    });
    Err(Error::Api {
        status,
        code: envelope.error.code,
        message: envelope.error.message,
    })
}

pub(crate) async fn read_bounded(mut response: Response, maximum: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(Error::MalformedResponse(format!(
            "response exceeds {maximum} bytes"
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(Error::MalformedResponse(format!(
                "response exceeds {maximum} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) async fn read_bounded_file(path: &Path, maximum: u64, kind: &str) -> Result<Vec<u8>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| Error::Input(format!("{kind} source: {error}")))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(Error::Input(format!(
            "{kind} source exceeds the {maximum}-byte contract"
        )));
    }
    tokio::fs::read(path)
        .await
        .map_err(|error| Error::Input(format!("{kind} source: {error}")))
}

fn read_owner_only_token(path: &Path) -> Result<Zeroizing<String>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| Error::Credential(format!("{}: {error}", path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::Credential(format!("{}: {error}", path.display())))?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(Error::Credential("credential file is unsafe".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(Error::Credential(
                "credential file is not owner-only".into(),
            ));
        }
    }
    #[cfg(not(unix))]
    return Err(Error::Credential(
        "this SDK build does not implement platform ACL verification".into(),
    ));

    #[cfg(unix)]
    {
        let mut token = String::with_capacity(metadata.len() as usize);
        file.take(16 * 1024 + 1)
            .read_to_string(&mut token)
            .map_err(|error| Error::Credential(format!("{}: {error}", path.display())))?;
        if token.len() > 16 * 1024 {
            return Err(Error::Credential("credential file is unsafe".into()));
        }
        let token = token.trim_end_matches(['\r', '\n']);
        if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(Error::Credential("credential token is malformed".into()));
        }
        Ok(Zeroizing::new(token.into()))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn credential_source_debug_never_contains_token_contents() {
        let source = CredentialSource::File(PathBuf::from("/safe/consumer.token"));
        assert_eq!(format!("{source:?}"), "File(\"/safe/consumer.token\")");
    }

    #[test]
    fn api_errors_use_machine_code_not_http_message() {
        let error = Error::Api {
            status: reqwest::StatusCode::UPGRADE_REQUIRED,
            code: "consumer_core_unsupported".into(),
            message: "upgrade required".into(),
        };
        assert!(error.to_string().contains("consumer_core_unsupported"));
    }

    #[cfg(unix)]
    async fn fixture_server(
        response_bodies: Vec<Vec<u8>>,
    ) -> (
        tempfile::TempDir,
        DiscoveryResolver,
        PathBuf,
        PathBuf,
        tokio::task::JoinHandle<String>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("infra-protocol");
        let registrations = root.join("registrations");
        let sockets = root.join("sockets");
        fs::create_dir_all(&registrations).unwrap();
        fs::create_dir_all(&sockets).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&registrations, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = registrations.join("infer-runtime--local.json");
        fs::write(
            &manifest,
            serde_json::to_vec(&serde_json::json!({
                "schema":"infra.discovery.registration",
                "schema_version":"20260812.1",
                "service":{"kind":"infer-runtime","instance_id":"local","generation":"gen_sdk_e2e"},
                "offers":[{"protocol":"infer-runtime.consumer-core","protocol_versions":["20260813.1"],"binding":"infer-runtime.http-loopback","endpoint":endpoint}]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        let token = temporary.path().join("consumer.token");
        fs::write(&token, b"sdk-test-token\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();

        let server = tokio::spawn(async move {
            let mut requests = String::new();
            for body in response_bodies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push_str(&String::from_utf8(request).unwrap());
                requests.push_str("\n---request---\n");
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.write_all(&body).await.unwrap();
            }
            requests
        });
        (
            temporary,
            DiscoveryResolver::with_runtime_root(root),
            token,
            manifest,
            server,
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_e2e_sends_core_but_no_capability_to_bootstrap() {
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema":"infer-runtime.consumer-core",
            "schema_version":"20260813.1",
            "core_contract":"infer-runtime.consumer-core@20260813.1",
            "supported_core_contracts":["infer-runtime.consumer-core@20260813.1"],
            "capability_catalog":{"schema":"infer-runtime.capability-catalog","schema_version":"20260813.1","url":"/infer/v1/capabilities"},
            "openapi_url":"/infer/v1/openapi.json",
            "openapi_sha256": crate::CONSUMER_OPENAPI_SHA256,
            "error_codes":["invalid_request_error"],
            "consumer_routes":[]
        }))
        .unwrap();
        let openapi =
            include_bytes!("../../../contracts/consumer-core/20260813.1/openapi.json").to_vec();
        let (_temporary, resolver, _token, _manifest, server) =
            fixture_server(vec![manifest, openapi]).await;
        Client::with_discovery(resolver)
            .build()
            .unwrap()
            .contract()
            .await
            .unwrap();
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(
            request.contains("infer-consumer-contract: infer-runtime.consumer-core@20260813.1")
        );
        assert!(!request.contains("infer-capability-contract:"));
        assert!(!request.contains("authorization:"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_e2e_sends_exact_capability_and_bearer_for_responses() {
        let catalog = serde_json::to_vec(&serde_json::json!({
                "schema":"infer-runtime.capability-catalog",
                "schema_version":"20260813.1",
                "core_contract":"infer-runtime.consumer-core@20260813.1",
                "capabilities":[{
                    "id":"infer.responses",
                    "schema_version":"20260812.1",
                    "stability":"stable",
                    "schema":{"format":"openapi-3.1","url":"/infer/v1/capability-schemas/infer.responses/20260812.1/openapi.json","sha256":"abfb3b4b9a3c5d3831d56bb877ecfdd43d62b4442ba101a5ef071ec2740adbd5"},
                    "routes":[{"method":"POST","path":"/v1/responses","execution_modes":["unary"]}]
                }]
            }))
            .unwrap();
        let schema = include_bytes!(
            "../../../contracts/capabilities/infer.responses/20260812.1/openapi.json"
        )
        .to_vec();
        let result = serde_json::to_vec(&serde_json::json!({
                "id":"resp_sdk","object":"response","created_at":1,"model":"text.summarize","status":"completed","output":[]
            }))
            .unwrap();
        let (_temporary, resolver, token, _manifest, server) =
            fixture_server(vec![catalog, schema, result]).await;
        let client = Client::with_discovery(resolver)
            .credential_file(token)
            .build()
            .unwrap();
        client
            .create_response(&crate::ResponsesRequest {
                model: "text.summarize".into(),
                input: serde_json::json!("bounded fixture"),
                instructions: None,
                stream: false,
                background: false,
                metadata: Default::default(),
                tools: Vec::new(),
                reasoning: None,
                max_output_tokens: None,
            })
            .await
            .unwrap();
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(
            request.contains("infer-consumer-contract: infer-runtime.consumer-core@20260813.1")
        );
        assert!(request.contains("infer-capability-contract: infer.responses@20260812.1"));
        assert!(request.contains("authorization: bearer sdk-test-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capability_catalog_cache_is_bound_to_discovery_generation() {
        async fn server(bodies: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let handle = tokio::spawn(async move {
                let mut requests = Vec::new();
                for body in bodies {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        request.extend_from_slice(&chunk[..read]);
                        if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    requests.push(String::from_utf8(request).unwrap());
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    stream.write_all(&body).await.unwrap();
                }
                requests
            });
            (endpoint, handle)
        }

        let catalog = serde_json::to_vec(&serde_json::json!({
            "schema":"infer-runtime.capability-catalog",
            "schema_version":"20260813.1",
            "core_contract":"infer-runtime.consumer-core@20260813.1",
            "capabilities":[{
                "id":"infer.responses","schema_version":"20260812.1","stability":"stable",
                "schema":{"format":"openapi-3.1","url":"/infer/v1/capability-schemas/infer.responses/20260812.1/openapi.json","sha256":"abfb3b4b9a3c5d3831d56bb877ecfdd43d62b4442ba101a5ef071ec2740adbd5"},
                "routes":[{"method":"POST","path":"/v1/responses","execution_modes":["unary","server_stream"]}]
            }]
        }))
        .unwrap();
        let schema = include_bytes!(
            "../../../contracts/capabilities/infer.responses/20260812.1/openapi.json"
        )
        .to_vec();
        let result = serde_json::to_vec(&serde_json::json!({
            "id":"resp_generation","object":"response","created_at":1,"model":"text.summarize","status":"completed","output":[]
        }))
        .unwrap();
        let (first_endpoint, first_server) =
            server(vec![catalog.clone(), schema.clone(), result.clone()]).await;
        let (second_endpoint, second_server) = server(vec![catalog, schema, result]).await;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("infra-protocol");
        let registrations = root.join("registrations");
        let sockets = root.join("sockets");
        fs::create_dir_all(&registrations).unwrap();
        fs::create_dir_all(&sockets).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&registrations, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = registrations.join("infer-runtime--local.json");
        let write_manifest = |generation: &str, endpoint: &str| {
            fs::write(
                &manifest,
                serde_json::to_vec(&serde_json::json!({
                    "schema":"infra.discovery.registration","schema_version":"20260812.1",
                    "service":{"kind":"infer-runtime","instance_id":"local","generation":generation},
                    "offers":[{"protocol":"infer-runtime.consumer-core","protocol_versions":["20260813.1"],"binding":"infer-runtime.http-loopback","endpoint":endpoint}]
                }))
                .unwrap(),
            )
            .unwrap();
            fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        };
        write_manifest("gen_first", &first_endpoint);
        let token = temporary.path().join("consumer.token");
        fs::write(&token, b"sdk-generation-token\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let client = Client::with_discovery(DiscoveryResolver::with_runtime_root(root))
            .credential_file(token)
            .build()
            .unwrap();
        let request = crate::ResponsesRequest {
            model: "text.summarize".into(),
            input: serde_json::json!("fixture"),
            instructions: None,
            stream: false,
            background: false,
            metadata: Default::default(),
            tools: Vec::new(),
            reasoning: None,
            max_output_tokens: None,
        };
        client.create_response(&request).await.unwrap();
        write_manifest("gen_second", &second_endpoint);
        client.create_response(&request).await.unwrap();

        let first = first_server.await.unwrap();
        let second = second_server.await.unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert!(first[0].starts_with("GET /infer/v1/capabilities "));
        assert!(first[1].starts_with("GET /infer/v1/capability-schemas/"));
        assert!(first[2].starts_with("POST /v1/responses "));
        assert!(second[0].starts_with("GET /infer/v1/capabilities "));
        assert!(second[1].starts_with("GET /infer/v1/capability-schemas/"));
        assert!(second[2].starts_with("POST /v1/responses "));
    }
}
