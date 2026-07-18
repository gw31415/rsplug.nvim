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
- [~] Replace the remaining flattened lifecycle plumbing with explicit models
      (`PluginSpec` ≈ `PluginConfig`, **`ResolvedGraph`** ✅, **`MaterializationPlan`**
      ✅ as `Plugin::load` stage-split: `resolve_target_commit` + `assemble_loaded_plugin`,
      `LazyRegistration` ≈ `PlugCtl`, `PackPlan` ≈ `PackPathState`). Remaining: explicit
      `LazyRegistration`/`PackPlan` are rename-only (low value).
- [x] Parallelize TOML config parsing (`main.rs`) via `JoinSet` +
      `spawn_blocking`, reassembling `config_paths.sort()` order by task index so
      the discovery stage no longer serializes read+parse. Byte-identical
      pack/lock verified across isolated-HOME runs.
- [x] Publish pack generations atomically via a staging directory and tie
      lockfile-write timing to successful publication.
- [x] Resolve GitHub revs for `--update`/`--install` in a single batched
      GraphQL query (Git backend GitHub HTTPS URLs included; wildcards and
      non-GitHub repos resolve per-repo inside load as before).
- [x] Exclude uninstalled repos from `--update` (no GraphQL resolve, no fetch);
      `--install` still resolves/installs them.
- [x] Stream parse → load through an event-driven scheduler
      (`run_load_scheduler`, Step 1), then BFS fan-out so each plugin loads only
      after its rev is resolved and all dependencies' loads complete (Step 2);
      `plugin_id` stays byte-identical across fan-out order.
- [x] Split `Plugin::load` into `load_early` (rev resolve → fetch/materialize →
      identity) and `load_late` (BUILD → assemble with final `order`/`lazy_type`),
      so EARLY can fan out before the full merge; `finalized` + dependency LATE
      completion gate the BUILD phase (Step 3).
- [ ] Full per-TOML streaming (Step 4): incremental conflict detection and
      per-arriving-plugin GraphQL / EARLY kick instead of waiting for
      ParsePhaseDone, so fetch stops being serialized behind config discovery.

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

### Atomic generation publication

The lock file is written via temp-file + rename + fsync (parent dir synced
best-effort), so a second rsplug invocation or Nix tooling never observes a
half-written file. The lock write runs only after `install()` succeeds, so a
failed publication does not advance the lock.

`install()` builds each new generation entirely under a private staging dir
`pack/_gen/.staging-<control_id>-<pid>-<nonce>/` and never touches the published
`opt/` while copying. Packages whose id already exists are reused (ids are
content hashes, so same id ≡ same content) instead of recopied. After copy,
manifest, and loader all succeed, new packages are `rename`d into `opt/` (each
atomic, no collisions because ids are new) and `init.lua` is swapped by `rename`
over a temp symlink — the single atomic publication point. Any failure before
that leaves `init.lua` pointing at the previous generation, so the published
tree stays bootable. A `flock` on `pack/_gen/.lock` serializes concurrent
invocations (Unix); stale `.staging-*` dirs are cleaned on entry and exit.

Pack generations (up to `RETAIN_GENERATIONS = 3`) coexist under `generations/`,
each addressable as `generations/<id>.lua` with `init.lua` symlinked to the
current one — old generations stay reachable until no retained manifest refers
to them.

### Batched rev resolution

`--update`/`--install` resolves GitHub repos' latest OID in one pre-fan-out
GraphQL query instead of per-repo REST calls inside the load fan-out. This covers
both `RepoSource::GitHub` and Git-backend `https://github.com/...` URLs
(`is_github_https` + `parse_github_url`). The query is chunked at 50; default
branch uses `defaultBranchRef`, a named ref uses `ref(qualifiedName)` with
heads/tags disambiguation. 40-hex commit revs are seeded directly. Resolved OIDs
flow into `load` through the existing `locked_rev` parameter (zero changes to
`Plugin::load`/`resolve_remote_oid`). Non-GitHub repos, wildcard refs, token-less
runs, and any GraphQL error/null keep the existing per-repo path inside `load`
(`resolve_remote_oid` → REST/git ls-remote), so the fetch fan-out starts as soon
as GitHub revs are resolved and the bottleneck stays on fetch (download/extract),
not on rev resolution.

### Uninstalled repos are not updated or GraphQL-resolved

