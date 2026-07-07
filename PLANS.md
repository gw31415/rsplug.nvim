# Lock file / cache directory synchronization ExecPlan

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document follows the repository-level `PLANS.md` convention described in `AGENTS.md` (ExecPlans section).


## Purpose / Big Picture

rsplug.nvim stores fetched plugin snapshots on disk under `~/.cache/rsplug/repos/<repo>/worktrees/<snapshot_key>/`. It also maintains a JSON lock file (`rsplug.lock.json`) that records the resolved commit hash (`rev`) for each repository URL.

The problem: the lock file is only written as a side-effect of `--install` / `--update` / `--locked` runs. If the user skips lock-file updates for a while, the lock file drifts out of sync with what is actually on disk:

1. A plugin may exist in `repos/` (installed) but be absent from the lock file, or have a stale `rev` that no longer matches any snapshot on disk.
2. A plugin may be listed in the lock file but its cache directory was deleted (manually or by a failed cleanup), so the locked `rev` points to nothing.
3. Stale snapshot directories from old commits accumulate in `worktrees/` and are never garbage-collected.

After this plan is fully implemented, the user will observe:

- Running rsplug without `--install`/`--update`/`--locked` (the default "load from cache" mode) always writes a lock file whose `rev` values reflect the snapshot directories actually present on disk — not the last-fetched values.
- Snapshot directories that do not correspond to the lock file's `rev` are garbage-collected (with `--gc`) or at least identified.
- The lock file never claims a revision that has no corresponding on-disk snapshot.

The key concern raised by the user: **the `repos/` directory is not immutable**. Snapshot worktrees can be mutated by build scripts, `lua_post_update`, or external tools, and `source.git` object stores are append-only bare repos. This plan must not assume immutability where it does not hold, and must explicitly separate the parts that are safe to implement now from the parts that depend on making the cache more robust.


## Progress

- [x] (2025-07-07) Analyzed current lock-file write path (`main.rs` lines 143-234) and cache layout (`plugin.rs` `latest_snapshot_oid`, `packpathstate.rs` `RepoSnapshotIdentity`).
- [x] (2025-07-07) Identified the three drift scenarios (lock missing entries, lock stale rev, orphaned snapshots).
- [x] (2025-07-07) Classified tasks into "safe now" (depends only on reading existing on-disk state) and "blocked on immutability" (assumes snapshots are never mutated post-creation).
- [x] (2026-07-07) Phase 1: Reconstruct lock file from on-disk snapshots during default (non-install/update/locked) runs. `locked_map` を常にロックファイルから初期化（NotFound は空マップ）。`Plugin::load` の結果から「repo 有りかつ `Ok(None)` = 未インストール」の URL を `urls_to_remove` に集めて lock から削除 → `lock_infos` を overlay。
- [x] (2026-07-07) Phase 2: Add `--gc` flag to remove orphaned snapshot directories not referenced by the lock file. URL→cachedir 正方向変換 (`rsplug::plugin::cachedir_from_url`) で lock をディレクトリパスと照合。`main.rs:gc_tests` で Acceptance 4/5 を検証済み。
- [ ] Phase 3 (deferred): Immutable snapshot design — make snapshot worktrees read-only after creation so GC and lock reconstruction can trust the on-disk commit hash.


## Surprises & Discoveries

- Observation: The lock file is written only when `!locked` (main.rs:216). On a default run (no flags), `locked` is `false`, so the lock file IS written — but only with `rev` values from `lock_infos`, which are populated only by `Plugin::load` when it actually fetches (install/update). On a default run with no fetch, `lock_infos` is empty for already-installed plugins, so the lock file is rewritten with whatever was in the pre-existing lock file (merged into `locked_map`). This means **the default run does not add missing entries or fix stale revs** — it just round-trips the old data.
  Evidence: `main.rs:97-107` (reads lock file into `locked_map` only when `--locked`), `main.rs:216-234` (writes lock file when `!locked`), `main.rs:197-212` (lock_infos collected from load results).

