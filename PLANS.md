# 4-phase performance ExecPlan

This document contains only unimplemented work. The previously completed
runtime-hot-path plan (R0-R5) is intentionally removed; Git history owns it.

## Goal and priority

Improve the following four phases, in this order of concern:

1. update (`rsplug --update`): remote revision discovery and changed-repository
   selection;
2. install (`rsplug --install`): acquisition and materialization of missing
   repositories;
3. snapshot-to-generation refresh: rebuilding/publishing the pack generation
   from immutable cached repository snapshots;
4. lazy load: the generated Lua path from a Neovim trigger to a usable plugin.

Performance is the primary decision criterion. Code cleanliness is secondary
and is accepted when it makes ownership, measurement, or later optimization
safer. Do not land a readability-only rewrite on a hot path without showing
that it is neutral in the structural counters and same-machine benchmark.

This is a plan, not an authorization to implement unrelated behavior. Work in
small commits in the milestone order below. At the end of each milestone,
delete its completed recipe from this file and leave only the result and any
remaining work.

## Terminology and phase boundaries

There is no `snapshot` CLI command. In current code, a repository snapshot is
the immutable directory
`repos/<canonical-repo>/worktrees/<snapshot-key>/`. The third phase in this
plan therefore means **refreshing the output generation from those snapshots**:
`PackPlan::load` through `PackPlan::install`, including merge, control package
generation, package reuse/copy, ft index creation, generation metadata,
`init.lua` publication, retention, and cleanup.

The boundaries are:

- update ends when every configured repository has an old/current target OID
  and a changed/unchanged decision;
- install ends when a missing target OID has a ready immutable repository
  snapshot (including required build hooks and its inventory);
- snapshot refresh consumes ready snapshots and ends when one generation is
  atomically bootable;
- lazy load starts after Neovim has loaded the generated control package and a
  trigger fires.

`Plugin::load_early/load_late` currently crosses the first three boundaries.
Splitting its data models is part of the plan; do not change hook order merely
to make the split easier.

## Evidence from the current tree

Read these paths before changing anything:

- orchestration and GraphQL rolling batches: `crates/rsplug/src/main.rs`,
  especially `app`, `run_load_scheduler`, `flush_graphql_chunks`, and the
  `NodeState`/staging types;
- revision selection, fetch, snapshot construction, builds, and assembly:
  `crates/rsplug/src/rsplug/entities/plugin.rs`, especially
  `load_early`, `load_late`, `resolve_target_commit`, `ensure_source_git`,
  `materialize`, `assemble_loaded_plugin`, `latest_snapshot_dir`, and
  `build_repo_snapshot_identity`;
- Git/GitHub/tarball and resource controls:
  `crates/rsplug/src/rsplug/util.rs` and
  `crates/adaptive_semaphore/src/lib.rs`;
- repository inventory: `crates/rsplug/src/rsplug/entities/manifest.rs`;
- merge, copy, publication, ft index, and retention:
  `crates/rsplug/src/rsplug/entities/pack_plan.rs`;
- generated runtime: `crates/rsplug/src/rsplug/entities/lazy_registration.rs`,
  `crates/rsplug/templates/lua/_rsplug/`, and
  `crates/rsplug/templates/plugin/`;
- behavior fixture: `crates/rsplug/tests/runtime_hot_paths.lua`.

Important observations that must be verified by tests during implementation:

- Network call sites acquire `AdaptiveSemaphorePermit` but the revision and
  tarball paths usually drop it without `finish(is_error)`. Dropped permits
  release capacity but contribute no latency/error sample, so the advertised
  adaptive limit is not adapting on the main network path.
- `load_late` writes `<repo>/latest-snapshot`, but
  `latest_snapshot_dir` does not read it. It scans `worktrees/` and stats every
  candidate. Update selection may repeat that work through `is_installed`,
  `latest_snapshot_oid`, and `snapshot_exists_for_oid`.
- The same canonical repository may appear in multiple plugin specs. Revision
  conflicts are rejected, but fetch/materialize/build work is not coalesced.
  `.building-<pid>` and manifest temp names are not unique per in-process task.
- A new snapshot can be walked independently for content hashing, manifest
  creation, root assembly, `lua/` roots, and recursive `doc/` extraction.
- `SnapshotManifest::kind_of` and `child_names` linearly scan the serialized
  entry vector. They are called inside recursive, pairwise merge probing.
- `LoadedPlugin::merge` repeatedly sorts groups and invokes
  `deterministic_cmp`; that comparator can recompute a full `plugin_id` hash.
- After package publication, `build_ft_index` re-scans output directories even
  though the snapshot inventory already knew the file set.
- A warm, identical run still writes generation metadata/loader/`init.lua`,
  scans retention state, performs cleanup, and writes/fsyncs the lock file.
- The install flock covers staging cleanup, all copy/helptags work, publish,
  and garbage collection rather than only the commit window.
- `on_map/init.lua::remove_pattern` scans all `id_patterns` entries for every
  removed pattern; manifest path validation uses linear duplicate checks; and
  trigger stubs can remain after another trigger loads their package.

Baseline verified on 2026-07-22:

    cargo test --workspace

passes (148 rsplug tests plus workspace tests; one ignored benchmark). The
existing ignored runtime benchmark produced these debug-build, same-machine
values under the JSON key `median_ns` from five samples:

| case | current median |
| --- | ---: |
| 1,000 autocmd records | 10,858 ns |
| ft-file path workload | 657,809 ns |
| 10,000 unrelated `require` calls | 510,436,500 ns |
| 10,000 unrelated mode changes | 3,749,266 ns |

The current Lua `measure` helper divides the total by the sample count, so these
are actually arithmetic means mislabeled as medians. M0 must correct this
before any runtime performance claim. These wall times are comparison data,
not CI thresholds. The unrelated-require value also includes failed standard
Lua loader filesystem searches; measure an otherwise identical control without
the rsplug searcher and compare the delta.

## Invariants

All milestones must preserve the following unless a step explicitly says that
a schema/ABI bump is expected:

- `--update` never installs a repository absent from the cache;
  `--install` installs only missing repositories; `--locked` performs no remote
  access and errors on a missing locked resource.
- Repository snapshot directories are immutable after publication. An update
  creates a new snapshot and never moves the old snapshot to another commit.
