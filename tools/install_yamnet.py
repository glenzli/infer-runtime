#!/usr/bin/env python3
"""Verify and atomically install the exact YAMNet TF Hub v1 archive."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import shutil
import tarfile
import tempfile


ARCHIVE_SHA256 = "b80da2a1a56926fb0767205051a200dd7b3beaf3ea1ea126c42a53943996e5e0"
ARCHIVE_SIZE_BYTES = 14_242_921
DIRECTORIES = {"assets", "variables"}
FILES = {
    "assets/yamnet_class_map.csv": (
        14_096,
        "cdf24d193e196d9e95912a2667051ae203e92a2ba09449218ccb40ef787c6df2",
    ),
    "saved_model.pb": (
        3_176_321,
        "672af6e1e34fe15a42d45d70217fd39f97e10aef9b0effbf9b0bf7826fccd462",
    ),
    "variables/variables.data-00000-of-00001": (
        15_077_564,
        "d6753f22f173b2a8b1ce78918eaae79bf0a41ca61f4cfe9a1b948c97ff094ddc",
    ),
    "variables/variables.index": (
        7_400,
        "0bc2ca10e56e8a71b96a2cad26adbbabff927e9344fbd2df8ae7275ddf76ae1e",
    ),
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_file(path: Path, size: int, digest: str) -> None:
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"missing regular artifact file: {path.name}")
    if path.stat().st_size != size or sha256(path) != digest:
        raise ValueError(f"artifact identity mismatch: {path.name}")


def verify_install(root: Path) -> None:
    entries = list(root.rglob("*"))
    if any(path.is_symlink() for path in entries):
        raise ValueError("installed model contains a symbolic link")
    actual_files = {
        str(path.relative_to(root))
        for path in entries
        if path.is_file()
    }
    actual_directories = {
        str(path.relative_to(root)) for path in entries if path.is_dir()
    }
    if actual_files != set(FILES) or actual_directories != DIRECTORIES:
        raise ValueError("installed model has missing or unexpected files")
    for relative, (size, digest) in FILES.items():
        verify_file(root / relative, size, digest)


def install(archive: Path, output: Path) -> None:
    verify_file(archive, ARCHIVE_SIZE_BYTES, ARCHIVE_SHA256)
    output = output.expanduser().absolute()
    if output.is_symlink():
        raise ValueError("output directory cannot be a symbolic link")
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        verify_install(output)
        print(f"verified existing YAMNet install: {output}")
        return

    staging_parent = Path(
        tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent)
    )
    staging = staging_parent / "model"
    staging.mkdir()
    try:
        with tarfile.open(archive, "r:*") as bundle:
            members = bundle.getmembers()
            names = {member.name.rstrip("/") for member in members}
            if names != DIRECTORIES | set(FILES):
                raise ValueError("archive has missing or unexpected members")
            for member in members:
                name = member.name.rstrip("/")
                pure = PurePosixPath(name)
                if pure.is_absolute() or ".." in pure.parts:
                    raise ValueError("archive contains an unsafe path")
                destination = staging.joinpath(*pure.parts)
                if name in DIRECTORIES:
                    if not member.isdir():
                        raise ValueError("archive directory member has the wrong type")
                    destination.mkdir(parents=True, exist_ok=True)
                    continue
                if not member.isfile():
                    raise ValueError("archive contains a non-regular artifact")
                source = bundle.extractfile(member)
                if source is None:
                    raise ValueError("archive member cannot be read")
                destination.parent.mkdir(parents=True, exist_ok=True)
                with source, destination.open("xb") as target:
                    shutil.copyfileobj(source, target)
        verify_install(staging)
        os.replace(staging, output)
    finally:
        shutil.rmtree(staging_parent, ignore_errors=True)
    print(f"installed verified YAMNet TF Hub v1 model: {output}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--output-directory", required=True, type=Path)
    args = parser.parse_args()
    install(args.archive.resolve(), args.output_directory)


if __name__ == "__main__":
    main()

