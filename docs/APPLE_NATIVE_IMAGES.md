# Apple native images (experimental)

`apple_image` wraps public Apple APIs in a bounded, one-request Swift worker and
routes requests through Infer's existing App ACL, admission, Job, deadline and
cancellation controls. Consumers send encoded image bytes, never host paths.
The worker targets Apple silicon on macOS 27 and must be built with its matching SDK.
System models remain owned by Apple; Infer records OS, worker binary SHA-256 and
request revision without inventing a model-weight digest or claiming unload control.

| Operation | Native API | Result / boundary |
| --- | --- | --- |
| `segment` | Vision `GenerateIterativeSegmentationRequest`, revision 1 | Full display-size PNG mask and confidence; up to 16 include/exclude points plus an optional box |
| `raw_render` | Core Image `CIRAWFilter`, decoder version 9 / 9.dng | Display-referred sRGB 8-bit PNG, exposure and luminance noise reduction; decoder support checked per file; local only |
| `ocr` | Vision `VNRecognizeTextRequest` | Bounded text lines, confidence and normalized boxes |
| `aesthetics` | Vision `VNCalculateImageAestheticsScoresRequest` | Native overall score and utility flag; not a calibrated photographic ranking |

Input points and boxes use normalized top-left coordinates on the EXIF-normalized
display image. Orientation is applied once. Native images are limited to 80 million
pixels. The mask is Apple's returned mask, scaled to display dimensions; it is not
advertised as calibrated per-pixel probability. Each request starts a worker, so the
first system-model invocation can be substantially slower than warm requests.
Iterative points are applied within one request; interactive session persistence,
scribbles and separate instance enumeration are not implemented.

RAW rendering is a separate display-rendering operation. It does not replace the
existing scene-linear `raw.foundation` / RawNIND contract. Apple RAW 9 support is
checked against `supportedDecoderVersions` for each input, accepting exactly `9`
or `9.dng` and never substituting RAW 8; an arbitrary DNG or RAW
extension does not prove compatibility. No generative fill, Photos Clean Up/Reframe
or headless Image Playground generation is exposed by this adapter.

## Build and explicit setup

```sh
bash tools/build_apple_image_worker.sh
printf '%s\n' '{"operation":"probe"}' | target/apple-image-worker
# Explicitly prepare Apple's segmentation assets before offline operation:
printf '%s\n' '{"operation":"prepare_segmentation"}' | target/apple-image-worker
cargo build -p inferd --offline
```

Copy `config/apple-image.example.toml` to an operator-owned config, replace the
absolute worker path, and supply `INFER_APPLE_IMAGE_TOKEN` in the environment. The
example binds a separate local port and does not modify Console's installed service.
The adapter explicitly requests segmentation asset download only in the preparation
operation. Native frameworks own their asset lifecycle; no packet capture was used
to establish a system-wide offline guarantee.
On the tested macOS build, a fresh segmentation request reports `not_ready` even
when previously prepared assets execute successfully. This lazy status is diagnostic,
not an admission gate. System model eligibility and system asset availability still
apply; Infer never accepts system license terms on the user's behalf.

## Consumer contract

- Endpoint: `POST /infer/v1/vision/apple-images`
- Core: `infer-runtime.consumer-core@20260813.1`
- Capability: `infer.vision.apple-native@20260926.2`
- Multipart fields: exactly `request` (JSON, at most 16 KiB) and `image`
  (encoded bytes, at most 100 MiB locally).
- Default placement: `local_only`. Explicit `private` permits approved paired nodes.
- Offline execution is required and fallback is `none`; cloud placement is rejected.
- The response includes a Job ID, echoed `source_revision`, chosen provider,
  deployment/build, typed result and native provenance. Local `execution_node` is
  null; ingress stamps authenticated remote node identity for paired execution.
- Successful responses use `Cache-Control: no-store`. Worker temporary images are
  deleted after execution. Native errors do not expose private filesystem paths.

For example, put this in `request.json` and use an app credential authorized for
`apple.segment`:

```json
{
  "model": "apple.segment",
  "source_revision": "photo-42-edit-7",
  "options": {
    "operation": "segment",
    "points": [{"x": 0.3, "y": 0.5, "include": true}]
  }
}
```

