#!/usr/bin/env python3
"""Persistent, offline-first MLX audio worker using a JSON-lines protocol."""

from __future__ import annotations

import contextlib
import base64
import gc
import json
import os
import sys
import traceback
from collections import OrderedDict
from pathlib import Path
from typing import Any

# A Hugging Face repo id is never required by the checked-in registry: model_id
# points at an exact local snapshot. Keep accidental resolution offline as well.
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")

MODEL_CACHE: OrderedDict[tuple[str, str], Any] = OrderedDict()
MAX_LOADED_MODELS = max(1, int(os.environ.get("INFER_AUDIO_MAX_LOADED_MODELS", "1")))


def _load_model(category: str, model_path: str) -> Any:
    key = (category, model_path)
    if key in MODEL_CACHE:
        MODEL_CACHE.move_to_end(key)
        return MODEL_CACHE[key]

    # Release the previous model before allocating the next one. Loading first
    # would briefly double resident model memory during a route change.
    while len(MODEL_CACHE) >= MAX_LOADED_MODELS:
        _, evicted = MODEL_CACHE.popitem(last=False)
        del evicted
        gc.collect()
        try:
            import mlx.core as mx

            mx.clear_cache()
        except Exception:
            pass

    # mlx-audio reports model-loading progress on stdout; stdout belongs to the
    # JSON protocol, so all library chatter is redirected to daemon stderr.
    with contextlib.redirect_stdout(sys.stderr):
        if category == "stt":
            from mlx_audio.stt.utils import load_model
        else:
            from mlx_audio.tts.utils import load_model
        model = load_model(model_path)

    MODEL_CACHE[key] = model
    MODEL_CACHE.move_to_end(key)
    return model


def _jsonable(value: Any) -> Any:
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, dict):
        return {str(key): _jsonable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_jsonable(item) for item in value]
    if hasattr(value, "__dict__"):
        return {
            key: _jsonable(item)
            for key, item in vars(value).items()
            if not key.startswith("_")
        }
    if hasattr(value, "tolist"):
        return value.tolist()
    return str(value)


def _transcribe(request: dict[str, Any]) -> dict[str, Any]:
    model = _load_model("stt", request["model"])
    kwargs: dict[str, Any] = {"verbose": False}
    if request.get("language"):
        kwargs["language"] = request["language"]
    if request.get("temperature") is not None:
        kwargs["temperature"] = request["temperature"]
    if request.get("prompt"):
        kwargs["system_prompt"] = request["prompt"]
    with contextlib.redirect_stdout(sys.stderr):
        result = model.generate(request["audio_path"], **kwargs)
    return {
        "text": result.text,
        "language": getattr(result, "language", None),
        "segments": _jsonable(getattr(result, "segments", None)),
        "usage": {
            "prompt_tokens": getattr(result, "prompt_tokens", 0),
            "completion_tokens": getattr(result, "generation_tokens", 0),
            "total_tokens": getattr(result, "total_tokens", 0),
        },
    }


def _align(request: dict[str, Any]) -> dict[str, Any]:
    model = _load_model("stt", request["model"])
    language = request.get("language") or "Chinese"
    with contextlib.redirect_stdout(sys.stderr):
        result = model.generate(
            request["audio_path"], text=request["text"], language=language
        )
    items = [
        {
            "text": item.text,
            "start": item.start_time,
            "end": item.end_time,
        }
        for item in result
    ]
    return {"text": request["text"], "language": language, "items": items}


