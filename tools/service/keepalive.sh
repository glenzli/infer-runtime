#!/usr/bin/env bash
# WSL systemd services alone do not retain the Windows WSL instance.
# The Windows host retains this invocation and reconnects after distro shutdown.
set -euo pipefail
systemctl --user start infer-runtime-ollama.service infer-runtime.service infer-runtime-console.service
# The host retains the stdin writer. A host crash/stop closes it, so this WSL
# invocation exits too instead of leaving an orphaned infinite sleep process.
exec /usr/bin/cat >/dev/null
