#!/usr/bin/env bash
# Own the local Console/Runtime and Ollama process lifetimes for this session.
set -euo pipefail
umask 077
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/infer-runtime}"
runtime_root="$(realpath -m -- "$runtime_root")"
case "$runtime_root/" in "$repo_root/"*) echo 'Runtime files must stay outside the source checkout.' >&2; exit 1;; esac
export CARGO_TARGET_DIR="$HOME/.cache/infer-runtime/target"
export INFRA_PROTOCOL_RUNTIME_DIR="$runtime_root/discovery"
export NO_PROXY="127.0.0.1,localhost,::1${NO_PROXY:+,$NO_PROXY}"
export no_proxy="$NO_PROXY"
# 11434 is reserved by Hyper-V on this Windows host; use an explicit free port.
export OLLAMA_HOST=127.0.0.1:12434
export OLLAMA_MODELS="$runtime_root/.infer-runtime/ollama-models"
export OLLAMA_CONTEXT_LENGTH=8192
export OLLAMA_MAX_LOADED_MODELS=1
export OLLAMA_NUM_PARALLEL=1
export OLLAMA_NO_CLOUD=1
runtime_libraries="$runtime_root/.infer-runtime/runtimes/onnx-cuda/lib"
export LD_LIBRARY_PATH="/usr/lib/wsl/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
for library in "$runtime_libraries"/python*/site-packages/nvidia/*/lib; do
    [[ -d "$library" ]] && export LD_LIBRARY_PATH="$library:$LD_LIBRARY_PATH"
done
source "$HOME/.cargo/env"
[[ -f "$runtime_root/config/infer.toml" ]] || { echo 'Install the WSL configuration first.' >&2; exit 1; }
cargo +1.97.1 build --locked --manifest-path "$repo_root/Cargo.toml" -p infer -p inferd
mkdir -p "$runtime_root/.infer-runtime/logs" "$OLLAMA_MODELS"
ollama_pid=''
console_pid=''
cleanup() {
    [[ -z "$console_pid" ]] || kill -INT "$console_pid" 2>/dev/null || true
    [[ -z "$console_pid" ]] || wait "$console_pid" 2>/dev/null || true
    [[ -z "$ollama_pid" ]] || kill -TERM "$ollama_pid" 2>/dev/null || true
    [[ -z "$ollama_pid" ]] || wait "$ollama_pid" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM
if ! curl --silent --fail --max-time 2 --noproxy '*' "http://$OLLAMA_HOST/api/version" >/dev/null; then
    "$runtime_root/.infer-runtime/runtimes/ollama-v0.40.11/bin/ollama" serve \
        >"$runtime_root/.infer-runtime/logs/ollama.log" 2>&1 &
    ollama_pid=$!
    ready=false
    for attempt in {1..40}; do
        if curl --silent --fail --max-time 2 --noproxy '*' "http://$OLLAMA_HOST/api/version" >/dev/null; then ready=true; break; fi
        kill -0 "$ollama_pid" || exit 1
        sleep 0.25
    done
    $ready || { echo 'Ollama did not become ready.' >&2; exit 1; }
fi
cd -- "$runtime_root"
"$CARGO_TARGET_DIR/debug/infer" --config "$runtime_root/config/infer.toml" \
    console --spawn --daemon-bin "$CARGO_TARGET_DIR/debug/inferd" --no-open &
console_pid=$!
wait "$console_pid"