- Build order remains: materialize, `lua_post_update` only for a real update,
  configured `build`/`lua_build`, identity/inventory publication, assembly.
  Dependency build runtime paths remain dependency-completion ordered.
- Pack publication is failure atomic. Before the final pointer swap, any error
  leaves the old generation bootable. Lockfile publication follows successful
  generation publication.
- Identical semantic input produces byte-identical output. No serialized
  artifact contains a build-machine absolute path. Any intentional hash,
  manifest, merge, or generation ABI change has an explicit version field and
  a one-time invalidation test.
- Preserve `merge=false`, doc aggregation, helptag correctness, dotgit
  behavior, symlink and non-UTF-8 behavior, exact load/hook/dependency order,
  and every lazy-trigger semantic covered by `runtime_hot_paths.lua`.
- A package loads at most once. Preserve the event workarounds for
  `InsertCharPre`, `BufNew`/`BufReadCmd`, `User` matching, ftplugin source order,
  mapping replay, and require recursion protection.
- Resource queues are bounded. Do not replace a bounded worker pool with one
  spawned task per repository, file, or cleanup candidate.
- CI gates deterministic structure and operation counts. Wall-clock/RSS
  comparisons are recorded on one machine and are not flaky CI pass/fail
  thresholds.

## Milestone order

Implement in this dependency order:

1. M0 measurement and reference behavior;
2. U1 snapshot catalog and update selection;
3. U2 adaptive remote resolution and request coalescing;
4. I1 fetch/materialization pipeline;
5. S1 single-pass inventory and indexed queries;
6. S2 filesystem-free merge and generation planning;
7. S3 no-op publication fast path;
8. S4 narrow-lock bounded publisher;
9. L1 common lazy runtime state and trigger retirement;
10. L2 per-trigger hot-path improvements;
11. C1 cleanup of orchestration boundaries after the fast paths are stable.
12. D1 update user-facing documentation after behavior and measurements are
    final.

Do not start S2 before S1: optimizing the present manifest lookup API would
cement its linear-query representation. Do not start the no-op shortcut before
M0 records every invalidation input.

## M0: measurement and reference behavior

### Deliverables

Add an ignored performance harness without changing production behavior.
Prefer test-only instrumentation behind a small `PerfCounters` trait or
`#[cfg(test)]` hooks; do not put an atomic increment on every production file
operation. Write reports below `target/`.

Create:

- `target/update_bench.json`;
- `target/install_bench.json`;
- `target/snapshot_refresh_bench.json`;
- keep and extend `target/runtime_hot_paths_bench.json`.

Each case records scenario name, scale, warmup count, measured iterations,
median, p95, CPU time when available, peak RSS when available, and structural
counters. Record toolchain, OS/filesystem, CPU count, and whether the build is
debug or release.

Fix `runtime_hot_paths.lua` measurement first: retain every sample, sort it,
choose the middle item as median, and choose `ceil(samples * 0.95)` as p95.
With five samples p95 is the maximum; state that in the report. JSON must expose
`scale`, `iterations`, `samples`, `median_ns`, `p95_ns`, `min_ns`, `max_ns`, and
`api_counts`, with deterministic key ordering. Measure 1k/2k/4k scales for
autocmd records, ft paths, and mapping edges. For `require`, measure active
rsplug searcher and otherwise identical temporarily-removed-searcher control
in the same Neovim process; report both and their delta. Separate repeated-name
dispatch from unique missing-module filesystem search.

### Fixtures

Build all network-sensitive fixtures against local servers/repositories so the
test does not depend on GitHub latency:

1. Generate 128 and 512 configured plugins with deterministic dependency DAGs.
2. Generate bare local Git repositories with commit A/B and shallow-compatible
   HTTP fixtures. Add a local HTTP server that can return GraphQL JSON,
   tarballs, delays, 404, 429/rate-limit metadata, truncated bodies, and
   connection reset.
3. Generate snapshot trees at 1k, 10k, and 100k entries. Include `lua/`, nested
   `doc/`, every supported ftplugin form, symlinks, ignored paths, a root
   file/directory conflict, and a last-leaf conflict.
4. Generate a 10k-package historical `opt/` fixture and 100 retained snapshot
   directories for scan-complexity tests.
5. Add an isolated-cache end-to-end driver for cold install, warm install,
   no-change update, 5%-changed update, flagless refresh, and `--locked`.

### Required counters

At minimum count:

- GraphQL/REST/ls-remote requests, tarball/fetch requests, retries, fallback
  fan-out, current/max concurrency, permit samples, and final limit;
- `worktrees` directory scans, directory entries, metadata calls, inventory
  parses/builds/repairs, recursive walks, content bytes hashed, and duplicate
  materialization/build jobs;
- merge compatibility probes, manifest full scans, path lookups, full
  `plugin_id` hashes, files/bytes copied, clone/reflink/hardlink/copy results,
  queued jobs, spawned workers, helptags processes, and GC candidates;
- generation/manifest/loader/init/lock writes and fsyncs, lock wait/hold time;
- Lua `packadd` attempts/successes, API calls, trigger callbacks removed,
  mappings visited/deleted, manifest path validations, and searcher invocations.

### Reference gates

Before optimization, add reference tests which compare the current and new
engines on small randomized inputs. Snapshot expected final trees and logical
events, not private intermediate struct layout. Add fault points before/after
materialize, build, inventory write, package rename, generation metadata write,
pointer swap, lock write, and GC.

M0 is complete only when a failing structural counter produces a readable test
failure naming the scenario and unexpected operation.

## U1: update selection must be O(repositories), not O(snapshot history)

### Target code

- `plugin.rs::{is_installed,latest_snapshot_dir,latest_snapshot_oid,
  snapshot_exists_for_oid}`
- the `ctx.update` selection branch in `main.rs::run_load_scheduler`
- the `latest-snapshot` write at the end of `Plugin::load_late`

### Data model

Introduce a `SnapshotCatalog` for one canonical repository. It owns:

- repository root and worktrees root;
- an optional validated latest key and OID;
- a lazily built `oid -> snapshot keys` map for fallback and build variants;
- whether the on-disk index was valid, repaired, or unavailable.

Use one scheduler-scope cache keyed by canonical repository. All specs for the
same repository share the same `Arc<SnapshotCatalog>`. Do not use a process
global cache because tests and multiple cache roots must not leak state.

### Implementation steps

