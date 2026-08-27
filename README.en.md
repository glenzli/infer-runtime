# infer-runtime

[中文](README.md) · [English](README.en.md)

`infer-runtime` is an AI inference runtime for local applications. A Consumer
submits an Intent together with capability, latency, placement, privacy, and
fallback constraints. Runtime selects a Provider and Deployment, then handles
queueing, quotas, model residency, cancellation, failover, and execution records.

The project unifies the control plane only. Text, audio, and vision retain their
own typed protocols instead of being wrapped in a universal JSON or tensor API.
`infer-runtime` is not a model marketplace, an agent orchestrator, or an AI
application.

## Current implementation

| Area | Implemented | Status |
| --- | --- | --- |
| Text | Responses-style unary requests and SSE, encrypted local background work, and local, cloud, and subscription Providers | `infer.responses@20260812.1` is frozen; the subscription bridge remains experimental |
| Audio | Transcription, forced alignment, speech synthesis, sound generation, voice cloning, AudioSet event detection, and audio-text retrieval | Basic audio and event-detection contracts are stable; streaming and some generation and retrieval capabilities remain experimental |
| Vision | ONNX/Core ML Providers, face detection and embeddings, image-text embeddings, click-guided subject segmentation, and face parsing | All are currently narrow experimental capabilities; some weights are restricted to research use |
| Scheduling | Intent routing, priority queues, deadlines, cancellation, retry/fallback, circuit breaking, and quotas | Available as an end-to-end runtime path |
| Local resources | Ollama/ONNX lifecycle, pressure sampling, load benchmarks, eviction recommendations, and maintenance leases | Automatic eviction is off by default |
| Integration and operations | Per-App ACLs, managed credentials, Infra Discovery, status interfaces, and Web Console | Used by local Consumers and Operators |

Local configuration determines which models and Providers are available. The
repository does not include model files or maintain an inventory of models on a
development machine. This is still a development preview; see
[ROADMAP.md](ROADMAP.md) for release gates and experimental capability status.

![Infer Console overview (synthetic demo data)](docs/images/console-overview-demo.png)

*All Providers, Deployments, metrics, and instance identities in the screenshot
are synthetic demonstration data.*

## Quick start

Base development requires Rust. The example configuration includes the shape
of an Ollama setup; audio, ONNX, cloud, and subscription Providers are optional.

```bash
cp config/infer.example.toml config/infer.toml
cargo build -p infer -p inferd
target/debug/infer console --spawn
```

Console opens `http://127.0.0.1:8790/` by default and starts the `inferd` process
it manages. The first run creates a local `local-operator` credential, so an API
key does not need to be stored in configuration. Console does not take over an
externally started daemon.

The daemon can also run on its own:

```bash
cargo run -p inferd
```

Submit a development request from another terminal:

```bash
cargo run -p infer -- run \
  --input 'Explain the central trade-offs of this design.' \
  --stream
```

The CLI connects to `http://127.0.0.1:8787` by default. Override it with
`--server` or `INFER_URL`. Product Consumers should use Infra Discovery instead
of depending on a fixed port.

Optional capabilities also require the corresponding Provider,
Build/Deployment, runtime, model files, and App ACL. Runtime does not download
models or relax placement automatically. See [Operations](docs/OPERATIONS.md)
for setup procedures.

## Web Console

Console provides Overview, Statistics, Jobs, Models & Resources, Apps & Access,
Logs, and Configuration pages. It is used to inspect runtime state, manage model
lifecycle, create Consumer credentials, and apply configuration. Resource
control and credential management are Operator-only; normal Consumers can
access only their own jobs.

The browser receives a random Console session proof, not the `local-operator`
bearer credential.

![Infer Console models and resources (synthetic demo data)](docs/images/console-models-demo.png)

## Consumer integration

A local application selects the following offer from
[Infra Discovery](docs/CONSUMER_DISCOVERY.md):

```text
protocol  = infer-runtime.consumer-core
versions  = [20260813.1]
binding   = infer-runtime.http-loopback
```

The Discovery manifest provides service identity, generation, and endpoint
offers. Availability is still determined by connecting and completing the
contract handshake. Use
[`infer-runtime-client`](crates/infer-runtime-client/README.md) where possible;
the SDK handles discovery, generation changes, tokens, Core/Capability headers,
and error parsing.

Create a separate identity for every Consumer in the Console **Apps & Access**
page. Grant only the required Intents, placement, priority, capability,
fallback, and cost ranges, and keep `resource_admin = false` for normal
applications. Do not give the `local-operator` token to a product. A managed
token is shown only when created or rotated and belongs in the Consumer's
owner-only secret store.

The request `model` is an Intent, not a physical model name:

```bash
curl "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $INFER_API_KEY" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "text.summarize",
    "input": "Content to summarize",
    "metadata": {
      "infer.placement": "local_only",
      "infer.fallback": "none"
    }
  }'
```

Clients should branch on HTTP status and `error.code`, not parse
`error.message`. Full fields, SSE, background, audio, and vision protocols are
documented in [Integration](docs/INTEGRATION.md) and
[`consumer-core@20260813.1`](contracts/consumer-core/20260813.1/README.md).

## Engineering boundaries

- Intent is separate from the physical model. Runtime selects the Provider,
  Build, and Deployment from the request constraints.
- `local_only`, offline, cloud-modality ACLs, and fallback grants are hard
  constraints and are not relaxed silently.
- Prompts, audio, pixels, biometric embeddings, and Provider secrets are not
  written to ordinary Job metadata or logs by default.
- Providers retain their own lifecycle and data protocols while sharing
  admission, reservation, and resource-pressure policy.
- Provider probes require an explicit Operator action and may incur real cost;
  automatic eviction is off by default.
- Models, credentials, local configuration, and runtime state do not belong in
  the public repository.

## Related projects

- [Infra Protocol](https://github.com/glenzli/infra-protocol): local service
  discovery contracts.
- [Infra Sentinel](https://github.com/glenzli/infra-sentinel): infrastructure
  observation from redacted status snapshots.
- [Shadow](https://github.com/glenzli/shadow): a vision Consumer.
- [Symbiont-d](https://github.com/glenzli/symbiont-d): a speech-transcription
  Consumer.

## Documentation

| Document | Contents |
| --- | --- |
| [DESIGN.md](DESIGN.md) | Product boundary, domain model, and component responsibilities |
| [ROADMAP.md](ROADMAP.md) | Current stage and release gates |
| [docs/INTEGRATION.md](docs/INTEGRATION.md) | Consumer integration and data-plane examples |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Console, model resources, and background operations |
| [docs/CONSUMER_DISCOVERY.md](docs/CONSUMER_DISCOVERY.md) | Infra Discovery contract |
| [contracts/consumer-core/20260813.1](contracts/consumer-core/20260813.1/README.md) | Current Core OpenAPI, fixtures, and machine contract |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Accepted decisions and ADR index |
| [docs/STATUS_PROTOCOL.md](docs/STATUS_PROTOCOL.md) | Read-only status interface and Discovery offer |
| [docs/REPOSITORY_BOUNDARY.md](docs/REPOSITORY_BOUNDARY.md) | Repository boundary for source and local assets |

## Development checks

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Audio, vision, model benchmark, and background-operation examples live in
[docs/OPERATIONS.md](docs/OPERATIONS.md) and
[docs/INTEGRATION.md](docs/INTEGRATION.md).
