#!/usr/bin/env python3
"""Persistent, offline YAMNet sound-event worker using the audio JSON-lines SPI.

The official TF Hub archive is installed out of tree. Every executable file
and the 521-class map are verified before TensorFlow loads the SavedModel.
Stdout belongs exclusively to the worker protocol; TensorFlow and decoder
diagnostics stay off that channel.
"""

from __future__ import annotations

import csv
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

os.environ.setdefault("TF_CPP_MIN_LOG_LEVEL", "3")
os.environ.setdefault("CUDA_VISIBLE_DEVICES", "-1")

MODEL_ID = "google/yamnet/1"
MODEL_ARCHIVE_SHA256 = "b80da2a1a56926fb0767205051a200dd7b3beaf3ea1ea126c42a53943996e5e0"
MODEL_ARTIFACT_SET_SHA256 = "4730c9bde533285dc0b74b8b94c798273b40d91fc746b17f0c9281ac8764d6b8"
MODEL_LICENSE_SPDX = "Apache-2.0"
TRAINING_DATA_LICENSE_SPDX = "CC-BY-4.0"
MODEL_FILES = {
    "saved_model.pb": "672af6e1e34fe15a42d45d70217fd39f97e10aef9b0effbf9b0bf7826fccd462",
    "variables/variables.data-00000-of-00001": "d6753f22f173b2a8b1ce78918eaae79bf0a41ca61f4cfe9a1b948c97ff094ddc",
    "variables/variables.index": "0bc2ca10e56e8a71b96a2cad26adbbabff927e9344fbd2df8ae7275ddf76ae1e",
    "assets/yamnet_class_map.csv": "cdf24d193e196d9e95912a2667051ae203e92a2ba09449218ccb40ef787c6df2",
}

ONTOLOGY_REVISION = "yamnet-class-map@cdf24d193e19"
ONTOLOGY_LICENSE_SPDX = "CC-BY-SA-4.0"
CLASS_ID_NAMESPACE = "audioset_mid"
CLASS_COUNT = 521

PREPROCESSING_IDENTITY = "ffmpeg_decode_mono_f32le_16khz_then_tfhub_yamnet_waveform_v1"
POLICY_REVISION = "yamnet-audioset-event-policy-v1"
SPEECH_CLASS_SET_REVISION = "yamnet-audioset-speech-family-indices-0-through-12-v1"
SAMPLE_RATE_HZ = 16_000
WINDOW_SECONDS = 0.96
HOP_SECONDS = 0.48
EVENT_SCORE_THRESHOLD = 0.10
SMOOTHING_WINDOW_FRAMES = 3
MAX_CLASSES_PER_WINDOW = 12
MAX_EVENTS = 10_000
MAX_AUDIO_SECONDS = 600
MAX_REQUEST_LINE_BYTES = 64 * 1024
SPEECH_PRESENT_THRESHOLD = 0.30
SPEECH_ABSENT_THRESHOLD = 0.05
# AudioSet's Speech parent and speech-specific descendants in YAMNet's fixed map.
SPEECH_CLASS_INDICES = tuple(range(13))

FFMPEG = os.environ.get("INFER_YAMNET_FFMPEG", "ffmpeg")
_MODEL_CACHE: dict[str, tuple[Any, list[tuple[str, str]], str]] = {}
_DECODER_VERSION: str | None = None


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _verify_model_directory(root: Path) -> None:
    if not root.is_dir():
        raise ValueError("configured YAMNet model directory is unavailable")
    for relative, expected in MODEL_FILES.items():
        path = root / relative
        if not path.is_file() or path.is_symlink() or _sha256(path) != expected:
            raise ValueError(f"configured YAMNet artifact failed identity verification: {relative}")


def _load_classes(path: Path) -> list[tuple[str, str]]:
    with path.open(newline="", encoding="utf-8") as source:
        rows = list(csv.DictReader(source))
    if len(rows) != CLASS_COUNT:
        raise ValueError("YAMNet class map does not contain the expected 521 classes")
    classes: list[tuple[str, str]] = []
    for expected_index, row in enumerate(rows):
        if int(row["index"]) != expected_index or not row["mid"] or not row["display_name"]:
            raise ValueError("YAMNet class map has an invalid index or class identity")
        classes.append((row["mid"], row["display_name"]))
    return classes


def _load_model(model_path: str) -> tuple[Any, list[tuple[str, str]], str]:
    configured = Path(model_path).expanduser()
    if configured.is_symlink():
        raise ValueError("configured YAMNet model directory cannot be a symbolic link")
    root = configured.resolve()
    key = str(root)
    if key in _MODEL_CACHE:
        return _MODEL_CACHE[key]
    _verify_model_directory(root)
    classes = _load_classes(root / "assets/yamnet_class_map.csv")
    import tensorflow as tf

    model = tf.saved_model.load(str(root))
    signature = model.signatures.get("serving_default")
    if signature is None:
        raise ValueError("YAMNet SavedModel has no serving_default signature")
    output = signature.structured_outputs.get("output_0")
    if output is None or output.shape.rank != 2 or output.shape[-1] != CLASS_COUNT:
        raise ValueError("YAMNet SavedModel output contract is not [windows, 521]")
    loaded = (model, classes, tf.__version__)
    _MODEL_CACHE.clear()
    _MODEL_CACHE[key] = loaded
    return loaded


