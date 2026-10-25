#!/usr/bin/env python3
"""Verify pinned public Shadow editing models before explicit Runtime import.

This stages weights only. It never changes deployments, ACLs, credentials, or
the running service, and rejects a staging root inside the source checkout.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import tarfile
import tomllib
import zipfile

from prepare_shadow_vision import REPOSITORY, fetch, sha256


BUILDS = (
    "grounding_dino_tiny_onnx_int8_v1",
    "lama_inpainting_onnx_v1",
    "bisenet_resnet18_face_parsing_onnx_cpu_v1",
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=REPOSITORY / "config/infer.example.toml")
    parser.add_argument("--staging-root", required=True, type=Path)
    parser.add_argument("--build", action="append", choices=BUILDS)
    parser.add_argument("--raw-foundation", action="store_true")
    parser.add_argument("--cuda-grounding", action="store_true")
    args = parser.parse_args()
    staging = args.staging_root.resolve()
    if staging == REPOSITORY or REPOSITORY in staging.parents:
        raise SystemExit("model staging must be outside the source checkout")
    config = tomllib.loads(args.config.read_text(encoding="utf-8"))
    entries = []
    for name in args.build or BUILDS:
        manifest = config["model_builds"][name]["onnx"]
        artifacts = {"model.onnx": manifest["artifact"]}
        artifacts.update({key + ".json": value for key, value in manifest.get("auxiliary_artifacts", {}).items()})
        for filename, artifact in artifacts.items():
            entries.append({"path": staging / name / filename,
                            "url": artifact["source_url"], "sha256": artifact["sha256"],
                            "size_bytes": artifact["size_bytes"]})
    staging.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(max_workers=3) as executor:
        verified = list(executor.map(fetch, entries))
    if args.raw_foundation:
        package = staging / "rawnind" / "rawdenoise-nind.dtmodel"
        verified.append(fetch({"path": package, "size_bytes": 57700134,
                               "sha256": "d71b5f1e727c85a359e6f74dca9e2016c9d8fc3e2f7ac3e9b347d80ceca969af",
                               "url": "https://github.com/darktable-org/darktable-ai/releases/download/release-5.6.0/rawdenoise-nind.dtmodel"}))
        graph = package.parent / "model_bayer.onnx"
        expected = "da27509dab6a2915da67e988acd86cf71f9d5bbc8d1aa0ed32933578a887b901"
        if not graph.exists():
            member = "rawdenoise-nind/model_bayer.onnx"
            if zipfile.is_zipfile(package):
                with zipfile.ZipFile(package) as archive:
                    content = archive.read(member)
            else:
                with tarfile.open(package) as archive:
                    with archive.extractfile(member) as source:
                        content = source.read(31056426)
            if len(content) != 31056425:
                raise RuntimeError("RawNIND graph size differs from the fixed Build")
            partial = graph.with_suffix(".partial")
            partial.write_bytes(content)
            if sha256(partial) != expected:
                raise RuntimeError("RawNIND graph digest differs from the fixed Build")
            partial.replace(graph)
        if graph.stat().st_size != 31056425 or sha256(graph) != expected:
            raise RuntimeError("existing RawNIND graph differs from the fixed Build")
        verified.append({"path": str(graph), "size_bytes": 31056425, "sha256": expected})
    if args.cuda_grounding:
        from export_shadow_editing_cuda import CUDA_DINO_SOURCE
        verified.append(fetch({"path": staging / "grounding_dino_tiny_onnx_cuda_fp32_v1" / "model.onnx",
                               "url": CUDA_DINO_SOURCE["source_url"], "sha256": CUDA_DINO_SOURCE["sha256"], "size_bytes": CUDA_DINO_SOURCE["size_bytes"]}))
    (staging / "verified-source-manifest.json").write_text(
        json.dumps({"artifacts": verified}, indent=2) + "\n", encoding="utf-8")
    print("Editing sources verified; import each Build before activation.")


if __name__ == "__main__":
    main()
