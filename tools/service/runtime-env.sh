#!/usr/bin/env bash
# Shared environment of the installed CUDA services, independent of the checkout.
set -euo pipefail
umask 077
export INFRA_PROTOCOL_RUNTIME_DIR="$INFER_RUNTIME_ROOT/discovery"
export NO_PROXY='127.0.0.1,localhost,::1'
export no_proxy="$NO_PROXY"
export OLLAMA_HOST=127.0.0.1:12434
export OLLAMA_MODELS="$INFER_RUNTIME_ROOT/.infer-runtime/ollama-models"
export OLLAMA_CONTEXT_LENGTH=8192
export OLLAMA_MAX_LOADED_MODELS=1
export OLLAMA_NUM_PARALLEL=1
export OLLAMA_KEEP_ALIVE=30s
export OLLAMA_NO_CLOUD=1
export LD_LIBRARY_PATH=/usr/lib/wsl/lib
for library in "$INFER_RUNTIME_ROOT"/.infer-runtime/runtimes/onnx-cuda/lib/python*/site-packages/nvidia/*/lib; do
    [[ ! -d "$library" ]] || export LD_LIBRARY_PATH="$library:$LD_LIBRARY_PATH"
done
cd -- "$INFER_RUNTIME_ROOT"
