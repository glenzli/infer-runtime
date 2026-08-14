#!/usr/bin/env python3
"""Generate immutable, capability-scoped OpenAPI artifacts.

The Consumer Core OpenAPI document remains the aggregate discovery document.
Each Capability Catalog entry instead fingerprints a document containing only
that capability's routes and the transitive local component references those
routes require. This keeps unrelated capability additions from changing an
existing capability identity.
"""

from __future__ import annotations

import json
import hashlib
import os
import sys
import tempfile
from copy import deepcopy
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "contracts/schema-source/consumer-api-20260813.1.json"
OUTPUT = ROOT / "contracts/capabilities"
IMMUTABLE_DIGESTS = ROOT / "contracts/immutable-contract-digests.json"
CHECK_ONLY = "--check" in sys.argv[1:]

CORE_ROUTES = (
    "/health",
    "/infer/v1/contract",
    "/infer/v1/capabilities",
    "/infer/v1/openapi.json",
    "/infer/v1/capability-schemas/{capability_id}/{version}/openapi.json",
    "/infer/v1/jobs",
    "/infer/v1/jobs/{response_id}",
    "/infer/v1/jobs/{response_id}/cancel",
    "/infer/v1/explain/{response_id}",
)

CAPABILITIES: dict[str, tuple[str, tuple[str, ...]]] = {
    "infer.responses": (
        "20260812.1",
        (
            "/v1/responses",
            "/v1/responses/{response_id}",
            "/v1/responses/{response_id}/cancel",
        ),
    ),
    "infer.audio.transcription": ("20260814.1", ("/v1/audio/transcriptions",)),
    "infer.audio.event-detection": ("20260813.2", ("/v1/audio/event-detections",)),
    "infer.audio.embedding": (
        "20260815.1",
        ("/v1/audio/embeddings", "/v1/audio/text-embeddings"),
    ),
    "infer.audio.alignment": ("20260811.1", ("/v1/audio/alignments",)),
    "infer.audio.speech": ("20260811.1", ("/v1/audio/speech",)),
    "infer.audio.voice-clone": ("20260811.1", ("/v1/audio/voice-clones",)),
    "infer.audio.transcription-stream": (
        "20260811.1",
        ("/v1/audio/transcriptions/stream",),
    ),
    "infer.vision.face-detection": (
        "20260811.1",
        ("/infer/v1/vision/face-detections",),
    ),
    "infer.vision.face-embedding": (
        "20260811.1",
        ("/infer/v1/vision/face-embeddings",),
    ),
    "infer.vision.subject-segmentation": (
        "20260813.1",
        ("/infer/v1/vision/subject-segmentations",),
    ),
    "infer.vision.subject-segmentation-soft-mask": (
        "20260814.1",
        ("/infer/v1/vision/subject-segmentations/soft-mask",),
    ),
    "infer.vision.face-parsing": (
        "20260813.1",
        ("/infer/v1/vision/face-parsings",),
    ),
    "infer.vision.image-embedding": (
        "20260811.1",
        ("/infer/v1/vision/image-embeddings",),
    ),
    "infer.vision.text-embedding": (
        "20260811.1",
        ("/infer/v1/vision/text-embeddings",),
    ),
    "infer.vision.image-description": (
        "20260811.1",
        ("/infer/v1/vision/image-descriptions",),
    ),
    "infer.vision.classification-review": (
        "20260811.1",
        ("/infer/v1/vision/classification-reviews",),
    ),
    "infer.text.embedding": (
        "20260812.1",
        (
            "/infer/v1/text/query-embeddings",
            "/infer/v1/text/document-embeddings",
        ),
    ),
    "infer.text.rerank": ("20260812.1", ("/infer/v1/text/rerank",)),
    "infer.document.ocr": ("20260812.1", ("/infer/v1/documents/ocr",)),
    "infer.raw-foundation": (
        "20260811.1",
        (
            "/infer/v1/raw/foundations/leases",
            "/infer/v1/raw/foundations",
            "/infer/v1/raw/foundations/{job_id}/cancel",
        ),
    ),
}