```sh
curl http://127.0.0.1:8788/infer/v1/vision/apple-images \
  -H "Authorization: Bearer $INFER_APPLE_IMAGE_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.apple-native@20260926.2' \
  -F 'request=<request.json;type=application/json' \
  -F 'image=@photo.jpg;type=image/jpeg'
```

The generated capability schema is checked in under
`contracts/capabilities/infer.vision.apple-native/20260926.2/openapi.json`.
The official Rust client exposes `infer_runtime_client::apple_image` with
`Client::apple_image` (file) and `Client::apple_image_bytes` (encoded preview).
It negotiates the immutable capability schema, uses existing Discovery and owner-only
credentials, defaults to local/offline/no-fallback, and validates source revision,
operation, device provenance, PNG digest/geometry and actual RAW decoder. Its bounded
response reader accommodates large RAW renders instead of the generic 4 MiB JSON cap.
Dropping the request future cancels the non-durable job. No Apple framework links
are needed by the consumer. This does not imply Windows Discovery or credentials
are implemented (see the Shadow/Windows section below).

A linked consumer example verifies Core/Capability schemas and the terminal Job:

```sh
cargo build -p infer-runtime-client --example apple_image --offline
# Discovery by default; this example requires a credential authorized for apple.*.
target/debug/examples/apple_image /absolute/credential /absolute/photo.cr3 raw_render /absolute/result.png
# Diagnostic loopback override and an explicitly authorized remote deployment:
target/debug/examples/apple_image --endpoint http://127.0.0.1:8788 --node-deployment remote_ocr /absolute/credential /absolute/preview.png ocr
```

The caller chooses the revision identifying the input snapshot; reject stale results
before applying them. Never reuse this native mask as a SAM calibrated-probability mask.

## Paired nodes

Images have a separate ALPN and envelope version,
`infer.node.apple-image@20260926.2`. Existing text protocol bytes retain
`infer.node.text@20260922.1`. Both share the existing explicit pairing, mTLS,
certificate pinning, App/export grants, leases, admission, cancellation and
unknown-outcome replay suppression. No routing metadata or host paths are forwarded.

This first image transport deliberately retains the existing 1 MiB frame and
512 KiB serialized result limits. It accepts at most **192 KiB of encoded input**.
It supports OCR, aesthetics and segmentation; it excludes RAW,
large-photo transfer, remote durable Jobs, streaming and multi-hop relay. A large
PNG mask can exceed the result bound and fail. Consumers must explicitly supply an
appropriately sized preview; Infer does not silently resize and alter coordinates.
`local_only` rejects a forced remote deployment even on the same physical machine.

Follow [Trusted node configuration](TRUSTED_NODES.md) for TLS and pairing. On B,
export a local Apple deployment, for example:

```toml
[node_server.exports.apple_ocr]
deployment = "apple_ocr"
intent = "apple.ocr"
```

Print B's offers with `inferd --config B.toml --print-node-offers` and explicitly
approve its export digest in A's `providers.node_b.node.imports`. A's imported
Build uses the same `apple_ocr` profile, image input and JSON output, with
`model_id = "apple_ocr"` (the export ID); its Deployment uses `provider = "node_b"`.
The node provider currently uses the shared catalog's `responses` capability profile,
while image execution always uses the separate image protocol. Allow `private` in
A's App request overrides, then set request metadata:

```json
{
  "infer.placement": "private",
  "infer.offline_required": "true",
  "infer.fallback": "none",
  "infer.deployment_ids": "remote_apple_ocr"
}
```

Both sides require an explicit App intent and named-route grant: include the
remote deployment in A's `apps.<app>.routing.deployment_ids` and the local export
deployment in B's corresponding grant. The destination re-admits work into its
own Runtime and forces its exact local export; a remote request cannot relay it.
`tools/smoke_apple_image_nodes.py` is a complete isolated pairing/config example.

## Validation and current host limits

