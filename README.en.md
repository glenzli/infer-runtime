# infer-runtime

[中文](README.md) · [English](README.en.md)

> A local-first AI inference control plane for heterogeneous intelligence resources.

`infer-runtime` lets an application express only an Intent, capability floor,
reasoning effort, latency, placement, privacy, and fallback constraints. The
Runtime selects a Provider and Deployment, then centrally handles queueing,
quotas, model residency, cancellation, failover, and audit.

It is not a model marketplace, an agent framework, or an AI application. Nor
does it flatten text, audio, vision, and subscription-backed models into one
universal JSON/tensor protocol. It unifies the control plane, not every data
plane.

## Core capabilities

| Capability | Current implementation | Stability |
| --- | --- | --- |
| Text inference | Responses-shaped unary/SSE and encrypted local background work; local, cloud, and subscription Providers | `infer.responses@20260812.1`; subscription bridge remains experimental |
| Local audio | Transcription, forced alignment, speech synthesis, voice design, voice cloning, and AudioSet event detection | Event detection is stable; streaming TTS/ASR and some generation capabilities remain experimental |
| Local vision | Typed ONNX/Core ML Providers; face detection/embeddings, image-text embeddings, click-guided subject segmentation, and face parsing | Narrow experimental slices; face parsing is restricted research use |
| Routing and execution | Intent → Model Profile → Build → Deployment; priority queues, deadline, cancellation, retry/fallback, and circuit breaking | M1/M2 are closed |
| Budget and recovery | App/provider/global quotas, reservations, usage ledger, SQLite migrations, and local background recovery | M3 is closed |
| Resource governance | Ollama/ONNX lifecycle, pressure sampling, reload benchmarks, eviction recommendations, and maintenance leases | M4 is closed; automatic eviction is off by default |
| Identity and discovery | Per-App Intent ACLs, managed credentials, and Infra Discovery Consumer/status offers | Available to local Consumers |
| Management UI | Daemon, statistics, Jobs, Providers, models, Apps & Access, logs, and configuration | Loopback Web Console |

The table describes implemented protocol families. Specific models and
Providers are configurable examples only: Runtime does not assume a particular
model is installed locally, and public documentation does not maintain a
developer-machine inventory.

Runtime retains explainable Job/Attempt records, Candidate Plans, reason codes,
and physical execution provenance. A normal Consumer sees only its own jobs;
resource control, Provider probes, and credential management are protected
Operator surfaces.

![Infer Console overview (synthetic demo data)](docs/images/console-overview-demo.png)

> Screenshots show Console structure only. All Providers, Deployments, metrics,
> and instance identities are synthetic demonstration data, not a developer or
> user environment.

## Quick start

Rust is required for base development. The example configuration can connect to
Ollama; audio, ONNX, cloud, and subscription Providers are optional. The
repository's [`config/infer.example.toml`](config/infer.example.toml) documents
shape only. Locally usable capabilities are decided by `config/infer.toml` and
runtime discovery.

Starting through the Web Console is recommended: it owns the daemon it starts.

```bash
cp config/infer.example.toml config/infer.toml
cargo build -p infer -p inferd
target/debug/infer console --spawn
```

The browser opens `http://127.0.0.1:8790/`. On first launch Runtime generates a
local `local-operator` credential; no API key belongs in configuration. Console
can start and stop only an `inferd` it created and never takes over an external
process.

To start only the daemon:

```bash
cargo run -p inferd
```

Submit one development request from another terminal:

```bash
cargo run -p infer -- run \
  --input 'Explain the central trade-offs of this design.' \
  --stream
```

The bundled CLI defaults to `http://127.0.0.1:8787`; override it with
`--server` or `INFER_URL`. Product Consumers should use Infra Discovery below,
not hard-code a port.

### Enabling optional local capabilities

Starting Runtime does not make every optional model routable. Each capability
needs all of: a configured Provider, an admitted Build/Deployment, necessary
local runtime support, and the target App's minimum ACL. If any is absent,
Runtime removes it from the Candidate Plan. It does not download a model,
weaken placement, or silently use cloud. The Console **Models & Resources**
page shows missing prerequisites and remediation hints.

`config/infer.example.toml` shows the typed configuration structure, while
model files, ArtifactStore content, owner-only Python environments, and
credentials remain under local-owner control and are never distributed with the
repository. Typical setup paths include:

