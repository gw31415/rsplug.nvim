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
      and publish snapshots only after successful extracted/build.
- [x] Make `merge = false` apply to both start and lazy plugins; preserve every
      `on_source` name and all files when compatible plugins merge.
- [x] Persist snapshot manifests and use them as a best-effort cache for merge
      probes and copy planning, with filesystem fallback for missing/stale data.
- [x] Add internal plugin identity: `name`, repository basename, or a
      deterministic script-content hash for anonymous script-only entries.
- [x] Canonical repository identity: normalize scheme/host, remove default
      ports and trailing `.git`, exclude userinfo; unify lock keys and cache
      paths on one identity; read legacy raw-URL keys compatibly; reject
      conflicting revisions.
- [ ] Replace the remaining flattened lifecycle plumbing with explicit models
      (`PluginSpec`, `ResolvedGraph`, `MaterializationPlan`, `LazyRegistration`,
      `PackPlan`).
- [ ] Publish pack generations atomically via a staging directory and tie
      lockfile-write timing to successful publication.

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

### Canonical repository identity

`RepoSource::canonical()` produces `host[:port]/path`: host is lowercased,
default ports for `http`/`https`/`ssh`/`git` are removed (non-default ports
retained), userinfo and scheme are dropped, trailing `.git` is removed, and
path case is preserved. GitHub shorthand becomes `github.com/owner/repo`. This
single identity drives **both** lock keys and cache paths, so URL-notation
variants (`https` vs `ssh`, with/without `.git` or userinfo, default ports) of
the same repository can no longer split into separate lock entries.

`LockFile::normalize_keys` rewrites every lock key — including legacy raw-URL
keys — through `util::repo::canonicalize_lock_key`, dedups same-revision
entries, and rejects conflicting revisions for one canonical identity. Lock
version is `"2"`; reads accept `"1"` and `"2"` via read-time normalization, so
a non-`--locked` run migrates v1 → v2 automatically. `RepoSource::url()` stays
separate because fetching, tarball download, and auth need the real URL.

> Cache paths may change for environments that previously used upper-case hosts
> or non-default ports (now retained); the cache is rebuildable. This is the
> intended normalization, not a regression.

### Atomic lock write and coexisting generations

The lock file is written via temp-file + rename + fsync (parent dir synced
best-effort), so a second rsplug invocation or Nix tooling never observes a
half-written file. Pack generations (up to `RETAIN_GENERATIONS = 3`) coexist
under `generations/`, each addressable as `generations/<id>.lua` with `init.lua`
symlinked to the current one — old generations stay reachable until no retained
manifest refers to them.

## Remaining phases

### Explicit models

Replace the remaining flattened lifecycle plumbing with `PluginSpec`,
`ResolvedGraph`, `MaterializationPlan`, `LazyRegistration`, and `PackPlan`,
keeping public TOML behavior stable. (Repository identity is already canonical
— see above.)

### Atomic generation publication

Build under `pack/_gen/.staging-*`; publish only after manifests, generated
loader code, and copies succeed. Keep old generations addressable until no
retained manifest refers to their generated runtime modules. The lock write
itself and multi-generation coexistence are already done; the remaining work is
the staging step and making publication failure leave the published tree intact.

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

Do not run `cargo check -q`. Focused coverage now includes lock compatibility
(legacy URL-key read, dedup, conflicting-revision rejection), canonical
identity forms (host case, default vs non-default ports, userinfo, `.git`),
anonymous scripts, all lazy triggers, generation publication failure, and merge
behavior. Network-dependent end-to-end and large synthetic benchmarks remain
optional follow-up work.
