#!/usr/bin/env python3
"""Export the pinned SigLIP 2 Base FixRes checkpoint into two typed ONNX encoders.

The checkpoint and generated files belong in infer-runtime's managed artifact
staging directory, never in Git. The image and text encoders deliberately emit
the same L2-normalized 768-dimensional embedding space.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path

import onnx
import torch
from torch import nn
from torch.nn import functional as F
from transformers import AutoModel


MODEL_ID = "google/siglip2-base-patch16-224"
MODEL_REVISION = "75de2d55ec2d0b4efc50b3e9ad70dba96a7b2fa2"
EXPECTED_SOURCE_DIGESTS = {
    "config.json": "fe8b5fe6d5734360678fd71c11c21e1ea3364bd8598d34295d9206335973ffd7",
    "model.safetensors": "612923381c76ec5a9bed335d1c48827e3f2e506ac31b044b63b2031fadee6a0b",
    "preprocessor_config.json": "9b36b57ebaf20f09bf4c22100ccc21877ea6bfe5aead0c00c59f8af8ccefacfc",
    "tokenizer.json": "cb9140fae3ac5122c972d37adf83e1248471a38147ad76f8215c8872c6fd8322",
    "tokenizer_config.json": "14afe629fe4959b9e0d51e1852b8d9f7ad074f90a1a7125a4fcdd17f06e78fc8",
}
OPSET = 17
EMBEDDING_DIMENSIONS = 768
NORMALIZATION_EPSILON = 1e-12


class ImageEncoder(nn.Module):
    def __init__(self, model: nn.Module) -> None:
        super().__init__()
        self.vision_model = model.vision_model

    def forward(self, pixel_values: torch.Tensor) -> torch.Tensor:
        pooled = self.vision_model(pixel_values=pixel_values).pooler_output
        return F.normalize(pooled, p=2.0, dim=-1, eps=NORMALIZATION_EPSILON)


class TextEncoder(nn.Module):
    def __init__(self, model: nn.Module) -> None:
        super().__init__()
        self.text_model = model.text_model

    def forward(self, input_ids: torch.Tensor) -> torch.Tensor:
        pooled = self.text_model(input_ids=input_ids).pooler_output
        return F.normalize(pooled, p=2.0, dim=-1, eps=NORMALIZATION_EPSILON)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def verify_checkpoint(checkpoint: Path) -> None:
    for name, expected in EXPECTED_SOURCE_DIGESTS.items():
        path = checkpoint / name
        if not path.is_file():
            raise SystemExit(f"missing pinned checkpoint file: {path}")
        actual = sha256(path)
        if actual != expected:
            raise SystemExit(
                f"checkpoint digest mismatch for {name}: expected {expected}, got {actual}"
            )


def export_model(module: nn.Module, example: torch.Tensor, path: Path, input_name: str) -> None:
    module.eval()
    with torch.no_grad():
        reference = module(example)
    if tuple(reference.shape) != (1, EMBEDDING_DIMENSIONS):
        raise SystemExit(f"unexpected reference output shape: {tuple(reference.shape)}")
    norm = torch.linalg.vector_norm(reference, dim=-1)
    if not torch.allclose(norm, torch.ones_like(norm), atol=1e-5, rtol=1e-5):
        raise SystemExit(f"reference output is not normalized: {norm.tolist()}")
    torch.onnx.export(
        module,
        (example,),
        path,
        input_names=[input_name],
        output_names=["embedding"],
        opset_version=OPSET,
        do_constant_folding=True,
        dynamo=False,
    )
    graph = onnx.load(path, load_external_data=True)
    onnx.checker.check_model(graph, full_check=True)
    if len(graph.graph.input) != 1 or len(graph.graph.output) != 1:
        raise SystemExit(f"unexpected ONNX contract in {path}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkpoint", required=True, type=Path)
    parser.add_argument("--output-directory", required=True, type=Path)
    args = parser.parse_args()

    checkpoint = args.checkpoint.resolve()
    output = args.output_directory.resolve()
    verify_checkpoint(checkpoint)
    output.mkdir(parents=True, exist_ok=True)

    torch.manual_seed(0)
    model = AutoModel.from_pretrained(checkpoint, local_files_only=True).eval()
    if model.config.vision_config.image_size != 224:
        raise SystemExit("pinned checkpoint no longer has a 224x224 vision contract")
    if model.config.text_config.max_position_embeddings != 64:
        raise SystemExit("pinned checkpoint no longer has a 64-token text contract")
    if model.config.vision_config.hidden_size != EMBEDDING_DIMENSIONS:
        raise SystemExit("pinned checkpoint embedding dimensions changed")

    image_path = output / "siglip2-base-patch16-224-image.onnx"
    text_path = output / "siglip2-base-patch16-224-text.onnx"
    export_model(
        ImageEncoder(model),
        torch.zeros((1, 3, 224, 224), dtype=torch.float32),
        image_path,
        "pixel_values",
    )
    export_model(
        TextEncoder(model),
        torch.zeros((1, 64), dtype=torch.int64),
        text_path,
        "input_ids",
    )

    manifest = {
        "schema_version": 1,
        "source": {
            "model_id": MODEL_ID,
            "revision": MODEL_REVISION,
            "digests": EXPECTED_SOURCE_DIGESTS,
            "license_spdx": "Apache-2.0",
        },
        "export": {
            "opset": OPSET,
            "embedding_dimensions": EMBEDDING_DIMENSIONS,
            "normalization": f"l2_eps_{NORMALIZATION_EPSILON:g}",
            "tools": {
                name: importlib.metadata.version(name)
                for name in ("torch", "transformers", "onnx", "huggingface-hub")
            },
        },
        "outputs": {
            path.name: {"sha256": sha256(path), "size_bytes": path.stat().st_size}
            for path in (image_path, text_path)
        },
        "tokenizer": {
            "file": "tokenizer.json",
            "sha256": EXPECTED_SOURCE_DIGESTS["tokenizer.json"],
            "lowercase": True,
            "padding": "max_length",
            "truncation": True,
            "max_length": 64,
            "pad_token_id": 0,
            "eos_token_id": 1,
        },
    }
    (output / "export-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(manifest["outputs"], sort_keys=True))


if __name__ == "__main__":
    main()