def _decoder_version() -> str:
    global _DECODER_VERSION
    if _DECODER_VERSION is None:
        completed = subprocess.run(
            [FFMPEG, "-version"],
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=10,
        )
        if completed.returncode != 0:
            raise ValueError("ffmpeg is unavailable for sound-event audio decoding")
        first_line = completed.stdout.decode("utf-8", errors="replace").splitlines()[0]
        parts = first_line.split()
        _DECODER_VERSION = " ".join(parts[:3]) if len(parts) >= 3 else first_line[:80]
    return _DECODER_VERSION


def _decode_audio(path: str) -> Any:
    import numpy as np

    # Read only a small amount beyond the contract maximum so compressed files
    # cannot expand without bound before duration rejection.
    completed = subprocess.run(
        [
            FFMPEG,
            "-nostdin",
            "-v",
            "error",
            "-i",
            path,
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            str(SAMPLE_RATE_HZ),
            "-t",
            f"{MAX_AUDIO_SECONDS + 0.05:.2f}",
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "pipe:1",
        ],
        check=False,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=MAX_AUDIO_SECONDS + 30,
    )
    if completed.returncode != 0:
        raise ValueError("sound-event audio decoding failed")
    waveform = np.frombuffer(completed.stdout, dtype="<f4").copy()
    if waveform.size == 0:
        raise ValueError("sound-event audio contains no decodable samples")
    duration = waveform.size / SAMPLE_RATE_HZ
    if duration > MAX_AUDIO_SECONDS + (1 / SAMPLE_RATE_HZ):
        raise ValueError(f"sound-event audio exceeds the {MAX_AUDIO_SECONDS} second limit")
    if not np.isfinite(waveform).all():
        raise ValueError("sound-event audio contains non-finite samples")
    return np.clip(waveform, -1.0, 1.0)


def _median_smooth(scores: Any) -> Any:
    import numpy as np

    if scores.shape[0] < SMOOTHING_WINDOW_FRAMES:
        return scores
    padded = np.pad(scores, ((1, 1), (0, 0)), mode="edge")
    return np.median(
        np.stack((padded[:-2], padded[1:-1], padded[2:]), axis=0), axis=0
    )


def _events(scores: Any, classes: list[tuple[str, str]], duration: float) -> list[dict[str, Any]]:
    import numpy as np

    active = scores >= EVENT_SCORE_THRESHOLD
    for frame_index, row in enumerate(scores):
        indices = np.flatnonzero(active[frame_index])
        if indices.size > MAX_CLASSES_PER_WINDOW:
            keep = np.argsort(-row, kind="stable")[:MAX_CLASSES_PER_WINDOW]
            active[frame_index] = False
            active[frame_index, keep] = row[keep] >= EVENT_SCORE_THRESHOLD

    detected: list[dict[str, Any]] = []
    for class_index, (class_id, label) in enumerate(classes):
        frames = np.flatnonzero(active[:, class_index])
        if frames.size == 0:
            continue
        run_start = int(frames[0])
        run_end = run_start
        for frame in map(int, frames[1:]):
            if frame == run_end + 1:
                run_end = frame
                continue
            detected.append(
                _event(class_id, label, class_index, run_start, run_end, scores, duration)
            )
            run_start = run_end = frame
        detected.append(
            _event(class_id, label, class_index, run_start, run_end, scores, duration)
        )
    if len(detected) > MAX_EVENTS:
        raise ValueError("sound-event result exceeds the versioned event-count bound")
    detected.sort(key=lambda event: (event["start_seconds"], -event["score"], event["class_id"]))
    return detected


def _event(
    class_id: str,
    label: str,
    class_index: int,
    first_frame: int,
    last_frame: int,
    scores: Any,
    duration: float,
) -> dict[str, Any]:
    start = first_frame * HOP_SECONDS
    end = min(duration, last_frame * HOP_SECONDS + WINDOW_SECONDS)
    return {
        "class_id": class_id,
        "label": label,
        "start_seconds": round(start, 6),
        "end_seconds": round(end, 6),
        "score": round(float(scores[first_frame : last_frame + 1, class_index].max()), 6),
    }


def _speech_presence(scores: Any) -> dict[str, Any]:
    maximum = float(scores[:, SPEECH_CLASS_INDICES].max())
    if maximum >= SPEECH_PRESENT_THRESHOLD:
        status = "present"
    elif maximum <= SPEECH_ABSENT_THRESHOLD:
        # This is model evidence over complete analyzed audio, never an
        # inference from missing transcript text.
        status = "absent"
    else:
        status = "unknown"
    return {"status": status, "max_score": round(maximum, 6)}


