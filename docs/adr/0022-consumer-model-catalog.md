# ADR-0022: Consumer-scoped text model discovery

Status: Accepted. Date: 2026-10-16.

`infer.model-catalog@20261016.1` adds the read-only `GET /infer/v1/model-catalog`
capability without modifying immutable Consumer Core. A normal App credential
returns only named unary text deployments granted for that App's Intents,
provider access classes and cloud text egress. Observer-only identities are denied.
No operator credential, account information or provider-native inventory is exposed.

The projection carries deployment reasoning efforts, per-Intent capability levels
and the last subscription availability observation. Reads do not create Jobs,
refresh providers, mutate routing, or admit newly discovered models. Unknown or
stale observations are separate from authorization and do not prove inference
readiness. Providers retain their bounded background observation cadence.

Consumers refresh when opening settings and may cache briefly. Refreshing must
preserve unsaved selections, custom deployment IDs and historical configuration;
unsupported efforts remain visible and require an explicit correction. Model
selection is an operator decision and cannot change solely because a new version
appears in discovery.

GPT-6.1 Sol is an explicit additional profile/build/deployment. Capability ratings
remain provisional. Applications must receive an explicit named deployment grant
per relevant Intent before listing or invoking it. Adding it never changes other
Apps or their baseline routing.