def local_references(value: Any) -> set[tuple[str, str]]:
    references: set[tuple[str, str]] = set()
    if isinstance(value, dict):
        reference = value.get("$ref")
        if isinstance(reference, str) and reference.startswith("#/components/"):
            parts = reference.split("/")
            if len(parts) == 4:
                references.add((parts[2], parts[3]))
        for child in value.values():
            references.update(local_references(child))
    elif isinstance(value, list):
        for child in value:
            references.update(local_references(child))
    return references


def referenced_components(source: dict[str, Any], paths: dict[str, Any]) -> dict[str, Any]:
    selected: dict[str, dict[str, Any]] = {}
    pending = sorted(local_references(paths), reverse=True)
    visited: set[tuple[str, str]] = set()
    while pending:
        section, name = pending.pop()
        if (section, name) in visited:
            continue
        visited.add((section, name))
        component = source["components"][section][name]
        selected.setdefault(section, {})[name] = component
        pending.extend(sorted(local_references(component) - visited, reverse=True))
    return {
        section: {name: selected[section][name] for name in sorted(selected[section])}
        for section in sorted(selected)
    }


def capability_paths(source: dict[str, Any], route_names: tuple[str, ...]) -> dict[str, Any]:
    """Project capability-owned operations without duplicating Core errors.

    The shared error envelope and its code inventory belong to Consumer Core.
    Keeping their local OpenAPI references here would cause a Core-only error
    addition to rewrite every capability digest.
    """
    paths = {name: deepcopy(source["paths"][name]) for name in route_names}
    for path_item in paths.values():
        for operation in path_item.values():
            if not isinstance(operation, dict):
                continue
            responses = operation.get("responses")
            if not isinstance(responses, dict):
                continue
            for status, response in list(responses.items()):
                if isinstance(response, dict) and response.get("$ref") == "#/components/responses/Error":
                    del responses[status]
    return paths


def encoded_json(document: dict[str, Any]) -> bytes:
    return (json.dumps(document, indent=2, ensure_ascii=False) + "\n").encode()


def write_json_atomic(destination: Path, document: dict[str, Any]) -> None:
    """Publish generated bytes atomically so a failed run cannot truncate a contract."""
    encoded = encoded_json(document)
    relative = destination.relative_to(ROOT).as_posix()
    locked = json.loads(IMMUTABLE_DIGESTS.read_text(encoding="utf-8"))[
        "sha256"
    ].get(relative)
    actual = hashlib.sha256(encoded).hexdigest()
    if locked is not None and locked != actual:
        raise SystemExit(
            f"refusing to rewrite immutable contract {relative}: "
            f"locked {locked}, generated {actual}; create a new dated version"
        )
    if CHECK_ONLY:
        if not destination.is_file() or destination.read_bytes() != encoded:
            raise SystemExit(f"generated contract is stale: {destination.relative_to(ROOT)}")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{destination.name}.", dir=destination.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(encoded.decode("utf-8"))
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> None:
    source = json.loads(SOURCE.read_text(encoding="utf-8"))
    core_paths = {name: deepcopy(source["paths"][name]) for name in CORE_ROUTES}
    core_document = {
        "openapi": "3.1.0",
        "info": {
            "title": "Infer Runtime Consumer Core",
            "version": "20260813.1",
            "description": (
                "Immutable shared discovery, admission, Job, routing, cancellation, "
                "provenance, and error-envelope contract. Typed data planes are "
                "versioned by independent capability documents."
            ),
        },
        "x-infer-consumer-core-contract": "infer-runtime.consumer-core@20260813.1",
        "paths": core_paths,
        "components": referenced_components(source, core_paths),
    }
    core_destination = ROOT / "contracts/consumer-core/20260813.1/openapi.json"
    write_json_atomic(core_destination, core_document)
    for capability_id, (version, route_names) in CAPABILITIES.items():
        paths = capability_paths(source, route_names)
        document = {
            "openapi": "3.1.0",
            "info": {
                "title": f"Infer Runtime capability {capability_id}",
                "version": version,
                "description": (
                    "Immutable capability-scoped wire schema. Authentication and "
                    "Consumer Core negotiation are inherited from the Core contract."
                ),
            },
            "x-infer-capability-contract": f"{capability_id}@{version}",
            "paths": paths,
            "components": referenced_components(source, paths),
        }
        destination = OUTPUT / capability_id / version / "openapi.json"
        write_json_atomic(destination, document)


if __name__ == "__main__":
    main()
