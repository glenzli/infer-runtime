#!/usr/bin/env python3
"""Persistent offline PP-OCRv6 worker using JSON Lines."""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
from pathlib import Path
from typing import Any

os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
os.environ.setdefault("PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK", "True")

OCR_ENGINE: Any | None = None
OCR_MODEL_IDENTITY: tuple[str, str] | None = None


def _load_engine(detection_model: str, recognition_model: str) -> Any:
    global OCR_ENGINE, OCR_MODEL_IDENTITY
    identity = (detection_model, recognition_model)
    if OCR_ENGINE is not None and OCR_MODEL_IDENTITY == identity:
        return OCR_ENGINE

    from paddleocr import PaddleOCR

    # Paddle initialization reports model paths. Keep generic daemon logs free
    # of local filesystem details and recognized document contents.
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
        io.StringIO()
    ):
        OCR_ENGINE = PaddleOCR(
            text_detection_model_dir=detection_model,
            text_recognition_model_dir=recognition_model,
            use_doc_orientation_classify=False,
            use_doc_unwarping=False,
            use_textline_orientation=False,
            engine="onnxruntime",
            device="cpu",
        )
    OCR_MODEL_IDENTITY = identity
    return OCR_ENGINE


def _recognize(request: dict[str, Any]) -> dict[str, Any]:
    from PIL import Image

    image_path = Path(request["image_path"])
    with Image.open(image_path) as image:
        width, height = image.size

    engine = _load_engine(
        request["detection_model"], request["recognition_model"]
    )
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
        io.StringIO()
    ):
        predictions = list(engine.predict(str(image_path)))

    lines: list[dict[str, Any]] = []
    for prediction in predictions:
        result = prediction.json["res"]
        for polygon, text, score in zip(
            result.get("rec_polys", []),
            result.get("rec_texts", []),
            result.get("rec_scores", []),
            strict=True,
        ):
            lines.append(
                {
                    "polygon": [
                        {"x": int(point[0]), "y": int(point[1])}
                        for point in polygon
                    ],
                    "text": str(text),
                    "confidence": float(score),
                }
            )
    return {
        "image": {
            "width": width,
            "height": height,
            "orientation": "display_pixels_orientation_normalized",
        },
        "lines": lines,
    }


def handle(request: dict[str, Any]) -> dict[str, Any]:
    if request.get("operation") == "recognize":
        return _recognize(request)
    raise ValueError("unsupported_operation")


def main() -> int:
    for line in sys.stdin:
        if not line.strip():
            continue
        request_id = "unknown"
        try:
            request = json.loads(line)
            request_id = str(request.get("request_id", request_id))
            result = handle(request)
            response = {"request_id": request_id, "ok": True, "result": result}
        except Exception as error:
            print(f"OCR worker request failed: {type(error).__name__}", file=sys.stderr)
            response = {
                "request_id": request_id,
                "ok": False,
                "error": "ocr_execution_failed",
            }
        sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
