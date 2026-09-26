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

`generate_sound_effect` uses the independent experimental
`infer.audio.sound-generation@20260926.1` capability. One request creates one
1–30 second 44.1 kHz stereo PCM16 WAV candidate from a prompt of at most 2000
UTF-8 bytes. The optional `u32` seed is returned even when assigned by Runtime.
The SDK validates the response SHA-256, WAV shape, local placement, and model
identity headers; Consumers save `SoundGenerationResponse.wav` into their own
artifact store and decide whether to accept or discard the candidate. The
result's Job ID can be read with `Client::job` for routing and attempt evidence.

### Explicit sound-prompt preparation

`Client::prepare_sound_prompt(original_prompt)` is an opt-in client helper under
`infer.sound-prompt-preparation@20260926.1`. English is returned byte-for-byte without a
request. Chinese or mixed descriptions use one `text.edit` Responses request to the
named local `ollama_qwen3_5_4b` deployment: background priority, balanced latency,
foundational floor, local-only/offline, no fallback and zero cost. It translates
faithfully without creative expansion. This does not alter `generate_sound_effect`.

Call the helper once per product batch, validate the result with
`PreparedSoundPrompt::validate_for(original_prompt, app_id)`, retain it for retries,
and explicitly pass `effective_prompt` to sound generation. Persist the original,
effective prompt, rule revision, preparation latency and optional text Job together
with the independent sound Job. The SDK does not own UI, caching, history or acceptance.
Changing the original invalidates reuse; preparation errors must stop generation.
Dropping the future stops waiting but does not claim provider-side cancellation.
