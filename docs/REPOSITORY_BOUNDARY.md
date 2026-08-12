# Repository boundary

`infer-runtime` separates portable source and contracts from one machine's configuration and
runtime state. A file being useful for development does not make it suitable for Git.

## Files that belong in the repository

- Rust, Python, JavaScript, HTML, and CSS source;
- `Cargo.toml` and `Cargo.lock`;
- versioned public contracts, fixtures, ADRs, design documents, and operating procedures;
- `config/infer.example.toml`, which demonstrates the complete configuration schema without
  claiming that any model or Provider is installed;
- reproducible export scripts, artifact manifests, public source revisions, and checksums that
  identify a Build without containing the model weights;
- screenshots generated from seeded demonstration data after checking that they contain no real
  App, Job, Provider inventory, credential, path, or usage information.

Model and Provider names may appear in an explicitly labelled example or immutable Build manifest.
They must not be presented as the current host inventory, measured capability, or recommended
default merely because they are available on a developer machine.

Every portable Build example declares its supply-chain owner through `provenance.source_kind`.
`provider_managed` means infer-runtime calls an independently managed service/cache and neither
owns nor redistributes its weights; `user_managed` means the operator installed the artifact.
`unreviewed` license status is intentionally honest and must never be rendered as an affirmative
license grant. Runtime-downloaded or bundled Builds require immutable source/digest evidence and a
verified license receipt before configuration validation succeeds.
Legacy local configuration that omits this field remains loadable but projects `unknown`; the
runtime never guesses ownership from a model id.

## Files that stay local

| Local data | Location or pattern | Reason |
| --- | --- | --- |
| Runtime configuration | `config/infer.toml` | Contains host paths, enabled Deployments, real App ACLs, and operator policy |
| Credentials and local state | `.infer-runtime/` | Managed tokens, payload spool, databases, artifacts, runtimes, notes, and captures |
| Environment secrets | `.env`, `.env.*`, `*.token`, private-key extensions | Secret material must use an owner-only local store |
| Runtime databases | `infer-runtime*.sqlite3*` | Contains machine-local operational metadata |
| Model/runtime artifacts | `*.onnx`, `*.safetensors`, `*.gguf`, native libraries | Large or licensed binaries belong in managed stores and provider caches |
| Build and interpreter output | `target/`, `__pycache__/`, `*.pyc` | Reproducible local output |
| Logs and live screenshots | `*.log`, `.infer-runtime/screenshots/` | May reveal local topology, activity, or identifiers |

The local model inventory and benchmark notebook lives under `.infer-runtime/notes/`; it is evidence
for this installation, not portable product documentation.

## First local setup

Create the ignored runtime configuration from the portable example and then edit only the local
copy:

```bash
cp config/infer.example.toml config/infer.toml
```

The daemon and Console continue to use `config/infer.toml` by default. Tests and CI use
`config/infer.example.toml`, so they do not depend on a contributor's installed models or local
paths.

## Before pushing

At minimum, verify that no ignored file is still tracked and that no developer-home path remains in
the public tree:

```bash
git ls-files -ci --exclude-standard
git grep -IlE '/Users/[A-Za-z0-9._-]+|/var/folders/|/private/var/'
git status --short --ignored
```

The first command must produce no output. Findings from the second command require review: test-only
synthetic paths may be valid, but real usernames and machine locations are not.
