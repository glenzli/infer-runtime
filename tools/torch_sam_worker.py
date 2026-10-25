#!/usr/bin/env python3
"""Offline, bounded SAM 2.1 Small worker with an explicit CUDA FP32 Build."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import select
import sys

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"

MODEL_REVISION = "ee5bba1d82bb8749febdf90f45e84b687142ba03"
MODEL_FILES = {
    "config.json": (5698, "97ff9f65b76d107acda4247885f0a5555d0048850ae3c5f97183df289aaecde9"),
    "model.safetensors": (184305280, "0a4067b11ce1e23d5229203f11c718a823060d15a4b23fa2372a7d4b77cbbc60"),
    "preprocessor_config.json": (683, "6ebf229ee259368ce4a8d4f2fe893a72b053023710853e257253939e601f583d"),
    "processor_config.json": (95, "f8a68e865cfad115c1c2763f3d93eca7b1c622da06da2a9273eb437fb2389b6d"),
}
RUNTIME_VERSION = "4.56.2/torch-2.8.0+cu128"
MAX_FRAME = 64 * 1024
MAX_IMAGE_PIXELS = 40_000_000
MAX_PROMPTS = 16
INPUT_SIZE = 1024
_loaded_root = None
_model = None
_processor = None
_image_sha256 = None
_image_embeddings = None


class WorkerError(Exception):
    pass


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def artifact_set_digest():
    return hashlib.sha256("".join(name + "\0" + digest + "\n" for name, (_, digest) in sorted(MODEL_FILES.items())).encode()).hexdigest()


def _load(root, artifact):
    global _loaded_root, _model, _processor, _image_sha256, _image_embeddings
    if _model is not None and _loaded_root == root and artifact == artifact_set_digest():
        return
    path = Path(root)
    if not path.is_absolute() or artifact != artifact_set_digest():
        raise WorkerError("sam_model_contract_mismatch")
    for name, (size, digest) in MODEL_FILES.items():
        candidate = path / name
        if candidate.is_symlink() or not candidate.is_file() or candidate.stat().st_size != size or file_sha256(candidate) != digest:
            raise WorkerError("sam_model_contract_mismatch")
    import torch
    import transformers
    import torchvision
    from transformers import Sam2Model, Sam2Processor
    if (torch.__version__ != "2.8.0+cu128" or torchvision.__version__ != "0.23.0+cu128"
            or transformers.__version__ != "4.56.2" or not torch.cuda.is_available()):
        raise WorkerError("sam_cuda_runtime_unavailable")
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    model, loading = Sam2Model.from_pretrained(path, local_files_only=True, dtype=torch.float32, attn_implementation="eager", output_loading_info=True)
    if loading.get("missing_keys") or loading.get("mismatched_keys") or loading.get("error_msgs"):
        raise WorkerError("sam_model_contract_mismatch")
    _model = model.eval().to("cuda")
    _processor = Sam2Processor.from_pretrained(path, local_files_only=True)
    if next(_model.parameters()).device.type != "cuda":
        raise WorkerError("sam_execution_provider_mismatch")
    _loaded_root = root
    _image_sha256 = None
    _image_embeddings = None


def _evict():
    global _model, _processor, _loaded_root, _image_sha256, _image_embeddings
    _model = _processor = _loaded_root = _image_sha256 = _image_embeddings = None
    if "torch" in sys.modules:
        sys.modules["torch"].cuda.empty_cache()


def _segment(request):
    global _image_sha256, _image_embeddings
    required = {"request_id", "operation", "model", "artifact_sha256", "image_path", "image_sha256", "output_path", "points"}
    if not isinstance(request, dict) or not required <= set(request) or set(request) - required - {"box_prompt"}:
        raise WorkerError("sam_invalid_request")
    if request["operation"] not in {"segment_subject", "segment_subject_soft_mask"}:
        raise WorkerError("sam_invalid_request")
    points = request["points"]
    box = request.get("box_prompt")
    if not isinstance(points, list) or not 1 <= len(points) + (2 if box is not None else 0) <= MAX_PROMPTS:
        raise WorkerError("sam_invalid_request")
    _load(request["model"], request["artifact_sha256"])
    import numpy as np
    import torch
    from PIL import Image
    Image.MAX_IMAGE_PIXELS = MAX_IMAGE_PIXELS
    image_path = Path(request["image_path"])
    output_path = Path(request["output_path"])
    if not image_path.is_absolute() or not output_path.is_absolute() or output_path.exists() or file_sha256(image_path) != request["image_sha256"]:
        raise WorkerError("sam_invalid_request")
    with Image.open(image_path) as opened:
        if opened.width * opened.height > MAX_IMAGE_PIXELS:
            raise WorkerError("sam_invalid_request")
        image = opened.convert("RGB")
    width, height = image.size
    kwargs = {}
    if points:
        coordinates, labels = [], []
        for point in points:
            if set(point) != {"x", "y", "label"} or point["label"] not in {"foreground", "background"}:
                raise WorkerError("sam_invalid_request")
            x, y = float(point["x"]), float(point["y"])
            if not all(math.isfinite(v) and 0 <= v <= 1 for v in (x, y)):
                raise WorkerError("sam_invalid_request")
            coordinates.append([x * width, y * height])
            labels.append(1 if point["label"] == "foreground" else 0)
        kwargs.update(input_points=[[coordinates]], input_labels=[[labels]])
    if box is not None:
        if set(box) != {"x", "y", "width", "height"}:
            raise WorkerError("sam_invalid_request")
        x, y, w, h = (np.float32(box[name]) for name in ("x", "y", "width", "height"))
        if not all(np.isfinite(v) for v in (x, y, w, h)) or x < 0 or y < 0 or w <= 0 or h <= 0 or x + w > 1 or y + h > 1:
            raise WorkerError("sam_invalid_request")
        kwargs["input_boxes"] = [[[float(x) * width, float(y) * height, float(x+w) * width, float(y+h) * height]]]
    with torch.inference_mode():
        inputs = _processor(images=image, return_tensors="pt", **kwargs).to("cuda")
        if _image_embeddings is None or _image_sha256 != request["image_sha256"]:
            _image_embeddings = _model.get_image_embeddings(inputs["pixel_values"])
            _image_sha256 = request["image_sha256"]
        if any(value.device.type != "cuda" for value in _image_embeddings):
            raise WorkerError("sam_execution_provider_mismatch")
        output = _model(image_embeddings=_image_embeddings, multimask_output=True,
                        **{name: inputs[name] for name in ("input_points", "input_labels", "input_boxes") if name in inputs})
        scores = output.iou_scores[0, 0].float()
        logits = output.pred_masks[0, 0].float()
        if scores.shape != (3,) or logits.shape != (3, 256, 256) or not bool(torch.isfinite(scores).all()) or not bool(torch.isfinite(logits).all()) or logits.device.type != "cuda":
            raise WorkerError("sam_model_contract_mismatch")
        best = int(torch.argmax(scores).item())
        selected = logits[best]
        if request["operation"] == "segment_subject_soft_mask":
            raster = torch.floor(torch.sigmoid(selected).clamp(0, 1) * 255 + 0.5).to(torch.uint8)
        else:
            raster = (torch.nn.functional.interpolate(selected[None, None], size=(height, width), mode="bilinear", align_corners=False)[0, 0] > 0).to(torch.uint8) * 255
        result = raster.cpu().numpy()
        score = float(scores[best].clamp(0, 1).item())
    partial = output_path.with_suffix(".partial")
    Image.fromarray(result).save(partial, format="PNG", optimize=True)
    os.replace(partial, output_path)
    return {"score": score, "actual_execution_provider": "cuda", "runtime_version": RUNTIME_VERSION}


def _response(value):
    frame = json.dumps(value, separators=(",", ":"), ensure_ascii=True).encode() + b"\n"
    if len(frame) > MAX_FRAME:
        raise WorkerError("sam_response_too_large")
    sys.stdout.buffer.write(frame)
    sys.stdout.buffer.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify-model")
    parser.add_argument("--artifact-sha256")
    args = parser.parse_args()
    if args.verify_model:
        try:
            _load(args.verify_model, args.artifact_sha256)
            _response({"ok": True, "runtime": "transformers", "runtime_version": RUNTIME_VERSION,
                       "actual_execution_provider": "cuda", "model_components": ["image", "prompt", "mask"],
                       "input_size": INPUT_SIZE, "max_prompts": MAX_PROMPTS})
            return 0
        except WorkerError as error:
            _response({"ok": False, "error": str(error)})
            return 2
        except Exception:
            _response({"ok": False, "error": "sam_model_unavailable"})
            return 2
    while True:
        # One cached image and model are released after the same 30 second idle
        # window as Qwen; a later request reloads from admitted local artifacts.
        if not select.select([sys.stdin.buffer], [], [], 30)[0]:
            _evict()
            continue
        line = sys.stdin.buffer.readline(MAX_FRAME + 1)
        if not line:
            return 0
        if len(line) > MAX_FRAME or not line.endswith(b"\n"):
            return 2
        request_id = "unknown"
        try:
            request = json.loads(line)
            request_id = request.get("request_id", "unknown")
            _response({"request_id": request_id, "ok": True, "result": _segment(request)})
        except WorkerError as error:
            _response({"request_id": request_id, "ok": False, "error": str(error)})
        except Exception:
            _response({"request_id": request_id, "ok": False, "error": "sam_execution_failed"})


if __name__ == "__main__":
    sys.exit(main())