- Observation: `Plugin::load` returns `lock_info = Some((url, head_rev_str))` only when it reaches the end of the fetch/materialize path (plugin.rs:602). When the plugin is already installed and loaded from cache (the `Some(existing) => existing` branch at plugin.rs:395), it still returns `lock_info` because `head_rev_str` is set from the existing snapshot's directory name. So the data IS available — but `main.rs` only merges `lock_infos` into `locked_map`, and when `--locked` is not set, `locked_map` starts empty (not from the file). So **the default run does write the lock file from lock_infos, but only for plugins present in the config** — plugins removed from config but still cached will be dropped from the lock file.
  Evidence: `main.rs:105-107` (`BTreeMap::new()` when not `--locked`), `plugin.rs:602` (`lock_info` always returned on success).

- Observation: The snapshot directory name encodes the commit hash: `<40-hex-commit>` or `<40-hex-commit>__v1_<hash>` (packpathstate.rs:68-85). `latest_snapshot_oid` (plugin.rs:928-939) parses this prefix. So **the on-disk commit is recoverable from the directory name without opening the git repo**.

- Observation: `latest_snapshot_dir` picks the newest snapshot by mtime. If multiple snapshots exist (e.g. before and after a build change), only the newest is reflected. The older ones become orphans.

- Observation: Snapshot worktrees ARE mutable. Build scripts (`build`, `lua_build`) run inside the worktree and produce uncommitted changes (plugin.rs:461-527). The `.rsplug_build_success` marker file records the build identity (plugin.rs:546-552). `is_dirty()` and `diff_hash()` exist to capture this, but the worktree itself is a standard git working tree that any external process could modify.
  Evidence: `plugin.rs:461-527`, `util.rs:176-211` (`diff_hash`, `is_dirty`), `packpathstate.rs:35-36` (`dirty_diff` field).

- Discovery: The lock file currently records only `{ type: "git", rev: <hash> }`. It does not record build status, dirty diff, or snapshot key. This means the lock file cannot distinguish between a clean checkout and a built-up snapshot at the same commit — but for lock/cache sync purposes, the commit hash is sufficient as the primary key.

- Discovery (2026-07-07): Phase 2 の初版 GC（`repo_url_from_cache_dir`）は dir→URL の逆変換だったが、`walk_repos_for_gc` が絶対パスを渡すため `parts[0]` が `/` になり host 判定が不可能 → lock ルックアップが全件不一致で **GC が何も削除しなかった**。加えて `default_cachedir` は `.git` 末尾を剥がすが `repo.url()`（lock キー）は剥がさないため、`.git` 付き URL でも不一致。正方向変換（URL→cachedir）で統一して解決した。Evidence: `plugin.rs:default_cachedir`, `util.rs:github::url`, `main.rs:garbage_collect`.


## Decision Log

- Decision: Split the work into three phases. Phase 1 (lock reconstruction) and Phase 2 (GC) are safe to implement now because they only read on-disk state. Phase 3 (immutability) is deferred because it requires a design change to how snapshots are created and managed.
  Rationale: The user explicitly asked to proceed with implementation where possible and defer only the parts that depend on the immutability concern.
  Date: 2025-07-07. Author: planning session.

- Decision: Phase 1 changes the default-run behavior so that the lock file always reflects on-disk snapshots. The lock file is written for every run (not just install/update), and `rev` values come from the actual snapshot directory names, not from stale lock file data.
  Rationale: The user's core complaint is that the lock file drifts. The fix is to make the lock file a function of the cache state, not of the fetch history.
  Date: 2025-07-07.

- Decision: For Phase 1, when multiple snapshots exist for a repo, use the same "newest by mtime" heuristic that `latest_snapshot_oid` already uses. This is consistent with the existing load behavior.
  Rationale: Changing the snapshot selection logic is out of scope; we sync what we would actually load.
  Date: 2025-07-07.