1. Specify the index file format and validation. A key is a single relative
   filename under `worktrees`, never absolute, never `.`/`..`, and its prefix
   before `__` must be a 40-hex OID. Resolve the candidate and verify it is a
   directory. Do not follow a path outside the repository root.
2. Read and validate `<repo>/latest-snapshot` first. For a valid index, return
   the path/OID without `read_dir(worktrees)` or per-entry metadata.
3. For missing, malformed, stale, or unsafe indices, scan `worktrees` once,
   ignore hidden building directories and invalid names, deterministically
   choose the same latest result as today, build the OID map, and atomically
   repair the index with a unique temp file plus rename.
4. Replace the four independent helper calls with methods on the shared
   catalog. A single run may perform at most one fallback scan per canonical
   repository.
5. Move update installed/uninstalled classification out of the serial
   `Parsed` event loop. Resolve catalogs concurrently through a bounded local
   I/O worker set, then route their results back to the scheduler. Do not await
   one filesystem scan while preventing other parse/GraphQL events from being
   consumed.
6. Make all index publications unique per task/process and atomic. Never use a
   PID-only temp name.
7. Once the OID is known, calculate the exact snapshot key (including build
   inputs) before deciding whether `source.git` is needed. Reuse only the exact
   ready snapshot. The existence of a different build variant for the same OID
   must not incorrectly skip acquisition needed by the desired variant.

### Acceptance

- A valid index causes zero `worktrees` `read_dir` calls and zero history-entry
  metadata calls.
- A missing/stale/bad index causes exactly one fallback scan per canonical
  repository per run and repairs the index. A second run takes the fast path.
- 100 historical snapshots cost the same number of catalog operations as one
  snapshot on the valid-index path.
- Path traversal, symlink escape, partial write, non-UTF-8 name, and concurrent
  reader/writer fixtures are safe and preserve the current fallback result.
- Uninstalled repositories under `--update` make zero remote requests and are
  still reported as not installed; `--install` and `--locked` semantics remain
  unchanged.

## U2: make remote resolution adaptive, deduplicated, and failure-aware

### Target code

- `main.rs::{flush_graphql_chunks,run_load_scheduler}`
- `plugin.rs::{resolve_target_commit,resolve_remote_oid,materialize}`
- `util.rs::github` and `adaptive_semaphore`

### Implementation steps

1. Add one outcome helper around every adaptive permit:

       let permit = limit.acquire().await;
       let result = operation.await;
       permit.finish(result.is_err());
       result

   Use it for GraphQL chunks, REST resolution, ls-remote, tarball download, and
   Git fetch as appropriate. Ensure cancellation/drop releases a permit but is
   separately counted as cancelled rather than a successful sample.
2. Write an integration test proving real call sites, not only semaphore unit
   tests, change the limit after synthetic good and error windows.
3. Deduplicate GraphQL inputs by `(canonical repository, requested rev)` before
   chunking. Preserve a deterministic list of every waiting plugin key and fan
   one result out to those keys. One repository in multiple plugin specs must
   produce one remote resolution.
4. Bound chunk tasks. Calculate chunk capacity from GraphQL fields/cost (a
   named ref emits head and tag fields) rather than assuming every repository
   costs one field. Keep request and result ordering independent from output
   ordering.
5. Replace chunk-error per-repository fallback bursts with a shared fallback
   policy. Rate-limit responses open a run-scoped circuit breaker for the API;
   subsequent work goes directly to bounded Git fallback. Transient failures
   receive a small, jittered, capped retry budget before fallback. Auth/404 and
   invalid-ref errors do not retry blindly.
6. Add per-host limits so GitHub API, codeload, and arbitrary Git hosts cannot
   consume one another's entire global budget. The global budget remains an
   upper bound.
7. Separate `ResolvedRevision` from `LoadRev`. It carries canonical ID, OID,
   resolution backend, old OID, and `Changed|Unchanged|Missing`. Use this value
   through EARLY instead of recomputing installed state.
8. Before LATE work starts, calculate the set of repositories whose OID or
   semantic configuration changed. Preserve dependency scheduling, but allow
   unchanged snapshots to reuse their catalog/inventory handle directly.

### Acceptance

- N duplicate specs for one canonical/rev perform one resolution and at most
  one fetch/materialization job.
- Every completed remote operation contributes exactly one adaptive success or
  error sample. The final limit decreases in the 429/reset fixture and can
  increase in the sustained-success fixture, within configured min/max.
- The number of simultaneously running GraphQL/fallback/host requests never
  exceeds its declared budget; queued work is bounded.
- One failed GraphQL chunk cannot start an unbounded per-repo fallback herd.
- Output and lockfile are byte-identical for different completion orders.
- No-change update has zero fetch, tarball, checkout, build, and inventory-build
  operations. Remote resolution remains the only network cost.

## I1: coalesce and pipeline repository acquisition

### Target code

- `plugin.rs::{ensure_source_git,materialize,run_repo_build,load_early,
  load_late}`
- `util.rs::{git,fetch::TarballFetch,dirty_diff_from_content,resources}`
- scheduler job ownership in `main.rs`

### Shared job model

Add a scheduler-owned `RepoJobRegistry`. Keys are typed and include every
input that changes the result:

- resolution job: canonical repo + requested rev;
- object acquisition: canonical repo + OID + backend/dotgit requirement;
- unbuilt materialization: canonical repo + OID + backend/dotgit requirement;
- built snapshot: canonical repo + OID + build argv + `lua_build` + relevant
  post-update state.

Values are shared futures/results. Remove an errored/cancelled job only after
all current waiters observe the same error so a duplicate spec cannot race a
second destructive attempt.

### Implementation steps

1. Replace `.building-<pid>` with a unique, repository-local staging directory
   owned by one job. Publish with rename. If another winner already published
   the same final key, validate and reuse the winner, then delete only this
   job's staging directory.
2. Serialize initialization/mutation of one `source.git` while allowing
   different repositories to proceed concurrently. Re-check whether the OID
   exists after acquiring that repository lock.
3. Coalesce duplicate fetch, checkout, build, manifest, and latest-index work.
   Plugin-specific lazy settings and scripts remain outside the shared snapshot
   job and are assembled separately.
4. Remove the tarball archive `sync_all`: the compressed archive is staging,
   not a durable artifact. First benchmark the simpler file-backed version
   without fsync. Then add an optional bounded producer/consumer path that
   streams reqwest chunks to the blocking gzip/tar reader while extracting to
   a private directory. The bridge must have a fixed byte capacity and must
   propagate producer, decoder, tar-safety, and cancellation errors both ways.
