#!/usr/bin/env bash
# Install already-built, verified binaries; never compile or download at startup.
set -euo pipefail
umask 077
[[ $# == 3 ]] || { echo 'Usage: install_wsl_service.sh RUNTIME_ROOT BIN_DIRECTORY RELEASE_ID' >&2; exit 2; }
runtime_root="$(realpath -e -- "$1")"
bin_directory="$(realpath -e -- "$2")"
release_id="$3"
[[ "$release_id" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$ ]] || exit 2
[[ "$runtime_root" == "$HOME/"* && "$runtime_root" != *[[:space:]\"%]* ]] || { echo 'Expected a user-owned Runtime root without systemd path escapes.' >&2; exit 2; }
[[ "$(stat -c '%a:%u:%F' -- "$runtime_root")" == "700:$(id -u):directory" ]] || exit 1
[[ -f "$runtime_root/config/infer.toml" && -x "$bin_directory/infer" && -x "$bin_directory/inferd" ]] || exit 1
script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
install_root="$HOME/.local/lib/infer-runtime"
release="$install_root/releases/$release_id"
[[ ! -e "$release" ]] || { echo 'This release is already installed; choose a new release ID.' >&2; exit 1; }
mkdir -p -- "$release" "$HOME/.config/systemd/user" "$runtime_root/discovery"
chmod 700 -- "$install_root" "$install_root/releases" "$release" "$runtime_root/discovery"
for binary in infer inferd; do
    install -m 700 -- "$bin_directory/$binary" "$release/$binary"
    # Keep executable behavior of the qualified build; discard debug symbols only.
    strip --strip-debug "$release/$binary"
done
for script in runtime-env.sh runtime-service.sh keepalive.sh; do
    install -m 700 -- "$script_root/service/$script" "$release/$script"
done
install -m 500 -- "$script_root/raw_foundation_wsl_bridge.py" "$release/raw_foundation_wsl_bridge.py"
install -m 500 -- "$script_root/torch_sam_worker.py" "$release/torch_sam_worker.py"
python3 - "$release" "$bin_directory" "$runtime_root" "$release_id" <<'PY'
import hashlib, json, pathlib, sys
release, source, runtime, identity = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3], sys.argv[4]
files = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in release.iterdir() if p.is_file()}
provenance = {name: hashlib.sha256((source / name).read_bytes()).hexdigest() for name in ('infer', 'inferd')}
(release / 'install-manifest.json').write_text(json.dumps({'release': identity, 'runtime_root': runtime,
    'binary_transform': 'strip-debug-only', 'source_binaries': provenance, 'installed_files': files}, indent=2) + '\n')
PY
ln -s -- "releases/$release_id" "$install_root/.current-$release_id"
mv -Tf -- "$install_root/.current-$release_id" "$install_root/current"
for component in daemon console ollama; do
    case "$component" in daemon) name=infer-runtime;; *) name="infer-runtime-$component";; esac
    dependencies=''
    if [[ "$component" == console ]]; then dependencies='After=infer-runtime.service'; fi
    cat >"$HOME/.config/systemd/user/$name.service" <<EOF
[Unit]
Description=Infer Runtime $component (installed WSL CUDA)
$dependencies
StartLimitIntervalSec=0

[Service]
Type=simple
Environment="INFER_RUNTIME_ROOT=$runtime_root"
WorkingDirectory=$runtime_root
ExecStart=$install_root/current/runtime-service.sh $component
Restart=always
RestartSec=3
TimeoutStopSec=15
KillSignal=SIGINT
KillMode=control-group
UMask=0077
StandardOutput=journal
StandardError=journal
SyslogIdentifier=$name

[Install]
WantedBy=default.target
EOF
done
systemctl --user daemon-reload
systemctl --user enable --now infer-runtime-ollama.service infer-runtime.service infer-runtime-console.service
printf 'Installed release %s at %s\n' "$release_id" "$install_root"