`-u` alone must only refresh already-installed repos; it must not newly install,
nor even resolve a rev for, an uninstalled one. Previously, GitHub HTTPS + token
repos entered `Plugin::load` with a GraphQL-preresolved `locked_rev=Some(oid)`
even when uninstalled. The `locked_rev` branch (`resolve_target_commit` in
`plugin.rs`) lacked the install-state check that the normal branch has, so an
uninstalled repo slipped past the `ensure_source_git` skip guard (which
`use_tarball` bypasses for GitHub) and fetched anyway — a GitHub-backend-only
bug.

Now the GraphQL preresolve selection (`main.rs`) drops uninstalled repos under
`--update` to the immediate-load path (no GraphQL, `locked_rev=None`), where the
normal `load` branch skips them. The `locked_rev` branch is also made
self-sufficient: uninstalled + `--install` newly installs (`was_installed=true`),
uninstalled + `--update` skips, and uninstalled + `--lock` (cache is assumed
present) errors with "Missing cached repository", matching the Git-backend path.
`--install` keeps resolving uninstalled repos via GraphQL since a new install
needs the rev. A new `Plugin::is_installed` helper drives the selection.

## Remaining phases

### Explicit models

`ResolvedGraph` is now extracted from `Plugin::new` (`plugin.rs`):
`resolve(Config) -> ResolvedGraph` runs DAG resolution (`order`,
`dependency_cachedirs`, dependent-aggregated `lazy_type`) identically to before,
and `From<ResolvedNode> for Plugin` is a pure field move.

`MaterializationPlan` is addressed by splitting `Plugin::load` (≈500 lines) into
stage functions: `resolve_target_commit` (rev resolution) and
`assemble_loaded_plugin` (`plugin_id` construction core: `read_dir` →
`entries.sort()` → `lazy_type` synthesis → `FileItem` → `LoadedPlugin`, now
unit-testable in isolation). The middle (fetch → materialize → identity → manifest)
stays in `Plugin::load` — it is tightly coupled via `FetchCtx`/`materialize`/build/
identity, and a full plan/execute split is structurally impossible because the plan
phase itself is I/O-dependent (`resolve_remote_oid`, `materialize`,
`build_repo_snapshot_identity`); forcing a 20+ field `LoadCtx` was judged worse for
readability than the stage split. Remaining: explicit `LazyRegistration`/`PackPlan`
(≈ `PlugCtl`/`PackPathState`, rename-only and low value). `Plugin` struct fields and
the `dag` crate are unchanged; pack/lock output is byte-identical (verified across
isolated-HOME runs). (Repository identity is already canonical — see above.)

### Runtime hot paths

Have generated event handlers track rsplug-owned groups/callbacks, use manifest
paths for filetype loading, and maintain a module-root index for `require`
loading. Remove one-shot loaders after use and avoid scanning every mapping on
each mode change.

### TOML collection / load pipeline

`main.rs` collects all TOML config paths, sorts, and parses them in parallel
(Step 1), but still waits for the full merge into one `Config` before
generating plugins and fanning out GraphQL/load. Until that merge completes
(ParsePhaseDone), neither rev resolution nor fetch starts, so large config
sets make discovery the critical path instead of the network.