5. Keep `tar::Entry::unpack_in` containment checks and the one-top-level-root
   rule. Never expose a partially extracted directory. On tarball failure,
   remove only its staging and perform bounded Git fallback.
6. Combine Git dirty detection and diff hashing into one blocking Git query
   returning `Option<digest>`. Cover untracked files explicitly. For a plain
   built tree, S1 will calculate the content digest during the inventory pass;
   do not retain a second recursive content-hash walk.
7. Build dependency runtime paths once for a snapshot job and reuse the same
   ordered result for `lua_post_update` and `run_repo_build`; the current path
   builds it twice when both hooks run.
8. Replace one spawned task per queued build/output line/copy unit with bounded
   workers/channels. Log order may reflect completion order, but deterministic
   artifacts may not.
9. Record backend and stage durations on the shared job so one logical job is
   counted once even when many plugin specs await it.

### Acceptance

- Duplicate specs with equal snapshot inputs produce one remote request,
  acquisition, checkout/extraction, build, inventory build, and publication.
- Different build inputs never share the final built snapshot but may share a
  read-only unbuilt acquisition safely.
- No repository has two concurrent `source.git` initializers or writers.
- A 100k-entry tarball uses bounded bridge/queue memory, overlaps download and
  extraction, and leaves no archive/staging residue on success, error, or
  cancellation.
- Tar traversal/symlink escape, truncation, decoder error, rename collision,
  build failure, and concurrent identical installs preserve the old snapshot
  and return a stable error.
- Same-machine cold-install median and p95 improve. Structural gates (one job,
  bounded queue, no staging fsync, no duplicate walk) are mandatory even where
  filesystem noise hides a wall-time win.

## S1: build one indexed snapshot inventory and reuse it everywhere

### Target code

- `entities/manifest.rs`
- snapshot identity/assembly in `plugin.rs`
- manifest cache and merge probes in `pack_plan.rs`

### Data model

Introduce `SnapshotHandle { root, identity, inventory }`. `root` is placement
state and never participates in the content identity hash. `inventory` is an
`Arc<SnapshotInventory>`.

Keep serialization compact and deterministic. Separate:

- persisted data: schema, sorted entries, kind, symlink target where needed,
  and derived information that saves a later pack scan;
- in-memory indices: path -> entry/kind, parent -> contiguous child range or
  child list, top-level entries, Lua roots, doc files, and ftplugin candidates.

The in-memory representation may be rebuilt in one linear pass when reading a
legacy v1 manifest. Do not serialize hash-table order.

### Implementation steps

1. Add schema validation. Current/legacy valid manifests load; unknown schema,
   corrupt JSON, and impossible paths use one filesystem fallback and atomic
   repair.
2. Build the inventory with one traversal after the snapshot is ready. Use
   `DirEntry::file_type` when sufficient; call metadata/readlink only where the
   semantics require it. Sort once at the serialization boundary.
3. While visiting a plain built tree, update the content digest with the exact
   deterministic path/content rules required by identity. Return inventory and
   digest together.
4. Derive top-level placement entries, unique Lua roots, recursive doc files,
   and ftplugin candidates from the inventory. Delete the independent root,
   `lua/`, `doc/`, and later output ftplugin walks on the valid-manifest path.
5. Change `assemble_loaded_plugin` to consume `SnapshotHandle`. Change
   `FileSource::Directory` or its replacement to retain the handle so merge
   never performs synchronous filesystem reads or global-cache locking.
6. Replace `kind_of` linear search with indexed lookup and `child_names` full
   scan with a parent-range/child-list lookup proportional only to returned
   children.
7. Preserve filesystem fallback for symlinks whose target kind must be
   followed, but resolve and cache that answer once per inventory/path. Count
   it; ordinary file/dir entries must not fall back.
8. Use unique atomic temp files for manifest and latest-index writes. A
   best-effort cache failure must not create a partial valid-looking file.

### Acceptance

- A newly built snapshot performs exactly one recursive inventory traversal.
  Plain+build content hashing is part of that pass.
- A reused valid snapshot performs zero recursive `read_dir`/stat calls and
  parses its manifest at most once per run/root.
- From the start of `PackPlan::load`, snapshot filesystem probes are zero for
  ordinary inventory entries.
- `kind` lookup is O(1) or O(log entries); child lookup is O(log entries +
  returned children), never O(all entries).
- Legacy v1 and fallback output matches the reference for top-level placement,
  Lua roots, recursive docs, ignore rules, dotgit, symlinks, and non-UTF-8
  paths.
- A 100k-entry inventory has a measured memory budget. Avoid duplicating every
  full path in several maps; prefer indices/ranges into sorted storage.

## S2: make merge planning filesystem-free and avoid repeated full hashes

### Target code

- `pack_plan.rs::{LoadedPlugin::merge,deterministic_cmp,Add,
  dirs_mergeable,entries_mergeable,union_files,normalize_sealed}`

### Implementation steps

1. Add a reference engine under `#[cfg(test)]` before replacing the current
   implementation. Generate random small inventories and compare final file
   ownership, conflict behavior, load order, scripts, source names, and package
   grouping.
2. Decorate each input once with its deterministic sort key and computed
   `plugin_id`. Comparators must never hash the full plugin. Compute one final
   hash per completed group.
3. Partition candidates before comparison:
   `merge=false` user singleton, generated control artifacts, then mergeable
   user plugins by `LazyType`. This must preserve the current rule that user
   and internal control artifacts never mix.
4. Represent an inventory/group as a sorted path overlay or trie. Probe file vs
   directory prefix conflicts, Conflict/Overwrite, and sealed directories
   without filesystem I/O. Overlay only the candidate's affected paths into a
   group; do not rescan the full manifest for each pair.
5. Replace repeated `groups.sort`, `Vec::remove`, and `Vec::insert` with a
   stable arena/index list. Preserve deterministic first-fit/fixed-point output
   if practical. If equivalent grouping cannot be preserved, define and bump a
   `MERGE_ABI`, document the one-time package-ID churn, and retain equivalent
   final runtime semantics.
6. Normalize sealed-directory overlays incrementally. The three-plugin
   non-transitive case must not restart a scan of every existing key.
7. Remove `MANIFEST_CACHE` and synchronous manifest reads from merge code after
   all callers pass `SnapshotHandle`.

