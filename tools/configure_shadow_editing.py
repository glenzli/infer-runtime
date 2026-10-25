#!/usr/bin/env python3
"""Prepare a new Shadow editing configuration without touching a live service.

Existing App credentials, observer/Sentinel settings and unrelated contracts are
preserved. New immutable CUDA Builds are distinct from the old CPU identities.
The destination must not exist; promotion/restart belongs to the service owner.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import tomllib

from torch_sam_worker import artifact_set_digest, MODEL_REVISION
from export_shadow_editing_cuda import CUDA_BUILDS, CUDA_DINO_SOURCE, EXPORT_IDENTITY

REPOSITORY = Path(__file__).resolve().parent.parent


def _install(table, key, value):
    if key in table and table[key] != value:
        raise ValueError(f"existing configuration differs from the fixed contract: {key}")
    table[key] = copy.deepcopy(value)


def configure(source, runtime_root, *, repository=REPOSITORY, cpu_reference=False, worker_script=None, raw_cuda=True):
    result = copy.deepcopy(source)
    result["runtimes"]["onnx"]["idle_timeout_ms"] = 30_000
    reference = tomllib.loads((repository / "config/infer.example.toml").read_text(encoding="utf-8"))
    routes = ("vision.segment_subject", "vision.ground_semantics", "vision.complete_image", "vision.parse_face")
    for route in routes:
        _install(result.setdefault("intents", {}), route, reference["intents"][route])
    for target, derived in CUDA_BUILDS.items():
        original = derived["reference"]
        build = copy.deepcopy(reference["model_builds"][original])
        _install(result.setdefault("model_profiles", {}), build["profile"], reference["model_profiles"][build["profile"]])
        if not cpu_reference:
            build["onnx"]["allowed_execution_providers"] = ["cuda"]
            build["variant"] += "-static-cuda12"
            artifact = copy.deepcopy(CUDA_DINO_SOURCE if original.startswith("grounding_dino") else build["onnx"]["artifact"])
            artifact.update({"sha256": derived["sha256"], "size_bytes": derived["size_bytes"],
                             "source_url": "infer-export:" + EXPORT_IDENTITY + "/" + target,
                             "source_revision": artifact["source_revision"] + "+" + EXPORT_IDENTITY + ":" + derived["source_sha256"]})
            if not artifact["sha256"] or not artifact["size_bytes"]:
                raise ValueError("CUDA export identity has not been qualified: " + target)
            build["onnx"]["artifact"] = artifact
            build["model_id"] = "sha256:" + artifact["sha256"]
            build["provenance"]["artifact_sha256"] = artifact["sha256"]
            build["provenance"]["source_revision"] = artifact["source_revision"]
            for tensor in build["onnx"]["inputs"]:
                tensor["shape"] = list(map(str, derived["shapes"][tensor["name"]]))
            for tensor in build["onnx"]["outputs"]:
                tensor["shape"] = ["1" if item == "batch" else item for item in tensor["shape"]]
                if original.startswith("bisenet"):
                    tensor["shape"] = ["1", "19", "512", "512"]
            if original.startswith("grounding_dino"):
                build["onnx"]["precision"] = "fp32"
                build["variant"] = "onnx-opset16-fp32-exact800-pad256-static-cuda12"
                build["onnx"]["text_preprocessing"].update({
                    "identity": "grounding_dino_bert_uncased_period_pad256_v1", "padding": "max_length"})
        else:
            target = original
        _install(result.setdefault("model_builds", {}), target, build)
        _install(result.setdefault("deployments", {}), "editing_" + target,
                 {"provider": "onnx-local", "build": target, "resource_class": "heavy", "estimated_cost_usd": 0.0})
    digest = artifact_set_digest()
    _install(result["model_profiles"], "sam21_small_torch", {"family": "sam2.1-small", "ratings": {
        "vision.segment_subject": {"level": "foundational", "status": "benchmarked", "eval_profile": "sam21-small-torch-prompted-mask-v1", "score": 1.0}}})
    _install(result["model_builds"], "sam21_small_torch_cuda_fp32_v1", {
        "profile": "sam21_small_torch", "model_id": "sha256:" + digest, "variant": "sam2.1-small-transformers-cuda12-fp32",
        "input_modalities": ["image"], "output_modalities": ["json"], "features": ["prompted_binary_mask", "normalized_point_and_box_prompts"],
        "provenance": {"source_kind": "user_managed", "upstream": "facebook/sam2.1-hiera-small", "source_revision": MODEL_REVISION, "artifact_sha256": digest},
        "license": {"status": "declared", "expression": "Apache-2.0", "license_url": "https://huggingface.co/facebook/sam2.1-hiera-small/blob/" + MODEL_REVISION + "/README.md"},
        "local_worker": {"adapter": "sam21_torch", "artifact_set_sha256": digest, "runtime": "transformers-4.56.2-torch-2.8.0-cu128", "precision": "float32",
                         "preprocessing_identity": "sam21_transformers_exact1024_rgb_imagenet_v1", "postprocessing_identity": "sam21_torch_best_iou_native256_sigmoid_binary_bilinear_v1",
                         "requested_execution_provider": "cuda", "actual_execution_provider": "cuda"}})
    _install(result.setdefault("providers", {}), "torch-sam-local", {
        "kind": "torch_sam_worker", "command": str(runtime_root / ".infer-runtime/runtimes/vision-cuda/bin/python"),
        "args": [str(worker_script or (Path.home() / ".local/lib/infer-runtime/current/torch_sam_worker.py"))], "placement": "local", "max_concurrency": 1, "max_queue": 8,
        "capability_profile": {"version": 1, "protocol": "torch_sam_worker"}})
    _install(result["deployments"], "sam21_small_torch_cuda_fp32_v1", {"provider": "torch-sam-local", "build": "sam21_small_torch_cuda_fp32_v1", "resource_class": "heavy", "estimated_cost_usd": 0.0})
    _install(result["providers"], "raw-foundation-local", reference["providers"]["raw-foundation-local"])
    _install(result["intents"], "raw.materialize_foundation", {"data_plane": "raw.foundation", "input_modalities": ["image"], "output_modalities": ["image"], "required_features": ["rawnind_foundation_ort127_exp1"], "default_capability_floor": "foundational", "default_policy": "local-first"})
    _install(result["model_profiles"], "rawnind", {"family": "rawnind", "ratings": {"raw.materialize_foundation": {"level": "foundational", "status": "benchmarked", "eval_profile": "rawnind-ort127-exp1", "score": 1.0}}})
    _install(result["model_builds"], "rawnind_ort127_exp1", {"profile": "rawnind", "model_id": "darktable-ai/rawnind-public-bayer", "variant": "onnxruntime-1.27.0-cpu-fp32", "input_modalities": ["image"], "output_modalities": ["image"], "features": ["rawnind_foundation_ort127_exp1"]})
    _install(result["deployments"], "rawnind_ort127_exp1", {"provider": "raw-foundation-local", "build": "rawnind_ort127_exp1", "resource_class": "heavy", "estimated_cost_usd": 0.0})
    result["raw_foundation"] = {"enabled": True, "graph": str(runtime_root / ".infer-runtime/staging/shadow-editing/rawnind/model_bayer.onnx"),
                                "graph_sha256": "da27509dab6a2915da67e988acd86cf71f9d5bbc8d1aa0ed32933578a887b901",
                                "runtime_library": result["runtimes"]["onnx"]["library"], "runtime_version": "1.27.0",
                                "socket_directory": str(runtime_root / "leases")}
    if raw_cuda:
        result["raw_foundation"]["execution_provider"] = "cuda"
        result["intents"]["raw.materialize_foundation"]["required_features"] = ["rawnind_foundation_ort127_cuda_exp1"]
        build = copy.deepcopy(result["model_builds"]["rawnind_ort127_exp1"])
        build["variant"] = "onnxruntime-1.27.0-cuda12-fp32"
        build["features"] = ["rawnind_foundation_ort127_cuda_exp1"]
        _install(result["model_builds"], "rawnind_ort127_cuda_exp1", build)
        _install(result["deployments"], "rawnind_ort127_cuda_exp1", {"provider": "raw-foundation-local", "build": "rawnind_ort127_cuda_exp1", "resource_class": "heavy", "estimated_cost_usd": 0.0})
    app = result["apps"]["shadow"]
    app["allowed_intents"] = list(dict.fromkeys(app.get("allowed_intents", []) + list(routes) + ["raw.materialize_foundation"]))
    if cpu_reference:
        result["runtimes"]["onnx"]["preferred_execution_providers"] = ["cpu"]
    return result


def dump_toml(value):
    def atom(value):
        if isinstance(value, bool):
            return str(value).lower()
        if isinstance(value, (str, int, float)):
            return json.dumps(value, ensure_ascii=False, allow_nan=False)
        if isinstance(value, list):
            return "[" + ", ".join(atom(item) for item in value) + "]"
        if isinstance(value, dict):
            return "{ " + ", ".join(json.dumps(key) + " = " + atom(item) for key, item in value.items()) + " }"
        raise TypeError("unsupported TOML value")
    lines = []
    def table(values, path):
        if path:
            lines.extend(["", "[" + ".".join(json.dumps(part) for part in path) + "]"])
        for key, item in values.items():
            if not isinstance(item, dict):
                lines.append(json.dumps(key) + " = " + atom(item))
        for key, item in values.items():
            if isinstance(item, dict):
                table(item, path + [key])
    table(value, [])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--runtime-root", required=True, type=Path)
    parser.add_argument("--cpu-reference", action="store_true")
    parser.add_argument("--worker-script", type=Path, help="Explicit development-only override for the installed worker")
    parser.add_argument("--raw-cpu", action="store_true", help="Retain the original experimental CPU RawNIND Build")
    args = parser.parse_args()
    if not args.runtime_root.is_absolute():
        parser.error("--runtime-root must be absolute")
    config = configure(tomllib.loads(args.source.read_text(encoding="utf-8")), args.runtime_root, cpu_reference=args.cpu_reference, worker_script=args.worker_script, raw_cuda=not args.raw_cpu)
    text = dump_toml(config)
    if tomllib.loads(text) != config:
        raise ValueError("configuration round trip differs")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as target:
        target.write(text)
        target.flush()
        os.fsync(target.fileno())
    print(json.dumps({"prepared": str(args.output), "source_preserved": True, "cpu_reference": args.cpu_reference}))


if __name__ == "__main__":
    main()
