# ADR-0016: Dedicated local retrieval and exact OCR data planes

- Status: accepted for implementation
- Date: 2026-08-12

## Decision

Infer Runtime exposes Qwen3 text retrieval and PP-OCRv6 exact OCR as distinct,
typed local capabilities. It does not reuse the SigLIP cross-modal embedding
space or the QwenVL image-understanding schema.

The first retrieval Build pair is Qwen3 Embedding 0.6B plus Qwen3 Reranker
0.6B. Query and document embeddings use the same 1024-dimensional, FP32
L2-normalized space, while the query-side instruction template is a versioned
part of Build provenance. Reranking returns an ordering and an uncalibrated
model relevance score; it does not present that score as probability or truth.

The first OCR Build is PP-OCRv6 medium detection plus recognition under ONNX
Runtime CPU. Input images must already be orientation-normalized display-pixel
JPEG or PNG artifacts. Results contain recognized lines, confidence and
four-point polygons in that exact pixel space.

The public capability contracts are additive to the dated Consumer Core:

- `infer.text.embedding@20260812.1`: `POST /infer/v1/text/query-embeddings`
  and `POST /infer/v1/text/document-embeddings`;
- `infer.text.rerank@20260812.1`: `POST /infer/v1/text/rerank`;
- `infer.document.ocr@20260812.1`: `POST /infer/v1/documents/ocr`.

They require the normal `Infer-Consumer-Contract` header plus the exact
`Infer-Capability-Contract` value. This slice does not grant any product
Consumer these Intents automatically; ACL expansion remains a separate
Consumer decision.

## Boundaries

- Text, images, OCR output and vectors never enter generic Job metadata, logs,
  metrics or durable background payloads.
- Both data planes require `local_only`, `offline_required=true` and no
  fallback. Consumers own indexes, source revision freshness and persistence.
- The worker dependency environments remain external managed runtimes. In
  particular, `mlx-embeddings` is invoked through a process protocol and is not
  linked into or redistributed by the Rust workspace.
- The artifact store owns a private multi-file manifest for every worker Build.
  It verifies every file by size and SHA-256 before atomic publication and
  again before provider assembly. Versioned routing config carries only the
  aggregate artifact-set identity; it does not expose filesystem paths.
- QwenVL remains suitable for captions, scene semantics and bounded visual
  review. It is not the primary contract for exact text transcription,
  line-level geometry or large-volume deterministic OCR.
- BGE-M3 is not admitted initially. It remains an evaluation candidate only if
  a consumer needs its combined dense/sparse/ColBERT retrieval modes.
- PaddleOCR-VL 1.6 is a later complex-layout/table/formula candidate. It does
  not block the PP-OCRv6 text-line slice and must pass its own artifact,
  residency, numerical and typed-schema gates.

## Model identity

The managed artifacts pin exact upstream revisions and SHA-256 digests. Build
identity also includes tokenizer files, instruction revision, pooling,
normalization, preprocessing, execution engine and precision. Embeddings from
different Build identities must never be compared silently.

Initial verified identities:

| Component | Upstream revision | Executable SHA-256 |
|---|---|---|
| Qwen3 Embedding 0.6B | `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3` | `0437e45c94563b09e13cb7a64478fc406947a93cb34a7e05870fc8dcd48e23fd` |
| Qwen3 Reranker 0.6B | `e61197ed45024b0ed8a2d74b80b4d909f1255473` | `5c9ce41a4123598a91b76b7c4b0d4efd0458edfb1bcea9b1a4a9ec196fbc0e40` |
| PP-OCRv6 medium detection ONNX | `61323801669c338b7891481ec7bac61ce31b576a` | `eb13b44b25bb36f89528b68720af8a61d9cf381176107f465db1757b65d086e1` |
| PP-OCRv6 medium recognition ONNX | `50c7eacafc52fa7bcf4194e8cd08e46f8558504b` | `9c09abf0957f7968c7586464b7397b84ad2387a0497a351af40e9acc71b673ba` |

All four upstream model cards declare Apache-2.0. The external
`mlx-embeddings` worker dependency declares GPL-3.0; that package is not
vendored, linked into, or redistributed with the Rust binaries.

The Qwen model-card license statements are currently recorded as `declared`;
the revisions do not expose a standalone license file at the expected path.
PP-OCRv6 uses the hash-verified PaddleOCR Apache-2.0 license text.