### Acceptance

- Merge performs zero filesystem syscalls and zero serialized-manifest full
  scans.
- Full `plugin_id` hashing is at most once per input plus once per final group.
- Doubling 128 to 256 and 256 to 512 disjoint plugins grows near 4x or better;
  it must not exhibit the present repeated-sort/hash/entry-scan multiplication.
- On the same machine, 128/512-plugin disjoint, root-conflict, and last-leaf
  fixtures improve median by at least 3x before this milestone is considered
  worthwhile. CI gates counts/equivalence, not the 3x time.
- Random property tests and fixed fixtures cover input permutation,
  merge=false, file/file, file/directory, nested sealed directories,
  Overwrite, doc split, and three-way non-transitive merges.

## S3: plan generations before I/O and make identical refresh a true no-op

### Generation model

Create a pure `GenerationPlan` whose stable hash input explicitly contains:

- generation schema and template/runtime ABI;
- merge ABI;
- sorted package IDs and their placement entries;
- control IDs and startup order;
- runtime ft index and any future runtime indices;
- retention-relevant entries.

The resulting `generation_id` is the publication identity. Adding a field to
the plan must force the implementer to decide whether it belongs in the hash.

### Implementation steps

1. Build the ft index from S1 inventories and S2 merged overlays. Preserve the
   exact order: `ftplugin/{ft}.vim`, `.lua`, sorted `{ft}_*.vim|lua`, then
   sorted immediate `ftplugin/{ft}/*.vim|lua`. Preserve symlink-follow rules.
   Delete the post-publication `build_ft_index` scan.
2. Store immutable package metadata separately from generation metadata. Do
   not overwrite a content-addressed control package with a generation-specific
   `manifest.json` after publication.
3. Publish `generations/<generation-id>.json` (or an equivalently named stable
   metadata file) atomically. It contains relative paths only. Generate the
   loader from this metadata and atomically point `init.lua` at it.
4. Before staging packages, compare the desired plan with the currently
   published generation. Validate the referenced metadata/control/package
   directories. If identical and intact, return without staging, copy,
   helptags, ft scan, metadata/loader/init write, retention scan, or GC.
5. Compare the final semantic lock map with the existing normalized map. If
   equal, do not serialize, write, fsync, rename, or fsync the parent.
6. Replace retention discovery through every `opt/<id>/manifest.json` with a
   small generation registry. Read at most the current plus the retained
   generation count. Derive package reachability from those metadata files.
7. Run GC only when unreachable entries exist or a size/count threshold is
   crossed. Apply a per-run item/time budget and continue next run. GC is not
   on the bootability commit path.
8. Batch helptags into at most one headless Neovim invocation per new
   generation. If doc aggregation guarantees one doc package, assert that
   structurally instead of creating a closure/process per package.

### Acceptance

- A second run with identical semantic input performs zero package copies,
  recursive walks, helptags processes, ft scans/stats, generation/manifest/
  loader/init/lock writes, fsyncs, retention scans, and deletes. Relevant mtimes
  remain unchanged.
- Changing one plugin/config rebuilds only affected control/generation
  artifacts; unchanged snapshot inventories and packages are not walked or
  copied.
- A valid ft index is byte deterministic, contains no absolute path, and passes
  all existing exact/suffix/subdir and runtime zero-lookup tests.
- Warm no-op refresh with 128 cached plugins improves p95 by at least 50% on the
  same machine. CI gates the zero-operation assertions.
- Every failpoint before pointer swap boots the old generation in headless
  Neovim. After swap, metadata and all referenced packages exist.
- Test the pre-existing report that install and later regenerate can produce
  different package IDs for byte-identical content. The same semantic config,
  OIDs, snapshot inventories, and backend-independent identity must produce
  identical package/generation IDs across `--install`, flagless, and
  `--locked`; otherwise warm reuse is not complete.

## S4: narrow publication locking and bound copy/cleanup pipelines

### Implementation steps

1. Build private staging packages and run helptags outside the global flock.
2. Give each staging root a lease/lock. Stale cleanup may remove only an
   unleased staging directory whose owner is gone or whose age exceeds the
   documented threshold. Never delete every `.staging-*` blindly while another
   process may be building.
3. Acquire the global flock only for: re-read current generation, detect a
   concurrent winner, publish missing staged packages, publish generation
   metadata/loader, and atomically swap the pointer. Release before GC.
4. Under the lock, re-check each content-addressed destination. If another
   process published it, validate/reuse it and delete the local staged loser.
5. Replace package-entry, copy-leaf, and cleanup one-task-per-item patterns with
   a bounded channel and fixed worker count. Walk and copy concurrently; do not
   accumulate every leaf path before starting copies.
6. Pass `DirEntry::file_type`/known inventory kind to workers to avoid duplicate
   metadata calls. Keep reflink -> hardlink -> copy fallback semantics under
   explicit tests. Decide separately whether hardlinking an immutable snapshot
   into a mutable pack is safe; do not trade snapshot immutability for speed.
7. Use an owned-staging guard so every early `?`, panic join error, and
   cancellation cleans only that run's private staging.

### Acceptance

- Instrumentation shows no recursive copy, headless Neovim process, recursive
  delete, or inventory/ft scan while holding the global publication lock.
- For 100k leaves, queued jobs are at most twice the worker count and task count
  is fixed, not proportional to leaves. Peak RSS no longer includes a vector of
  all source/destination paths.
- Two concurrent identical and two concurrent different generations all
  succeed or return a documented winner result; final `init.lua` references one
  complete generation, no package is missing, and active staging is preserved.
- Copy, helptags, package rename, metadata write, loader write, pointer swap,
  and GC failpoints preserve the atomicity invariants.

## L1: introduce common lazy runtime state and retire stale trigger stubs

The completed R0-R5 work remains the baseline: linear autocmd discovery,
manifest-driven ft loading, require-root reconciliation/searcher retirement,
and map observer retirement must not be reimplemented or regressed.

### Target code

- `lazy_registration.rs::From<LazyRegistration> for Vec<LoadedPlugin>`
- `templates/lua/_rsplug/init.stpl`
- all `_rsplug/on_*.{lua,stpl}` and `plugin/on_*.{lua,stpl}` templates

### Data model

Generate one `RuntimeState`/`TriggerRegistry` with:

