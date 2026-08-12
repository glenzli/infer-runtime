#!/usr/bin/env python3
"""Payload-free real smoke for the external retrieval and OCR workers."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

from PIL import Image, ImageDraw, ImageFont


class WorkerClient:
    def __init__(self, worker: Path) -> None:
        self.process = subprocess.Popen(
            [sys.executable, str(worker)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def round_trip(self, request: dict[str, Any]) -> dict[str, Any]:
        if self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("worker protocol streams are unavailable")
        self.process.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
        self.process.stdin.flush()
        response_line = self.process.stdout.readline()
        if not response_line:
            raise RuntimeError("worker exited before returning a JSON frame")
        response = json.loads(response_line)
        if response.get("request_id") != request["request_id"]:
            raise RuntimeError("worker response identity mismatch")
        if not response.get("ok"):
            raise RuntimeError(f"worker failed: {response.get('error', 'unknown')}")
        return response["result"]

    def close(self) -> None:
        if self.process.stdin is not None:
            self.process.stdin.close()
        try:
            return_code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return_code = self.process.wait(timeout=5)
        if return_code != 0:
            raise RuntimeError(f"worker exited with status {return_code}")

    def __enter__(self) -> "WorkerClient":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--embedding", type=Path, required=True)
    parser.add_argument("--reranker", type=Path, required=True)
    parser.add_argument("--ocr-detection", type=Path, required=True)
    parser.add_argument("--ocr-recognition", type=Path, required=True)
    parser.add_argument("--font", type=Path, required=True)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    retrieval_worker = repo / "workers" / "text_retrieval_worker.py"
    ocr_worker = repo / "workers" / "ocr_worker.py"

    with WorkerClient(retrieval_worker) as retrieval:
        embedded = retrieval.round_trip(
            {
                "request_id": "embed-query-smoke",
                "operation": "embed_query",
                "model": str(args.embedding),
                "texts": ["如何在本地识别图片中的文字"],
            }
        )
        documents = retrieval.round_trip(
            {
                "request_id": "embed-documents-smoke",
                "operation": "embed_documents",
                "model": str(args.embedding),
                "texts": ["OCR 可以提取图片里的文字", "今天的天气很好"],
            }
        )
        reranked = retrieval.round_trip(
            {
                "request_id": "rerank-smoke",
                "operation": "rerank",
                "model": str(args.reranker),
                "query": "如何在本地识别图片中的文字",
                "candidates": [
                    {"id": "ocr", "text": "OCR 可以提取图片里的文字"},
                    {"id": "weather", "text": "今天的天气很好"},
                ],
            }
        )
    if (
        embedded["dimensions"] != 1024
        or documents["dimensions"] != 1024
        or not embedded["normalized"]
        or not documents["normalized"]
    ):
        raise RuntimeError("embedding contract mismatch")
    if reranked["results"][0]["candidate_id"] != "ocr":
        raise RuntimeError("reranker ordering mismatch")

    with tempfile.TemporaryDirectory(prefix="infer-ocr-smoke-") as directory:
        image_path = Path(directory) / "input.png"
        image = Image.new("RGB", (1200, 420), "white")
        draw = ImageDraw.Draw(image)
        font = ImageFont.truetype(str(args.font), 64)
        expected = ["Infer Runtime 本地文字识别", "Qwen3 检索 + PP-OCRv6"]
        draw.text((70, 70), expected[0], font=font, fill="black")
        draw.text((70, 190), expected[1], font=font, fill="black")
        image.save(image_path)
        with WorkerClient(ocr_worker) as ocr:
            recognized = ocr.round_trip(
                {
                    "request_id": "ocr-smoke-1",
                    "operation": "recognize",
                    "detection_model": str(args.ocr_detection),
                    "recognition_model": str(args.ocr_recognition),
                    "image_path": str(image_path),
                }
            )
            recognized_again = ocr.round_trip(
                {
                    "request_id": "ocr-smoke-2",
                    "operation": "recognize",
                    "detection_model": str(args.ocr_detection),
                    "recognition_model": str(args.ocr_recognition),
                    "image_path": str(image_path),
                }
            )
    texts = [line["text"] for line in recognized["lines"]]
    if texts != expected:
        raise RuntimeError("OCR content mismatch")
    if recognized_again != recognized:
        raise RuntimeError("warm OCR output changed for an identical artifact")

    print(
        json.dumps(
            {
                "embedding_dimensions": embedded["dimensions"],
                "rerank_top_candidate": reranked["results"][0]["candidate_id"],
                "ocr_lines": len(recognized["lines"]),
                "ocr_min_confidence": min(
                    line["confidence"] for line in recognized["lines"]
                ),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
