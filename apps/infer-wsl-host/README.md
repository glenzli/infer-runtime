# Windows WSL host

This installed, console-free .NET Framework 4.8 executable has two owners:

- `Program.cs` composes the per-user singleton, bounded host log, WSL activation
  lifetime and reconnect loop. Windows Task Scheduler owns login and host restart.
- `StatusBridge.cs` owns the read-only `infer-runtime.status` byte pipe, client
  SID admission, framing/deadlines, observer-only HTTP request, Windows generation
  mapping, and reconciliation of canonical/legacy Windows registrations.

`PrivateFiles.cs` owns private local Windows installation settings and atomic
owner-only manifest publication. Linux process supervision belongs to the
systemd units installed by `tools/install_wsl_service.sh`; provider execution,
accounting, and the status snapshot remain in their existing Rust owners.

Build and installation graph: `tools/install_windows_wsl.ps1`. Focused tests run
the production sources through `tools/test_windows_wsl_host.ps1`. The independent
Windows installation does not depend on a source checkout, compiler, or Codex.
See [operator instructions](../../docs/WINDOWS_INSTALL.md).
