#!/usr/bin/env python3
"""Offline Stable Audio 3 Small-SFX bridge for Infer's audio worker protocol.

Each request runs in this worker process, so killing the worker also cancels
MLX inference. No Gradio server, shell, or child model process is started.
"""

from __future__ import annotations

import contextlib
import argparse
import hashlib
import json
import os
import runpy
import sys
import traceback
import wave
from pathlib import Path

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"

MAX_REQUEST_BYTES = 4096
MAX_OUTPUT_BYTES = 6 * 1024 * 1024
MODEL_ID = "stabilityai/stable-audio-3-optimized@da6edc54ddba10bfd79a077102ded687f80e882b:sm-sfx"
REQUIRED_WEIGHTS = {
    "t5gemma_f16.npz": "8deb20489f36d9aec539f26c9c67321f99bc5fe300d470435ed6e76be4f16bbd",
    "dit_sm-sfx_f16.npz": "7e702d2640699a57fe436ca975fda16832040ba568c1e092c2ae826987558118",
    "same_s_decoder_f32.npz": "909928a8e6937c1ebe6ac4b729f0462bd3773704a11ea18278e42671dc69bfe4",
}
_verified_root: Path | None = None


def _verify_model(root: Path) -> Path:
    global _verified_root
    root = root.resolve(strict=True)
    if root == _verified_root:
        return root
    if not (root / "scripts/sa3_mlx.py").is_file():
        raise ValueError("sound_model_unavailable")
    for name, expected in REQUIRED_WEIGHTS.items():
        path = root / "models/mlx" / name
        if not path.is_file():
            raise ValueError("sound_model_unavailable")
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        if digest != expected:
            raise ValueError("sound_model_integrity_error")
    _verified_root = root
    return root


def _run(request: dict, model_root: Path) -> dict:
    if request.get("operation") != "generate_sound":
        raise ValueError("unsupported_operation")
    model = request.get("model")
    prompt = request.get("prompt")
    duration = request.get("duration_seconds")
    seed = request.get("seed")
    output = request.get("output_path")
    if model != MODEL_ID:
        raise ValueError("sound_model_unavailable")
    if not isinstance(prompt, str) or not 1 <= len(prompt.encode("utf-8")) <= 2000 or not prompt.strip() or any(ord(c) < 32 or ord(c) == 127 for c in prompt):
        raise ValueError("invalid_sound_prompt")
    if type(duration) is not int or not 1 <= duration <= 30:
        raise ValueError("invalid_sound_duration")
    if type(seed) is not int or not 0 <= seed <= 0xFFFFFFFF:
        raise ValueError("invalid_sound_seed")
    if not isinstance(output, str) or not Path(output).is_absolute() or Path(output).suffix != ".wav":
        raise ValueError("invalid_sound_output")

    root = _verify_model(model_root)
    script = root / "scripts/sa3_mlx.py"
    output_path = Path(output)
    previous_argv = sys.argv
    try:
        sys.argv = [str(script), "--prompt", prompt, "--dit", "sm-sfx", "--decoder", "same-s", "--seconds", str(duration), "--seed", str(seed), "--out", str(output_path)]
        with contextlib.redirect_stdout(sys.stderr):
            runpy.run_path(str(script), run_name="__main__")
    finally:
        sys.argv = previous_argv
    if not output_path.is_file() or not 44 <= output_path.stat().st_size <= MAX_OUTPUT_BYTES:
        raise ValueError("invalid_sound_output")
    with wave.open(str(output_path), "rb") as wav:
        if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate(), wav.getnframes()) != (2, 2, 44100, duration * 44100):
            raise ValueError("invalid_sound_output")
    return {"format": "wav", "sample_rate_hz": 44100, "channels": 2, "duration_seconds": duration, "seed": seed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-root", type=Path, required=True)
    args = parser.parse_args()
    for raw in sys.stdin.buffer:
        if len(raw) > MAX_REQUEST_BYTES:
            continue
        request_id = None
        try:
            request = json.loads(raw)
            request_id = request["request_id"]
            result = _run(request, args.model_root)
            response = {"request_id": request_id, "ok": True, "result": result}
        except Exception as exc:
            traceback.print_exc(file=sys.stderr)
            error = str(exc) if isinstance(exc, ValueError) else "sound_generation_failed"
            response = {"request_id": request_id, "ok": False, "error": error}
        sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