def _speech(request: dict[str, Any], *, voice_clone: bool) -> dict[str, Any]:
    import mlx.core as mx
    from mlx_audio.audio_io import write as audio_write

    model = _load_model("tts", request["model"])
    kwargs: dict[str, Any] = {
        "text": request["text"],
        "lang_code": request.get("language") or "auto",
        "speed": request.get("speed") or 1.0,
        "verbose": False,
        "stream": False,
    }
    if voice_clone:
        kwargs["ref_audio"] = request["reference_audio_path"]
        kwargs["ref_text"] = request["reference_text"]
    else:
        if request.get("voice"):
            kwargs["voice"] = request["voice"]
        if request.get("instructions"):
            kwargs["instruct"] = request["instructions"]

    with contextlib.redirect_stdout(sys.stderr):
        results = list(model.generate(**kwargs))
    if not results:
        raise RuntimeError("speech model produced no audio")
    chunks = [result.audio for result in results]
    audio = mx.concatenate(chunks, axis=0) if len(chunks) > 1 else chunks[0]
    output_path = Path(request["output_path"])
    output_path.parent.mkdir(parents=True, exist_ok=True)
    audio_format = request.get("format") or "wav"
    with contextlib.redirect_stdout(sys.stderr):
        audio_write(output_path, audio, results[0].sample_rate, format=audio_format)
    return {
        "sample_rate": results[0].sample_rate,
        "segments": len(results),
        "output_path": str(output_path),
    }


def _emit_stream_frame(frame: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(frame, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def _speech_stream(request: dict[str, Any]) -> None:
    """Emit native TTS generator chunks as mono signed-16-bit little-endian PCM."""
    import numpy as np

    model = _load_model("tts", request["model"])
    kwargs: dict[str, Any] = {
        "text": request["text"],
        "lang_code": request.get("language") or "auto",
        "speed": request.get("speed") or 1.0,
        "verbose": False,
        "stream": True,
    }
    if request.get("voice"):
        kwargs["voice"] = request["voice"]
    if request.get("instructions"):
        kwargs["instruct"] = request["instructions"]

    request_id = str(request["request_id"])
    started = False
    with contextlib.redirect_stdout(sys.stderr):
        results = iter(model.generate(**kwargs))
    while True:
        try:
            with contextlib.redirect_stdout(sys.stderr):
                result = next(results)
        except StopIteration:
            break
        sample_rate = int(result.sample_rate)
        if not started:
            _emit_stream_frame(
                {
                    "request_id": request_id,
                    "ok": True,
                    "event": "started",
                    "sample_rate": sample_rate,
                    "channels": 1,
                    "sample_format": "pcm_s16le",
                }
            )
            started = True
        audio = np.asarray(result.audio, dtype=np.float32).reshape(-1)
        pcm = (np.clip(audio, -1.0, 1.0) * 32767.0).astype("<i2").tobytes()
        if pcm:
            _emit_stream_frame(
                {
                    "request_id": request_id,
                    "ok": True,
                    "event": "audio_chunk",
                    "audio_base64": base64.b64encode(pcm).decode("ascii"),
                }
            )
    if not started:
        raise RuntimeError("speech model produced no audio")
    _emit_stream_frame(
        {"request_id": request_id, "ok": True, "event": "completed"}
    )


def handle(request: dict[str, Any]) -> dict[str, Any]:
    operation = request.get("operation")
    if operation == "transcribe":
        return _transcribe(request)
    if operation == "align":
        return _align(request)
    if operation == "speech":
        return _speech(request, voice_clone=False)
    if operation == "voice_clone":
        return _speech(request, voice_clone=True)
    raise ValueError(f"unsupported operation: {operation}")


def main() -> int:
    for line in sys.stdin:
        if not line.strip():
            continue
        request_id = "unknown"
        try:
            request = json.loads(line)
            request_id = str(request.get("request_id", request_id))
            if request.get("operation") == "speech_stream":
                try:
                    _speech_stream(request)
                except Exception as error:
                    traceback.print_exc(file=sys.stderr)
                    _emit_stream_frame(
                        {"request_id": request_id, "ok": False, "error": str(error)}
                    )
                continue
            result = handle(request)
            response = {"request_id": request_id, "ok": True, "result": result}
        except Exception as error:
            traceback.print_exc(file=sys.stderr)
            response = {"request_id": request_id, "ok": False, "error": str(error)}
        sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
