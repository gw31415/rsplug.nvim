# rsplug.nvim ExecPlan

This living plan tracks the remaining work after the `main...HEAD` changes.
Completed implementation details belong in Git history; keep this file focused
on decisions, open work, and validation.

## Goal

Keep installation, update, pack generation, and lazy loading predictable at
large plugin counts. Separate configuration, repository identity, materialized
snapshots, runtime registration, and pack output without making the generated
pack depend on the source cache.

## Current status

- [x] Bound fetch, Git, extraction, build, and copy concurrency; stage tarballs
      and publish snapshots only after successful extraction/build.
- [x] Make `merge = false` apply to both start and lazy plugins; preserve every
      `on_source` name and all files when compatible plugins merge.
- [x] Persist snapshot manifests and use them as a best-effort cache for merge
      probes and copy planning, with filesystem fallback for missing/stale data.
- [x] Add internal plugin identity: `name`, repository basename, or a
      deterministic script-content hash for anonymous script-only entries.
- [ ] Replace the remaining flattened lifecycle plumbing with explicit models
      (`PluginSpec`, `ResolvedGraph`, `MaterializationPlan`, `LazyRegistration`,
      `PackPlan`) and canonical repository identity.
- [ ] Publish pack generations and lockfile updates atomically; make runtime
      handlers consume manifest/registration data directly.

## Decisions already implemented

### Resource bounds and snapshots

Use independent limits for HTTP fetches, Git work, archive extraction, builds,
and copy fan-out. Tarball extraction occurs in a temporary directory and only a
complete snapshot is renamed into place. Archive paths must remain inside the
destination. A failed tarball path is removed before Git fallback.

`.rsplug-manifest-v1.json` records snapshot-relative paths, entry kinds, and
symlink targets. It intentionally remains a cache: invalid or absent manifests
fall back to filesystem inspection. Build digests and copy eligibility are
derived from configuration/build state and are not part of schema v1.

### Identity and loading

The internal ID is not a TOML field. It is `name`, repository basename, or a
content hash for an anonymous script-only entry. `depends` and `on_source`
resolve using that identity/name model. `start = true` wins over lazy triggers;
the triggers are ignored so users can toggle a plugin without editing them.

`merge = false` isolates a user artifact in either start or lazy output.
Generated control/help artifacts are internal and are not governed by a user
setting.

## Remaining phases

### Model and repository identity

Introduce the explicit models above, keeping public TOML behavior stable.
Separate remote URL parsing from repository identity. Canonical identity should
normalize scheme/host, remove default ports and trailing `.git`, exclude
userinfo, and retain the meaningful path. Use it for lock keys and collision-
resistant cache paths. Read existing lock entries compatibly, reject conflicting
revisions, and write the new representation only when the design is settled.

### Atomic generation publication

Build under `pack/_gen/.staging-*`; publish only after manifests, generated
loader code, and copies succeed. Write the lock file with temp-file + rename
(and fsync where supported). Keep old generations addressable until no retained
manifest refers to their generated runtime modules.

### Runtime hot paths

Have generated event handlers track rsplug-owned groups/callbacks, use manifest
paths for filetype loading, and maintain a module-root index for `require`
loading. Remove one-shot loaders after use and avoid scanning every mapping on
each mode change.

## Validation

After implementation changes, run:

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

Do not run `cargo check -q`. Add focused coverage for lock compatibility,
manifest reuse/fallback, anonymous scripts, all lazy triggers, generation
publication failure, and merge behavior. Network-dependent end-to-end and
large synthetic benchmarks remain optional follow-up work.
