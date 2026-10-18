#!/usr/bin/env python3
"""Install an isolated pinned runtime and prepare an ArtifactStore publication.

The generated request is private machine-local data. Publish it with the
infer-artifact example; this tool does not edit App ACLs or restart services.
"""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

REPO = "mlx-community/embeddinggemma-2-bf16"
REVISION = "1a4ffddb7905d3f63486748deabe091a01fb6201"
BUILD = "embeddinggemma2_bf16_mlx_1a4ffddb"
ARTIFACT_SET_SHA256 = 'b61556c9a6fc78532be5d55753b3e31d4619d2877a0071268e4a6e7870e71ea2'
FILES = {'1_Pooling/config.json': {'license_spdx': 'Apache-2.0',
                           'sha256': '8759bdf7c77efc7df7723f64856a593c8943b71ee38baf2a88771fbaf78438f9',
                           'size_bytes': 90,
                           'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                           'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/1_Pooling/config.json'},
 '2_Normalize/config.json': {'license_spdx': 'Apache-2.0',
                             'sha256': 'cdb09dfca347a56aa2d691744e38d5ad3c7cbc2834e7181272b9a15328b82524',
                             'size_bytes': 97,
                             'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                             'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/2_Normalize/config.json'},
 'chat_template.jinja': {'license_spdx': 'Apache-2.0',
                         'sha256': '4b852efc0b9960283e735363331e6f325b33bc74bdbaa076f595bc4e9b94d85e',
                         'size_bytes': 1016,
                         'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                         'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/chat_template.jinja'},
 'config.json': {'license_spdx': 'Apache-2.0',
                 'sha256': '24bc60679eecb0a61bcf9f5ecf7b01bbd9603b8e47aa469fe9f0569e17c55ed9',
                 'size_bytes': 5258,
                 'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                 'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/config.json'},
 'config_sentence_transformers.json': {'license_spdx': 'Apache-2.0',
                                       'sha256': '031e56a498d33c349ab489a21885bcfe25b4fcba841149dc99e1e90d4a7c28f5',
                                       'size_bytes': 1565,
                                       'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                                       'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/config_sentence_transformers.json'},
 'model.safetensors': {'license_spdx': 'Apache-2.0',
                       'sha256': 'ffaf2604b011ad0634022529bac930a4f6a2feba22687d8d5890434d186b459c',
                       'size_bytes': 1488914930,
                       'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                       'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/model.safetensors'},
 'model.safetensors.index.json': {'license_spdx': 'Apache-2.0',
                                  'sha256': '2f178c9f396d7b5b8ddfbeedf2cd122f9ae4941e97a4f33f4cf0cd0936ef382a',
                                  'size_bytes': 117495,
                                  'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                                  'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/model.safetensors.index.json'},
 'modules.json': {'license_spdx': 'Apache-2.0',
                  'sha256': '3d02572a0455b832de67fb8e63a54981bc7e8b46e337c95e917bd8122a533bfd',
                  'size_bytes': 413,
                  'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                  'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/modules.json'},
 'preprocessor_config.json': {'license_spdx': 'Apache-2.0',
                              'sha256': 'ea2ae257e901064abdd98dceb19f2b0da06af600bed15e0f99f5c85c37ee9d78',
                              'size_bytes': 511,
                              'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                              'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/preprocessor_config.json'},
 'processor_config.json': {'license_spdx': 'Apache-2.0',
                           'sha256': '168f6a08522f3ce5dea596d94d003af2fd691742d4f41fe1f9d8cce76bfbf69c',
                           'size_bytes': 1788,
                           'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                           'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/processor_config.json'},
 'sentence_bert_config.json': {'license_spdx': 'Apache-2.0',
                               'sha256': 'b1bcd9f2dce3ae863b359e87d0710b5dbc3314a59ecb4e2f97c7778fc8e4b228',
                               'size_bytes': 747,
                               'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                               'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/sentence_bert_config.json'},
 'tokenizer.json': {'license_spdx': 'Apache-2.0',
                    'sha256': '4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4',
                    'size_bytes': 32170510,
                    'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                    'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/tokenizer.json'},
 'tokenizer_config.json': {'license_spdx': 'Apache-2.0',
                           'sha256': '17bd5d6e9364ca49a534e1502076593317c298d4a663623091ed45388f004874',
                           'size_bytes': 1599,
                           'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                           'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/tokenizer_config.json'},
 'validation.json': {'license_spdx': 'Apache-2.0',
                     'sha256': 'c45144b823e546ecb1b3bea66e027f6e15754426b244b97549f220b894acc00a',
                     'size_bytes': 2598,
                     'source_revision': '1a4ffddb7905d3f63486748deabe091a01fb6201',
                     'source_url': 'https://huggingface.co/mlx-community/embeddinggemma-2-bf16/resolve/1a4ffddb7905d3f63486748deabe091a01fb6201/validation.json'}}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--artifact-root", required=True, type=Path)
    parser.add_argument("--request", required=True, type=Path)
    parser.add_argument("--snapshot", type=Path)
    parser.add_argument("--uv", default="uv")
    parser.add_argument("--skip-runtime", action="store_true", help="Reuse an independently verified pinned environment")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if not args.skip_runtime:
        if args.runtime.exists():
            parser.error("runtime destination already exists; preserve it or use --skip-runtime")
        subprocess.run([args.uv, "venv", "--python", "3.11.15", str(args.runtime)], check=True)
        subprocess.run([args.uv, "pip", "install", "--python", str(args.runtime / "bin/python"), "-r", str(root / "tools/requirements-embeddinggemma2-20261013.txt")], check=True)
    snapshot = args.snapshot
    if snapshot is None:
        destination = args.request.parent / "embeddinggemma2-download"
        program = "from huggingface_hub import snapshot_download; import sys; snapshot_download(sys.argv[1],revision=sys.argv[2],local_dir=sys.argv[3])"
        subprocess.run([str(args.runtime / "bin/python"), "-c", program, REPO, REVISION, str(destination)], check=True)
        snapshot = destination
    snapshot = snapshot.resolve(strict=True)
    sources = {}
    for name, identity in FILES.items():
        file = snapshot / name
        if not file.is_file() or file.stat().st_size != identity["size_bytes"]:
            raise ValueError("model artifact size mismatch")
        digest = hashlib.sha256()
        with file.open("rb") as stream:
            for block in iter(lambda: stream.read(8 * 1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != identity["sha256"]:
            raise ValueError("model artifact digest mismatch")
        sources[name] = str(file)
    request = {"artifact_root": str(args.artifact_root), "manifest": {"schema_version": 1, "build_id": BUILD, "adapter": "embedding_gemma2", "artifacts": FILES}, "sources": sources}
    args.request.parent.mkdir(parents=True, exist_ok=True)
    args.request.write_text(json.dumps(request, indent=2) + "\n")
    args.request.chmod(0o600)
    print(json.dumps({"build": BUILD, "artifact_set_sha256": ARTIFACT_SET_SHA256, "verified_files": len(sources), "publication_request": str(args.request)}))


if __name__ == "__main__":
    main()
