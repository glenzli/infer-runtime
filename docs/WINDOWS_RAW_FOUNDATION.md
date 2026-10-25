# Windows / WSL RAW Foundation binding

The official Rust SDK supports native Windows callers of the existing
`infer.raw-foundation@20260811.1` API. The Runtime still runs in WSL and receives
the original UID-checked Unix descriptor lease. Core and capability HTTP schemas
are unchanged.

An installed Windows host publishes an additional private Discovery offer:

```json
{
  "protocol": "infer-runtime.artifact-lease",
  "protocol_versions": ["20260811.1"],
  "binding": "infer-runtime.wsl-stdio-scm-rights",
  "endpoint": "wsl-stdio://Ubuntu-24.04/home/USER/.local/lib/infer-runtime/current/raw_foundation_wsl_bridge.py"
}
```

The endpoint identifies installed code. It never identifies a photo, sample file,
or output path. This dated binding accepts literal installation paths without
URI escapes, queries or fragments. The normal SDK builder discovers it from the
same owner-only registration as Consumer Core. An explicit
`RawFoundationWslBridge::new(distribution, installed_script)` builder override is
available for isolated diagnostics.

## Ownership and lifetime

`register_raw_foundation_handles(&grant, &input, &output)` inspects the supplied
open Windows objects, including owner SID, private DACL, regular-file type, link
count, access rights and volume/file identity. Input must be read-only; output
must be empty, writable without append-only access, distinct from input and
exclusively lockable. Current-user and SYSTEM maintenance access are accepted;
other principals are rejected. No source or output path is reopened.

The SDK launches the system `wsl.exe` with explicit arguments and inherited
private pipes. `raw_foundation_wsl_bridge.py` runs under that user's WSL identity,
verifies the Unix socket's private parent, owner and peer UID, and registers two
already-open, owner-only regular files using the unchanged one-shot ticket.
App, Job, daemon generation and expiry stay owned by the original registry.

The versioned `infer.raw-foundation.wsl-stdio@20261025.1` framing belongs only to
this private data channel. HTTP carries bounded descriptors and lease identity.
The adapter copies through 64 KiB buffers into temporary Linux staging and then
back into the caller's unpublished Windows output. This is a copying bridge;
the 64 KiB transfer buffer is not a claim about total model RSS. The binding
admits at most 512 MiB of input samples and 2 GiB of output.

Registration is bounded by ticket expiry, at most 30 seconds for this RAW API.
Unused native handles are released on expiry. Execution retains the model's
deadline and explicit cancellation; collection is bounded to 120 seconds and
verifies the complete file size and SHA-256 against the authenticated receipt.
Failure, cancellation and an abandoned collection clear the unpublished native
output. Linux staging is removed on collection, EOF or its bounded timeout.
The caller retains final verification, source-revision checks, cache publication
and deletion of its unpublished path.

## Preparation and smoke

Install a release with `tools/install_wsl_service.sh`, which includes the private
bridge and records its digest in `install-manifest.json`. Update the Windows
host from the same checkout so its Discovery offer references that installed
bridge. The Python adapter needs only the WSL system Python standard library;
it does not install drivers or inference dependencies.

The SDK example `raw_foundation` accepts a private credential file, Discovery
root, staging descriptor JSON, an existing read-only sample file, and a new
unpublished output. Its `cancel` and `cancel-running` modes check that no output
is published. It prints receipts and bounded diagnostic summaries, never tickets,
credentials or sample values.

## Verified source and exact cache identity

The initial real-source acceptance uses rawpy's public NASA Nikon D3S fixture at
`letmaik/rawpy@d050d8a1a061d84e8d4eb6bf3f68a22640aeade3/test/iss030e122639.NEF`.
Its source SHA-256 is
`5922721d13f11795557d97fdeb0a60b900086c402bc82a848ff280d15b99ffd4`.
Read-only LibRaw 0.21.4 / rawpy 0.25.1 decoding produced 4284×2844 RGGB samples
with SHA-256
`328155ed45c8d8fc403cd0ae5b2f507ec19502af4f94c67da7a2f8f8937aac1f`.

The existing CPU Build remains `rawnind_ort127_exp1`, implementation
`rawnind-public-bayer-foundation-ort127-exp1`, cache
`rawnind-cpu-ort127-exp1`, ORT 1.27.0 FP32 / `CPUExecutionProvider`.
Linux direct execution and native Windows SDK execution produced identical
146,208,248-byte artifacts, SHA-256
`362822e7f13e41b86e01f217911be8602886a8a8cde43d2f4c8bc8b8664e1bbf`.
Both receipts reported 80 tile inferences, 824 maximum accumulator rows and zero
explicit full-output buffers. The native request took approximately 55 seconds
on this machine. Cancellation before execution and during execution passed;
the running cancellation completed in approximately 1.24 seconds.

Runtime consumes decoded `uint16-le-row-major-active-bayer` staging, not NEF or
another camera container. The supported descriptor layouts are RGGB, BGGR,
GRBG and GBRG, with per-site black/white levels and actual-read sample digest.
X-Trans, non-Bayer, floating-point, interleaved or otherwise incompatible staging
must fail closed. This D3S result establishes one real camera source; another
camera's decoder and pixel contract must be independently verified before
claiming support. Original photos are never modified by this binding.

## Independent ORT 1.27 CUDA Build

The CUDA variant is `rawnind_ort127_cuda_exp1`, implementation
`rawnind-public-bayer-foundation-ort127-cuda-exp1`, cache
`rawnind-cuda-ort127-exp1`. It uses the same pinned graph and algorithm with
FP32, TF32 disabled, and CPU fallback disabled. Artifact execution metadata is
exactly engine `onnxruntime`, runtime version `onnxruntime-1.27.0`, requested
provider `cuda` and active providers `["CUDAExecutionProvider"]`. CPU and legacy
1.24.4 artifacts remain separate and cannot be cross-labeled.

The same D3S source completed through the native Windows SDK in 45.566 seconds,
producing 146,208,255 bytes with file SHA-256
`563a9d0231906f25cdb147543603c76dc27fc9ec35f65e633d22d1b862dce90e`.
Its 80 tile inferences retained the same 824-row accumulator limit and zero
explicit full-output buffers. The CUDA cache key is
`e9287ffc4bf43b0901782fe17756116292100c3f1da4f381a164a0f2d19e5f9a`.

An independent reader verified both artifact footers, manifests and every stripe
digest, then compared all 36,551,088 float values. All were finite; maximum
absolute CPU/CUDA error was `4.76837158203125e-7`, mean absolute error
`2.2915429183070495e-9`, and RMSE `5.441726989071746e-9`. No value differed by
more than `1e-5`. These are measurements for this fixture and machine, not a
universal numerical tolerance or a camera support claim. CUDA sessions are
released after each Job so they do not retain the RAW model between requests.

Native Windows lifecycle checks on the same CUDA Build also passed: a 5-second
deadline ended the running Job as `expired` / `deadline_exceeded` in 5.40 seconds;
explicit running cancellation ended as `cancelled` in 1.17 seconds; aborting the
in-flight HTTP operation automatically cancelled its Job in 1.23 seconds. Each
check verified zero native output bytes and deletion of its unpublished partial.
An unused lease expired and released its native file lock after approximately
31 seconds. These times include registration and test setup. A real-model HTTP /
SCM_RIGHTS regression also verifies that a rejected duplicate execution request
cannot cancel the original running Job. Reservations remain held until the
native worker's cancellation cleanup completes.
