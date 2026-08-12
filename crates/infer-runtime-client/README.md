# infer-runtime-client

Official Rust client for Infer Runtime's dated Consumer Core contract.

The crate owns the shared integration mechanics that product Consumers must not
reimplement:

- strict `infra.discovery.registration@20260812.1` selection;
- exact `infer-runtime.consumer-core@20260813.1` negotiation;
- owner-only managed credential loading;
- loopback HTTP with proxies and redirects disabled;
- generation-aware reconnect and machine-readable errors;
- exact Capability Catalog intersection and immutable OpenAPI digest validation;
- typed text, audio, vision, retrieval, OCR, capability-catalog, and Job clients.

Applications still own their product data, persistence, stale-result decisions,
and user-facing workflow. They request an Intent or an ACL-authorized named
Deployment/Profile; they do not send provider-specific physical model names.

During the coordinated migration, depend on an immutable Git revision:

```toml
[dependencies]
infer-runtime-client = { git = "https://github.com/glenzli/infer-runtime.git", rev = "<migration-commit>" }
```

The package can move to a registry release after all local Consumers complete
the hard cut. No compatibility fallback to candidate contracts or a fixed port
is included.

The experimental RAW foundation route is not part of this first stable SDK
surface. It must gain a dedicated typed SDK module before activation; products
must not bypass that gate by reimplementing Core Discovery or generic handle
transport.

The package currently supports Unix owner/mode verification (macOS/Linux). A Windows release is
blocked until Infra Discovery and credential ACL checks are implemented with the platform's native
current-user security model. Stable Responses SSE and speech PCM streaming use separate bounded
session APIs (`stream_response` and `stream_speech`); unary return types remain unchanged.
