# ADR-0021: Optional EmbeddingGemma 2 unified media retrieval

- Status: Accepted as an optional experimental provider
- Date: 2026-10-13

Qwen3 remains the default for `semantic.embed_query` and
`semantic.embed_documents`. EmbeddingGemma 2 serves a separately selected media
index. Its text, image, audio and visual-video vectors share one exact 768d space.
A text query for that index uses the Gemma text encoder and SearchQuery prefix;
Qwen3, SigLIP and CLAP vectors remain independent. Consumers own segmentation,
indexes, freshness and result ranking.

The independent `infer.media.embedding@20261013.1` capability has four bounded,
typed routes: query JSON, JPEG/PNG image multipart, audio multipart and video
multipart. Consumer inputs never contain physical models, paths, URLs, tensors,
decoder arguments or execution providers. Existing capability artifacts and
Consumer Core remain immutable. Registration alone does not grant an App access.

The first Build pins the MLX community BF16 checkpoint
`1a4ffddb7905d3f63486748deabe091a01fb6201`, converted from Google's upstream
`914f7f89142e33e77833254d9c9b90c3cef7303b`. Fourteen execution/configuration files
have a per-file SHA-256 manifest and aggregate identity in ArtifactStore. Model
weights and private configuration stay outside Git.

The isolated runtime pins MLX 0.32.3, MLX-VLM implementation
`3d87e88402f307efbf68e568971aa887ee7d9ed0`, and Transformers processor
`e880ddc57baa4e642c357e7c0a8e77b8d7f1b3f0`. The released Transformers package
used during the initial evaluation lacked the native processor. Serving requires
the exact public development revision and Metal; it has no CPU/cloud fallback,
request-time downloads or remote custom model code. BF16 weights produce finite
FP32, L2-normalized vectors. Inputs exceeding the official 8192-token context
are rejected rather than silently truncated.

One process owns the model, serialized requests and native residency. Resource
Manager owns admission and eviction. Cancellation, deadline, protocol failure,
unload or dropped execution kills the process group, including decoders; Rust
owns and removes the complete input/frame spool directory. Jobs and ordinary logs
contain operational identity, never query text, input media, vectors or paths.

Files are at most 25 MiB. Images are normalized display pixels and at most 16
megapixels. Audio is at most 30 seconds, decoded to mono 16 kHz FP32. Video is at
most 16 seconds, sampled from time zero at 1 fps into at most 16 bounded frames.
Unknown duration and unsupported containers are rejected. Video embeds visual
content only; sound uses the separate audio route. Consumers must preserve that
distinction when combining evidence. Media decoders disable playlist/concat and
network input formats/protocols.

Ratings remain provisional. Native forward checks and a small source-labelled
real-media sanity evaluation support making the model selectable, not a general
quality claim or acceptance of a particular consumer's photo/audio library.
Evaluation queries and expected labels must be frozen before scoring; all results,
including misses and cross-modality confusion, must be retained privately.

Sources: [Google model card](https://ai.google.dev/gemma/docs/embeddinggemma/model_card_2),
[developer guide](https://developers.googleblog.com/embeddinggemma-2-the-developer-guide/),
[pinned MLX model](https://huggingface.co/mlx-community/embeddinggemma-2-bf16/tree/1a4ffddb7905d3f63486748deabe091a01fb6201).
