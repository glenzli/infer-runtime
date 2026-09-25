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
- typed Agent file tasks, text, audio, vision, retrieval, OCR, capability-catalog, Job, and
  opt-in local RAW foundation clients.

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

The experimental RAW foundation client is deliberately Unix-only. It creates
an authenticated bounded lease, registers exactly two already-open descriptors
over the owner-only `uds-scm-rights` binding, then executes or cancels the Job.
It never accepts paths, moves pixels over HTTP, or exposes a generic handle
transport. Windows remains blocked pending the frozen named-pipe/HANDLE binding.

The package currently supports Unix owner/mode verification (macOS/Linux). A Windows release is
blocked until Infra Discovery and credential ACL checks are implemented with the platform's native
current-user security model. Stable Responses SSE and speech PCM streaming use separate bounded
session APIs (`stream_response` and `stream_speech`); unary return types remain unchanged.

`embed_audio_file` and `embed_audio_text` implement the additive experimental
`infer.audio.embedding@20260815.1` capability. They validate one 512d
L2-normalized audio/text retrieval space, exact Build provenance, and source/query revision shape;
unknown response fields remain available in `AudioEmbeddingResponse::extra`. They do not turn
audio evidence into transcript/event facts or claim multilingual retrieval quality.
