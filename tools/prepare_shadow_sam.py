#!/usr/bin/env python3
"""Stage the exact official SAM 2.1 Small safetensors Build; never activate it."""
from __future__ import annotations

import argparse
import ast
import hashlib
import json
from pathlib import Path

from prepare_shadow_vision import REPOSITORY, fetch


_source = ast.parse((REPOSITORY / "tools/torch_sam_worker.py").read_text(encoding="utf-8"))
_constants = {node.targets[0].id: ast.literal_eval(node.value) for node in _source.body
              if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name)
              and node.targets[0].id in {"MODEL_REVISION", "MODEL_FILES"}}
REVISION, FILES = _constants["MODEL_REVISION"], _constants["MODEL_FILES"]


def artifact_set_digest() -> str:
    return hashlib.sha256("".join(name + "\0" + digest + "\n" for name, (_, digest) in sorted(FILES.items())).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staging-root", required=True, type=Path)
    parser.add_argument("--artifact-root", type=Path)
    parser.add_argument("--publication-request", type=Path)
    args = parser.parse_args()
    root = args.staging_root.resolve()
    if root == REPOSITORY or REPOSITORY in root.parents:
        raise SystemExit("model staging must be outside the source checkout")
    for name, (size, digest) in FILES.items():
        fetch({"path": root / name, "size_bytes": size, "sha256": digest,
               "url": f"https://huggingface.co/facebook/sam2.1-hiera-small/resolve/{REVISION}/{name}"})
    if args.publication_request:
        if not args.artifact_root:
            parser.error("--publication-request needs --artifact-root")
        manifest = {"schema_version": 1, "build_id": "sam21_small_torch_cuda_fp32_v1", "adapter": "sam21_torch",
                    "artifacts": {name: {"sha256": digest, "size_bytes": size, "source_url": f"https://huggingface.co/facebook/sam2.1-hiera-small/resolve/{REVISION}/{name}",
                                         "source_revision": REVISION, "license_spdx": "Apache-2.0"} for name, (size, digest) in FILES.items()}}
        request = {"artifact_root": str(args.artifact_root.resolve()), "manifest": manifest,
                   "sources": {name: str(root / name) for name in FILES}}
        args.publication_request.parent.mkdir(parents=True, exist_ok=True)
        with args.publication_request.open("x", encoding="utf-8") as output:
            output.write(json.dumps(request, indent=2) + "\n")
        args.publication_request.chmod(0o600)
    print(json.dumps({"source_revision": REVISION, "artifact_set_sha256": artifact_set_digest()}))


if __name__ == "__main__":
    main()
