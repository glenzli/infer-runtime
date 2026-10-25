#!/usr/bin/env bash
# Pinned, user-local Ollama installation. Does not create a system service.
set -euo pipefail
umask 077
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/infer-runtime}"
runtime_root="$(realpath -m -- "$runtime_root")"
case "$runtime_root/" in "$repo_root/"*) echo 'Runtime files must stay outside the source checkout.' >&2; exit 1;; esac
command -v zstd >/dev/null || { echo 'Install zstd before extracting Ollama.' >&2; exit 1; }
archive="$HOME/.cache/infer-runtime/downloads/ollama-v0.40.11-linux-amd64.tar.zst"
destination="$runtime_root/.infer-runtime/runtimes/ollama-v0.40.11"
mkdir -p "$(dirname -- "$archive")" "$destination"
if [[ ! -f "$archive" ]]; then
    curl -fL --retry 3 --continue-at - --output "$archive.partial" \
        https://github.com/ollama/ollama/releases/download/v0.40.11/ollama-linux-amd64.tar.zst
    printf '%s  %s\n' 7bedab90dd122661be78798e9f0edf37fccf0cccf2699d2c58ea1a3afdc04b6d "$archive.partial" | sha256sum --check
    mv -- "$archive.partial" "$archive"
fi
printf '%s  %s\n' 7bedab90dd122661be78798e9f0edf37fccf0cccf2699d2c58ea1a3afdc04b6d "$archive" | sha256sum --check
tar --zstd -xf "$archive" -C "$destination"
printf 'Ollama installed: %s\n' "$destination/bin/ollama"