- package state `unloaded | loading | loaded`;
- trigger kind/key -> ordered package IDs;
- package ID -> registrations that can be removed or reduced after load;
- one cached local reference to the core runtime in each module;
- optional counters enabled only in the headless benchmark fixture.

Do not generate a generic abstraction that adds table lookups to every hot
call. Specialized trigger modules may hold direct local references into this
state.

### Implementation steps

1. Make `packadd` a transaction. Return immediately for `loaded`; prevent
   recursion for `loading`; run `lua_before`, `packadd`, `lua_after`, dependency
   and on-source order exactly as today; mark `loaded` only after the successful
   boundary defined by reference tests. On error, restore a retryable state and
   preserve the original traceback.
2. Register trigger cleanup callbacks/reverse records at startup. After a
   package loads by any trigger, remove that package from event, ft, require,
   command, function, mapping, and source registrations. Remove an actual stub
   only when no unloaded package still needs its key.
3. Event: remove the package ID from shared `event2pkgid`; delete the one-shot
   loader when the event has no remaining IDs. Preserve shared-event loading
   order and the existing autocmd replay algorithm.
4. Command/function: delete a dummy only when all packages registered for it
   are loaded or immediately before delegating to the real definition. A plugin
   loaded through another trigger must not leave a dummy that shadows the real
   command/function.
5. Filetype: reconcile `processed` and loaded state centrally so another
   trigger removes the ID without sourcing its ftplugin later. Retain the
   current at-most-once ftplugin rule.
6. Require: reuse the existing root counters and searcher retirement. Feed
   central `on_loaded(id)` into reconciliation instead of a separate callback
   scan. Unknown modules still allocate/store no per-module state.
7. Mapping: remove only the mappings whose remaining ID set became empty. Do
   not scan all plugin IDs or all patterns.
8. On-source/dependency cascades use an iterative work queue with a visited/
   state check to avoid deep Lua recursion while preserving deterministic
   before/packadd/after ordering. Prove equivalence before changing this part;
   keep recursion if the queue changes hook semantics.

### Acceptance

- Each package transitions to loaded once. Recursive, simultaneous-trigger,
  hook-error, packadd-error, and retry fixtures have explicit expected state.
- Loading through any trigger removes/reduces every other registration in time
  proportional to that package's reverse records, not all registrations.
- No stale dummy command/function/mapping shadows the plugin's real object.
- Shared trigger keys remain until their final unloaded package is handled.
- After all packages are loaded, rsplug-owned searcher/autocmd observers,
  commands/functions, and mappings that have no remaining work are absent.
- Generated output is deterministic and movable. The runtime ABI/schema is
  bumped if generated layout changes.

## L2: remove remaining per-trigger avoidable work

### Mapping hot path

1. Make `id_patterns[id]` a pattern-membership set. Keep `pattern_ids[pattern]`
   as a deterministic load-order array plus a membership set; do not replace
   configured order with hash or BTree order accidentally.
2. Make `remove_pattern(pattern)` visit only `pattern_ids[pattern]`, delete that
   reverse edge, and delete an empty `id_patterns[id]`. Remove the current
   `pairs(id_patterns)` full scan and every array-scanning `add_unique`.
3. Stable-deduplicate `(mode, pattern)` package IDs in Rust before rendering.
   Render mode modules as ordered record arrays and iterate them with `ipairs`,
   not nondeterministic `pairs`.
4. Cache the core runtime module in a local; `all_loaded` must not call
   `require('_rsplug')` once per probe.
5. Precompute `nvim_replace_termcodes(pattern, ...)` while installing the stub
   and capture it in the callback; the first user keypress must not recompute
   termcodes.
6. Replace the nested `startswith` closure and per-call result allocation in
   `parse_mode` with direct prefix tests plus shared constant result arrays or
   direct pending-mode tests. Do not load a mode module for an unrelated mode,
   and retain observer retirement.

Acceptance: deleting K related patterns visits only their modes/IDs; operation
counts are independent of unrelated plugin count. Existing duplicate-pattern,
special-key replay, reachable-mode, and 10k unrelated-mode tests pass. Rendered
code has no `pairs(id_patterns)`, linear `add_unique`, `pairs` over mode records,
callback-time `nvim_replace_termcodes`, or nested function in `parse_mode`.

### Filetype/manifest hot path

1. S3 generation metadata is trusted only after one load-time schema/path
   validation. Validate relative path, `..`, package membership, and ft ordering
   once, then cache generation-root-relative or absolute resolved lists.
2. Replace `vim.list_contains(result, full)` linear dedup with a set or remove it
   when the Rust plan proves paths unique. Keep stable output order.
3. Hoist `entries['opt/' .. id]` and `prefix = 'opt/' .. id .. '/'` outside the
   per-path loop. If the package is not a generation entry, do not visit its
   paths. Use `result[#result + 1]` on the append path.
4. Cache `manifest`, `gen_root`, and `entries` without repeated
   `debug.getinfo`, `fnamemodify`, `readfile`, or JSON decode.

Acceptance: first use reads/decodes metadata once; later filetypes do no path
component revalidation for already validated records; one ft trigger performs
zero `nvim_get_runtime_file` calls and sources each indexed path once. Valid
1k/2k/4k indices perform exactly that many path visits, contain no
`vim.list_contains`, and scale linearly.

### Event hot path

1. Capture the already-required `on_event` module in the setup callback instead
   of requiring it again on every event.
2. Before taking autocmd snapshots, filter the event's IDs against central
   loaded state. If no unloaded ID remains, retire the loader and return with
   zero `nvim_get_autocmds` calls. If another unloaded package shares the event,
   keep the loader and load only the remaining IDs.
3. Generate the reverse package-to-event keys and retain the actual autocmd ID
   per logical key, including the original match key for unknown events mapped
   onto `User`. A load through any other trigger can then retire an otherwise
   stale event loader before it incurs the two O(A) queries.
4. Assert the real Neovim autocmd record shape. Exclude rsplug ownership by the
   numeric `group` ID (and use `group_name` only for diagnostics); do not rely
   on synthetic fixtures where `group` is a string.
5. For ft replay, deduplicate by `(event, group-id)`, not group alone. A newly
   created group that registers both `Syntax` and `BufEnter` must replay both
   exactly once.
6. Reuse immutable event lists/excluded-group sets where safe. Avoid
   `vim.tbl_deep_extend` for the fixed replay options; construct the small table
   directly without altering `User`/buffer/data behavior.