Fully streaming "per-TOML GraphQL/load" is blocked by ordering today:
`order = depth * (total + 1) + index` (computed in `Plugin::resolve`), DAG
dependency resolution (`depends` id lookup, also in `Plugin::resolve`), and
conflict detection (`run_load_scheduler`'s ParsePhaseDone handler) all require
every plugin to be merged first. `load` itself is order-independent and runs in
parallel, so a pipeline is feasible but needs the order/dependency/conflict
stages reworked to finalize after the fan-out (best-effort dependency
resolution semantics must be preserved).

Low-risk incremental win **done**: TOML parsing is now parallelized in `main.rs`
via `JoinSet` + `spawn_blocking` (async `read_to_string` + `from_str` on the
blocking pool), with results reassembled in `config_paths.sort()` order by task
index so determinism is preserved.

**Scheduler foundation done — Step 1** (`run_load_scheduler`, commit `5568884`):
a single-consumer scheduler consumes `SchedEvent` (Parsed/ParsePhaseDone/ParseError)
from the parallel parse producer and drives load fan-out via `tokio::select!` +
`JoinSet`. `Plugin::new` (batch) is used as-is, with GraphQL chunk integration
done; behavior byte-identical (pack/lock/generation id match across isolated
HOMEs). This is the event-driven pipeline base.

**BFS load ordering (done — Step 2)**: `Plugin`/`ResolvedNode` carry internal
`id`/`depends` (commit `0bf1ee4`; NOT in `plugin_id` Hash).
`run_load_scheduler` now drives a 2-gate fan-out: each plugin is spawned only
after (a) its rev is resolved (non-GitHub immediately, GitHub via chunk
completion) and (b) all its dependencies' load completes (`NodeState` /
`pending_deps` / `dependents` / `try_schedule`). This removes the
build-runtimepath race while keeping `plugin_id` byte-identical
(`LoadedPlugin::Ord` + `merge()` + `PackPathState::load`'s `drain()` + `merge()`
are fan-out-order independent). The `select!` branches are `if`-guarded so the
post-`ParsePhaseDone` `parse_rx` terminal doesn't busy-loop and GraphQL chunk
completions are reliably consumed (the old `None => break` could drop in-flight
chunks to per-repo fallback). Rev-undetermined nodes (chunk panic) are rescued
with per-repo fallback at shutdown; a leftover never-ready node is treated as a
deadlock (theoretical only — `try_dag` rejects cycles). Byte-identical verified
via `cargo test` / `clippy` / `fmt`; isolated-HOME end-to-end remains optional
follow-up.

**EARLY/LATE load split (done — Step 3)**: The split rests on two facts about
`Plugin::load`. **FACT 1**: everything before BUILD is dependency-free — only
the BUILD step reads `dependency_cachedirs`, so rev resolve → fetch/materialize
→ identity-core can run as soon as the rev is known, before `order`/`lazy_type`/
dependencies are final. **FACT 2**: `order` and `lazy_type` are baked into
`LoadedPlugin`'s `Hash` derive (hence into `plugin_id`) and cannot be
overwritten after BUILD, so assembly with the final values must wait until the
full resolve (DAG order + aggregated `lazy_type`) completes. Accordingly
`Plugin::load` is split into `load_early` (FACT 1: rev resolve → fetch/
materialize → identity-core) and `load_late` (FACT 2: BUILD via
`dependency_cachedirs` → assemble with the final `order`/`lazy_type`). New
intermediate types `MaterializeOutcome`/`EarlyOutcome` carry the EARLY result;
`EarlyOutcome`/`MaterializeOutcome`/`RunningGuard`/`load_early`/`load_late` are
`pub(crate)` and `EarlyOutcome` is re-exported from `rsplug::`. `run_load_scheduler`
is now a 2-phase scheduler with `EarlySlot`/`finalized`/`pending_deps` on
`NodeState`: EARLY tasks (`load_early`) fan out on rev resolution; LATE tasks
(`load_late`) run only after `finalized` (resolve done at ParsePhaseDone) +
EARLY done + dependency LATE completion. The `finalized` gate enforces FACT 2
(`order`/`lazy_type` reach `load_late` only after resolve). `Plugin::load` is
kept as a `#[cfg(test)]` serial wrapper for existing tests. EARLY still starts
after ParsePhaseDone (GraphQL/conflict remain batched there), so the fetch
bottleneck is not yet addressed — that is the remaining work below. Byte-identical
verified via `cargo test` / `clippy` / `fmt`.

**Remaining: full per-TOML streaming (Step 4, deferred to a later session)**:
move conflict detection into the `Parsed` handler (incremental), kick GraphQL
per-arriving-plugin (rolling batch of 25), and start EARLY (`load_early`) on each
`Parsed` instead of after ParsePhaseDone. Requires: a staging table (id-keyed)
for plugins arriving before the full merge; `PluginConfig -> Plugin` (order/
lazy_type dummy) generation so pre-resolve EARLY is possible (EARLY ignores
those fields per FACT 1); promotion at ParsePhaseDone that merges staged EARLY
state into the resolved `nodes`; design points are same-name id overwrite and
EARLY (dummy Plugin) / LATE (resolved Plugin) id-binding. `Config`/`PluginConfig`
must become `pub(crate)` so `main.rs` can reach plugins per-TOML. isolated-HOME
E2E (byte-identical pack/lock/generation vs HEAD) to follow once implemented.

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
