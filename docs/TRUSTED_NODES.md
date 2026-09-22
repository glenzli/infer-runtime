# Trusted text nodes (experimental)

This opt-in slice lets a local Runtime execute unary text requests on explicitly
paired Runtime nodes. Default configurations keep the existing loopback-only
Consumer surface. Audio, vision, streams, durable remote Jobs, automatic LAN
discovery and large artifact transfer are not enabled.
The initial credential-permission implementation supports Unix (macOS/Linux).
Other platforms reject Node credential loading until owner/ACL validation exists.

## Same-host acceptance test

```sh
cargo build -p inferd
python3 tools/smoke_trusted_nodes.py --report /tmp/infer-node-smoke.json
```

The script requires Python 3 and OpenSSL. It starts three separate inferd processes
with separate identities, randomly selected loopback ports, databases, credential
directories and `INFRA_PROTOCOL_RUNTIME_DIR` locations. It creates test-only CA
and leaf certificates in an owner-only temporary directory. All processes and
temporary credentials are cleaned up. Local deterministic HTTP backends return
A/B/C markers; no model installation is required or implied.

Forced named routing to `b_text` proves the result came through B's Node listener.
Concurrent held requests to B and C prove both backends start before either is
released, while each node accounts for its own admission slot.
Filling B's admission slots must leave C eligible for a trusted-node request.
The same request with `local_only` must fail. The test also covers overlapping
capabilities, `anywhere`/`cloud_only` placement boundaries, bounded node payloads,
C selection while B is offline, rejoin, authorization, revocation,
protocol mismatch, lost dispatch acknowledgement, duplicate/altered replay,
concurrent reservation exhaustion/expiry, cancellation, late output and B crash without
automatic fallback to C. A machine allowing loopback TCP binds is required.

## Verified on 2026-09-23 (Asia/Shanghai)

The current linked `inferd` passed all 21 same-host acceptance groups, including
certificate/name/digest rejection, explicit App grants, running lease expiry,
unknown-outcome replay suppression, authenticated operator probes and authorized
fallback after confirmed failure. Oversized requests are filtered before remote
routing, while an authorized local candidate remains usable.
The harness records the executable and script SHA-256 in its JSON report.

- Executable SHA-256: `8e89e4bd8743c1fdfffe934e2ee1a97e87b874ff7883c74de45ec9ca2dd02753`
- Harness SHA-256: `7e8b0945281ec8ca4c4bf85829cb6287173d9f281ec8823d817a28eb8480863a`
- `cargo build -p inferd --offline`: current binary linked successfully.
- `cargo test -p infer-node -p infer-control --offline`: 6 node and 91 control tests passed.
- `cargo fmt --all -- --check` and Python syntax parse: passed.

The previous source snapshot passed `cargo test -p inferd --offline` (2 daemon tests)
and focused daemon/node Clippy. Its broader workspace run is retained as baseline
evidence for unchanged code, not as validation of the current routing change:

- `cargo test --workspace --offline -- --skip tests::migration_backfills_priority_for_existing_job_metadata`:
  435 passed, 24 ignored, one known baseline test filtered out.

The unfiltered workspace gates are not clean: the existing store migration test
still expects schema version 8 while the implementation uses version 9; strict
Clippy reports existing argument-count warnings in the vision client, and focused
core Clippy reports the existing manually implemented default. This slice does not
change the store or vision client. These exceptions are not node validation passes.

The dummy processes, credentials and databases were removed after the test.
Cross-machine LAN and real-model validation remain outstanding.

## Configure B to export a local deployment

Use distinct node certificates with TLS server/client EKUs, valid DNS SANs and a
trusted CA. B's private key and pairing file must be regular owner-only files
(`0600` on Unix). Private keys, pairing grants and real machine configuration must
remain outside Git. Copy and verify certificate fingerprints over a trusted path.

Add to B's existing Runtime configuration (replace example paths and IDs):

```toml
[node_server]
node_id = "b"
bind = "192.168.1.20:8843"
peers_file = "/absolute/private/b-peers.json"
max_active = 2

[node_server.tls]
certificate = "/absolute/private/b.pem"
private_key = "/absolute/private/b.key"
ca_certificate = "/absolute/private/ca.pem"

[node_server.exports.summary]
deployment = "local_summary"
intent = "text.summarize"
```

The existing `local_summary` Deployment must point to a local text Responses
Provider using a numeric loopback HTTP endpoint. It must support unary execution
and its Model Profile must assess the exported Intent. Its configured App must
grant `local_summary` through `apps.APP.routing.deployment_ids` and allow
`local_only`, `offline_required` and `fallback=none` request overrides.