```sh
python3 tools/smoke_apple_image.py --photo /absolute/photo.jpg --raw /absolute/supported.cr3 --raw /absolute/supported.dng --unsupported-raw /absolute/unsupported.nef --sdk "$PWD/target/debug/examples/apple_image" --report /tmp/apple-local.json
python3 tools/smoke_apple_image_nodes.py --photo /absolute/small-photo.jpg --sdk "$PWD/target/debug/examples/apple_image" --report /tmp/apple-nodes.json
python3 tools/smoke_trusted_nodes.py --report /tmp/text-nodes.json
# Every supplied --raw must succeed with decoder 9 or 9.dng; failures fail the harness.
```

The scripts create separate ephemeral credentials, databases, discovery directories
and daemon processes. They preserve the running production service. Receipts include
binary and native worker digests. Two processes on one host demonstrate transport and
real native execution, not cross-machine LAN availability or firewall compatibility.

The current narrowed `20260926.2` capability is verified in
[description-removal receipts](../workers/apple_image/validation-removal-20260926.json):
29 focused Rust tests, 13 local checks and 10 paired-node checks passed. Real OCR,
aesthetics, segmentation, RAW 9 / 9.dng and linked SDK calls succeeded. Removed
`describe` requests fail before admission and in the native worker; retired capability
and node protocol requests fail explicitly. The rebuilt worker no longer links
Foundation Models. All 28 earlier frozen contract digests and bytes remain unchanged.
These are focused validation results; the earlier full-suite limitations below were
not re-run or reclassified.

On macOS 27.0 (26A428), Apple M1 Pro, the worker and full Consumer API succeeded
with local OCR, aesthetics, segmentation and real RAW files. Follow-up evidence is
in [RAW 9 / SDK receipts](../workers/apple_image/validation-raw9-20260926.json);
the earlier receipt below is retained as historical evidence, including its then-open RAW check.

Follow-up checks: all 30 client library tests passed; the 10 focused image tests
(core, provider, cancellation/deadline, client) passed; all 9 image-node groups passed.
Owned-source formatting, immutable contract validation and Apple schema reproducibility passed.

| Local fixture | Decoder offered by the OS | Accepted output |
| --- | --- | --- |
| Canon EOS R CR3 | 7, 8, 9 | **9**, 6720 × 4480, HTTP 200; SDK + terminal Job verified |
| Leica M8 DNG | 6.dng, 7.dng, 8.dng, 9.dng | **9.dng**, 3916 × 2634, HTTP 200; SDK + terminal Job verified |
| Nikon Z9 NEF (two sample files probed) | 8 | RAW 9 rejected; API HTTP 400 `upstream_invalid_request`; no fallback |

A display-sized Canon render was inspected for nonblank content and geometry; these
are functional checks, not a denoising or photographic-quality benchmark. Decoder 9
advertised 69 camera models on this host; resources and support can change with OS
updates. Check each actual input, not only a camera list or filename extension:

```sh
printf '%s\n' '{"operation":"probe_raw","input_path":"/absolute/photo.NEF"}' | target/apple-image-worker
```

Foundation Models reports `deviceNotEligible`. Read-only system eligibility diagnostics
identify the device-region input as the denial, and the machine's sales-region code
is mainland China. Apple documents that M1-or-later Macs meet the chip requirement
but mainland-purchased devices are currently ineligible for Apple Intelligence.
This is a regional eligibility restriction, not inadequate M1 Pro hardware.
Apple image description is therefore temporarily removed from the worker, provider,
configuration registry, SDK and current capability schema. A `describe` request is
rejected as an unsupported operation (400), before admission or native execution.
The worker no longer imports or links Foundation Models. Contract, worker and image
node protocols advance to `20260926.2`; both paired daemons and the SDK must be updated.
Frozen v1 schema files and receipts are retained only as history; the current capability catalog does not advertise v1.
System license/region settings were not modified. Shadow's existing Qwen image
understanding is a separate capability and was not removed.

Two isolated nodes also completed real OCR, aesthetics and segmentation over mTLS;
a 512-pixel preview was explicitly prepared for the bounded transport. The linked SDK
negotiated the capability and executed remote OCR with authenticated node provenance.
Visual inspection of the earlier 960 × 640 local mask confirmed the seeded person was
selected with matching display orientation. Same-host processes do not establish
physical LAN or Windows interoperability.

Validation on 2026-09-26 (Asia/Shanghai), with [recorded binary identities and receipts](../workers/apple_image/validation-20260926.json):

