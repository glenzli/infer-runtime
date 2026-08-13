#!/usr/bin/env python3
"""Bounded local worker for Apple SAM 2.1 CoreML image segmentation."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import sys
import tempfile
from pathlib import Path

import coremltools as ct
import numpy as np
from PIL import Image


MAX_REQUEST_BYTES = 64 * 1024
MAX_IMAGE_PIXELS = 40_000_000
MAX_PROMPTS = 16
INPUT_SIZE = 1024

Image.MAX_IMAGE_PIXELS = MAX_IMAGE_PIXELS

MODEL_FILES = {
    "image": "SAM2_1SmallImageEncoderFLOAT16.mlpackage",
    "prompt": "SAM2_1SmallPromptEncoderFLOAT16.mlpackage",
    "mask": "SAM2_1SmallMaskDecoderFLOAT16.mlpackage",
}

EXPECTED_IO = {
    "image": ({"image"}, {"image_embedding", "feats_s0", "feats_s1"}),
    "prompt": ({"points", "labels"}, {"sparse_embeddings", "dense_embeddings"}),
    "mask": (
        {"image_embedding", "sparse_embedding", "dense_embedding", "feats_s0", "feats_s1"},
        {"low_res_masks", "scores"},
    ),
}

_loaded_root: str | None = None
_models: dict[str, object] = {}
_cached_image_sha256: str | None = None
_cached_image_encoding: dict[str, np.ndarray] | None = None
_compiled_cache_root: Path | None = None
_compute_units = ct.ComputeUnit.ALL

COMPILED_MODEL_NAMES = {
    "image": "image.mlmodelc",
    "prompt": "prompt.mlmodelc",
    "mask": "mask.mlmodelc",
}


class StableWorkerError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _model_paths(root: str) -> dict[str, Path]:
    path = Path(root)
    if not path.is_absolute() or not path.is_dir():
        raise StableWorkerError("sam_model_unavailable")
    models = {name: path / relative for name, relative in MODEL_FILES.items()}
    if any(not model.is_dir() for model in models.values()):
        raise StableWorkerError("sam_model_unavailable")
    return models


def _verify_specs(root: str) -> None:
    for name, path in _model_paths(root).items():
        model = ct.models.MLModel(str(path), skip_model_load=True)
        description = model.get_spec().description
        inputs = {feature.name for feature in description.input}
        outputs = {feature.name for feature in description.output}
        if (inputs, outputs) != EXPECTED_IO[name]:
            raise StableWorkerError("sam_model_contract_mismatch")


def _host_compile_identity() -> dict[str, str]:
    return {
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "coremltools": ct.__version__,
        "python": platform.python_version(),
    }


def _prepared_directory(cache_root: Path, artifact_sha256: str) -> Path:
    if (
        len(artifact_sha256) != 64
        or any(character not in "0123456789abcdef" for character in artifact_sha256)
    ):
        raise StableWorkerError("sam_invalid_artifact_identity")
    identity = json.dumps(
        _host_compile_identity(), sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    host_key = hashlib.sha256(identity).hexdigest()[:24]
    return cache_root / artifact_sha256 / host_key


def _prepared_models(cache_root: Path, artifact_sha256: str) -> dict[str, Path]:
    directory = _prepared_directory(cache_root, artifact_sha256)
    manifest_path = directory / "manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except Exception as error:
        raise StableWorkerError("sam_model_not_prepared") from error
    expected = {
        "schema": "infer-runtime.coreml-sam-compiled-cache",
        "schema_version": 1,
        "artifact_sha256": artifact_sha256,
        "host": _host_compile_identity(),
        "components": COMPILED_MODEL_NAMES,
    }
    if manifest != expected:
        raise StableWorkerError("sam_model_not_prepared")
    models = {name: directory / relative for name, relative in COMPILED_MODEL_NAMES.items()}
    if any(not path.is_dir() for path in models.values()):
        raise StableWorkerError("sam_model_not_prepared")
    return models


def _prepare_models(root: str, cache_root: Path, artifact_sha256: str) -> Path:
    _verify_specs(root)
    destination = _prepared_directory(cache_root, artifact_sha256)
    try:
        _prepared_models(cache_root, artifact_sha256)
        return destination
    except StableWorkerError:
        pass
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix="prepare-", dir=destination.parent))
    try:
        for name, source in _model_paths(root).items():
            ct.models.utils.compile_model(
                str(source), str(staging / COMPILED_MODEL_NAMES[name])
            )
        manifest = {
            "schema": "infer-runtime.coreml-sam-compiled-cache",
            "schema_version": 1,
            "artifact_sha256": artifact_sha256,
            "host": _host_compile_identity(),
            "components": COMPILED_MODEL_NAMES,
        }
        (staging / "manifest.json").write_text(
            json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        try:
            os.replace(staging, destination)
        except OSError:
            _prepared_models(cache_root, artifact_sha256)
        return destination
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def _load_models(root: str, artifact_sha256: str) -> dict[str, object]:
    global _loaded_root, _models, _cached_image_sha256, _cached_image_encoding
    cache_identity = f"{root}:{artifact_sha256}"
    if _loaded_root == cache_identity and _models:
        return _models
    _verify_specs(root)
    try:
        if _compiled_cache_root is None:
            raise StableWorkerError("sam_model_not_prepared")
        paths = _prepared_models(_compiled_cache_root, artifact_sha256)
        _models = {
            name: ct.models.CompiledMLModel(str(path), compute_units=_compute_units)
            for name, path in paths.items()
        }
    except StableWorkerError:
        raise
    except Exception as error:
        raise StableWorkerError("sam_model_unavailable") from error
    _loaded_root = cache_identity
    _cached_image_sha256 = None
    _cached_image_encoding = None
    return _models


def _validate_request(request: dict) -> None:
    expected = {
        "request_id",
        "operation",
        "model",
        "artifact_sha256",
        "image_path",
        "image_sha256",
        "output_path",
        "points",
        "box_prompt",
    }
    if set(request) - expected or request.get("operation") not in (
        "segment_subject",
        "segment_subject_soft_mask",
    ):
        raise StableWorkerError("sam_invalid_request")
    if not isinstance(request.get("request_id"), str) or not request["request_id"]:
        raise StableWorkerError("sam_invalid_request")
    points = request.get("points")
    box = request.get("box_prompt")
    if not isinstance(points, list):
        raise StableWorkerError("sam_invalid_request")
    prompt_slots = len(points) + (2 if box is not None else 0)
    if prompt_slots < 1 or prompt_slots > MAX_PROMPTS:
        raise StableWorkerError("sam_invalid_request")


def _prompt_arrays(request: dict) -> tuple[np.ndarray, np.ndarray]:
    coordinates: list[list[float]] = []
    labels: list[int] = []
    for point in request["points"]:
        if set(point) != {"x", "y", "label"}:
            raise StableWorkerError("sam_invalid_request")
        x = float(point["x"])
        y = float(point["y"])
        if not np.isfinite(x) or not np.isfinite(y) or not 0 <= x <= 1 or not 0 <= y <= 1:
            raise StableWorkerError("sam_invalid_request")
        label = point["label"]
        if label not in ("foreground", "background"):
            raise StableWorkerError("sam_invalid_request")
        coordinates.append([x * INPUT_SIZE, y * INPUT_SIZE])
        labels.append(1 if label == "foreground" else 0)
    box = request.get("box_prompt")
    if box is not None:
        if set(box) != {"x", "y", "width", "height"}:
            raise StableWorkerError("sam_invalid_request")
        x = float(box["x"])
        y = float(box["y"])
        width = float(box["width"])
        height = float(box["height"])
        if (
            not all(np.isfinite(value) for value in (x, y, width, height))
            or x < 0
            or y < 0
            or width <= 0
            or height <= 0
            or x + width > 1
            or y + height > 1
        ):
            raise StableWorkerError("sam_invalid_request")
        coordinates.extend(
            [
                [x * INPUT_SIZE, y * INPUT_SIZE],
                [(x + width) * INPUT_SIZE, (y + height) * INPUT_SIZE],
            ]
        )
        labels.extend([2, 3])
    return (
        np.asarray(coordinates, dtype=np.float32)[None, :, :],
        np.asarray(labels, dtype=np.int32)[None, :],
    )


def _encode_image(models: dict[str, object], request: dict) -> tuple[dict, tuple[int, int]]:
    global _cached_image_sha256, _cached_image_encoding
    image_path = Path(request["image_path"])
    if not image_path.is_absolute() or not image_path.is_file():
        raise StableWorkerError("sam_invalid_request")
    with Image.open(image_path) as opened:
        image = opened.convert("RGB")
        width, height = image.size
        if width <= 0 or height <= 0 or width * height > MAX_IMAGE_PIXELS:
            raise StableWorkerError("sam_invalid_request")
        actual_digest = _sha256_file(image_path)
        if actual_digest != request["image_sha256"]:
            raise StableWorkerError("sam_invalid_request")
        if _cached_image_sha256 != actual_digest or _cached_image_encoding is None:
            resized = image.resize((INPUT_SIZE, INPUT_SIZE), Image.Resampling.BILINEAR)
            try:
                _cached_image_encoding = models["image"].predict({"image": resized})
            except Exception as error:
                raise StableWorkerError("sam_execution_failed") from error
            _cached_image_sha256 = actual_digest
    return _cached_image_encoding, (width, height)


def _segment(request: dict) -> dict:
    _validate_request(request)
    models = _load_models(request["model"], request["artifact_sha256"])
    image_encoding, original_size = _encode_image(models, request)
    points, labels = _prompt_arrays(request)
    try:
        prompt = models["prompt"].predict({"points": points, "labels": labels})
        decoded = models["mask"].predict(
            {
                "image_embedding": image_encoding["image_embedding"],
                "sparse_embedding": prompt["sparse_embeddings"],
                "dense_embedding": prompt["dense_embeddings"],
                "feats_s0": image_encoding["feats_s0"],
                "feats_s1": image_encoding["feats_s1"],
            }
        )
    except Exception as error:
        raise StableWorkerError("sam_execution_failed") from error
    scores = np.asarray(decoded["scores"], dtype=np.float32).reshape(-1)
    masks = np.asarray(decoded["low_res_masks"], dtype=np.float32)
    if scores.size != 3 or masks.shape != (1, 3, 256, 256) or not np.all(np.isfinite(scores)):
        raise StableWorkerError("sam_model_contract_mismatch")
    best = int(np.argmax(scores))
    logits = masks[0, best].astype(np.float32)
    output_path = Path(request["output_path"])
    if not output_path.is_absolute() or output_path.exists() or not output_path.parent.is_dir():
        raise StableWorkerError("sam_invalid_request")
    partial = output_path.with_suffix(".partial")
    if request["operation"] == "segment_subject":
        resized_logits = Image.fromarray(logits).resize(
            original_size, Image.Resampling.BILINEAR
        )
        output = (np.asarray(resized_logits, dtype=np.float32) > 0).astype(np.uint8) * 255
    else:
        # Keep the native 256x256 SAM raster.  Gray8 uses a stable sigmoid
        # quantization, so consumers can map it to input pixels themselves
        # without a hidden resize/threshold policy in the Runtime.
        positive = logits >= 0
        probabilities = np.empty_like(logits)
        probabilities[positive] = 1.0 / (1.0 + np.exp(-logits[positive]))
        negative = np.exp(logits[~positive])
        probabilities[~positive] = negative / (1.0 + negative)
        output = np.floor(np.clip(probabilities, 0.0, 1.0) * 255.0 + 0.5).astype(np.uint8)
    Image.fromarray(output, mode="L").save(partial, format="PNG", optimize=True)
    os.replace(partial, output_path)
    return {"score": float(np.clip(scores[best], 0.0, 1.0))}


def _write_response(request_id: str, *, result: dict | None = None, error: str | None = None) -> None:
    response = {"request_id": request_id, "ok": error is None}
    if error is None:
        response["result"] = result
    else:
        response["error"] = error
    encoded = json.dumps(response, separators=(",", ":"), ensure_ascii=True).encode("utf-8")
    if len(encoded) > MAX_REQUEST_BYTES:
        encoded = json.dumps(
            {"request_id": request_id, "ok": False, "error": "sam_response_too_large"},
            separators=(",", ":"),
        ).encode("utf-8")
    sys.stdout.buffer.write(encoded + b"\n")
    sys.stdout.buffer.flush()


def _serve() -> int:
    while True:
        line = sys.stdin.buffer.readline(MAX_REQUEST_BYTES + 1)
        if not line:
            return 0
        if len(line) > MAX_REQUEST_BYTES or not line.endswith(b"\n"):
            _write_response("unknown", error="sam_request_too_large")
            return 2
        request_id = "unknown"
        try:
            request = json.loads(line)
            if isinstance(request, dict) and isinstance(request.get("request_id"), str):
                request_id = request["request_id"]
            result = _segment(request)
            _write_response(request_id, result=result)
        except StableWorkerError as error:
            _write_response(request_id, error=error.code)
        except Exception:
            _write_response(request_id, error="sam_execution_failed")


def main() -> int:
    global _compiled_cache_root, _compute_units
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--verify-model")
    parser.add_argument("--prepare-model")
    parser.add_argument("--compiled-cache-root")
    parser.add_argument("--artifact-sha256")
    parser.add_argument(
        "--compute-units",
        choices=("coreml_all", "coreml_cpu_and_gpu", "coreml_cpu_only", "coreml_cpu_and_ne"),
        default="coreml_all",
    )
    args = parser.parse_args()
    _compute_units = {
        "coreml_all": ct.ComputeUnit.ALL,
        "coreml_cpu_and_gpu": ct.ComputeUnit.CPU_AND_GPU,
        "coreml_cpu_only": ct.ComputeUnit.CPU_ONLY,
        "coreml_cpu_and_ne": ct.ComputeUnit.CPU_AND_NE,
    }[args.compute_units]
    if args.compiled_cache_root:
        _compiled_cache_root = Path(args.compiled_cache_root).expanduser().resolve()
    if args.verify_model or args.prepare_model:
        if _compiled_cache_root is None or not args.artifact_sha256:
            print(json.dumps({"ok": False, "error": "sam_cache_configuration_invalid"}))
            return 2
        try:
            model = args.verify_model or args.prepare_model
            if args.prepare_model:
                _prepare_models(model, _compiled_cache_root, args.artifact_sha256)
            else:
                _verify_specs(model)
                _prepared_models(_compiled_cache_root, args.artifact_sha256)
        except StableWorkerError as error:
            print(json.dumps({"ok": False, "error": error.code}))
            return 2
        except Exception:
            print(json.dumps({"ok": False, "error": "sam_model_unavailable"}))
            return 2
        print(
            json.dumps(
                {
                    "ok": True,
                    "runtime": "coremltools",
                    "runtime_version": ct.__version__,
                    "model_components": sorted(MODEL_FILES),
                    "input_size": INPUT_SIZE,
                    "max_prompts": MAX_PROMPTS,
                    "prepared": True,
                },
                separators=(",", ":"),
            )
        )
        return 0
    if _compiled_cache_root is None:
        return 2
    return _serve()


if __name__ == "__main__":
    raise SystemExit(main())