- Decision: Phase 2 (`--gc`) is opt-in via a flag, not automatic. Deleting snapshot directories is destructive and should not happen silently during normal operation.
  Rationale: The user may have manually created snapshots or may want to keep old ones. GC must be explicit.
  Date: 2025-07-07.

- Decision: Phase 3 (immutable snapshots) is documented but not implemented in this plan. The concern is valid: until snapshots are immutable, GC and lock reconstruction trust directory names that could theoretically be renamed. However, rsplug itself never renames snapshot directories after creation, so the practical risk is low.
  Rationale: Documenting the concern and the desired direction is better than silently ignoring it.
  Date: 2025-07-07.

- Decision: GC は dir→URL の逆変換ではなく、URL→cachedir の正方向変換（`rsplug::plugin::cachedir_from_url`）で lock と on-disk ディレクトリを照合する。
  Rationale: 逆変換は `.git`/scheme/auth/port の扱いが `default_cachedir` と一致せず脆弱（絶対パス渡りでも破綻）。正方向変換は `RepoSource` を経由するため常に整合する。PLANS.md の「GC ロジックは main.rs」は維持し、変換ヘルパのみライブラリに共有した。
  Date: 2026-07-07.


## Outcomes & Retrospective

- (2026-07-07) Phase 1 実装（`main.rs`）: `locked_map` を常にロックファイルから初期化（`NotFound` は空マップ）。`Plugin::load` の結果から「repo 有りかつ `Ok(None)`（= 未インストール/フェッチ失敗）」の URL を `urls_to_remove` に集め、lock から削除した上で `lock_infos` を overlay。デフォルト実行後に lock の `rev` が on-disk snapshot と一致し、設定にあって未インストールの repo は lock から除去される（Acceptance 1/2/3 設計通り）。
- (2026-07-07) Phase 2 実装（`main.rs` + `plugin.rs`）: `--gc` で orphaned snapshot を削除。初版は dir→URL 逆変換（`repo_url_from_cache_dir`）を使っていたが絶対パス渡りで破綻し GC が機能していなかった。root-cause 修正として URL→cachedir 正方向変換ヘルパ `cachedir_from_url` を `plugin.rs` に追加し、GC は lock を cachedir マップに変換して `repos_dir` からの相対パスで照合。`gc_tests`（orphan 削除 / locked 保持 / build 接尾辞付き / lock 無し repo スキップ / `.git` URL / 空 lock 拒否 / cachedir 変換）で Acceptance 4/5 を担保。
- (2026-07-07) 検証: `cargo check` / `cargo test --workspace`（全パス、`gc_tests` 5件追加）/ `cargo clippy --workspace --all-targets -D warnings`（warning なし）/ `cargo fmt --check` すべて通過。
- Retrospective: GC の逆変換バグはテスト不在により発見が遅れた。Acceptance をテストで先に書く、あるいは GC 実装と同時にテストを追加すべきだった（今回あわせて追加）。Phase 1 の「未インストール vs script-only」区別は `Plugin::load` の戻り値形状（`Ok(None)` は repo 有りかつ未インストールのみ）に依存しており、将来 load の戻り値が変わると壊れうる点に注意。


## Context and Orientation

rsplug.nvim is a Rust workspace. The main binary crate is `crates/rsplug`. The cache directory layout is:

    ~/.cache/rsplug/
    ├── rsplug.lock.json          # lock file (JSON)
    └── repos/
        └── <repo_cachedir>/      # e.g. github.com/owner/repo
            ├── source.git/       # bare object store (GitFetch path only)
            └── worktrees/
                ├── <snapshot_key>/   # fixed checkout at a specific commit
                └── .building-<pid>-<nonce>/  # temporary build worktree

The snapshot key is either `<40-hex-commit>` (no build) or `<40-hex-commit>__v1_<hash>` (with build). The commit hash is always the first `__`-delimited segment.

