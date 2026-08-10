# `infer-runtime.status` Protocol

Status: canonical local contract

Protocol: `infer-runtime.status`

Protocol version: `20260810.1`

This document owns Infer Runtime's read-only local status request, response,
framing, errors, and completion semantics. It is independent of Infra Discovery.
The discovery publisher conforms to `infra.discovery.registration@20260810.1`
as frozen at infra-protocol commit `555f024`.

## Unix stream contract

The automatic discovery offer is:

- protocol: `infer-runtime.status`
- protocol version: `20260810.1`
- binding: `infra.local.unix-socket`

Before reading application bytes, the server obtains the connected peer's
effective UID and requires it to equal the server's effective UID. The socket
parent is owner-only mode `0700`; the socket is mode `0600`.

One connection carries exactly one request and one response:

1. The client writes one UTF-8 JSON document followed by LF.
2. The request, including LF, is at most 512 bytes.
3. The server parses JSON semantically. Object field order and insignificant
   whitespace do not matter; unknown fields are rejected.
4. The server writes one UTF-8 JSON document followed by LF.
5. The response, including LF, is at most 256 KiB.
6. The server closes its write side and connection after that frame. EOF is
   part of successful completion; a trailing frame or bytes are invalid.

The request receive timeout is two seconds. A connection cannot issue a second
operation.

## Request

Schema: `infer-runtime.status.request`

```json
{"schema":"infer-runtime.status.request","schema_version":"20260810.1","operation":"snapshot"}
```

All three fields are required. The only operation in this version is
`snapshot`.

## Snapshot response

Schema: `infer-runtime.status.snapshot`

```json
{
  "schema": "infer-runtime.status.snapshot",
  "schema_version": "20260810.1",
  "service": {
    "kind": "infer-runtime",
    "instance_id": "local",
    "generation": "gen_0123456789abcdef0123456789abcdef"
  },
  "sequence": 1,
  "captured_at": "2026-08-10T12:00:00Z",
  "status": {
    "state": "healthy",
    "reason_codes": []
  },
  "headline_metrics": [
    "infer.workload.active_attempts",
    "infer.workload.queued_jobs",
    "infer.resources.pressure"
  ],
  "metrics": [
    {
      "id": "infer.workload.active_attempts",
      "kind": "gauge",
      "value": 0
    }
  ],
  "issues": [],
  "extensions": {
    "infer-runtime": {}
  },
  "links": {
    "console_url": "http://127.0.0.1:8790/"
  },
  "redaction": {
    "excluded": [
      "credentials",
      "filesystem_paths",
      "job_identifiers",
      "job_metadata",
      "payloads",
      "raw_errors",
      "usage_ledger"
    ]
  }
}
```

`service.kind`, `service.instance_id`, and `service.generation` must exactly
match the selected live discovery registration. A new process start always
uses a new generation. `sequence` is monotonic only within that generation.

`status.state` is one of `starting`, `healthy`, `degraded`, `unavailable`, or
`stopping`. Issue severity is one of `info`, `warning`, or `critical`.

`headline_metrics` contains at most three metric ID strings, and every ID must
exist in `metrics`. A metric value is never JSON null; an unknown metric is
omitted. Consumers must ignore unknown response fields, metric IDs, reason
codes, issue codes, and extension members.

`links` is optional. When Infer Runtime has a loopback Console URL configured,
`links.console_url` provides a human deep-link. It is application data and is
never included in Infra Discovery.

The snapshot never exposes Job IDs, payloads, request metadata, raw provider
errors, credentials, filesystem paths, or the full accounting ledger.

## Error response

Schema: `infer-runtime.status.error`

```json
{
  "schema": "infer-runtime.status.error",
  "schema_version": "20260810.1",
  "error": {
    "code": "invalid_request"
  }
}
```

Stable codes in this version:

- `invalid_request`: framing, size, JSON, schema, version, operation, unknown
  field, or extra request frame is invalid.
- `snapshot_unavailable`: snapshot collection or bounded serialization failed.

A peer UID mismatch is rejected before application bytes are exchanged, so it
does not receive an application error envelope.

## Infra Discovery publication

The publisher uses the platform runtime root defined by Infra Discovery.
`INFRA_PROTOCOL_RUNTIME_DIR` may override the final absolute root. The retired
`INFRA_SENTINEL_REGISTRATION_DIR` variable is not read.

On Unix the layout is:

```text
<runtime-root>/
├── registrations/
│   └── infer-runtime--local.json
└── sockets/
    └── ir-<process-unique>.sock
```

The manifest contains only `service`, `lease`, and protocol `offers`. The Unix
endpoint is relative to the runtime root. Directories are owner-only mode
`0700`, manifests and sockets are mode `0600`, and manifest replacement is
atomic. The publisher normally renews every 15 seconds with a 45-second TTL.

Infer Runtime holds an exclusive per-service publication authority before its
first manifest write. During handoff, the predecessor stops all writes and
releases that authority before the successor publishes. An old unexpired
manifest may then be atomically replaced by the successor's new generation.
Shutdown never unlinks the stable manifest; it is left to expire naturally.
Only the process-unique socket is removed after it closes.

Example offer:

```json
{
  "schema": "infra.discovery.registration",
  "schema_version": "20260810.1",
  "service": {
    "kind": "infer-runtime",
    "instance_id": "local",
    "generation": "gen_0123456789abcdef0123456789abcdef"
  },
  "lease": {
    "renewed_at": "2026-08-10T12:00:00Z",
    "expires_at": "2026-08-10T12:00:45Z"
  },
  "offers": [
    {
      "protocol": "infer-runtime.status",
      "protocol_versions": ["20260810.1"],
      "binding": "infra.local.unix-socket",
      "endpoint": "sockets/ir-0123456789ab.sock"
    }
  ]
}
```

Discovery does not publish a token, HTTP route, PID, product version, display
name, Console URL, request schema, framing rule, or status metric.

## Explicit HTTP diagnostic fallback

`GET /infer/v1/observer/snapshot` remains available as a separately configured
diagnostic route. It returns the same snapshot JSON with `Cache-Control:
no-store`, but HTTP provides its own framing and completion semantics.

It requires an App with `observer_access = "summary"`. That credential cannot
invoke inference, Job, resource, provider, budget, or operator mutation
surfaces. The HTTP route is not advertised by the automatic discovery offer.
`observer.http_credential_id` may reserve that App against accidental Console
revocation; the identifier remains daemon-local and is not serialized into
Discovery or the status snapshot.