7. Fold after-record filtering and new-group collection into one pass; do not
   allocate a full `discovered` record array and scan it again only to extract
   groups.
8. Keep exactly one before and one after `nvim_get_autocmds` query when there is
   actual work. Any proposal
   to remove those queries must first prove equivalent discovery of plugin
   autocmds, including existing groups and groupless definitions.

Acceptance: structural gate is zero autocmd queries for an empty/stale trigger
and exactly two for actual work; module-cache lookup, full discovered-array,
and deep-merge counts are zero in the callback; one new group spanning three
events replays each event once; all R2 replay/workaround tests pass.

### Require hot path

1. Keep the searcher before the normal Lua filesystem searcher so a plugin can
   extend paths. Benchmark inserting after preload versus the current first
   position and preserve preload semantics.
2. Replace Lua pattern root extraction with a lower-overhead byte/string scan
   only if the benchmark demonstrates a win for ASCII and non-ASCII module
   names. Do not add state for unrelated modules.
3. Cache `next(remaining_roots) == nil` as a remaining-root count if that avoids
   repeated traversal; update it only in the existing reconcile path.

Acceptance: 10k unrelated requires leave pending/in-progress sizes unchanged,
registered modules still load, recursion works, and the searcher disappears
after all roots are satisfied. Same-machine median must improve by at least 20%
to retain a micro-optimization; otherwise revert it for clarity.

### Command/function hot path

1. Cache core/module references and ID lists in closures.
2. For command completion, avoid `nvim_get_commands` over all user commands if
   the real command metadata can be captured directly after load. Preserve
   custom/customlist behavior and command range/bang/args replay exactly.
3. Characterize `args.range`, count, register, modifiers, bang, bar, and args,
   then build the delegated command once. Remove the current normal execution
   followed by an E481-dependent second execution for range commands.
4. Preserve `FuncUndefined` eventignore restoration through errors with a
   single finally-style helper.

Acceptance: first invocation/completion loads once and delegates once; unrelated
user-command count does not affect completion operation count.

### Hook module and generated-code cost

1. Today every `lua_before`/`lua_after` string becomes a separate generated Lua
   module and `packadd` performs one `require` per script. Generate at most one
   lazy hook module per package, returning ordered before/after function arrays.
   Keep it lazy so startup does not compile hooks for never-loaded packages.
2. Wrap each configured script in its own function so a script-local `return`
   cannot suppress later scripts. Characterize errors, nested packadd, and
   tracebacks before replacing the representation.
3. Require the hook module at most once per package and execute before -> real
   packadd -> after -> on-source in the reference order.
4. Benchmark string-form `vim.cmd('packadd ...')` against the structured Neovim
   command API on the supported baseline. Adopt structured invocation only if
   faster and behaviorally equivalent for escaped IDs and bang/startup mode.
5. Add a shared Rust `LuaStringLiteral` renderer and use it for every generated
   event, command, ft, function, module, mapping, source name, and package ID.
   Reuse it in `lua_build_wrapper`. This is primarily safety/cleanliness, but it
   also permits templates to emit direct tables without runtime escaping.

Acceptance: N hooks for one package cause at most one hook-module load; hook
order and error behavior match the reference; quote, backslash, newline, CR,
control-character, UTF-8, and special-key fixtures render loadable Lua;
generated control IDs may change once but identical input remains deterministic.

## C1: code structure cleanup after performance milestones

Do this only after the performance/behavior gates above are green.

1. Split `main.rs::run_load_scheduler` into an explicit `LoadScheduler` with
   methods for parse, catalog, GraphQL, EARLY, promotion, and LATE events.
   State transitions stay in one owner; do not scatter shared maps across
   tasks.
2. Replace boolean combinations (`install`, `update`, `locked`) with a validated
   `RunMode` enum and replace nested optional results with named
   `ResolvedRevision`, `SnapshotJobResult`, and `LoadOutcome` models.
3. Split `plugin.rs` responsibilities into repository catalog/resolution,
   acquisition/materialization, build, inventory, and plugin assembly modules.
   Keep public surface minimal.
4. Split `pack_plan.rs` into pure `PackPlanner`, `MergePlanner`,
   `PackageMaterializer`, `GenerationPublisher`, `GenerationRetainer`, and
   `CopyEngine`. Only the publisher owns the global lock/pointer swap.
5. Remove process-global mutable caches/strategies where run-scoped ownership
   is sufficient. Make resource budgets injectable in tests.
6. Centralize atomic-file publication and owned-staging guards. Every caller
   must state whether the artifact is durable state, best-effort cache, or
   disposable staging; only durable state gets fsync.
7. Keep persisted structs separate from indexed runtime structs. Serde layout
   changes must not accidentally change in-memory algorithm choices.
8. Document complexity next to public planner/catalog operations and assert the
   corresponding structural counter in tests.
9. Pass the already-read GitHub token through `LoadCtx`; do not read/leak the
   environment once per plugin. Replace synchronous `Path::exists/is_dir/
   is_symlink` calls made on async scheduler paths with already-known catalog/
   inventory facts or explicit async/blocking boundaries.

Acceptance: no output or counter regression relative to the optimized commit
immediately before cleanup. Run identical randomized/reference and end-to-end
fixtures before and after each extraction.

## D1: update user-facing documentation last

Run this only after all implementation milestones, performance reports, schema
versions, and CLI-visible behavior are final. Documentation must describe the
shipped behavior, not an intermediate implementation idea.

### Target files

- `README.md`;
- `crates/rsplug/templates/doc/rsplug.txt` (generated `:help rsplug` text);
- CLI help/comments in `crates/rsplug/src/main.rs` if a user-visible option,
  default, error, or output behavior changed;
- `example.toml` when a documented configuration example should demonstrate a
  changed user-visible behavior;
- Nix-facing documentation/config comments only if output layout, lockfile, or
  reproducibility behavior changed.

### Implementation steps

1. Build a documentation fact sheet from final tests and benchmark reports:
   command semantics, cache/snapshot layout, generation publication behavior,
   lockfile behavior, lazy-load guarantees, compatibility/schema changes, and
   observable performance claims.
2. Compare every README command and statement against the final CLI help and
   isolated end-to-end fixtures. Correct stale flags, paths, defaults, and
   statements about what does or does not contact remotes.
