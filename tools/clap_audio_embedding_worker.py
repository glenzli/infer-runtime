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
import urllib.error
import urllib.request
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
MAX_NORMALIZER_RESPONSE_BYTES = 64 * 1024


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

    def embed_text(self, text: str, normalizer: dict | None = None) -> tuple[list[float], dict | None]:
        if not text.strip():
            raise WorkerFailure("text_required")
        if len(text.encode("utf-8")) > MAX_TEXT_BYTES:
            raise WorkerFailure("text_too_large")
        normalizer_provenance = None
        if normalizer is not None:
            text = self._normalize_zh_query(text, normalizer)
            normalizer_provenance = {
                "deployment": normalizer["deployment"],
                "build": normalizer["build"],
                "prompt_revision": normalizer["prompt_revision"],
                "source_language": normalizer["source_language"],
                "target_language": normalizer["target_language"],
            }
        try:
            inputs = self.processor(text=[text], return_tensors="pt", padding=True)
            inputs = {key: value.to(self.device) for key, value in inputs.items()}
            with torch.inference_mode():
                vector = self.model.get_text_features(**inputs)
                vector = F.normalize(vector, p=2, dim=-1)
            torch.mps.synchronize()
            return vector[0].float().cpu().tolist(), normalizer_provenance
        except Exception as error:
            raise WorkerFailure("clap_text_embedding_failed") from error

    def _normalize_zh_query(self, text: str, normalizer: dict) -> str:
        required = {
            "endpoint", "model", "deployment", "build", "prompt_revision",
            "source_language", "target_language", "max_query_bytes", "max_output_bytes",
        }
        if set(normalizer) != required or normalizer["source_language"] != "zh" or normalizer["target_language"] != "en":
            raise WorkerFailure("query_normalizer_unavailable")
        if len(text.encode("utf-8")) > normalizer["max_query_bytes"]:
            raise WorkerFailure("query_normalizer_input_too_large")
        payload = json.dumps({
            "model": normalizer["model"], "input": text,
            "instructions": "Translate this Chinese sound-search query into concise English only. Return plain English text. No explanation, quotation, markdown, or newlines.",
            "stream": False, "reasoning": {"effort": "none"}, "max_output_tokens": 48,
        }).encode("utf-8")
        request = urllib.request.Request(normalizer["endpoint"], data=payload, headers={"Content-Type": "application/json"}, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                if response.status != 200:
                    raise WorkerFailure("query_normalizer_unavailable")
                body = response.read(MAX_NORMALIZER_RESPONSE_BYTES + 1)
        except (OSError, urllib.error.URLError, urllib.error.HTTPError) as error:
            raise WorkerFailure("query_normalizer_unavailable") from error
        if len(body) > MAX_NORMALIZER_RESPONSE_BYTES:
            raise WorkerFailure("query_normalizer_invalid_output")
        try:
            result = " ".join(
                item["text"] for output in json.loads(body)["output"]
                if output.get("type") == "message"
                for item in output.get("content", [])
                if item.get("type") == "output_text" and isinstance(item.get("text"), str)
            ).strip()
        except (KeyError, TypeError, json.JSONDecodeError) as error:
            raise WorkerFailure("query_normalizer_invalid_output") from error
        if not result or "\n" in result or not result.isascii() or len(result.encode("utf-8")) > normalizer["max_output_bytes"]:
            raise WorkerFailure("query_normalizer_invalid_output")
        return result

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
    vector, _ = runtime.embed_text("runtime readiness probe")
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
                "request_id", "operation", "model", "audio_path", "text", "language", "normalizer"
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
                normalizer_provenance = None
            elif operation == "embed_text" and isinstance(request.get("text"), str):
                vector, normalizer_provenance = runtime.embed_text(request["text"], request.get("normalizer"))
            else:
                raise WorkerFailure("invalid_worker_request")
            print(
                json.dumps(
                    {
                        "request_id": request_id,
                        "ok": True,
                        "result": {"embedding": vector, "dimensions": len(vector), "normalized": True, **({"normalizer": normalizer_provenance} if normalizer_provenance else {})},
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