The pairing file is JSON keyed by A's lowercase SHA-256 **leaf certificate DER**
fingerprint. For example, after replacing the placeholder with 64 hex characters:

```json
{
  "A_LEAF_CERTIFICATE_SHA256": {
    "node_id": "a",
    "apps": {"a-consumer": "b-delegated-app"},
    "exports": ["summary"]
  }
}
```

The authenticated A node may act for `a-consumer` only through B's
`b-delegated-app`. It does not receive B's App token. To revoke pairing, atomically
replace the owner-only file with a version without A's entry. New RPCs fail
authorization; existing tasks are cancelled by the next lease sweep. Invalid or
unreadable grants fail closed. Changing TLS keys or exports requires a restart.

Print B's payload-free export records before starting it:

```sh
target/debug/inferd --config /absolute/private/b.toml --print-node-offers
```

Record the `summary` export's contract digest. This command describes configured
exports; actual availability is checked over the authenticated Node connection.

## Configure A to import B

```toml
[providers.node_b]
kind = "trusted_node"
placement = "trusted_node"

[providers.node_b.capability_profile]
version = 1
protocol = "responses"
capabilities = ["responses", "instructions", "max_output_tokens", "metadata"]

[providers.node_b.node]
node_id = "b"
address = "192.168.1.20:8843"
server_name = "b.example.internal"
certificate_sha256 = "B_LEAF_CERTIFICATE_SHA256"

[providers.node_b.node.imports]
summary = "B_SUMMARY_EXPORT_CONTRACT_SHA256"

[providers.node_b.node.tls]
certificate = "/absolute/private/a.pem"
private_key = "/absolute/private/a.key"
ca_certificate = "/absolute/private/ca.pem"

[model_builds.summary_on_b]
profile = "approved_summary_profile"
model_id = "summary"
input_modalities = ["text"]
output_modalities = ["text"]

[deployments.b_summary]
provider = "node_b"
build = "summary_on_b"
supported_execution_modes = ["unary"]
```

Use actual 64-character digests, and a `server_name` present in B's certificate.
`model_id` names the remote export, not a backend model. Keep A's imported resource
estimates at zero: B reserves its own physical resources. Configure a local Model
Profile/Intent assessment and grant `b_summary` to the appropriate App. Advertised
capabilities never create an App grant automatically. Define C as a separate
Provider and Deployment, even when it exports the same name/model.

From A, probe every configured import through the real mutual-TLS Node connection:

```sh
target/debug/inferd --config /absolute/private/a.toml --probe-nodes
```

The command prints payload-free JSON with each Provider's import match and current
admission slots. It exits nonzero if no trusted peers are configured, a peer is
unreachable or unauthorized, an approved export digest is absent, or the peer has
no free admission slot. The probe does not submit a Job or prove model execution.

Applications still connect to A using their existing credentials. An authorized
request can set:

```json
{
  "model": "text.summarize",
  "input": "Summarize this text.",
  "metadata": {
    "infer.placement": "private",
    "infer.deployment_ids": "b_summary",
    "infer.fallback": "none"
  }
}
```

Without named narrowing, existing policies select between eligible local/B/C
Deployments. `infer.prefer=trusted_node` is a preference within the allowed set;
`local_only` remains a hard prohibition on B/C. A failed or stale catalog cannot
admit a remote Deployment. A contract change requires explicit import approval.
Unary text requests that exceed the node wire limit are excluded from trusted-node
candidate planning. An allowed local Deployment can still run such a request;
explicitly narrowing to an oversized node request returns `no_candidate` before
dispatch. The node client also checks the limit as a final transport guard.

Node requests are capped at 120 seconds. A shorter ingress deadline still wins.
If the response says the remote outcome is unknown, do not automatically resubmit:
the backend may already have performed computation. The first slice deliberately
has no cross-restart replay. See [ADR-0019](adr/0019-trusted-text-nodes.md) for bounds
and ownership, and [M5](../ROADMAP.md#8-m5可信远程节点) for remaining multi-node gates.

The frozen Consumer error taxonomy remains unchanged: indeterminate execution is
reported as `upstream_protocol` with a payload-free explanation. Consumers must
not infer permission to replay remote work from HTTP 5xx alone or parse the human
message as a machine contract. Runtime automatic retry/fallback is suppressed for
this case. A dedicated cross-node public recovery contract is still a future gate.
