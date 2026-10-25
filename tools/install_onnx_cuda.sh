#!/usr/bin/env bash
# User-local ORT installation. Does not install a Linux GPU driver.
set -euo pipefail
umask 077
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/infer-runtime}"
runtime_root="$(realpath -m -- "$runtime_root")"
case "$runtime_root/" in "$repo_root/"*) echo 'Runtime files must stay outside the source checkout.' >&2; exit 1;; esac
environment="$runtime_root/.infer-runtime/runtimes/onnx-cuda"
archive="$runtime_root/.infer-runtime/staging/onnxruntime-linux-x64-gpu_cuda12-1.27.0.tgz"
destination="$runtime_root/.infer-runtime/runtimes/onnxruntime-1.27.0-cuda12"
mkdir -p "$(dirname -- "$archive")" "$destination"
python3 -m venv "$environment"
"$environment/bin/pip" install -r "$repo_root/tools/requirements-shadow-cuda.txt"
if [[ ! -f "$archive" ]]; then
    curl -fL --retry 3 --continue-at - --output "$archive.partial" \
        https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-linux-x64-gpu_cuda12-1.27.0.tgz
    printf '%s  %s\n' 3fed2d2f45f01f8bc1c1597a31afe29efd692c7ea4648d58e1844a8a0d0a48cb "$archive.partial" | sha256sum --check
    mv -- "$archive.partial" "$archive"
fi
printf '%s  %s\n' 3fed2d2f45f01f8bc1c1597a31afe29efd692c7ea4648d58e1844a8a0d0a48cb "$archive" | sha256sum --check
tar -xzf "$archive" --strip-components=1 -C "$destination"
printf 'ORT CUDA 12 installed: %s\n' "$destination/lib/libonnxruntime.so.1.27.0"