- **Audio event detection:** import a verified YAMNet artifact and provide
  Python/TensorFlow plus `ffmpeg`. Runtime safely discovers `ffmpeg` from the
  inherited absolute `PATH`, conventional Homebrew paths, and system paths; an
  absolute configuration path can pin it.
- **Subject segmentation:** publish manifests for the three SAM 2.1 Small Core
  ML packages and precompile derived `.mlmodelc` caches in a maintenance
  window. The default is `cpu_and_gpu`; do not use an unverified `coreml_all`
  configuration that makes the first request pay compile or ANE-specialization
  cost.
- **Face detection, embeddings, and parsing:** import YuNet, SFace, and
  BiSeNet ONNX files through ArtifactStore. On currently supported hosts these
  ONNX Builds are pinned to CPU, so an incompatible Core ML EP attempt is never
  misrepresented as acceleration. BiSeNet weights also carry a non-commercial
  research restriction.

See [`docs/OPERATIONS.md`](docs/OPERATIONS.md) for artifact, license, runtime,
and readiness procedures.

## Web Console

Console provides seven graphical pages: Overview, Statistics, Jobs, Models &
Resources, Apps & Access, Logs, and Configuration. It can:

- inspect throughput, failures, queues, budgets, memory pressure, and
  Job/Attempt provenance;
- inspect statically admitted and dynamically discovered Provider model groups
  (the UI reflects current configuration);
- explicitly refresh inventory, load/unload models, and execute approved
  eviction;
- create least-privilege Consumers, reveal a managed token once, rotate, or
  revoke access;
- validate configuration and apply it through an explicit restart.

The UI follows the system light/dark appearance. The browser receives only a
random Console session proof; the `local-operator` bearer credential is never
sent to the page.

![Infer Console models and resources (synthetic demo data)](docs/images/console-models-demo.png)

## Consumer integration

### 1. Discover Runtime

A local application reads the [Infra Discovery](docs/CONSUMER_DISCOVERY.md)
registration and selects exactly:

```text
protocol  = infer-runtime.consumer-core
versions  = [20260813.1]
binding   = infer-runtime.http-loopback
```

The Discovery manifest publishes only service identity, a generation that
changes each startup, and endpoint offers. It contains no lease, heartbeat,
App ID, token, or ACL. A manifest is a candidate entry point: the Consumer must
establish a real connection to determine availability. Products retain no
fixed-port fallback; explicit endpoint overrides are for development and
diagnostics only. Every request sends
`Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1`; typed
capability requests also send the exact `Infer-Capability-Contract` supplied by
the Catalog.

Use the official [`infer-runtime-client`](crates/infer-runtime-client/README.md)
where possible. The SDK implements Discovery, generation rediscovery,
owner-only token handling, proxy/redirect disabling, Core/Capability handshakes,
and stable error parsing so products do not reimplement those concerns.

### 2. Create a least-privilege App

Create a separate identity for every Consumer in the Console **Apps & Access**
page. Explicitly set:

- `resource_admin = false`;
- the allowed Intent list;
- placement, priority, capability, reasoning-effort, fallback, and cost caps;
- whether cloud/subscription execution and outbound modalities are permitted.

Never give a product the `local-operator` token. A managed token is shown only
when it is created or rotated: write it immediately into the Consumer's own
owner-only secret store, never source, project files, or logs.

### 3. Call a stable Intent

`model` is an Intent, not an Ollama tag or physical model name. The following
`INFER_BASE_URL` has been selected and validated through Discovery/override.

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

Request fields are strict; responses may add unknown fields. Branch on HTTP
status and `error.code`, never parse `error.message`. Full fields, SSE,
background, audio, and experimental vision contracts are in
[`docs/INTEGRATION.md`](docs/INTEGRATION.md) and
[`contracts/consumer-core/20260813.1`](contracts/consumer-core/20260813.1/README.md).

Legacy candidate Consumers must complete the whole
[Core hard migration](docs/MIGRATION-CONSUMER-CORE-20260813.1.md). Final Runtime
does not publish a candidate offer and rejects business requests without the
Core/Capability handshake.

Contract and OpenAPI endpoints need no bearer credential. Contract and Catalog
probes require exactly one Core header:

```bash
curl "$INFER_BASE_URL/health"
curl -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  "$INFER_BASE_URL/infer/v1/contract"
curl -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  "$INFER_BASE_URL/infer/v1/capabilities"
curl "$INFER_BASE_URL/infer/v1/openapi.json"
```

