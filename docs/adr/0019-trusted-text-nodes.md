# ADR-0019: Explicitly paired unary text nodes

Status: Accepted for an experimental unary text slice. Full M5 remains open.

## Ownership

Every machine keeps its own Runtime. The ingress Runtime owns the Consumer Job
and its final result. The execution Runtime owns its local App authorization,
queue, physical resource reservations, Provider call and cancellation. A separate
Node TLS listener shares the same Runtime instance as that machine's loopback
Consumer API; it is not a second independent resource scheduler.

`infer-node` owns the protocol, mutual TLS, paired certificate authorization,
admission leases, bounded result reconciliation and cancellation. `infer-core`
owns opt-in configuration. `infer-provider::trusted_node` adapts an admitted
Attempt to that protocol. `infer-control::trusted_nodes` composes the destination
with its existing local execution path. No separate node-agent binary is needed
for this slice; the Node service has an independent listener and lifecycle in
`inferd`.

## Pairing and execution boundaries

- TLS 1.3 with mandatory client certificates, CA/name validation and exact leaf
  fingerprint pinning on A. B maps approved client fingerprints to node identity,
  origin-App/destination-App mappings and export allowlists. There is no bearer
  forwarding, automatic trust or Consumer/admin API on this listener.
- Static addresses and explicit operator-approved imports/exports. The wire is
  `infer.node.text@20260922.1`, ALPN plus a bounded length-prefixed JSON message.
  Each RPC negotiates the exact protocol and checks destination identity.
- Only local, numeric-loopback Responses Providers with text input/output can
  be exported. Operators remain responsible for truthful local backend placement.
  Imported nodes and cloud Providers cannot be re-exported. No transitive routing.
- An export digest binds Intent, local Deployment, Build, Model Profile and
  Provider capability profile. An import pins this digest and the node certificate.
  Discovery never changes grants or admits additional models automatically.
- A retains separate Deployment IDs for local, B and C implementations. Existing
  hard constraints and policy sorting select among them. `local_only` excludes a
  trusted node even when its endpoint is loopback in a test.
- Export execution uses a configured destination App, exact local Deployment,
  `local_only`, `offline_required=true`, and `fallback=none`. The App must explicitly
  permit these constraints and that named route. Peer permission alone is not an
  App execution grant.

## Lifecycle and failure contract

Catalog is a live, authenticated view separate from the stable Consumer Capability
Catalog. Routing refreshes approved offers before admission and dispatch rechecks
identity/digest. Node generation changes invalidate all earlier reservations.

Reserve obtains a payload-free remote admission slot. These slots bound accepted
remote work, not GPU or memory ownership. Actual resource admission uses the
destination Runtime and the existing CPU/unified-memory/accelerator dimensions.
Imported Deployment resource estimates must be zero on the ingress machine;
remote estimates describe the destination and cannot reserve A's physical memory.
The general M5 Resource Pool decision remains open.

Tasks are namespaced by authenticated peer fingerprint, ingress Job ID and Attempt
number, within one destination generation. Repeated Reserve/Dispatch does not
execute twice; a changed payload under an existing key is rejected. Status renews
the five-second activity lease without extending the hard deadline (at most 120
seconds). Destination sweeps expired and revoked tasks every 500 ms. RPCs and TLS
handshakes have bounded timeouts and concurrent connections are capped at 32.

After Dispatch has been attempted, a lost reply is reconciled through Status,
never by resending Dispatch. Missing state, a changed generation or prolonged
unreachability yields `OutcomeUnknown`; ingress retry and fallback are prohibited
for that error. Ordinary task failure may follow the admitted fallback policy.
Cancellation is best-effort RPC plus lease expiry. Destination and ingress both
fence late results. This guarantees one accepted ingress result, not exactly-once
physical computation across process failures or a backend that ignores cancellation.

Plaintext payloads/results exist only in bounded process memory on the Node
transport. Frames are at most 1 MiB, retained results below 512 KiB, and at most 64
task records exist per node. Terminal records expire after 180 seconds from
reservation; exhaustion rejects new reservations rather than evicting active or
reconcilable records. Runtime SQLite retains its normal payload-free Job metadata.
Remote background recovery, streams and large artifacts are not supported.

## Validation boundary

The executable smoke starts isolated A/B/C inferd processes and deterministic
local HTTP backends, generates temporary certificates, and uses real mutually
authenticated TCP/TLS links. It proves protocol/routing/lifecycle behavior on one
host. It does not prove real model quality, Wi-Fi/LAN reachability, firewall rules,
throughput, sleep/wake, or heterogeneous machines. Those remain separate release
gates. No installed service or production credential is changed by the smoke.
