#!/usr/bin/env bash
set -euo pipefail
release="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$release/runtime-env.sh"
case "${1:-}" in
    daemon) exec "$release/inferd" --config "$INFER_RUNTIME_ROOT/config/infer.toml" ;;
    console) exec "$release/infer" --config "$INFER_RUNTIME_ROOT/config/infer.toml" console --no-open ;;
    ollama) exec "$INFER_RUNTIME_ROOT/.infer-runtime/runtimes/ollama-v0.40.11/bin/ollama" serve ;;
    *) echo 'Expected daemon, console, or ollama.' >&2; exit 2 ;;
esac
