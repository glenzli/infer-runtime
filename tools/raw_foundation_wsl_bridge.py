#!/usr/bin/env python3
"""Private, one-request WSL data plane for the official Windows RAW client.

Only inherited stdio carries samples/artifacts. The authenticated HTTP API and
the existing UID-checked SCM_RIGHTS registration retain their original schemas.
No Windows file path is resolved by this process. Temporary Linux files have
0700/0600 protection and live only until collection, EOF, or timeout.
"""
from __future__ import annotations

import array
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import stat
import struct
import sys
import tempfile
import time


SCHEMA = "infer.raw-foundation.wsl-stdio@20261025.1"
MAX_FRAME = 4096
STRIPE = 64 * 1024
MAX_INPUT = 512 * 1024 * 1024
MAX_OUTPUT = 2 * 1024 * 1024 * 1024


def _timeout(_signal, _frame):
    raise TimeoutError("bridge deadline exceeded")


def read_frame(stream):
    line = stream.readline(MAX_FRAME + 1)
    if not line or len(line) > MAX_FRAME or not line.endswith(b"\n"):
        raise ValueError("invalid bridge frame")
    value = json.loads(line)
    if not isinstance(value, dict):
        raise ValueError("invalid bridge frame")
    return value


def write_frame(stream, value):
    frame = json.dumps(value, separators=(",", ":")).encode() + b"\n"
    if len(frame) > MAX_FRAME:
        raise ValueError("bridge frame too large")
    stream.write(frame)
    stream.flush()


def _register(endpoint, ticket, input_fd, output_fd):
    endpoint = Path(endpoint)
    if not endpoint.is_absolute():
        raise ValueError("invalid lease endpoint")
    for path, kind, mode in ((endpoint.parent, stat.S_ISDIR, 0o700), (endpoint, stat.S_ISSOCK, 0o600)):
        info = path.lstat()
        if not kind(info.st_mode) or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != mode:
            raise ValueError("lease endpoint is not owner-only")
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(2)
        peer.connect(str(endpoint))
        _pid, uid, _gid = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != os.geteuid():
            raise ValueError("lease peer identity mismatch")
        frame = json.dumps({"schema": "infer.artifact-lease.register", "schema_version": "20260811.1",
                            "operation": "register", "ticket_id": ticket}, separators=(",", ":")).encode() + b"\n"
        if len(frame) > MAX_FRAME:
            raise ValueError("registration too large")
        sent = peer.sendmsg([frame], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [input_fd, output_fd]))])
        if sent != len(frame):
            raise ValueError("short registration frame")
        peer.shutdown(socket.SHUT_WR)
        with peer.makefile("rb") as stream:
            response = read_frame(stream)
        if set(response) != {"schema", "schema_version", "lease_id", "expires_at_unix_ms"} or response["schema"] != "infer.artifact-lease.registered" or response["schema_version"] != "20260811.1":
            raise ValueError("invalid lease response")
        return response


def serve(source, sink):
    signal.signal(signal.SIGALRM, _timeout)
    signal.setitimer(signal.ITIMER_REAL, 30)
    request = read_frame(source)
    if set(request) != {"schema", "endpoint", "ticket_id", "sample_bytes", "expires_at_unix_ms"} or request["schema"] != SCHEMA:
        raise ValueError("invalid registration")
    size = request["sample_bytes"]
    expiry = request["expires_at_unix_ms"]
    now = int(time.time() * 1000)
    if type(size) is not int or not 0 < size <= MAX_INPUT or type(expiry) is not int or not now < expiry <= now + 60_000:
        raise ValueError("invalid registration bounds")
    if not isinstance(request["ticket_id"], str) or not 1 <= len(request["ticket_id"]) <= 128:
        raise ValueError("invalid ticket")
    signal.setitimer(signal.ITIMER_REAL, (expiry - now) / 1000)
    with tempfile.TemporaryDirectory(prefix="infer-raw-wsl-") as directory:
        root = Path(directory)
        os.chmod(root, 0o700)
        input_path, output_path = root / "input", root / "output"
        fd = os.open(input_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_CLOEXEC, 0o600)
        with os.fdopen(fd, "wb") as target:
            remaining = size
            while remaining:
                block = source.read(min(remaining, STRIPE))
                if not block:
                    raise EOFError("incomplete sample stream")
                target.write(block)
                remaining -= len(block)
            target.flush()
        with open(input_path, "rb") as input_file:
            fd = os.open(output_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_CLOEXEC, 0o600)
            with os.fdopen(fd, "wb", buffering=0) as output_file:
                registered = _register(request["endpoint"], request["ticket_id"], input_file.fileno(), output_file.fileno())
                if registered["expires_at_unix_ms"] != expiry:
                    raise ValueError("lease expiry mismatch")
                write_frame(sink, {"schema": SCHEMA, "lease_id": registered["lease_id"], "expires_at_unix_ms": expiry})
                if read_frame(source) != {"schema": SCHEMA, "operation": "execute"}:
                    raise ValueError("execution not acknowledged")
                # The one-shot lease must be consumed before expiry. Execution
                # has its own bounded deadline and HTTP cancellation lifecycle.
                signal.setitimer(signal.ITIMER_REAL, 3600)
                collect = read_frame(source)
                if set(collect) != {"schema", "operation", "artifact_file_bytes", "artifact_file_sha256"} or collect["schema"] != SCHEMA or collect["operation"] != "collect":
                    raise ValueError("invalid collection")
                expected_size = collect["artifact_file_bytes"]
                expected_digest = collect["artifact_file_sha256"]
                if type(expected_size) is not int or not 0 < expected_size <= MAX_OUTPUT:
                    raise ValueError("invalid output bound")
                if output_path.stat().st_size != expected_size:
                    raise ValueError("artifact length mismatch")
                digest = hashlib.sha256()
                with open(output_path, "rb") as artifact:
                    for block in iter(lambda: artifact.read(STRIPE), b""):
                        digest.update(block)
                    if digest.hexdigest() != expected_digest:
                        raise ValueError("artifact digest mismatch")
                    artifact.seek(0)
                    write_frame(sink, {"schema": SCHEMA, "artifact_file_bytes": expected_size})
                    for block in iter(lambda: artifact.read(STRIPE), b""):
                        sink.write(block)
                    sink.flush()
    signal.setitimer(signal.ITIMER_REAL, 0)


if __name__ == "__main__":
    try:
        serve(sys.stdin.buffer, sys.stdout.buffer)
    except (Exception, KeyboardInterrupt):
        # Neither ticket values nor paths/contents enter normal diagnostics.
        sys.exit(2)
