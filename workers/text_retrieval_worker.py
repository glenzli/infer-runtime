#!/usr/bin/env python3
"""Persistent offline Qwen3 text retrieval worker using JSON Lines.

The worker owns MLX model residency and the exact instruction templates.  Raw
query/document text is accepted only on stdin and is never written to stderr.
"""

from __future__ import annotations

import contextlib
import gc
import io
import json
import os
import sys
from collections import OrderedDict
from typing import Any

os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")

QUERY_TEMPLATE_REVISION = "qwen3-retrieval-query-v1"
RERANK_TEMPLATE_REVISION = "qwen3-retrieval-rerank-v1"
RETRIEVAL_INSTRUCTION = (
    "Given a search query, retrieve relevant passages that answer the query"
)
RERANK_SYSTEM = (
    "Judge whether the Document meets the requirements based on the Query and "
    'the Instruct provided. Note that the answer can only be "yes" or "no".'
)
RERANK_PREFIX = (
    f"<|im_start|>system\n{RERANK_SYSTEM}<|im_end|>\n<|im_start|>user\n"
)
RERANK_SUFFIX = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"

MODEL_CACHE: OrderedDict[str, tuple[Any, Any]] = OrderedDict()
MAX_LOADED_MODELS = max(
    1, int(os.environ.get("INFER_RETRIEVAL_MAX_LOADED_MODELS", "1"))
)


def _quietly_load(model_path: str) -> tuple[Any, Any]:
    if model_path in MODEL_CACHE:
        MODEL_CACHE.move_to_end(model_path)
        return MODEL_CACHE[model_path]

    while len(MODEL_CACHE) >= MAX_LOADED_MODELS:
        _, evicted = MODEL_CACHE.popitem(last=False)
        del evicted
        gc.collect()
        try:
            import mlx.core as mx

            mx.clear_cache()
        except Exception:
            pass

    from mlx_embeddings import load

    # Dependency progress may include the local artifact path. Neither paths
    # nor request text belong in the daemon's generic logs.
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
        io.StringIO()
    ):
        loaded = load(model_path)
    MODEL_CACHE[model_path] = loaded
    return loaded


def _normalized_embeddings(model_path: str, texts: list[str]) -> list[list[float]]:
    import mlx.core as mx
    from mlx_embeddings import generate

    model, tokenizer = _quietly_load(model_path)
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
        io.StringIO()
    ):
        output = generate(
            model,
            tokenizer,
            texts=texts,
            max_length=8192,
            padding=True,
            truncation=True,
        )
    embeddings = output.text_embeds.astype(mx.float32)
    # Some MLX paths calculate the first normalization in a reduced dtype.
    # A final FP32 normalization makes the public vector contract precise.
    embeddings /= mx.maximum(
        mx.linalg.norm(embeddings, axis=-1, keepdims=True), mx.array(1e-12)
    )
    mx.eval(embeddings)
    return [[float(value) for value in row] for row in embeddings.tolist()]


def _embed(request: dict[str, Any], *, query: bool) -> dict[str, Any]:
    texts = request["texts"]
    prepared = (
        [f"Instruct: {RETRIEVAL_INSTRUCTION}\nQuery:{text}" for text in texts]
        if query
        else texts
    )
    embeddings = _normalized_embeddings(request["model"], prepared)
    return {
        "embeddings": embeddings,
        "dimensions": len(embeddings[0]) if embeddings else 0,
        "normalized": True,
        "instruction_revision": QUERY_TEMPLATE_REVISION if query else None,
    }


def _rerank(request: dict[str, Any]) -> dict[str, Any]:
    import mlx.core as mx

    model, tokenizer = _quietly_load(request["model"])
    query = request["query"]
    candidates = request["candidates"]
    bodies = [
        f"<Instruct>: {RETRIEVAL_INSTRUCTION}\n<Query>: {query}\n<Document>: {item['text']}"
        for item in candidates
    ]
    prefix_tokens = tokenizer.encode(RERANK_PREFIX, add_special_tokens=False)
    suffix_tokens = tokenizer.encode(RERANK_SUFFIX, add_special_tokens=False)
    encoded = tokenizer(
        bodies,
        padding=False,
        truncation=True,
        max_length=8192 - len(prefix_tokens) - len(suffix_tokens),
    )["input_ids"]
    encoded = [prefix_tokens + item + suffix_tokens for item in encoded]
    batch = tokenizer.pad(
        {"input_ids": encoded}, padding=True, return_tensors="np", max_length=8192
    )
    input_ids = mx.array(batch["input_ids"])
    attention_mask = mx.array(batch["attention_mask"])
    hidden = model.model(input_ids, attention_mask=attention_mask)[:, -1, :]
    logits = model.model.embed_tokens.as_linear(hidden)
    no_id = tokenizer.convert_tokens_to_ids("no")
    yes_id = tokenizer.convert_tokens_to_ids("yes")
    scores = mx.softmax(
        mx.stack([logits[:, no_id], logits[:, yes_id]], axis=-1).astype(mx.float32),
        axis=-1,
    )[:, 1]
    mx.eval(scores)
    ranked = [
        {"candidate_id": candidate["id"], "score": float(score)}
        for candidate, score in zip(candidates, scores.tolist(), strict=True)
    ]
    ranked.sort(key=lambda item: (-item["score"], item["candidate_id"]))
    for rank, item in enumerate(ranked, start=1):
        item["rank"] = rank
    return {
        "results": ranked,
        "instruction_revision": RERANK_TEMPLATE_REVISION,
        "score_semantics": "model_relevance_not_calibrated_probability",
    }


def handle(request: dict[str, Any]) -> dict[str, Any]:
    operation = request.get("operation")
    if operation == "embed_query":
        return _embed(request, query=True)
    if operation == "embed_documents":
        return _embed(request, query=False)
    if operation == "rerank":
        return _rerank(request)
    raise ValueError("unsupported_operation")


def main() -> int:
    for line in sys.stdin:
        if not line.strip():
            continue
        request_id = "unknown"
        try:
            request = json.loads(line)
            request_id = str(request.get("request_id", request_id))
            result = handle(request)
            response = {"request_id": request_id, "ok": True, "result": result}
        except Exception as error:
            # Error class is useful for local diagnosis; exception messages may
            # echo request text or artifact paths and are deliberately omitted.
            print(
                f"retrieval worker request failed: {type(error).__name__}",
                file=sys.stderr,
            )
            response = {
                "request_id": request_id,
                "ok": False,
                "error": "retrieval_execution_failed",
            }
        sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