The lock file format is:

    {
      "version": "1",
      "locked": {
        "<repo-url>": { "type": "git", "rev": "<40-hex-commit>" }
      }
    }

Key terms:

- **Lock file** (`rsplug.lock.json`): records the resolved commit hash for each repository URL. Written by rsplug, read by `--locked` runs.
- **Snapshot**: a fixed checkout of a repository at a specific commit, stored under `worktrees/<snapshot_key>/`.
- **`latest_snapshot_oid`**: function in `plugin.rs` that finds the newest snapshot directory and parses its commit hash from the directory name.
- **`locked_map`**: in-memory `BTreeMap<String, LockedResource>` built in `main.rs`. When `--locked`, it is loaded from the lock file. Otherwise, it starts empty and is populated from `lock_infos` returned by `Plugin::load`.

Key files:

- `crates/rsplug/src/main.rs` — entry point. Lines 97-107 build `locked_map` (from file only if `--locked`). Lines 143-213 load plugins and collect `lock_infos`. Lines 216-234 write the lock file when `!locked`.
- `crates/rsplug/src/rsplug/entities/plugin.rs` — `Plugin::load` (line 281). Returns `Option<(LoadedPlugin, Option<(String, String)>)>` where the second element is `(url, rev)`. `latest_snapshot_oid` (line 928) reads on-disk commit from snapshot dir name.
- `crates/rsplug/src/rsplug/entities/lockfile.rs` — `LockFile` struct, read/write methods.
- `crates/rsplug/src/rsplug/entities/packpathstate.rs` — `RepoSnapshotIdentity` and `snapshot_key()` logic.


## Plan of Work

### Phase 1 — Lock file reconstruction from cache (safe now)

**Goal**: After any rsplug run, the lock file's `rev` values match the snapshot directories actually on disk.

Currently, `main.rs` builds `locked_map` from the lock file only when `--locked`. On default runs, `locked_map` starts empty and is populated from `lock_infos` (the revs that `Plugin::load` resolved). This works for install/update but does not fix drift for already-installed plugins whose lock entry is stale or missing.

The change: always initialize `locked_map` from the existing lock file (if present), then overlay `lock_infos` from `Plugin::load`. This ensures:

1. Plugins already in the lock file but not in the config are preserved (not dropped).
2. Plugins in the config get their `rev` updated from the actual on-disk snapshot.
3. Plugins in the config but not on disk (skipped via `PluginNotInstalled`) are removed from the lock file (their `lock_info` is `None`, and we need to explicitly handle removal).

The key insight: `Plugin::load` already returns `lock_info = Some((url, rev))` when it successfully loads from cache (plugin.rs:602, where `head_rev_str` comes from the existing snapshot). So the data is already flowing — we just need `main.rs` to use it correctly.

**Implementation in `main.rs`**:

1. Always read the lock file into `locked_map` at startup (not just when `--locked`). This gives us the baseline.
2. After collecting `lock_infos` from `Plugin::load`, build the final `locked_map` by:
   - Starting from the file-loaded `locked_map` (preserves entries for plugins not in config).
   - For each plugin in the config: if `lock_info` is `Some`, update the entry; if `None` (plugin not installed / skipped), remove the entry from `locked_map`.
3. Write the lock file for every run (currently gated on `!locked`, which is correct — `--locked` runs should not rewrite the lock file).

To remove entries for not-installed plugins, we need to track which URLs were processed. We can build a set of URLs seen during loading and subtract from `locked_map`.

### Phase 2 — Garbage collection (`--gc` flag) (safe now)

**Goal**: Remove snapshot directories that do not correspond to the lock file's `rev`.

Add a `--gc` CLI flag. When invoked, rsplug:

1. Reads the lock file.
2. For each repo in `repos/`, scans `worktrees/` for snapshot directories.
3. Keeps only the snapshot whose commit matches the lock file's `rev` (the `<commit>` prefix of the snapshot key). Removes all others.
4. Optionally removes `source.git` object stores that have no remaining snapshots.

