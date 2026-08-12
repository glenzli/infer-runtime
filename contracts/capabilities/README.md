# Capability wire artifacts

Each `<capability-id>/<schema-version>/openapi.json` is an immutable,
capability-scoped wire artifact referenced by the Runtime Capability Catalog.
It contains only that capability's routes and the transitive OpenAPI components
required by those routes.

Shared authentication and error-envelope definitions belong to Consumer Core
and are intentionally not copied into these artifacts. Otherwise an additive
Core error code would rewrite every unrelated capability digest.

Regenerate mechanically from the aggregate Consumer Core document with:

```sh
python3 tools/generate_capability_schemas.py
```

CI/read-only verification uses:

```sh
python3 tools/generate_capability_schemas.py --check
```

If regeneration changes an already published digest, either the source change
was additive in a way that preserves the exact bytes (no digest change), or the
capability needs a new dated schema version. Never silently replace a published
artifact or update only its digest constant.

During a capability migration the Catalog publishes one record per immutable
version. Separate records may share a route; one record must never claim more
than one schema version because it has only one URL and digest.
