# rsplug.nvim ExecPlan — performance, merge, and domain-model reconstruction

This is the active living plan. The previous lock/cache and copy-unification
plans are complete; Git history is their authoritative record. Keep this file
current as each phase is implemented, including discoveries and validation.

## Goal

Make installs, updates, pack generation, and lazy loading scale predictably;
make `merge = false` apply to every user plugin; and separate configuration,
repository identity, materialization, runtime registration, and pack output
into explicit models. The generated pack remains self-contained and portable.

## Progress

- [x] Phase 1: bound network/archive/build/copy resources; stage tarball
  extraction; enforce `merge = false` for all user plugins.
- [x] Phase 2: persist snapshot manifests and use them for merge/copy/doc/index
  planning instead of repeated filesystem walks.
- [ ] Phase 3: introduce `PluginSpec`, `ResolvedGraph`, `MaterializationPlan`,
  `LazyRegistration`, `PackPlan`, stable configuration IDs, and canonical
  repository identity.
- [ ] Phase 4: atomically publish pack generations and lockfile v2; make lazy
  runtime handlers manifest/registration driven.

## Phase 1 — bounded work and immediate merge semantics

### Design

Tarballs download to a temporary archive and extract into a temporary snapshot
directory. Reject archive members that escape the destination. Rename the
completed staging directory into the final snapshot only after extraction and
build succeed; remove staging data before a Git fallback.

Use independent semaphores for fetch, archive extraction, Git materialization,
build processes, and copy work. CPU capacity is `available_parallelism()` or
four when unavailable. Fetch starts at `min(16, CPU * 2)` and never exceeds 64;
archive extraction is `min(4, CPU)`; Git work is `CPU`; builds are
`max(1, CPU / 2)`; copy workers are `min(16, max(2, CPU * 2))`.

`merge = false` means a user artifact may not merge with any other user
artifact, whether it is start or opt. Generated control and help artifacts are
not governed by a user setting. Preserve all source registrations through a
merge; never retain only the left-hand `source_name`.

### Acceptance

- No archive body is accumulated in RAM before extraction.
- Failed tarball downloads/extractions leave no partial final snapshot and Git
  fallback works.
- Copy, build, extraction, and fetch fan-out cannot exceed their resource
  budgets.
- Start and opt plugins with `merge = false` are distinct packs; `on_source`
  remains valid after a compatible merge.

### Status (2026-07-11)

Implemented and validated (`cargo test --workspace`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo fmt --all -- --check` all green).

- Tarball staging + containment + rename-on-success and Git-fallback isolation
  were already in the working tree (`util::fetch::TarballFetch`).
- Bounded budgets centralized in `rsplug::util::resources`: `available_cpus()`,
  `GIT_SEMAPHORE` (CPU), `BUILD_SEMAPHORE` (`max(1, CPU/2)`). `fetch` stays in
  `main.rs` (`min(16, CPU*2)`, max 64) and tarball extraction in
  `fetch::EXTRACTION_SEMAPHORE` (`min(4, CPU)`). Git operations
  (`init_source`/`fetch_oid`/`init_snapshot`) moved off the fetch semaphore onto
  `GIT_SEMAPHORE`; builds (`run_repo_build` + `lua_post_update`) gated by
  `BUILD_SEMAPHORE`.
- Copy fan-out bounded on two axes: the entry-level `yank_semaphore` is now
  capped at `min(16, max(2, CPU*2))` (was unbounded to 256), and `copy_tree`'s
  leaf fan-out is gated by `resources::COPY_LEAF`.
- `LoadedPlugin.source_name: Option<String>` → `source_names: BTreeSet<String>`;
  merge takes the union so both sides' `on_source` names survive. `PlugCtl::create`
  registers every name. **plugin_id-incompatible** (existing pack/lock regenerates).
- `merge = false` already applied to both start and opt; kept. Added tests
  `merge_preserves_all_source_names` plus the start/opt merge-disabled tests.
- Deferred: a live end-to-end `generate` smoke test (needs network + token) and
  the Phase-spanning synthetic benchmarks (32 tarballs / 10,000-file copies /
  128-plugin merge) listed under Validation.

## Phase 2 — immutable snapshot manifests and merge planner

Write `.rsplug-manifest-v1.json` after each staged snapshot is ready. It lists
relative path, kind, symlink target, copy eligibility, and (for builds) one
persisted output digest. Exclude `.git`, build-success state, and the manifest
itself unless `dotgit` explicitly requests `.git`. Store a per-repository
latest-snapshot index atomically.

Replace sealed-directory entries and recursive merge probes with a flat trie
from that manifest. Bucket artifacts by load policy and merge eligibility;
stable source ID order determines first-fit selection. `MergePlan` contains the
copy source/destination entries and is the only input to bounded copying. Docs
are manifest entries aggregated once before a single helptags invocation.

### Status (2026-07-11)

Implemented and validated (`cargo test --workspace`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo fmt --all -- --check` all green).