This is safe because it only deletes directories, and only when the user explicitly asks.

**Note on immutability concern**: GC trusts that the snapshot directory name accurately reflects its commit. Since rsplug creates these directories via `git2::Repository::clone` + `set_head_detached`, and never renames them, the name is reliable in practice. A corrupted or externally-renamed directory would be GC'd or kept incorrectly, but this is an edge case the user accepts by running `--gc`.

### Phase 3 — Immutable snapshot design (deferred)

**Goal**: Make snapshot worktrees read-only after creation so that GC and lock reconstruction can fully trust on-disk state.

This is a design-level change that affects:
- How `build` / `lua_build` / `lua_post_update` execute (they need write access during build, then the tree is frozen).
- How `source.git` relates to snapshots (currently shared via hardlinks for GitFetch path).
- Whether the `.rsplug_build_success` marker and dirty diff should be part of the immutable record.

This phase is documented for future work. It is not implemented now because:
- The user asked to defer parts that depend on the immutability concern.
- It requires significant changes to the build pipeline and snapshot lifecycle.


## Concrete Steps

Unless noted, all commands run from the repository root (`/Users/ama/.herdr/worktrees/rsplug.nvim/lockfornix`).

**Step 1 — Always read lock file at startup (Phase 1a).**

Edit `crates/rsplug/src/main.rs`: change the `locked_map` initialization so it always reads the lock file (not just when `--locked`). The `--locked` flag still controls whether the lock file is used to pin revisions, but the file is always read as the baseline for the output lock file.

Expected: `cargo build -p rsplug` succeeds. No behavior change visible yet.

**Step 2 — Track processed URLs and sync lock file (Phase 1b).**

In `main.rs`, after the plugin loading loop, build the final lock file:

