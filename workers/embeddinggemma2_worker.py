#!/usr/bin/env python3
"""Pinned, offline EmbeddingGemma 2 worker. Request payloads never enter logs.

One process owns one model and its Metal allocation. The Rust bridge terminates
the whole process group on cancellation, including any active media decoder.
"""

from __future__ import annotations

import argparse
import contextlib
import importlib.metadata as metadata
import io
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
os.environ["TOKENIZERS_PARALLELISM"] = "false"

MAX_QUERY_BYTES = 16384
MAX_FILE_BYTES = 25 * 1024 * 1024
MAX_IMAGE_PIXELS = 16 * 1024 * 1024
MAX_AUDIO_SECONDS = 30.0
MAX_VIDEO_SECONDS = 16.0
MAX_VIDEO_FRAMES = 16
MAX_TOKENS = 8192
QUERY_PREFIX = "task: search result | query: "
MODEL = None
PROCESSOR = None
MODEL_PATH = None


class InputError(ValueError):
    pass


@contextlib.contextmanager
def quiet_dependencies():
    # Silence dependency progress, warnings and paths without retaining them.
    with open(os.devnull, "w") as sink:
        with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
            yield


def validate_runtime():
    versions = {name: metadata.version(name) for name in ("mlx", "mlx-vlm", "transformers")}
    if versions != {"mlx": "0.32.3", "mlx-vlm": "0.7.4", "transformers": "5.19.0.dev0"}:
        raise RuntimeError("media_runtime_revision_mismatch")
    for name, revision in (
        ("mlx-vlm", "3d87e88402f307efbf68e568971aa887ee7d9ed0"),
        ("transformers", "e880ddc57baa4e642c357e7c0a8e77b8d7f1b3f0"),
    ):
        receipt = metadata.distribution(name).read_text("direct_url.json")
        if receipt is None or revision not in receipt:
            raise RuntimeError("media_runtime_source_mismatch")
    import mlx.core as mx
    if not mx.metal.is_available():
        raise RuntimeError("media_metal_unavailable")
    mx.set_default_device(mx.gpu)


def load_model(model_path: str):
    global MODEL, PROCESSOR, MODEL_PATH
    path = str(Path(model_path).resolve(strict=True))
    if MODEL is not None:
        if MODEL_PATH != path:
            raise RuntimeError("media_model_identity_changed")
        return MODEL, PROCESSOR
    validate_runtime()
    import torch
    from mlx_vlm.embedding_loader import load_embedding_model
    from transformers import AutoProcessor
    torch.set_num_threads(4)
    with quiet_dependencies():
        MODEL = load_embedding_model(Path(path))
        PROCESSOR = AutoProcessor.from_pretrained(path, local_files_only=True, trust_remote_code=False)
    MODEL_PATH = path
    return MODEL, PROCESSOR


def validate_vector(values):
    if len(values) != 768 or not all(math.isfinite(value) for value in values):
        raise RuntimeError("media_embedding_invalid")
    if abs(math.sqrt(sum(value * value for value in values)) - 1.0) > 1e-4:
        raise RuntimeError("media_embedding_not_normalized")


def decoder_paths(ffmpeg: str):
    path = Path(ffmpeg).resolve(strict=True)
    ffprobe = path.parent / "ffprobe"
    if not ffprobe.is_file() or not os.access(ffprobe, os.X_OK):
        raise RuntimeError("media_ffprobe_unavailable")
    return str(path), str(ffprobe)


def probe_media(path: str, ffprobe: str, kind: str):
    # Exclude playlists, concat and network protocols, even when MIME is spoofed.
    result = subprocess.run(
        [ffprobe, "-v", "error", "-protocol_whitelist", "file,pipe",
         "-format_whitelist", "wav,flac,mp3,ogg,mov,matroska,webm",
         "-show_entries", "format=duration:stream=codec_type,width,height",
         "-of", "json", path],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=15, check=False,
    )
    if result.returncode or len(result.stdout) > 16384:
        raise InputError("media_decode_invalid")
    try:
        data = json.loads(result.stdout)
        seconds = float(data["format"]["duration"])
    except (ValueError, KeyError, TypeError):
        raise InputError("media_duration_unknown") from None
    if not math.isfinite(seconds) or seconds <= 0:
        raise InputError("media_duration_invalid")
    bound = MAX_AUDIO_SECONDS if kind == "audio" else MAX_VIDEO_SECONDS
    if seconds > bound + 1e-6:
        raise InputError("media_duration_exceeded")
    streams = [stream for stream in data.get("streams", []) if stream.get("codec_type") == kind]
    if not streams:
        raise InputError("media_stream_missing")
    if kind == "video" and any(
        not (0 < stream.get("width", 0) * stream.get("height", 0) <= MAX_IMAGE_PIXELS)
        for stream in streams
    ):
        raise InputError("media_video_dimensions_exceeded")
    return seconds