- Apple core contract/config tests: 3 passed; provider boundary tests: 2 passed.
- Native cancellation/disconnected-waiter and deadline tests: 2 passed.
- Node library: 8 passed, including protocol binding and changed-payload replay.
- API suite: 58 passed, 13 pre-existing environment-dependent tests ignored.
- Control suite: 98 passed; one pre-existing image-generation routing assertion
  failed (expects one configured candidate, current fixture has three).
- The broader core run exposed a pre-existing Shape App grant assertion: it expects
  two intents while the checked-in fixture grants three. Both that assertion and
  the fixture are unchanged from HEAD. Focused native-image checks pass.
- Image-node harness: 8 groups passed. Text-node regression: all 21 groups passed.
- Workspace compile check, source format check, generated schema reproducibility,
  and byte/digest checks for all previously published contracts passed.

Full-suite status is therefore not clean; the two existing fixture assertions are
not reclassified as passes. RAW functional acceptance is now complete for the two
supported fixtures above; a real second machine remains open.
The installed Console-managed daemon was not switched to this experimental build.

## Shadow integration and Windows boundary

The SDK module and linked example are ready for consumer integration. Shadow itself
has not been enabled for these new operations in this checkout. Its current SDK
pin predates this capability; use an immutable published Infer revision when changing
that dependency, rather than committing a developer-specific absolute path.

The source review identifies these product boundaries:

- **Aesthetics / OCR** are the smallest first integration. Submit an explicit,
  orientation-normalized preview (at most 192 KiB for paired execution). Retain photo
  and edit revision, operation, system/worker revision and execution-node provenance.
  Keep aesthetics as temporary review evidence; it is neither human stars/Reject nor
  a calibrated technical score or personal preference. Never log photo payloads.
- **Iterative subject mask** needs an explicit Apple route in Shadow's existing mask
  preview/apply workflow. The Apple PNG has different semantics from the current SAM
  2.1 sigmoid-probability contract. Do not forge weight hashes or insert it as a silent
  SAM fallback. Convert coordinates from the exact submitted preview, cancel on edit
  changes, reject stale evidence, and preserve undo/explicit apply.
- **RAW 9** is a rendered sRGB derivative. Keep it separate from scene-linear
  `raw.foundation` / RawNIND and original-file identity. Current paired transport does
  not carry full RAWs; remote RAW requires a separately bounded artifact-transfer
  contract before it can become a Windows editing route.
- **Description** is temporarily removed from this adapter. Other established Shadow image
  understanding providers continue independently.

The intended route is Shadow → local Infer SDK/daemon → explicitly paired Mac →
Apple system API. The Apple call is remote from the consumer, but the current Infer
Windows implementation still fails closed for two real prerequisites:

1. `infer-runtime-client` does not implement Windows owner/ACL validation for managed
   credential files or Infra Discovery registrations.
2. `infer-node` rejects non-Unix TLS credential ownership checks; therefore a Windows
   ingress cannot yet load a paired-node identity.

Removing these guards, copying a bearer token into an unrestricted file, or exposing
the loopback API directly on the LAN would not complete Windows support. Implement
handle-based Windows ownership/DACL/reparse-point checks and secure credential
creation, then test discovery, pairing/revocation, cancellation and one real consumer
request on Windows + this Mac. No Windows device was available in this task; no
Windows or production Shadow UI support is claimed by these receipts.

## Apple references

- [Vision segmentation](https://developer.apple.com/videos/play/wwdc2026/237/)
- [Apple Intelligence requirements and regional availability](https://support.apple.com/zh-cn/121115)
- [Foundation Models device and region availability](https://developer.apple.com/documentation/foundationmodels/systemlanguagemodel)
- [Core Image RAW 9](https://developer.apple.com/videos/play/wwdc2026/305/)
- [macOS developer changes](https://developer.apple.com/macos/whats-new/)
- [Image Playground changes](https://developer.apple.com/news/?id=dz9wvq0r)

Image Playground's UI and Private Cloud Compute generation are not equivalent to an
unattended local image model service. The discontinued `ImageCreator` path is not
advertised as a macOS 27 backend.