1. Collect a `HashSet<String>` of URLs that were processed (from the config's plugins).
2. For each URL in the config: if `Plugin::load` returned `lock_info`, update `locked_map`; if it returned `None` (not installed), remove the URL from `locked_map`.
3. Write the lock file (when `!locked`).

The `Plugin::load` return value currently cannot distinguish "not installed" from "script-only plugin" (both return `Ok(None)` without lock_info). We need to ensure that for plugins with a `repo` field that returned `None`, the URL is known so we can remove it. Since we have the URL in the closure (main.rs:152), we can collect processed URLs there.

Expected: After a default run, the lock file matches the cache state.

**Step 3 — Add `--gc` flag (Phase 2).**

Add `gc: bool` to `Args` in `main.rs`. When `--gc` is set:

1. Read the lock file.
2. Walk `repos/` directory.
3. For each repo dir, walk `worktrees/`.
4. For each snapshot dir, parse the commit from the directory name.
5. If the commit does not match the lock file's `rev` for this repo's URL, remove the directory.
6. If `worktrees/` is empty after cleanup, optionally remove the repo dir and `source.git`.

Expected: `cargo build -p rsplug` succeeds. `rsplug --gc --config ...` removes orphaned snapshots.

**Step 4 — Lint and test (after every step).**

    cargo fmt
    cargo test --workspace
    cargo clippy --workspace --all-targets

Note: `AGENTS.md` says do not run `cargo check -q`.


## Validation and Acceptance

Behavior-based criteria:

1. After a default run (no flags), the lock file's `rev` for each installed plugin matches the commit hash in the newest snapshot directory name under `repos/<repo>/worktrees/`. Verified by comparing `jq .locked <lockfile>` with `ls repos/<repo>/worktrees/`.

2. A plugin removed from the config but still cached: its entry is preserved in the lock file after a default run (not dropped). Verified by checking the lock file before and after.

3. A plugin in the config but not installed (cache deleted): its entry is removed from the lock file after a default run. Verified by checking the lock file.

4. `--gc` removes snapshot directories whose commit does not match the lock file. Verified by creating a stale snapshot dir manually, running `--gc`, and confirming it is removed.

5. `--gc` does not remove the snapshot matching the lock file's `rev`. Verified by checking the matching snapshot survives.

6. `cargo test --workspace` passes, including existing lock file and snapshot tests.

7. `cargo clippy --workspace --all-targets` produces no new warnings.

Manual verification commands:

    # Check lock file matches cache
    jq .locked ~/.cache/rsplug/rsplug.lock.json | head
    ls ~/.cache/rsplug/repos/github.com/*/worktrees/

    # Test GC
    # (create a fake stale snapshot)
    mkdir -p ~/.cache/rsplug/repos/github.com/test/fake/worktrees/0000000000000000000000000000000000000000
    rsplug --gc --config ...
    ls ~/.cache/rsplug/repos/github.com/test/fake/worktrees/  # fake dir should be gone


## Idempotence and Recovery

- Phase 1 (lock reconstruction) is idempotent: running it multiple times produces the same lock file as long as the cache state is unchanged.
- If the lock file is deleted, a default run recreates it from the cache state. No data loss.
- If the cache is deleted, the lock file entries for those repos are removed on the next run (Phase 1b removes entries for not-installed plugins).
- Phase 2 (`--gc`) is destructive but opt-in. Recovery: re-run `--install` to re-fetch deleted snapshots. Clearing the cache (`rm -rf ~/.cache/rsplug/repos`) is always safe.
- `--gc` should never remove the snapshot matching the lock file's `rev`. If the lock file is empty or missing, `--gc` should warn and refuse to delete anything (to avoid wiping the entire cache).


## Artifacts and Notes

Current lock file write logic (main.rs:216-234):

    if !locked {
        let mut merged_locked = Arc::try_unwrap(locked_map).expect("...");
        for (url, resolved_rev) in lock_infos {
            merged_locked.insert(url, LockedResource { ... });
        }
        LockFile { version: "1".into(), locked: merged_locked }.write(...).await?;
    }

Problem: `merged_locked` starts from `locked_map`, which is empty on non-`--locked` runs. So entries for plugins not in the config are lost. And entries for plugins in the config but not installed are preserved (stale).

After Phase 1:

    if !locked {
        let mut merged_locked = Arc::try_unwrap(locked_map).expect("...");
        // Remove entries for not-installed plugins in the config
        for url in processed_urls_with_no_lock_info {
            merged_locked.remove(&url);
        }
        // Update entries from actual load results
        for (url, resolved_rev) in lock_infos {
            merged_locked.insert(url, LockedResource { ... });
        }
        LockFile { version: "1".into(), locked: merged_locked }.write(...).await?;
    }


## Interfaces and Dependencies

Interfaces that must exist after this plan:

- `main.rs` — `locked_map` is always initialized from the lock file (if present), regardless of `--locked`. The `--locked` flag still controls rev pinning, not file reading.
- `main.rs` — new logic to track processed URLs and remove stale entries from the lock file.
- `main.rs` — new `--gc` CLI flag and GC logic.
- `LockFile` struct — no changes needed (existing read/write is sufficient).
- `Plugin::load` — no changes needed (already returns correct `lock_info` for cache-loaded plugins).

No changes to the lock file format, Lua runtime integration, or the cache directory layout (Phase 1-2). Phase 3 (deferred) would change the snapshot creation path.


## Revision Notes

- 2025-07-07: Initial ExecPlan created. Phase 1 (lock reconstruction) and Phase 2 (GC) are scoped as safe to implement now. Phase 3 (immutable snapshots) is documented but deferred per the user's instruction to separate concerns that depend on cache immutability.
- 2026-07-07: Phase 1 / Phase 2 を実装完了。Phase 2 の初版 GC は逆変換バグで機能していなかったため、URL→cachedir 正方向変換で根本修正し `gc_tests` を追加。Phase 3（不変 snapshot）は引き続き defer。
