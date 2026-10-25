#!/usr/bin/env python3
"""Stage pinned public Shadow vision artifacts for explicit operator import.

This does not enable a Deployment or change the Runtime configuration. SigLIP
source snapshots still pass through export_siglip2.py before ONNX admission.
"""
from __future__ import annotations

import argparse
import ast
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import tomllib
import urllib.request


REPOSITORY = Path(__file__).resolve().parent.parent


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(2 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(entry: dict) -> dict:
    path = entry["path"]
    expected = entry["sha256"]
    if path.is_file() and sha256(path) == expected:
        return {"path": str(path), "sha256": expected, "reused": True}
    if path.exists():
        raise RuntimeError(f"existing artifact differs from the pinned identity: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(path.suffix + ".partial")
    offset = partial.stat().st_size if partial.exists() else 0
    headers = {"User-Agent": "infer-runtime-operator-staging"}
    if offset:
        headers["Range"] = f"bytes={offset}-"
    request = urllib.request.Request(entry["url"], headers=headers)
    with urllib.request.urlopen(request, timeout=120) as response:
        resumed = response.status == 206 and response.headers.get("Content-Range", "").startswith(f"bytes {offset}-")
        with partial.open("ab" if resumed else "wb") as output:
            for block in iter(lambda: response.read(2 * 1024 * 1024), b""):
                output.write(block)
            output.flush()
            os.fsync(output.fileno())
    if entry.get("size_bytes") and partial.stat().st_size != entry["size_bytes"]:
        raise RuntimeError(f"artifact size differs from the pinned identity: {path}")
    actual = sha256(partial)
    if actual != expected:
        raise RuntimeError(f"artifact digest differs from the pinned identity: {path}: {actual}")
    partial.replace(path)
    print(f"verified {path.name} ({path.stat().st_size} bytes)", flush=True)
    return {"path": str(path), "sha256": actual, "size_bytes": path.stat().st_size}


def siglip_source() -> dict:
    source = ast.parse((REPOSITORY / "tools/export_siglip2.py").read_text(encoding="utf-8"))
    names = {"MODEL_ID", "MODEL_REVISION", "EXPECTED_SOURCE_DIGESTS"}
    return {node.targets[0].id: ast.literal_eval(node.value)
            for node in source.body if isinstance(node, ast.Assign)
            and isinstance(node.targets[0], ast.Name) and node.targets[0].id in names}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=REPOSITORY / "config/infer.example.toml")
    parser.add_argument("--staging-root", required=True, type=Path)
    args = parser.parse_args()
    staging = args.staging_root.resolve()
    if staging == REPOSITORY or REPOSITORY in staging.parents:
        raise SystemExit("model staging must be outside the source checkout")
    config = tomllib.loads(args.config.read_text(encoding="utf-8"))
    entries = []
    for build in ("yunet_2026may_onnx_cpu_v1", "sface_2021dec_onnx_cpu_v1"):
        artifact = config["model_builds"][build]["onnx"]["artifact"]
        entries.append({"path": staging / (build + ".onnx"),
                        "url": artifact["source_url"], "sha256": artifact["sha256"],
                        "size_bytes": artifact["size_bytes"]})
    source = siglip_source()
    for name, digest in source["EXPECTED_SOURCE_DIGESTS"].items():
        entries.append({"path": staging / "siglip2-source" / name, "sha256": digest,
                        "url": f'https://huggingface.co/{source["MODEL_ID"]}/resolve/{source["MODEL_REVISION"]}/{name}'})
    staging.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(max_workers=3) as executor:
        results = list(executor.map(fetch, entries))
    (staging / "verified-source-manifest.json").write_text(
        json.dumps({"artifacts": results}, indent=2) + "\n", encoding="utf-8")
    print("Pinned vision sources verified. Export SigLIP and import each Build before activation.")


if __name__ == "__main__":
    main()
