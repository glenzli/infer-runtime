# Windows Shadow editing models

The official Windows SDK reaches the installed, same-user WSL Runtime. Shadow
keeps image decoding, Recipe state, candidate approval, source revision checks,
cache publication, undo/redo and export. Runtime owns model execution and returns
the actual Build, artifact, preprocessing, postprocessing and execution provider.

## Qualified model identities

| Intent | CUDA Build | Execution |
| --- | --- | --- |
| `vision.segment_subject` | `sam21_small_torch_cuda_fp32_v1` | PyTorch 2.8.0+cu128 / Transformers 4.56.2 |
| `vision.ground_semantics` | `grounding_dino_tiny_onnx_cuda_static_fp32_v1` | ONNX Runtime 1.27.0, CUDA 12 |
| `vision.parse_face` | `bisenet_resnet18_face_parsing_onnx_cuda_static_v1` | ONNX Runtime 1.27.0, CUDA 12 |
| `vision.complete_image` | `lama_inpainting_onnx_cuda_static_fp32_v1` | ONNX Runtime 1.27.0, CUDA 12 |
| `raw.materialize_foundation` | `rawnind_ort127_cuda_exp1` | ONNX Runtime 1.27.0, CUDA 12 |

The ONNX Builds allow CUDA only, disable CPU fallback and TF32, and fail session
admission if any executable node is assigned to CPU. The separate legacy CPU and
Core ML Builds retain their existing identities and contracts.

The source Grounding DINO, LaMa and BiSeNet exports contain shape operations which
prevent strict CUDA admission. `tools/export_shadow_editing_cuda.py` verifies the
original graph digest, fixes batch/image dimensions, and applies ORT basic graph
optimization in a separate CPU environment. No learned weight is replaced.
Grounding DINO also uses a distinct fixed-256 tokenizer contract. Two negative
Shape guards are folded by their nonnegative-dimension invariant; four boolean
Tile operations use exact bool-to-float32-to-bool casts. All image inference in
the resulting Build remains CUDA. Build-time optimization is not CPU inference
fallback.

LaMa's upstream graph contains 299 constant blocked-INT8 dequantization nodes
which the CUDA kernel cannot execute. The exporter evaluates only these constant
weights with the pinned CPU ORT and stores their exact FP32 values as initializers.
The distinct `cuda_static_fp32_v1` graph performs no runtime dequantization or CPU
work. Its image output is checked against the original CPU graph.

The derived artifact digest includes these graph changes. Original exports,
derived graphs and local export manifests stay outside the source checkout.
Their `infer-export:` source identity refers to this explicit derivation rather
than implying that the derived bytes can be downloaded from the upstream URL.

BiSeNet's pretrained weights retain
`MIT AND LicenseRef-CelebAMask-HQ-NC-Research` and restricted license status.
This setup does not grant a commercial license for those weights.

## Prepare without changing a running service

Reuse the installed ORT 1.27.0 **CUDA 12** archive and NVIDIA user-space libraries.
The CPU export environment is separate from that Runtime library. Do not replace
it with the default CUDA 13 Python wheel or install a Linux NVIDIA kernel driver.

1. Stage pinned public sources with `tools/prepare_shadow_editing.py`, using an
   external `--staging-root`, `--cuda-grounding` and optionally `--raw-foundation`.
2. In a separate environment pinned by `tools/requirements-editing-export.txt`,
   run `tools/export_shadow_editing_cuda.py --source-root STAGING --output-root
   STAGING/cuda-static-v1`. Each output must match its fixed digest and size.
3. Prepare the pinned SAM snapshot using `tools/prepare_shadow_sam.py`; use the
   existing CUDA Python environment and `tools/requirements-torch-sam-runtime.txt`.
   Publish the verified local-worker bundle with the existing
   `publish_local_worker_build` operator example before activation.
4. Generate a new configuration with `tools/configure_shadow_editing.py --source
   CURRENT --output NEW --runtime-root RUNTIME_ROOT`. The output must not already
   exist. Existing App credentials, observer identity and Sentinel permissions
   are preserved. The default worker path is in the installed release.
5. Use `infer --config NEW import-onnx BUILD --file GRAPH` for each derived Build,
   then `import-onnx-auxiliary grounding_dino_tiny_onnx_cuda_static_fp32_v1 tokenizer
   --file TOKENIZER`. Imports use the normal content-addressed Artifact Store.
6. Install qualified binaries with `tools/install_wsl_service.sh`; it includes
   both the RAW bridge and SAM worker and records their digests. Promote the
   reviewed config and restart the daemon/console after the active work finishes.
   The Windows host must come from the same checkout to publish the RAW offer.

`--cpu-reference` selects the original three CPU editing Builds. `--raw-cpu`
retains the original ORT 1.27 experimental CPU RAW identity. Neither option
relabels a CUDA artifact or widens an immutable Build's execution providers.

## Runtime lifetime and typed consumer checks

SAM keeps one model and one image embedding for responsive refinement. It evicts
both after 30 seconds idle. Cancellation stops and waits for its worker before
releasing the request. ONNX sessions use the configured 30-second idle limit;
active references prevent eviction. CUDA RAW sessions are created for the Job
and released after success or cancellation. Ollama retains its existing
30-second keep-alive. Actual coexistence still depends on the selected Qwen size,
image shape and available GPU memory.

The SAM soft mask is native 256×256 Gray8 sigmoid output. Its separate binary
result is resized to the input image dimensions. The exact Torch provenance is:

- provider `torch-sam-local`, Build/deployment `sam21_small_torch_cuda_fp32_v1`;
- artifact set `36c9535ba0a7bd7240fac01563b97c22e1a5230d5f07cd7ba8c69a49c8cbf119`;
- runtime `transformers-4.56.2-torch-2.8.0-cu128`, precision `float32`;
- preprocessing `sam21_transformers_exact1024_rgb_imagenet_v1`;
- postprocessing `sam21_torch_best_iou_native256_sigmoid_binary_bilinear_v1`;
- requested/actual provider `cuda`, no fallback reason.

Consumers must add this independent profile to their strict validation. They
must not accept it under an old Core ML cache identity. Source revision is bound
by the typed response's top-level `source_revision`; the common VisionProvenance
schema does not add `source_id`, `exact_revision` or `api_contract_revision`.

The initial public-image acceptance uses OpenCV's pinned Lena sample. It checks
native Windows SDK requests, output digests and shape, meaningful foreground and
background, actual click refinement, nonempty grounded regions, 19-class face
labels, and exact preservation of all unselected completion pixels. This is a
functional sample acceptance, not an exhaustive image-quality benchmark.

On the initial RTX 4070 Ti / 12 GiB setup, the native Windows SDK measured SAM
cold start 5.6 seconds and warm refinement 120 ms; Grounding DINO returned three
regions in 27.7 seconds; BiSeNet returned the 19-class map in 2.16 seconds. LaMa
completed in 8.44 seconds: all 3,721 selected pixels changed and every unselected
pixel remained exactly equal. These measurements include cold session loading
and local transport and do not establish a latency SLO.

All three derived graphs were independently exported twice with matching
digests. CPU checks against the original graphs found complete BiSeNet label
agreement (maximum logit difference `9.54e-7`), LaMa maximum output difference
`1.53e-4`, and Grounding DINO maximum valid-token logit difference `9.88e-4` /
box-coordinate difference `1.52e-4` after fixed-length text padding. Small numeric
differences do not permit cache identity reuse across these Builds.

For RAW source/layout checks, exact cache identities and measured CPU/CUDA
numerics, see [Windows RAW Foundation](WINDOWS_RAW_FOUNDATION.md).
