#!/usr/bin/env python3
"""Publish private NTFS objects through WSL, preserving their Windows ACLs.

MSIX redirects newly created AppData files. Stage outside AppData with native
Windows ACLs, then rename the bounded bridge tree into the actual user folder.
No credential bytes are read, copied, or placed on the command line here.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import stat

LAYOUT = {
    "infra-protocol": True,
    "infra-protocol/registrations": True,
    "infra-protocol/registrations/infer-runtime--local.json": False,
    "Shadow": True,
    "Shadow/credentials": True,
    "Shadow/credentials/infer-runtime-shadow.token": False,
}


def validate_layout(root: Path) -> None:
    pending = [root]
    while pending:
        for entry in pending.pop().iterdir():
            key = entry.relative_to(root).as_posix()
            if entry.is_symlink() or key not in LAYOUT:
                raise ValueError("unexpected bridge entry")
            mode = entry.stat().st_mode
            if LAYOUT[key]:
                if not stat.S_ISDIR(mode):
                    raise ValueError("expected a bridge directory")
                pending.append(entry)
            elif not stat.S_ISREG(mode):
                raise ValueError("expected a regular bridge file")


def reject_links(path: Path) -> None:
    for item in (*reversed(path.parents), path):
        if item.is_symlink():
            raise ValueError(f"bridge publication cannot traverse a link: {item}")


def publish(source: Path, destination: Path, depth: int = 0) -> None:
    reject_links(source)
    reject_links(destination)
    if depth > 2:
        raise ValueError("bridge tree exceeds its bounded layout")
    source_mode = source.stat().st_mode
    if stat.S_ISDIR(source_mode):
        if not destination.exists():
            os.replace(source, destination)
            return
        if not destination.is_dir():
            raise ValueError("bridge destination is not a directory")
        for child in source.iterdir():
            if child.name not in {"registrations", "credentials", "infer-runtime--local.json", "infer-runtime-shadow.token"}:
                raise ValueError("unexpected bridge entry")
            publish(child, destination / child.name, depth + 1)
        source.rmdir()
    elif stat.S_ISREG(source_mode):
        if destination.exists() and not destination.is_file():
            raise ValueError("bridge destination is not a regular file")
        os.replace(source, destination)
    else:
        raise ValueError("bridge source is not a regular object")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--destination", required=True, type=Path)
    args = parser.parse_args()
    source, destination = args.source.absolute(), args.destination.absolute()
    reject_links(source)
    reject_links(destination)
    if source == destination or source in destination.parents or destination in source.parents:
        raise ValueError("bridge staging and destination must be separate")
    if not destination.is_dir() or source.stat().st_dev != destination.stat().st_dev:
        raise ValueError("bridge publication requires an existing destination on the same NTFS volume")
    validate_layout(source)
    entries = list(source.iterdir())
    if not entries or any(entry.name not in {"infra-protocol", "Shadow"} for entry in entries):
        raise ValueError("unexpected bridge root")
    for entry in entries:
        publish(entry, destination / entry.name)
    source.rmdir()


if __name__ == "__main__":
    main()