3. Update `rsplug.txt` with the same semantics using Vim help conventions:
   tags, headings, examples, cross references, and line wrapping. Do not claim
   an internal optimization as a user guarantee unless it has a stable test.
4. If generation metadata, manifest schema, retention, or no-op behavior
   changed, document migration/compatibility and the observable cache effects.
   State whether old output is accepted, rebuilt, or safely ignored.
5. If new profiling/debug output is intentionally exposed to users, document
   its opt-in interface, output location, non-gating nature, and privacy/data
   boundaries. Do not document test-only hooks as public API.
6. Update examples only when necessary. Validate every TOML example by parsing
   it in a test or isolated command; validate README shell commands against the
   actual argument parser without relying on a user's home directory.
7. Generate a temporary pack from the updated help template and run `:helptags`
   plus a headless assertion that `:help rsplug` resolves. Verify README links
   and all help tags/references point to existing targets.
8. Add a concise release-note entry only if the project has a release-note
   convention by that time. Do not create a new changelog format solely for
   this work.

### Acceptance

- README, `--help`, and `:help rsplug` agree on every user-visible command,
  flag, cache/lock location, and lazy-loading guarantee they mention.
- All documented examples parse and their documented network/lock behavior is
  covered by an automated fixture.
- Help tags resolve in a generated pack; no broken README relative link or Vim
  help cross reference remains.
- Performance wording is backed by the final same-machine report and qualified
  as workload-dependent; structural guarantees are stated only when tested.
- The documentation diff contains no machine-specific absolute path, token,
  benchmark fixture path, or unshipped implementation detail.

## Validation matrix

Run focused tests after each substep, then all gates after each milestone:

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

Never run `cargo check -q`.

Required end-to-end scenarios, each in isolated cache/config directories:

| scenario | network | expected mutation |
| --- | --- | --- |
| cold `--install` | local mock allowed | missing snapshots + one generation + lock |
| warm identical `--install` | only resolution if required | none after semantic equality |
| no-change `--update` | resolution only | none |
| 5%-changed `--update` | changed acquisition only | new snapshots + one generation + lock |
| update with missing repo | none for missing repo | warning only; no install |
| flagless refresh, same config | none | none |
| flagless one-config change | none | affected control/generation only |
| `--locked`, warm cache | none | generation only if semantic input differs |
| concurrent identical runs | local mock allowed | one complete winner; loser reuses |
| concurrent different runs | local mock allowed | one complete current generation; both remain valid if retained |

For deterministic verification, compare normalized trees including file bytes,
symlink targets, executable bits where relevant, manifests, loaders, and lock
files. Ignore mtimes only in byte comparison; separately assert that no-op
mtimes do not change.

For runtime validation, render the real Sailfish templates and run:

    nvim --headless --clean -i NONE -n -c 'luafile <fixture>' -c 'qa!'

Isolate XDG directories. Missing Neovim is a clear test failure. Use Neowright
only when an interactive/TUI failure cannot be explained headlessly. If it is
needed, use a named session, inspect with `wait`/`eval`/`snapshot`, and close the
session afterward.

## Completion checklist

- [ ] M0 reports and structural counters exist.
- [ ] U1 valid snapshot lookup performs no history scan.
- [ ] U2 adaptive samples, deduplication, bounded fallback, and update change
      selection pass.
- [ ] I1 duplicate repository work is coalesced and acquisition queues are
      bounded.
- [ ] S1 uses one inventory traversal and indexed queries.
- [ ] S2 merge is filesystem-free and avoids repeated full hashes.
- [ ] S3 identical generation/lock publication is a zero-mutation fast path.
- [ ] S4 lock window and copy/GC queues satisfy their bounds.
- [ ] L1 cross-trigger retirement and transactional package state pass.
- [ ] L2 mapping/ft/event/require/command/function structural gates pass.
- [ ] C1 cleanup is performance-neutral relative to the optimized baseline.
- [ ] D1 README, Vim help, examples, and any affected CLI/Nix documentation
      match the final behavior and pass their documentation checks.
- [ ] All validation commands and isolated end-to-end scenarios pass.
- [ ] Reports contain before/after medians and p95 for every four-phase fixture,
      with structural counters explaining the improvement.

## Progress

Planning and baseline inspection are complete. M0 runtime measurement work is
implemented: the Lua harness now computes a sorted-sample median and p95,
records the requested scale/iteration fields and deterministic API counters,
and compares the active rsplug require searcher with an otherwise identical
temporarily removed-searcher control. The ignored benchmark produced
`target/runtime_hot_paths_bench.json` on 2026-07-22. The update, install, and
snapshot-refresh reports and structural counter infrastructure remain to be
implemented. A first scoped L2 filetype hot-path change is now implemented:
the valid v2 manifest resolver uses a reverse path set for stable de-duplication
and hoists package-entry checks outside its path loop. The broader L2 milestone
and U1/U2/I1/S1/S2/S3/S4/L1/C1/D1 remain incomplete. A user-prioritized S3
slice is implemented: `--install` checks the local snapshot catalog before
GraphQL and only resolves missing repositories; an unchanged, intact
generation skips package copy, manifest/loader/init publication, GC, and an
equivalent v2 lockfile write. Inventory-derived ft indices and the strict
zero-scan generation plan remain future S3 work.

## Discoveries and decision log

- 2026-07-22: Defined “snapshot update” as snapshot-to-generation refresh
  because the CLI has no snapshot operation. Repository snapshot creation is a
  shared install/update layer.
- 2026-07-22: Existing R0-R5 runtime plan is fully implemented and was removed
  from the active plan rather than duplicated.
- 2026-07-22: Chose structural counters as CI gates and same-machine wall/RSS as
  evidence. Network benchmarks use local deterministic fixtures.
- 2026-07-22: Prioritized making existing mechanisms effective (adaptive
  permit completion, latest index reads, indexed manifests) before adding more
  concurrency.
- 2026-07-22: A content/generation identity mismatch across install, flagless,
  and locked modes is treated as a performance bug because it defeats warm
  package reuse, even if the resulting file bytes happen to match.
- 2026-07-22: The first bounded implementation slice after M0 targets the
  valid-manifest filetype resolver: a set replaces linear result membership
  checks while preserving manifest order and path validation.
- 2026-07-22: Warm `--install` favors a catalog probe over unconditional
  GraphQL. A generation is a no-op only after validating package directories,
  manifest, loader, and init symlink; any inconsistency falls back to repair.