## Design boundaries

- **Intent is separate from a physical model:** an application asks for a
  capability; Runtime selects the Provider, Build, and Deployment.
- **Placement is a hard constraint:** `local_only`, offline, cloud-modality ACL,
  and fallback grants are never silently relaxed.
- **Data planes stay typed:** text uses Responses/SSE; audio and vision use
  their own typed contracts.
- **Sensitive payloads are not persisted by default:** prompts, audio, pixels,
  biometric embeddings, and Provider secrets do not enter ordinary Job metadata
  or default logs.
- **Native lifecycles remain typed:** Ollama, MLX, and ONNX retain native
  controllers while accepting shared admission, reservation, and pressure
  policy.
- **Automation fails closed:** automatic eviction is off by default; a Provider
  probe requires an explicit Operator action and may incur actual billing.

## Current stage

The implementation is closing the local Consumer hard migration for
`infer-runtime.consumer-core@20260813.1`; it is not a formal public release.
The core M1–M4 vertical slices are complete. Full traces, a 24-hour mixed soak,
more continuous Consumer use, and formal release gates remain in progress.
Except for stable text, basic audio, and event-detection contracts, ONNX/Core ML
vision, streaming audio, and the Codex subscription bridge remain experimental;
their presence in configuration never upgrades them to stable contracts.

See [`ROADMAP.md`](ROADMAP.md) for precise progress, acceptance gates, and
future M5/M6/M7 work.

## Related projects

Cross-project references use public, verifiable GitHub URLs only:

- [Infra Protocol](https://github.com/glenzli/infra-protocol): the shared local
  service-discovery contract.
- [Infra Sentinel](https://github.com/glenzli/infra-sentinel): the facility
  observer consuming redacted status snapshots.
- [Shadow](https://github.com/glenzli/shadow): a typed vision Consumer.
- [Symbiont-d](https://github.com/glenzli/symbiont-d): a local speech
  transcription Consumer.

## Documentation map

| Document | Contents |
| --- | --- |
| [`DESIGN.md`](DESIGN.md) | Product boundary, domain model, component ownership, and key flows |
| [`ROADMAP.md`](ROADMAP.md) | Stages, dependencies, risks, and acceptance gates |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | Accepted/open decisions and ADR entry points |
| [`docs/INTEGRATION.md`](docs/INTEGRATION.md) | Consumer onboarding and examples for each data plane |
| [`docs/CORE-CONTRACT-20260813.1.md`](docs/CORE-CONTRACT-20260813.1.md) | Dated Consumer Core identity and stable boundary |
| [`docs/CAPABILITY-CATALOG-20260813.1.md`](docs/CAPABILITY-CATALOG-20260813.1.md) | Independently evolving typed capability catalog and version rules |
| [`docs/MIGRATION-CONSUMER-CORE-20260813.1.md`](docs/MIGRATION-CONSUMER-CORE-20260813.1.md) | One-time hard migration for all candidate Consumers |
| [`docs/CONSUMER_DISCOVERY.md`](docs/CONSUMER_DISCOVERY.md) | Consumer Infra Discovery contract |
| [`contracts/consumer-core/20260813.1`](contracts/consumer-core/20260813.1/README.md) | Current Core OpenAPI, fixtures, and machine contract |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Console, resource lifecycle, background, and operations procedures |
| [`docs/STATUS_PROTOCOL.md`](docs/STATUS_PROTOCOL.md) | Read-only status socket, snapshot, and Discovery offer |
| [`docs/REPOSITORY_BOUNDARY.md`](docs/REPOSITORY_BOUNDARY.md) | Boundary between committed source and local configuration, credentials, models, and runtime state |
| [`docs/CONTRACT_AUDIT.md`](docs/CONTRACT_AUDIT.md) | Consumer/Operator boundary audit and remaining release gates |

## Common development commands

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Audio CLI, ONNX artifact import, model benchmarks, resource governance, and
background-operation examples live in [`docs/OPERATIONS.md`](docs/OPERATIONS.md)
and [`docs/INTEGRATION.md`](docs/INTEGRATION.md), avoiding drift between this
README and versioned contracts.

> infer-runtime is not a model abstraction layer. It is a resource orchestration layer for intelligence.