- `entities::manifest::SnapshotManifest` writes `.rsplug-manifest-v1.json` at
  snapshot-ready (`Plugin::load`, after the build-success marker), plus a
  per-repo `<repo>/latest-snapshot` index. Both are best-effort caches; reuse
  runs skip the manifest rewrite to avoid redundant walks.
- Manifest is a **pure filesystem record** (path, kind, symlink target).
  `copy_eligible` and the build-output `build_digest` are config/build-cache
  derived, not filesystem facts, so they are intentionally omitted from schema
  v1 (added when consumed).
- Merge probes (`entries_mergeable` `is_dir`, and `read_dir_children` in
  `entries_mergeable`/`expand_dir_union`/`expand_sealed_into`) now consult a
  process-global manifest cache via `merge_is_dir`/`merge_children`. **Filesystem
  fallback** preserves exact behavior when the manifest is absent, stale, or for
  symlinks (manifest does not follow; `is_dir`/`read_dir` do). Sealed-dir
  representation and macOS clonefile copy are unchanged.
- Docs are already aggregated into a single `_rsplug:doc` package (Phase 8
  `split_doc`), so helptags already runs once — no separate work needed.
- Deferred from the Phase 2 text: an explicit `MergePlan` struct (the merged
  `LoadedPlugin` files already are the copy plan; introducing the name is
  ceremonial) and the `copy_eligible`/`build_digest` manifest fields.

## Phase 3 — explicit public and internal models

Add `id` to TOML. `depends` and `on_source` refer only to it. For one
compatibility release, infer a missing ID from legacy `name`, then repository
basename; reject duplicate inferred IDs and warn that `name` is deprecated.
Script-only entries require `id` and reject build/dotgit fields. Reject
`start=true` combined with a lazy trigger.

Replace flattened lifecycle handling with `PluginSpec`, `ResolvedGraph`,
`MaterializationPlan`, `LazyRegistration`, `Artifact`, and `PackPlan`.
`propagate_to_dependency()` replaces the overloaded `LazyType` bit-and.

Separate `RemoteUrl` from `RepoIdentity`. Canonical identity lowercases scheme
and host, removes default ports and trailing `.git`, excludes userinfo, and
retains scheme/host/non-default-port/path. Use it as the lock v2 key and in a
cache path with a 128-bit hash suffix. Read lock v1 by normalizing configured
keys in memory; reject conflicting revisions; write v2 on non-locked runs.

### Status (2026-07-11) — partial (3A done)

Implemented and validated (test/clippy/fmt green; e2e backward-compat
confirmed: id-less configs still infer, explicit `id` works, generated
init.lua loads in nvim headless).

- **3A — `id` field (done):** `PluginConfig.id: Option<String>`;
  `stable_id()` = explicit `id` > legacy `name` > repo basename. `depends`/
  `on_source`/DAG/source_name all reference `stable_id` (so existing configs
  keep working via inference). Validation in `Plugin::new` (routed via a new
  `Error::ConfigValidation` + `Error::Dag(#[from])`): script-only (no repo)
  requires a stable id and rejects `build`/`lua_build`/`lua_post_update`/
  `dotgit`; duplicate ids via DAG (`DagError::DuplicateName`). `start=true` +
  lazy trigger rejected at deserialize (`LazyType: TryFrom<LazyTypeDeserializer>`).
- **Deferred from 3A:** the `name` deprecation *warning* (needs a log Message
  variant + rendering; non-fatal, deferred).
- **3B — `RemoteUrl`/`RepoIdentity` + canonical identity + lock v2:** not started.
- **3C — explicit models (`PluginSpec`/`ResolvedGraph`/...) + `propagate_to_dependency`:** not started (largest piece).

## Phase 4 — atomic publish and runtime hot paths

Build each pack generation under `pack/_gen/.staging-*`, then publish it only
after manifest, loader, and copies succeed. Write lock after generation publish
using temp-file, fsync, rename, and parent-directory fsync.

Generated event code tracks rsplug-owned groups/callbacks rather than comparing
all autocmds. Filetype loading uses manifest paths, require uses a registered
module-root set and removes its loader after all roots load, and mappings use
reverse indices rather than scanning every pattern on a mode change.

## Validation

Run `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and
`cargo fmt --check` after every phase. Add synthetic tests for 32 tarballs,
10,000-file copies, 128-plugin merge plans, manifest/index reuse, lock v1/v2,
and every lazy trigger. Add a JSON benchmark harness reporting wall time, peak
RSS, maximum in-flight work, and filesystem scan count so CI detects regressions.