def decode_audio(path: str, ffmpeg: str):
    import numpy as np
    result = subprocess.run(
        [ffmpeg, "-nostdin", "-v", "error", "-protocol_whitelist", "file,pipe",
         "-format_whitelist", "wav,flac,mp3,ogg,mov,matroska,webm", "-i", path,
         "-map", "0:a:0", "-vn", "-t", "30.001", "-ac", "1", "-ar", "16000",
         "-f", "f32le", "pipe:1"],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=60, check=False,
    )
    if result.returncode or not result.stdout or len(result.stdout) % 4:
        raise InputError("media_decode_invalid")
    samples = np.frombuffer(result.stdout, dtype="<f4")
    if len(samples) > int(MAX_AUDIO_SECONDS * 16000) or not np.isfinite(samples).all():
        raise InputError("media_decoded_audio_exceeded")
    return samples


def decode_video(path: str, ffmpeg: str):
    import numpy as np
    from PIL import Image
    # Rust owns the parent spool directory and removes it even after SIGKILL.
    with tempfile.TemporaryDirectory(prefix="frames-", dir=Path(path).parent) as directory:
        result = subprocess.run(
            [ffmpeg, "-nostdin", "-v", "error", "-protocol_whitelist", "file,pipe",
             "-format_whitelist", "mov,matroska,webm", "-i", path, "-map", "0:v:0",
             "-an", "-t", "16.001", "-vf", "fps=1:start_time=0:round=up,scale=512:512:force_original_aspect_ratio=decrease,pad=512:512:(ow-iw)/2:(oh-ih)/2",
             "-frames:v", "17", str(Path(directory) / "%03d.png")],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60, check=False,
        )
        files = sorted(Path(directory).glob("*.png"))
        if result.returncode or not 1 <= len(files) <= MAX_VIDEO_FRAMES:
            raise InputError("media_video_frames_invalid")
        frames = []
        for frame in files:
            with Image.open(frame) as image:
                frames.append(np.asarray(image.convert("RGB")))
        return np.stack(frames)


def handle(request, ffmpeg):
    kind = request.get("kind")
    if kind == "load":
        load_model(request["model"])
        return {"loaded": True}
    if kind not in ("text", "image", "audio", "video"):
        raise InputError("media_kind_invalid")
    processing = {"input_tokens": 0, "decoded_seconds": None, "video_frames": None, "video_audio_included": False}
    if kind == "text":
        text = request.get("text")
        if not isinstance(text, str) or not text.strip() or len(text.encode()) > MAX_QUERY_BYTES:
            raise InputError("media_query_invalid")
        kwargs = {"text": [QUERY_PREFIX + text]}
    else:
        path = request.get("file")
        if not isinstance(path, str) or not Path(path).is_file() or not 0 < Path(path).stat().st_size <= MAX_FILE_BYTES:
            raise InputError("media_file_invalid")
        if kind == "image":
            from PIL import Image
            try:
                with Image.open(path) as image:
                    if image.format not in ("JPEG", "PNG") or not 0 < image.width * image.height <= MAX_IMAGE_PIXELS:
                        raise InputError("media_image_invalid")
                    # Image route accepts the caller's normalized display pixels.
                    kwargs = {"images": [[image.convert("RGB")]]}
            except (OSError, ValueError):
                raise InputError("media_image_invalid") from None
        else:
            ffmpeg, ffprobe = decoder_paths(ffmpeg)
            processing["decoded_seconds"] = probe_media(path, ffprobe, kind)
            if kind == "audio":
                kwargs = {"audio": [decode_audio(path, ffmpeg)], "sampling_rate": 16000}
            else:
                frames = decode_video(path, ffmpeg)
                processing["video_frames"] = len(frames)
                kwargs = {"videos": [frames], "do_sample_frames": False}
    model, processor = load_model(request["model"])
    import mlx.core as mx
    with quiet_dependencies():
        encoded = processor(return_tensors="np", padding=True, **kwargs)
        tokens = encoded["input_ids"].shape[-1]
        if tokens > MAX_TOKENS:
            raise InputError("media_context_exceeded")
        processing["input_tokens"] = int(tokens)
        output = model(**{key: mx.array(value) for key, value in encoded.items()}).text_embeds.astype(mx.float32)
        output /= mx.maximum(mx.linalg.norm(output, axis=-1, keepdims=True), mx.array(1e-12))
        mx.eval(output)
        if output.shape != (1, 768):
            raise RuntimeError("media_embedding_shape_invalid")
        values = output[0].tolist()
        validate_vector(values)
    return {"embedding": values, "processing": processing}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ffmpeg", required=True)
    parser.add_argument("--verify-model")
    args = parser.parse_args()
    if args.verify_model:
        try:
            decoder_paths(args.ffmpeg)
            load_model(args.verify_model)
            print(json.dumps({"ok": True, "runtime": "mlx-vlm", "precision": "bf16", "dimensions": 768, "device": "metal"}))
            return 0
        except Exception:
            print(json.dumps({"ok": False, "error": "media_runtime_unavailable"}))
            return 1
    for line in sys.stdin:
        request_id = "unknown"
        try:
            if len(line) > 64 * 1024:
                raise InputError("media_worker_frame_exceeded")
            request = json.loads(line)
            request_id = request.get("request_id", request_id)
            result = handle(request, args.ffmpeg)
            response = {"request_id": request_id, "ok": True, "result": result}
        except Exception as error:
            code = str(error) if isinstance(error, InputError) else "media_execution_failed"
            response = {"request_id": request_id, "ok": False, "error": code, "invalid_input": isinstance(error, InputError)}
        print(json.dumps(response, allow_nan=False, separators=(",", ":")), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