def _detect_events(request: dict[str, Any]) -> dict[str, Any]:
    model, classes, runtime_version = _load_model(str(request["model"]))
    waveform = _decode_audio(str(request["audio_path"]))
    duration = waveform.size / SAMPLE_RATE_HZ
    scores, _, _ = model(waveform)
    scores = scores.numpy()
    if scores.ndim != 2 or scores.shape[0] == 0 or scores.shape[1] != CLASS_COUNT:
        raise ValueError("YAMNet returned an invalid score matrix")
    smoothed = _median_smooth(scores)
    return {
        "object": "audio.event_detection",
        "events": _events(smoothed, classes, duration),
        "speech_presence": _speech_presence(smoothed),
        "coverage": {
            "status": "full",
            "input_duration_seconds": round(duration, 6),
            "analyzed_start_seconds": 0.0,
            "analyzed_end_seconds": round(duration, 6),
            "analyzed_seconds": round(duration, 6),
            "ratio": 1.0,
            "window_count": int(scores.shape[0]),
            "window_seconds": WINDOW_SECONDS,
            "hop_seconds": HOP_SECONDS,
        },
        "ontology": {
            "id": "audioset",
            "revision": ONTOLOGY_REVISION,
            "class_id_namespace": CLASS_ID_NAMESPACE,
            "class_count": CLASS_COUNT,
            "artifact_sha256": MODEL_FILES["assets/yamnet_class_map.csv"],
            "license_spdx": ONTOLOGY_LICENSE_SPDX,
        },
        "policy": {
            "revision": POLICY_REVISION,
            "score_kind": "raw_sigmoid",
            "event_score_threshold": EVENT_SCORE_THRESHOLD,
            "smoothing": {
                "method": "centered_median_edge_padded",
                "window_frames": SMOOTHING_WINDOW_FRAMES,
            },
            "max_classes_per_window": MAX_CLASSES_PER_WINDOW,
            "max_events": MAX_EVENTS,
            "speech_class_set_revision": SPEECH_CLASS_SET_REVISION,
            "speech_present_threshold": SPEECH_PRESENT_THRESHOLD,
            "speech_absent_threshold": SPEECH_ABSENT_THRESHOLD,
            "max_audio_seconds": MAX_AUDIO_SECONDS,
        },
        "provenance": {
            "model": MODEL_ID,
            "model_archive_sha256": MODEL_ARCHIVE_SHA256,
            "artifact_set_sha256": MODEL_ARTIFACT_SET_SHA256,
            "model_license_spdx": MODEL_LICENSE_SPDX,
            "training_data_license_spdx": TRAINING_DATA_LICENSE_SPDX,
            "runtime": "tensorflow-saved-model",
            "runtime_version": runtime_version,
            "decoder": "ffmpeg",
            "decoder_version": _decoder_version(),
            "preprocessing_identity": PREPROCESSING_IDENTITY,
        },
    }


def handle(request: dict[str, Any]) -> dict[str, Any]:
    if request.get("operation") != "detect_events":
        raise ValueError("unsupported operation for YAMNet worker")
    return _detect_events(request)


def main() -> int:
    if len(sys.argv) == 3 and sys.argv[1] == "--verify-model":
        _, classes, runtime_version = _load_model(sys.argv[2])
        print(
            json.dumps(
                {
                    "model": MODEL_ID,
                    "model_archive_sha256": MODEL_ARCHIVE_SHA256,
                    "classes": len(classes),
                    "runtime_version": runtime_version,
                    "policy_revision": POLICY_REVISION,
                },
                sort_keys=True,
            )
        )
        return 0
    if len(sys.argv) != 1:
        raise SystemExit("usage: yamnet_audio_worker.py [--verify-model DIRECTORY]")

    while True:
        encoded = sys.stdin.buffer.readline(MAX_REQUEST_LINE_BYTES + 1)
        if not encoded:
            break
        if len(encoded) > MAX_REQUEST_LINE_BYTES or not encoded.endswith(b"\n"):
            print("yamnet worker request frame exceeded limit", file=sys.stderr, flush=True)
            return 2
        line = encoded.decode("utf-8", errors="strict")
        if not line.strip():
            continue
        request_id = "unknown"
        try:
            request = json.loads(line)
            request_id = str(request.get("request_id", request_id))
            response = {"request_id": request_id, "ok": True, "result": handle(request)}
        except Exception:
            # Serving diagnostics must not echo source paths, decoded audio,
            # transcripts, or request payloads to either protocol output or logs.
            print("yamnet worker request failed", file=sys.stderr, flush=True)
            response = {
                "request_id": request_id,
                "ok": False,
                "error": "sound-event worker request failed",
            }
        sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
