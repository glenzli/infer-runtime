#!/usr/bin/env python3
"""Read-only validation for frozen Consumer Core and Capability artifacts."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "contracts/schema-source/consumer-api-20260813.1.json"
LOCK = ROOT / "contracts/immutable-contract-digests.json"
FIXTURES = {
    "responses-request.json": "ResponsesRequest",
    "responses-response.json": "ResponseObject",
    "background-queued.json": "BackgroundResponse",
    "error.json": "ErrorEnvelope",
    "job-list.json": "JobListPage",
    "job-snapshot.json": "JobSnapshot",
}


def load(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def local_references(value: Any) -> set[str]:
    found: set[str] = set()
    if isinstance(value, dict):
        reference = value.get("$ref")
        if isinstance(reference, str) and reference.startswith("#/"):
            found.add(reference)
        for child in value.values():
            found.update(local_references(child))
    elif isinstance(value, list):
        for child in value:
            found.update(local_references(child))
    return found


def resolve(document: dict[str, Any], reference: str) -> Any:
    current: Any = document
    for token in reference[2:].split("/"):
        token = token.replace("~1", "/").replace("~0", "~")
        if not isinstance(current, dict) or token not in current:
            raise ValueError(f"unresolved local reference {reference}")
        current = current[token]
    return current


def validate_openapi(document: dict[str, Any], path: Path) -> None:
    if document.get("openapi") != "3.1.0":
        raise ValueError(f"{path}: expected OpenAPI 3.1.0")
    for reference in sorted(local_references(document)):
        resolve(document, reference)
    operation_ids: set[str] = set()
    for route, path_item in document.get("paths", {}).items():
        if not route.startswith("/") or not isinstance(path_item, dict):
            raise ValueError(f"{path}: invalid route {route!r}")
        for operation in path_item.values():
            if not isinstance(operation, dict) or "operationId" not in operation:
                continue
            operation_id = operation["operationId"]
            if operation_id in operation_ids:
                raise ValueError(f"{path}: duplicate operationId {operation_id}")
            operation_ids.add(operation_id)
    for name, schema in document.get("components", {}).get("schemas", {}).items():
        try:
            Draft202012Validator.check_schema(schema)
        except Exception as error:
            raise ValueError(f"{path}: invalid component schema {name}: {error}") from error


def validate_fixtures(source: dict[str, Any]) -> None:
    fixtures = ROOT / "contracts/consumer-core/20260813.1/fixtures"
    for filename, schema in FIXTURES.items():
        document = {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "components": source["components"],
            "$ref": f"#/components/schemas/{schema}",
        }
        Draft202012Validator(document).validate(load(fixtures / filename))

    response = load(fixtures / "responses-response.json")
    response["future_additive_field"] = {"enabled": True}
    response_schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "components": source["components"],
        "$ref": "#/components/schemas/ResponseObject",
    }
    Draft202012Validator(response_schema).validate(response)

    request = load(fixtures / "responses-request.json")
    request["modle"] = "typo"
    request_schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "components": source["components"],
        "$ref": "#/components/schemas/ResponsesRequest",
    }
    if not list(Draft202012Validator(request_schema).iter_errors(request)):
        raise ValueError("strict request schema accepted an unknown field")


def validate_locked_artifacts() -> None:
    for relative, expected in load(LOCK)["sha256"].items():
        path = ROOT / relative
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != expected:
            raise ValueError(
                f"immutable artifact changed: {relative}: expected {expected}, got {actual}"
            )
        validate_openapi(load(path), path)


def main() -> None:
    source = load(SOURCE)
    validate_openapi(source, SOURCE)
    validate_fixtures(source)
    validate_locked_artifacts()
    print("contract artifacts, references, schemas, fixtures, and digests are valid")


if __name__ == "__main__":
    main()
