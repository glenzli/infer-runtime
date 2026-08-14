#!/usr/bin/env python3
"""Bounded local worker for one admitted CLAP audio-text embedding Build.

The Rust Provider owns admission, artifact identity and temporary input files.
This process deliberately exposes only two typed operations over JSON-lines:
``embed_audio`` and ``embed_text``.  It never downloads a model, accepts a
Consumer-provided path, or logs text/audio payloads.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
# These must be set before importing torch: the admitted Build is MPS-only and
# local-artifact-only. A missing MPS kernel must surface as a worker failure,
# never silently run on CPU or download an upstream artifact.
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
os.environ["PYTORCH_ENABLE_MPS_FALLBACK"] = "0"

import numpy as np
import torch
import torch.nn.functional as F
from transformers import ClapModel, ClapProcessor


SAMPLE_RATE_HZ = 48_000
MAX_AUDIO_SECONDS = 10
MAX_PCM_BYTES = SAMPLE_RATE_HZ * MAX_AUDIO_SECONDS * 4
MAX_TEXT_BYTES = 16 * 1024
MAX_REQUEST_BYTES = 128 * 1024


class WorkerFailure(Exception):
    """A deliberately payload-free, stable worker failure."""


@dataclass
class ClapRuntime:
    model_path: Path
    device: torch.device
    model: ClapModel
    processor: ClapProcessor
    ffmpeg: str

    @classmethod
    def load(cls, model_path: str, ffmpeg: str) -> "ClapRuntime":
        if not torch.backends.mps.is_built() or not torch.backends.mps.is_available():
            raise WorkerFailure("mps_unavailable")
        path = Path(model_path)
        if not path.is_dir():
            raise WorkerFailure("admitted_model_unavailable")
        device = torch.device("mps")
        try:
            model = ClapModel.from_pretrained(path, local_files_only=True).to(device).eval()
            processor = ClapProcessor.from_pretrained(path, local_files_only=True)
        except Exception as error:  # Model/runtime details are operator-only.
            raise WorkerFailure("clap_model_load_failed") from error
        return cls(path, device, model, processor, ffmpeg)

    def embed_audio(self, audio_path: str) -> list[float]:
        waveform = self._decode_audio(audio_path)
        try:
            inputs = self.processor(
                audios=waveform, sampling_rate=SAMPLE_RATE_HZ, return_tensors="pt"
            )
            inputs = {key: value.to(self.device) for key, value in inputs.items()}
            with torch.inference_mode():
                vector = self.model.get_audio_features(**inputs)
                vector = F.normalize(vector, p=2, dim=-1)
            torch.mps.synchronize()
            return vector[0].float().cpu().tolist()
        except Exception as error:
            raise WorkerFailure("clap_audio_embedding_failed") from error

    def embed_text(self, text: str) -> list[float]:
        if not text.strip():
            raise WorkerFailure("text_required")
        if len(text.encode("utf-8")) > MAX_TEXT_BYTES:
            raise WorkerFailure("text_too_large")
        try:
            inputs = self.processor(text=[text], return_tensors="pt", padding=True)
            inputs = {key: value.to(self.device) for key, value in inputs.items()}
            with torch.inference_mode():
                vector = self.model.get_text_features(**inputs)
                vector = F.normalize(vector, p=2, dim=-1)
            torch.mps.synchronize()
            return vector[0].float().cpu().tolist()
        except Exception as error:
            raise WorkerFailure("clap_text_embedding_failed") from error

    def _decode_audio(self, audio_path: str) -> np.ndarray:
        path = Path(audio_path)
        if not path.is_file():
            raise WorkerFailure("audio_input_unavailable")
        try:
            completed = subprocess.run(
                [
                    self.ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-i",
                    str(path),
                    "-map",
                    "a:0",
                    "-ac",
                    "1",
                    "-ar",
                    str(SAMPLE_RATE_HZ),
                    "-f",
                    "f32le",
                    "-",
                ],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                timeout=30,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise WorkerFailure("audio_decode_failed") from error
        if completed.returncode != 0 or not completed.stdout:
            raise WorkerFailure("audio_decode_failed")
        if len(completed.stdout) > MAX_PCM_BYTES:
            raise WorkerFailure("audio_duration_exceeds_10_seconds")
        if len(completed.stdout) % 4:
            raise WorkerFailure("audio_decode_failed")
        return np.frombuffer(completed.stdout, dtype="<f4").copy()


def verify(model_path: str, ffmpeg: str) -> int:
    runtime = ClapRuntime.load(model_path, ffmpeg)
    vector = runtime.embed_text("runtime readiness probe")
    if len(vector) != 512:
        raise WorkerFailure("unexpected_embedding_dimensions")
    print(json.dumps({"dimensions": 512, "runtime": "pytorch-mps"}, sort_keys=True))
    return 0


def request_error(request_id: str, code: str) -> None:
    print(json.dumps({"request_id": request_id, "ok": False, "error": code}, sort_keys=True), flush=True)


def serve(ffmpeg: str) -> int:
    runtime: ClapRuntime | None = None
    for raw_line in sys.stdin.buffer:
        request_id = ""
        if len(raw_line) > MAX_REQUEST_BYTES or not raw_line.endswith(b"\n"):
            request_error("", "invalid_worker_frame")
            continue
        try:
            request = json.loads(raw_line)
            if not isinstance(request, dict) or set(request) - {
                "request_id", "operation", "model", "audio_path", "text"
            }:
                raise WorkerFailure("invalid_worker_request")
            request_id = request.get("request_id")
            if not isinstance(request_id, str) or not request_id:
                raise WorkerFailure("invalid_worker_request")
            model_path = request.get("model")
            if not isinstance(model_path, str):
                raise WorkerFailure("invalid_worker_request")
            # The Provider sends this private path only after resolving the
            # versioned Build through ArtifactStore. A Worker never receives
            # model identity from a Consumer request.
            if runtime is None or str(runtime.model_path) != model_path:
                runtime = ClapRuntime.load(model_path, ffmpeg)
            operation = request.get("operation")
            if operation == "embed_audio" and isinstance(request.get("audio_path"), str):
                vector = runtime.embed_audio(request["audio_path"])
            elif operation == "embed_text" and isinstance(request.get("text"), str):
                vector = runtime.embed_text(request["text"])
            else:
                raise WorkerFailure("invalid_worker_request")
            print(
                json.dumps(
                    {
                        "request_id": request_id,
                        "ok": True,
                        "result": {"embedding": vector, "dimensions": len(vector), "normalized": True},
                    },
                    separators=(",", ":"),
                ),
                flush=True,
            )
        except WorkerFailure as error:
            request_error(request_id, str(error))
        except Exception:
            request_error(request_id, "worker_protocol_failure")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ffmpeg", required=True)
    parser.add_argument("--verify-model")
    args = parser.parse_args()
    if args.verify_model:
        return verify(args.verify_model, args.ffmpeg)
    return serve(args.ffmpeg)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except WorkerFailure as error:
        print(json.dumps({"ok": False, "error": str(error)}, sort_keys=True))
        raise SystemExit(2)
