# rsplug の cache / generation / workspace 設計変更案

This ExecPlan is a living document. It must be kept in sync with the guidance in
`PLANS.md` itself as work proceeds: update `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` whenever the
implementation state changes. This file intentionally preserves the full
original update plan below, without removing technical content from `PLAN.md`.

## Purpose / Big Picture

The goal is to make rsplug's repository cache stable across updates while
preserving the current generation and `_gen` output model. After this change,
`to_sym` plugins should no longer point at a mutable live checkout that can move
when a later `--update` or `--locked` run checks out another revision. Instead,
generated entries should point at fixed repository snapshot worktrees under
`repos/<repo>/worktrees/<snapshot_key>/`.

A maintainer can see the change working by installing a plugin with `to_sym`,
recording the `_gen/opt/<plugin_id>` symlink target, running an update that
resolves a different commit, and confirming that the old generation still points
at the original snapshot while the new generation points at a new snapshot.
The existing `generations/`, root `init.lua`, `pack/_gen/`, control plugin
`manifest.json`, TOML schema, and JSON lock file format must remain compatible.

## Progress

- [x] (2026-07-03 10:02Z) Captured the existing `PLAN.md` design in this
  `PLANS.md` file without deleting technical content.
- [x] (2026-07-03 10:02Z) Converted `PLANS.md` into an ExecPlan-style living
  document by adding purpose, progress, discoveries, decision log,
  implementation plan, concrete steps, validation, recovery, artifacts, and
  interface sections.
- [x] (2026-07-03) Phase-1 milestone: identity / hash 安全化 + 配置/identity 分離
  (PLANS §15.1). `RepoSnapshotIdentity` / `RepoFileIdentity` / `FileIdentity` を導入し、
  `HowToPlaceFiles::SymlinkDirectory(Arc<Path>)` → `RepoSnapshotLink { target, identity }`
  （`target` を hash/eq から除外）、`FileItem.identity` 追加、`LoadedPlugin.repo_meta` 削除。
  絶対パス不変・merge 全 repo 反映の unit test を追加。`cargo fmt` / `cargo clippy
  --workspace --all-targets -- -D warnings` / `cargo test` 全て通過。
- [ ] Introduce repository source and snapshot worktree paths under
- [x] (2026-07-03) Phase-2 Step A+B: snapshot infra landed (not yet wired into
  `Plugin::load`). Path helpers (`repo_root`/`source_git_dir`/`worktrees_dir`/
  `snapshot_root`) and `RepoSnapshotIdentity::snapshot_key()` (§15.2, §15.3) +
  unit tests. `util::git` source/worktree split (§15.4): bare `source.git`
  (`init_source`/`open_source`/`has_object`/`fetch_oid`) and snapshot worktree
  (`init_snapshot` = local clone + detached checkout, validated by a git2
  integration test). `LoadedPlugin::snapshot_root()` and `Plugin.depth` added
  for the upcoming DAG-ordered load. Items are `#[allow(dead_code)]` until the
  load rewrite consumes them.
- [ ] Introduce repository source and snapshot worktree paths under
  `repos/<repo>/source.git` and `repos/<repo>/worktrees/<snapshot_key>/`.
- [ ] Move build, file scan, copy, symlink, `lua_build`, and `lua_post_update`
  operations to the resolved snapshot worktree.
- [ ] Update dependency runtimepath handling to use resolved snapshot paths
  instead of repository cache-relative paths.
- [ ] Add unit and integration tests that prove snapshot stability, path
  independence, merge identity correctness, and compatibility with existing
  generation output.
- [ ] Run formatting, tests, and relevant lint checks before considering the
  implementation complete.

## Surprises & Discoveries

- Observation: The original plan is already detailed and includes scope,
  migration, risks, implementation order, and completion criteria, but it did
  not yet use the living-document sections expected by OpenAI's ExecPlan
  guidance.
  Evidence: `PLAN.md` has 811 lines and sections 1 through 22, while the
  ExecPlan-required sections were added here above the preserved design body.

- Observation: The plan identifies an important identity risk in the current
  design: `HowToPlaceFiles::SymlinkDirectory(Arc<Path>)` derives `Hash`, so a
  placement path can affect plugin identity unless logical identity is separated
  from runtime placement.
  Evidence: See the preserved sections `2.3`, `10`, `12`, and `18.2` below.

## Decision Log

- Decision: Preserve the full original `PLAN.md` content below the living
  ExecPlan sections instead of summarizing it.
  Rationale: The user explicitly requested that content not be reduced. Keeping
  the detailed body intact prevents loss of edge cases, risks, and implementation
  ordering while still making the file follow the ExecPlan workflow.
  Date/Author: 2026-07-03 / Codex.

- Decision: Treat `generations/`, root `init.lua`, `pack/_gen/`, control plugin
  `manifest.json`, TOML configuration schema, and `rsplug.lock.json` format as
  compatibility boundaries for the initial implementation.
  Rationale: The preserved design repeatedly marks those areas as unchanged.
  Keeping them stable narrows the blast radius to repository cache layout,
  snapshot identity, placement identity, build marker location, and dependency
  runtimepath flow.
  Date/Author: 2026-07-03 / Codex.

- Decision: Do not introduce root-level `tmp/`, `locks/`, `logs/`, `gc/`, or
  repository snapshot `meta.json` in the initial scope.
  Rationale: The original plan explicitly defers these additions. Snapshot
  identity should be represented by `snapshot_key`, lock-file reproducibility by
  `rsplug.lock.json`, and generation retention by existing `_gen` manifests.
  Date/Author: 2026-07-03 / Codex.

- Decision (Phase-1, 2026-07-03): Unify `RepoMeta` into `RepoSnapshotIdentity`
  (adds relative `repo_cache_dir`) and **remove the `LoadedPlugin.repo_meta`
  field** entirely, moving logical identity into `FileItem.identity`
  (`RepoFile` / `GeneratedFile`) and `RepoSnapshotLink.identity`.
  Rationale: PLANS §2.2 permits unifying the two types, and per-file/per-link
  identity is the principled carrier. Because `Add for LoadedPlugin` unions the
  file maps, merging two CopyEachFile plugins now preserves **both** repos'
  identities automatically — no separate `repo_metas: BTreeSet` field is needed
  (this both implements and subsumes §12 / §15.1.9). `HowToPlaceFiles` gains a
  custom `Hash`/`PartialEq`/`Eq` that excludes the absolute `target`, mirroring
  the existing `FileSource` precedent. The build-success marker id is now derived
  from `RepoSnapshotIdentity` (superset of the old `RepoMeta` inputs), so the
  marker formula changes once and existing caches rebuild once on upgrade.
  Date/Author: 2026-07-03 / Claude.

## Outcomes & Retrospective

### Phase-1 (identity / hash safety, 2026-07-03) — DONE

Implemented §15.1 as a standalone, compiling, tested milestone. Files changed:
`crates/rsplug/src/rsplug/entities/{packpathstate,plugin,plugctl}.rs`.

- `RepoMeta` removed; replaced by `RepoSnapshotIdentity` (with relative
  `repo_cache_dir`), plus `RepoFileIdentity` and `FileIdentity`.
- `HowToPlaceFiles::SymlinkDirectory(Arc<Path>)` →
  `RepoSnapshotLink { target, identity }` with manual `Hash`/`Eq` that ignore the
  absolute `target`. `FileItem` gained `identity`. `LoadedPlugin.repo_meta` removed.
- `Plugin::load` builds one `RepoSnapshotIdentity` and threads it into the link
  and each CopyEachFile `RepoFileIdentity`; the build marker id uses the same.
- `plugctl.rs` routes all generated files through a `generated_file_item` helper
  (`GeneratedFile` identity from `data_hash`), and `collect_doc_files_from_root`
  carries the snapshot identity for doc extraction.
- New tests (all passing): absolute-path invariance for CopyEachFile and
  RepoSnapshotLink, `repo_cache_dir`/`head_rev` sensitivity, merged-CopyEachFile
  reflects all repos (merge-bug regression), `GeneratedFile` path/data sensitivity.

Validation: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test` all green.

### Remaining (Phase-2 and beyond) — NOT STARTED

§15.2 (path model), §15.3 (snapshot key + rename flow), §15.4 (`util::git`
source/worktree split, **bare** `source.git`), §15.5 (`Plugin::load` split onto
`source.git`/`snapshot_root`), §15.6-15.8 (link/copy/marker point at
`snapshot_root`), §15.9 (dependency runtimepath via **DAG-ordered loading**),
§15.10 (migration fallback), §15.11 (integration tests). User-confirmed
directions (§19): dirty_diff included in `snapshot_key` (rename flow), bare
`source.git`, DAG-ordered dependency runtimepath.

## Context and Orientation

rsplug.nvim is a Rust external binary that reads one or more TOML files plus a
JSON lock file and produces a Neovim `pack` directory. It is not a traditional
Vim or Neovim plugin. The runtime integration is generated Lua, including a
control plugin whose loader lives under `generations/` and whose generated
artifact lives under `pack/_gen/opt/<control_id>/`.

The current design uses `repos/<repo>` as both repository cache and mutable
checkout. This is the root problem: if `_gen/opt/<plugin_id>` is a symlink into
that mutable checkout for a `to_sym` plugin, later repository updates can move
the content seen by an older generation. The desired design splits the cache
into a fetch source, `source.git`, and immutable-or-nearly-immutable snapshot
worktrees, `worktrees/<snapshot_key>/`, and makes `_gen` entries refer to the
snapshot worktree.

Important terms used in this plan:

- `generation`: a generated loader state. The root `init.lua` points to the
  current loader in `generations/`.
- `_gen`: the generated artifact database under `pack/_gen/opt/`.
- `control plugin`: the generated plugin that wires lazy loading, scripts,
  dependencies, and runtime paths.
- `plugin_id`: the `_gen/opt/<plugin_id>` identity derived from
  `LoadedPlugin::plugin_id()`.
- `RepoMeta`: a repository/build identity component. It is not the `_gen` entry
  id by itself.
- `RepoSnapshotIdentity`: the logical identity of a repository snapshot, based
  on source identity, commit, build inputs, and optional dirty diff, not absolute
  filesystem placement.
- `snapshot_key`: the directory name under `worktrees/` that identifies a
  snapshot.
- `snapshot_root`: the concrete filesystem path used to read plugin files from
  the snapshot.
- `to_sym`: a plugin mode that places a symlink in `_gen` instead of copying
  each tracked file into `_gen`.

The detailed design below is preserved from `PLAN.md` and remains normative
unless a later entry in `Decision Log` explicitly changes it.

## Plan of Work

Implement this in the order described by the preserved `15. 実装順` section.
Start with logical identity and hash safety because that de-risks later path
layout changes: no absolute `snapshot_root`, cache root, or symlink target path
may influence `LoadedPlugin::plugin_id()`. Then introduce path helper functions
for `repo_root`, `source_git_dir`, `worktrees_dir`, `snapshot_root`, and hidden
building worktrees. After that, add snapshot key generation and tests.

Next, separate `util::git` concepts into a source repository and runtime
worktree repository. The preferred final shape is a bare `source.git`, but a
staged implementation can use a normal repository internally if the runtime
symlink target never points at a mutable checkout. Once the Git operations are
available, update `Plugin::load()` to stop using one `proj_root` for every
repository role and instead use `repo_root`, `source_git_dir`, and
`snapshot_root`.

After the load path is split, replace the current symlink placement with a
repository snapshot link that carries both `target: snapshot_root` and
`identity: RepoSnapshotIdentity`. Update `CopyEachFile` so every repository file
has identity `RepoSnapshotIdentity + relative_path`. Move build success marker
handling to the snapshot worktree, and update dependency runtimepath handling so
`lua_build` and `lua_post_update` receive already-resolved dependency snapshot
paths. Finally, add migration fallback tests for old checkouts and integration
tests for snapshot stability, generation compatibility, and script-only plugin
behavior.

## Concrete Steps

Run commands from `/Users/ama/rsplug.nvim` unless otherwise stated.

1. Inspect the relevant code before editing:

       rg "LoadedPlugin|RepoMeta|HowToPlaceFiles|SymlinkDirectory|FileItem|FileSource|dependency_cachedirs|rsplug_build_success|default_cachedir|Plugin::load" src tests

2. Add or adjust focused unit tests for identity behavior before changing the
   load path. At minimum, add tests proving that different absolute cache roots
   do not change plugin identity, that repository file identity includes
   relative path, and that merged copied plugins retain all repository identity
   inputs.

3. Implement the identity model described in sections `2`, `10`, `12`, and
   `15.1`. Keep placement paths and logical identity as separate fields.

4. Add path helpers and snapshot key generation as described in sections `7`,
   `15.2`, and `15.3`.

5. Extend `util::git` to support a fetch source repository and snapshot
   worktree repository as described in sections `8`, `9`, and `15.4`.

6. Update `Plugin::load()` so fetch and commit resolution operate on
   `source.git`, while file scanning, build hooks, Lua build/update hooks,
   copies, symlinks, and build marker writes operate on `snapshot_root`.

7. Update dependency runtimepath flow so resolved loaded dependencies pass their
   snapshot paths to dependent build/update hooks.

8. Add integration tests listed in section `15.11`.

9. Format and verify:

       cargo fmt
       cargo test

   Also run the relevant clippy check used for this repository if one exists in
   local project scripts or CI metadata. Do not use `cargo check -q`, per
   `AGENTS.md`.

## Validation and Acceptance

The implementation is accepted when the conditions in the preserved `21.
実装時の完了条件` section are true. The most important observable behavior is
that a `to_sym` plugin's `_gen/opt/<plugin_id>` symlink points at
`repos/<repo>/worktrees/<snapshot_key>` and that an older generation's symlink
target does not move after a later update resolves a different commit.

Validation must include `cargo fmt` and `cargo test`. Tests should also cover:
same lock file reuses the same snapshot, update creates or uses a new snapshot
without mutating old ones, copy-based plugin output remains compatible,
dependency Lua build/update hooks see dependency snapshot runtime paths,
existing lock files still read, and existing `generations/` / `init.lua` /
`_gen` retention behavior remains unchanged.

## Idempotence and Recovery

The new snapshot creation flow should be safe to rerun. If a snapshot already
exists for a final `snapshot_key`, reuse it. If a process fails while creating a
snapshot, the partial work should be confined to a hidden building directory
inside `worktrees/`; a later install or update may remove that hidden directory
and retry. Do not add root-level `tmp/` or `locks/` directories for the initial
implementation.

Existing legacy checkouts under `repos/<repo>` must not be deleted
automatically. If only an old checkout exists, install/update may create the new
layout next to it. A locked run without the needed object in the new layout or
old checkout should fail as a cache-miss style error rather than mutating or
guessing.

## Artifacts and Notes

The complete original design text from `PLAN.md` is preserved below starting at
`## 1. 背景`. Keep concise test transcripts, important diffs, and unexpected
outputs in this section or in `Surprises & Discoveries` as implementation
proceeds. Do not remove the preserved design details unless they are superseded
by an explicit `Decision Log` entry and the replacement preserves the same
technical information.

## Interfaces and Dependencies

The implementation primarily touches Rust code that models loaded plugins,
placement identity, repository sources, Git operations, and build/runtimepath
flow. Expected interface concepts include:

- `RepoSnapshotIdentity`, representing logical repository snapshot identity
  without absolute placement paths.
- `RepoFileIdentity { snapshot, relative_path }`, representing a file copied
  from a repository snapshot.
- `FileIdentity`, distinguishing repository files from generated files.
- `HowToPlaceFiles::RepoSnapshotLink { target, identity }`, replacing or
  specializing the current symlink-directory placement for repository snapshots.
- `SnapshotKeyInput`, a small `#[derive(Hash)]` input type for stable snapshot
  directory names.
- Source/worktree Git operations equivalent to `open_source`, `init_source`,
  `has_object`, `fetch_oid`, `resolve_remote`, `create_worktree`,
  `open_worktree`, `head_hash`, `is_dirty`, `diff_hash`, and `ls_files`.

Use existing repository patterns and Rust types where possible. The names above
are prescriptive about responsibilities, not necessarily exact final symbol
names if a nearby module already has a clearer naming convention.

## Detailed Design Preserved From PLAN.md

## 1. 背景

rsplug は、TOML 設定と lock file から Neovim の pack directory を生成する外部バイナリである。現在の出力と cache は、概ね以下の責務に分かれている。

- `generations/`
  - 現在および直近の generation loader を置く。
  - `generations/<control_id>.lua` は `pack/_gen/opt/<control_id>/` にある生成済み control plugin を `packadd` するための Lua script である。
- `init.lua`
  - 現在の generation loader への入口。
  - control plugin がある通常ケースでは `generations/<control_id>.lua` への symlink になる。
  - control plugin がない zero-plugin ケースでは通常ファイルになる。
- `pack/_gen/`
  - インストール済み plugin と生成済み control plugin の成果物 DB。
  - `pack/_gen/opt/<plugin_id>/` に plugin 単位の成果物を置く。
  - control plugin には `manifest.json` を置き、その generation が参照する `_gen` entry を記録する。
- `repos/`
  - Git repository の cache 置き場。
  - 現在は `repo.default_cachedir()` による repo ごとの固定 root が、そのまま Git checkout 兼 plugin 実体として使われる。

現行実装の主要な流れは次の通り。

1. `RepoSource::default_cachedir()` が `repos/` namespace からの相対パスを返す。
   - GitHub shorthand は `github.com/<owner>/<repo>`。
   - URL source は scheme、userinfo、port、末尾 `.git` を除去して `host/path` にする。
2. `Plugin::load()` が `proj_root = cache_dir.join(repo.default_cachedir())` を作る。
3. `util::git::open(proj_root)` または `util::git::init(proj_root, url)` で repository を開く。
4. `--update` または `--locked` の場合は対象 commit を fetch し、`proj_root` を detached HEAD に checkout する。
5. `build` / `lua_build` がある場合は `proj_root` で実行し、`.rsplug_build_success` に build 成功 marker を記録する。
6. `to_sym` の plugin は `HowToPlaceFiles::SymlinkDirectory(proj_root)` として `_gen` から `proj_root` へ symlink する。
7. `to_sym` でない plugin は `repository.ls_files()` の結果を `CopyEachFile` として `pack/_gen/opt/<plugin_id>/` にコピーする。

この設計では、repo ごとの cache root が可変の checkout でもある。次回の `--update` や別 lock file による `--locked` 実行で同じ `proj_root` が別 commit に checkout されると、過去 generation が参照している `SymlinkDirectory` plugin の実体も動く。

`CopyEachFile` の plugin は `_gen` にコピー済みなので影響が小さい。一方で `SymlinkDirectory` の plugin は `_gen` entry が symlink であり、参照先が live checkout のため、generation loader と `_gen` manifest が保持されていても plugin 実体の revision 固定が弱い。

## 2. 現在の main で追加された前提

この設計案の初稿は plugin identity / hash 周りのリファクタ前に作ったものだが、その後の変更はすでに main に入っている。そのため、実装前に次の前提を明示しておく。

### 2.1 `_gen` entry id は `LoadedPlugin::plugin_id()` である

現在の `_gen/opt/<plugin_id>` の `<plugin_id>` は `RepoMeta` 単体から作られるものではない。`LoadedPlugin` 全体の `Hash` から `LoadedPlugin::plugin_id()` で導出される。

関係は次の通り。

- `PluginID`
  - 128-bit の hash 値。
  - `HasPluginId` trait 経由で、`Hash` 実装を持つ値から導出される。
- `PluginIDStr`
  - `PluginID` の 32 桁 hex 表現。
  - directory 名や Lua template への埋め込みに使う。
- `LoadedPlugin::plugin_id()`
  - `_gen/opt/<plugin_id>` の identity。
  - `LoadedPlugin` 全体の `Hash` によって決まる。
- `RepoMeta`
  - Git repository 由来の identity component。
  - `head_rev`、`dirty_diff`、`build`、`lua_build` を保持する。
  - `_gen` id そのものではなく、`LoadedPlugin` の hash 入力の一部である。
- `.rsplug_build_success`
  - build skip 判定用の marker。
  - 現在は `RepoMeta` を `HasPluginId` 経由で hash した文字列を使っている。
  - `_gen/opt/<plugin_id>` の id と同一である必要はない。

したがって、この文書では以下の用語を使い分ける。

| 用語 | 意味 |
| --- | --- |
| `plugin_id` | `_gen/opt/<plugin_id>` の id。`LoadedPlugin::plugin_id()` で決まる。 |
| `repo_meta_id` | `RepoMeta` 由来の id。build success marker に使える。 |
| `snapshot_key` | `repos/<repo>/worktrees/<snapshot_key>` の directory 名。repo snapshot の identity。 |
| `snapshot_root` | plugin 実体として読む固定 worktree path。 |

### 2.2 placement path ではなく logical content identity を hash する

意味論的には、配置される plugin の identity は「実際に配置される内容」で決まるのが理想である。しかし、`CopyEachFile` で全ファイルの内容 hash を毎回導出すると、plugin 数やファイル数に比例して計算量と I/O が増えすぎる。

そこで今回の設計では、実ファイル全体の content hash ではなく、Git snapshot に対する logical identity を使う。

- Git repository 由来の file は、`RepoSnapshotIdentity` と repo 内 relative path の組み合わせで hash する。
- Git repository 全体を link する場合は、`RepoSnapshotIdentity` だけで hash する。
- machine-local absolute path は identity に含めない。
- `snapshot_root` は placement のための runtime path であり、identity ではない。

`RepoSnapshotIdentity` は、少なくとも次を表す値である。

```rust
#[derive(Hash, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct RepoSnapshotIdentity {
    // source URL / GitHub shorthand の正規化結果。URL 変更を同一 repo とみなすかは実装時に決める。
    repo_cache_dir: PathBuf,
    // lock file に書く 40 桁 commit SHA。
    head_rev: Box<[u8]>,
    // build 後 dirty state を含める場合の diff hash。
    dirty_diff: Option<[u8; 16]>,
    // TOML build command list。
    build: Arc<[String]>,
    // TOML lua_build script。
    lua_build: Option<Arc<str>>,
}
```

`RepoSnapshotIdentity` は `RepoMeta` と同じ入力から作ってよいが、用途を分ける。

- `RepoMeta`: build marker や `LoadedPlugin` の repo identity component。
- `RepoSnapshotIdentity`: placement 対象がどの Git snapshot 由来かを表す identity。
- `SnapshotKeyInput`: `worktrees/<snapshot_key>` の directory 名を作るための hash 入力。

初期実装では `RepoMeta` と `RepoSnapshotIdentity` を同じ struct に統合してもよい。ただし、コード上のコメントでは「absolute path ではなく logical snapshot identity を hash する」ことを明確にする。

### 2.3 `HowToPlaceFiles` は source 種別ごとの logical identity を持つ

現在 `FileSource::Directory` は custom `Hash` 実装で絶対 path を hash に含めない。一方、`HowToPlaceFiles::SymlinkDirectory(Arc<Path>)` は `derive(Hash)` のため、そのままだと symlink target path が `LoadedPlugin::plugin_id()` に混入する。

snapshot worktree 導入後に `SymlinkDirectory(snapshot_root)` を使うと、cache root の絶対 path が異なる環境で `_gen` id が変わり得る。これは rsplug の deterministic / portable な出力モデルと相性が悪い。

この問題は `SymlinkDirectory` だけの特殊問題ではない。`CopyEachFile` でも、`FileSource::Directory` の path を hash しないだけでは「どの repository snapshot のどの relative path か」という identity が `FileItem` 側に明示されない。

推奨する実装は、placement path と logical identity を分けることである。

```rust
#[derive(Hash, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct RepoFileIdentity {
    snapshot: RepoSnapshotIdentity,
    relative_path: PathBuf,
}

#[derive(Hash, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum FileIdentity {
    RepoFile(RepoFileIdentity),
    GeneratedFile { path: PathBuf, data_hash: [u8; 16] },
}

struct FileItem {
    source: Arc<FileSource>,
    identity: FileIdentity,
    merge_type: MergeType,
}

enum HowToPlaceFiles {
    CopyEachFile(BTreeMap<PathBuf, FileItem>),
    RepoSnapshotLink {
        target: Arc<Path>,
        identity: RepoSnapshotIdentity,
    },
}
```

`RepoSnapshotLink` は現在の `SymlinkDirectory` に相当するが、Git repo snapshot への link に限定されるため、`SymlinkDirectory` という名前より意味が明確である。もし将来、任意 directory への symlink を許すなら、その variant は別名で追加する。

この設計により、計算量と正確性のバランスを取れる。

- `CopyEachFile`: `RepoSnapshotIdentity + relative_path` を hash するため、全ファイル内容を読み直さなくてよい。
- `RepoSnapshotLink`: `RepoSnapshotIdentity` を hash するため、link target の absolute path に依存しない。
- generated file: すでにメモリ上に data があるので、`data_hash` を identity に含めてよい。

実装上は `HowToPlaceFiles` / `FileItem` の `derive(Hash)` を維持してもよい。ただし、hash される field は logical identity であり、`Arc<Path>` の absolute path ではないようにする。

### 2.4 hash 入力は `#[derive(Hash)]` する小さな型に集約する

現在の hash utility は `std::hash::Hash` 入力を `xxh3` で digest する形に寄っている。手書きの byte 連結ではなく、identity ごとに小さな `#[derive(Hash)]` 型を作る。

注意点。

- `Vec<T>` は Hash 内で長さも hash する。
- `[T]` slice は長さを hash しない。
- identity を意図通りに固定したい場合は、`Arc<[T]>` / `Vec<T>` / tuple の使い分けを明示する。
- path を入れる場合は、absolute path ではなく、repo 内 relative path や filesystem-safe な logical key にする。

## 3. 目的

この変更の目的は、現行の generation / `_gen` モデルを維持したまま、`repos/` を immutable に近い snapshot cache として整理し、`to_sym` plugin でも generation が参照する plugin 実体を安定させることである。

達成したいことは以下。

1. `generations/` の構成を変えない。
2. `init.lua` を現在 generation への入口にする仕組みを変えない。
3. `_gen` を成果物 DB として維持する。
4. `repos/` は repo ごとの live checkout ではなく、repo source と snapshot worktree を分けて管理する。
5. 同じ lock file / 同じ revision / 同じ build 入力から、同じ snapshot と同じ symlink 参照先が得られるようにする。
6. `_gen` entry id は absolute path に依存しないようにする。
7. repo snapshot link の symlink 先を可変 checkout ではなく固定 snapshot worktree にする。
8. generation ごとの repo directory、root 直下の `tmp/`、`locks/`、`logs/`、`gc/` の新設はしない。
9. repo snapshot 用の `meta.json` は初期設計では追加しない。必要性が明確になった場合だけ後段で検討する。

## 4. 変更しないもの

以下は今回の変更範囲外とする。

- `generations/` の loader 配置。
- root `init.lua` の symlink / zero-plugin fallback の挙動。
- `pack/_gen/opt/<id>/` を成果物 DB とするモデル。
- control plugin 内の `manifest.json`。
- lock file format。`rsplug.lock.json` は引き続き JSON で、repository URL と full commit hash を記録する。
- TOML の設定 schema。
- lazy-loading trigger、`PlugCtl`、merge、dependency co-loading の runtime semantics。
- root 直下の `tmp/`、`locks/`、`logs/`、`gc/` の新しい管理階層。
- repo snapshot 用 `meta.json` の初期追加。

## 5. 新しい directory layout

`repos/` は Git source と snapshot worktree の cache として扱う。

- repo root は `repo.default_cachedir()` の結果を維持する。
- repo root 直下に fetch 対象の `source.git` を置く。
- repo root 直下に snapshot 単位の `worktrees/<snapshot_key>/` を置く。
- plugin の実体として参照するのは `worktrees/<snapshot_key>/` だけにする。

最終形は次の layout とする。

```text
~/.cache/rsplug/
  init.lua                         -> generations/<control_id>.lua
  generations/
    <control_id>.lua
  rsplug.lock.json
  repos/
    github.com/
      owner/
        repo/
          source.git/
          worktrees/
            <snapshot_key>/
              .git                 # git worktree metadata or equivalent
              plugin/
              lua/
              doc/
              .rsplug_build_success
    gitlab.com/
      owner/
        repo/
          source.git/
          worktrees/
            <snapshot_key>/
  pack/
    _gen/
      opt/
        <plugin_id>/                # copied plugin, symlink plugin, or generated control plugin
          manifest.json             # control plugin only
```

`source.git` は fetch 対象であり、plugin runtime の参照先にしない。`worktrees/<snapshot_key>/` は plugin runtime の参照先であり、同じ key なら再利用する。

`repos/` は generation DB ではない。generation の保持と `_gen` cleanup は現行通り `pack/_gen/opt/<control_id>/manifest.json` と `generations/` が担当する。

## 6. data model

実装上は次の値を明示的に分ける。

| 名前 | 内容 | 例 |
| --- | --- | --- |
| `repo_cache_dir` | `repos/` からの repo 相対パス | `github.com/owner/repo` |
| `repo_root` | repo cache の root | `~/.cache/rsplug/repos/github.com/owner/repo` |
| `source_git_dir` | fetch する Git object store | `<repo_root>/source.git` |
| `worktrees_dir` | snapshot worktree の親 directory | `<repo_root>/worktrees` |
| `head_rev` | lock file に書く commit SHA | `40 hex` |
| `dirty_diff` | worktree dirty state の hash | `Option<[u8; 16]>` |
| `build_inputs` | `build` と `lua_build` | TOML 設定由来 |
| `snapshot_key` | worktree directory 名 | `<head_rev>` または `<head_rev>__<input_hash>` |
| `snapshot_root` | plugin 実体として読む worktree | `<repo_root>/worktrees/<snapshot_key>` |
| `repo_meta` | repo/build identity component | `RepoMeta { head_rev, dirty_diff, build, lua_build }` |
| `repo_meta_id` | build marker 用 id | `repo_meta.plugin_id().as_str()` 相当 |
| `plugin_id` | `_gen` entry の id | `LoadedPlugin::plugin_id().as_str()` |

`RepoMeta` は `_gen` entry identity そのものではない。`RepoMeta` は repository content と build 入力を表す identity component であり、`LoadedPlugin` の hash 入力の一部である。

`snapshot_key` は `repos/` 内の worktree identity である。初期実装では `RepoMeta` と同じ入力から作ってよいが、責務は分ける。

## 7. snapshot key

`<snapshot_key>` は filesystem-safe な ASCII 文字列にする。

推奨形式は次の通り。

```text
<head_rev>[__<input_hash>]
```

- `<head_rev>` は 40 桁の commit SHA。
- `<input_hash>` は `dirty_diff`、`build`、`lua_build` を含めた build/snapshot 入力 hash。
- dirty diff も build 入力もない場合は `<head_rev>` のみ。

ただし、build 後 dirty state を snapshot identity に含める場合は、build 実行前には最終 `snapshot_key` が確定しない。その場合は次の flow にする。

1. 対象 commit の clean worktree を一時 worktree として作る。
2. `build` / `lua_build` を一時 worktree で実行する。
3. build 後の `dirty_diff` を計算する。
4. `SnapshotKeyInput` から final `snapshot_key` を作る。
5. 一時 worktree を `worktrees/<snapshot_key>` に rename する。
6. 以後の `ls_files` / copy / symlink / build marker は final `snapshot_root` を使う。

root 直下に `tmp/` は追加しない。必要な一時 directory は `worktrees/.building-<pid>-<nonce>` のように `worktrees/` 内の hidden directory として扱う。

推奨する hash input 型は次のような形である。

```rust
#[derive(Hash)]
struct SnapshotKeyInput<'a> {
    schema: u8,
    head_rev: &'a [u8],
    dirty_diff: Option<[u8; 16]>,
    build: &'a [String],
    lua_build: Option<&'a str>,
}
```

`schema` は必須ではないが、後で snapshot key の意味を変える可能性を考えると入れておく方が安全である。`meta.json` を追加しない方針でも、directory 名に schema component を含めることで将来の migration を単純にできる。

例:

```text
<40hex>
<40hex>__v1_<32hex>
```

初期実装でより短くしたい場合は `v1_` を省略してもよい。ただし、その場合は snapshot key schema の変更時に別の互換策が必要になる。

## 8. Git / snapshot 作成の流れ

`Plugin::load()` の Git 処理は、概念上次の順に変える。

1. `repo_cache_dir = repo.default_cachedir()` を作る。
2. `repo_root = cache_dir.join(repo_cache_dir)` を作る。
3. `source_git_dir = repo_root.join("source.git")` を開く。
4. `source.git` がなければ、`--install` または `--update` 時だけ作る。
5. 対象 commit を決める。
   - `--locked` なら lock file の full commit SHA を使う。
   - `--update` なら remote から `rev` の最新 commit を解決する。
   - それ以外は既存 cache から現在使える commit を読む。
6. `source.git` に対象 commit がなければ fetch する。
7. 対象 commit の worktree を用意する。
8. build 入力と dirty state から `SnapshotKeyInput` を作る。
9. `snapshot_key` を決める。
10. `snapshot_root = repo_root/worktrees/snapshot_key` を決める。
11. `snapshot_root` が存在すれば再利用する。
12. `snapshot_root` が存在しなければ、対象 commit から worktree を作る。
13. `build` / `lua_build` / `lua_post_update` が必要なら `snapshot_root` で実行する。
14. build 後 dirty state を identity に含める場合は、build 後に final `snapshot_key` を確定し、必要なら rename する。
15. `RepoMeta` を final `snapshot_root` から作る。
16. `.rsplug_build_success` を final `snapshot_root` に書く。
17. `repository.ls_files()` と file copy / symlink の参照元は final `snapshot_root` にする。

実装では `util::git::Repository` が現在 worktree repository 前提なので、最小変更ならまず `source.git` を bare repository ではなく通常 repository として扱う段階的実装もあり得る。ただし最終仕様は `source.git` と `worktrees/` の分離である。中間実装を入れる場合も、runtime symlink が可変 checkout を指さないことを必須条件にする。

## 9. `source.git` と worktree 操作

推奨は `source.git` を bare repository にすることである。理由は、runtime から読まないことを構造上強制できるためである。

`util::git` には最終的に次の操作を追加する。

- `open_source(source_git_dir) -> SourceRepository`
- `init_source(source_git_dir, url) -> SourceRepository`
- `SourceRepository::has_object(oid) -> bool`
- `SourceRepository::fetch_oid(url, oid)`
- `SourceRepository::resolve_remote(url, rev) -> Oid`
- `SourceRepository::create_worktree(snapshot_root, oid) -> Repository`
- `open_worktree(snapshot_root) -> Repository`
- `Repository::head_hash()`
- `Repository::is_dirty()`
- `Repository::diff_hash()`
- `Repository::ls_files()`

初期実装では API 名はこの通りでなくてもよいが、source repository と runtime worktree repository の責務を混ぜない。

## 10. repo snapshot link と `CopyEachFile`

`to_sym` の有無で `_gen` への配置方法は変わるが、参照元は両方とも `snapshot_root` に統一する。

### 10.1 repo snapshot link

- `to_sym` の `_gen/opt/<plugin_id>` は `snapshot_root` への symlink。
- この placement variant は任意 directory symlink ではなく、Git repo snapshot への link を表す。
- 実装名は `SymlinkDirectory` より `RepoSnapshotLink` / `RepoSnapshotSymlink` のような名前が望ましい。
- `snapshot_root` は `<snapshot_key>` で固定されるため、後続 update で別 commit に動かない。
- `snapshot_root` の absolute path は `plugin_id` の hash 入力に含めない。
- identity は `RepoSnapshotIdentity` で表す。
- doc extraction 用の `collect_doc_files_from_root(snapshot_root)` は引き続き snapshot を読む。
- helptags は現在通り symlink source を mutate しない。PlugCtl 側に copy された doc files から生成する。

### 10.2 `CopyEachFile`

- `snapshot_root` から tracked file を読み、従来通り `_gen/opt/<plugin_id>` にコピーする。
- `FileSource::Directory { path: snapshot_root }` の path は hash しない。
- 各 file の identity は `RepoSnapshotIdentity + relative_path` で表す。
- 全ファイル内容を再 hash しない。Git snapshot identity と relative path で、計算量を抑えつつ正確性を確保する。
- コピー後の `_gen` は従来通り generation manifest で保持 / cleanup する。
- `repository.ls_files()` は final `snapshot_root` の repository に対して実行する。

### 10.3 dependency runtimepath

dependency runtimepath も `repo.default_cachedir()` 直下ではなく、依存 plugin の resolved `snapshot_root` を指す必要がある。

現行の `dependency_cachedirs: Vec<PathBuf>` は repo cache 相対 path だけを保持しているため、worktree 導入時には依存先の resolved snapshot path を渡せる形に変更する。

推奨する data flow は次の通り。

1. `Plugin::new()` では依存先の repo path を最終 runtimepath として確定しない。
2. DAG 順は維持し、依存先 plugin が先に `load()` されることを利用する。
3. `Plugin::load()` の戻り値、または `LoadedPlugin` に `snapshot_root: Option<Arc<Path>>` 相当の情報を持たせる。
4. 依存元の `lua_build` / `lua_post_update` 実行時には、依存先の resolved snapshot path を runtimepath に入れる。
5. script-only dependency は runtimepath を持たないので従来通り除外する。

注意: `snapshot_root` を `LoadedPlugin` に持たせる場合、その field が `LoadedPlugin::plugin_id()` の hash に混入しないようにする。path は identity ではなく placement/runtime 情報である。

## 11. build 成功 cache

`.rsplug_build_success` は `snapshot_root` 直下に置く。

内容は `_gen` の `plugin_id` ではなく、build skip 判定用の identity を書く。初期実装では `RepoMeta` を `HasPluginId` 経由で hash した `repo_meta_id` でよい。

判定は次の通り。

1. final `snapshot_root/.rsplug_build_success` を読む。
2. 内容が今回の `repo_meta_id` と一致すれば build を skip する。
3. 一致しなければ `.rsplug_build_success` を削除して build を実行する。
4. build 成功後に final `RepoMeta` を再計算し、成功 id を書く。

build が worktree 内に成果物を生成する plugin は、build 後の dirty diff が `RepoMeta` と `snapshot_key` に反映される。これにより、同じ commit でも build 入力や build 成果物が変わる場合は別 `_gen` entry / 別 snapshot として扱える。

ただし、`CopyEachFile` は現行と同じく tracked file を対象にする。build 後に生成された untracked file は `_gen` へコピーされない。この semantics は今回変更しない。runtime に build 成果物が必要な plugin は `to_sym` を使うか、成果物を tracked path として扱う必要がある。

## 12. merge と identity

現在の `LoadedPlugin::merge()` は CopyEachFile 同士を merge できる。snapshot 導入時には、merge 後の identity が含まれる全 repository content を表す必要がある。

注意点。

- `FileSource::Directory` は absolute path を hash しない。
- そのため、CopyEachFile の file source path だけでは repo content identity を保持できない。
- CopyEachFile の各 `FileItem` は `RepoSnapshotIdentity + relative_path` を identity として持つ。
- repo snapshot link は `RepoSnapshotIdentity` を identity として持つ。
- 現在の `repo_meta: Option<RepoMeta>` は、merge 時に片方だけ残る実装になっている。
- 複数 repo 由来の CopyEachFile が merge された場合、全 repo の `RepoMeta` を identity に含める必要がある。

推奨する変更。

```rust
// 現在
repo_meta: Option<RepoMeta>

// 推奨
repo_metas: BTreeSet<RepoMeta>
// または deterministic order の Vec<RepoMeta>
```

`RepoMeta` を set / sorted vec にする場合は、`Hash` だけでなく deterministic ordering も必要になる。`head_rev`、`dirty_diff`、`build`、`lua_build` の順に比較できるように `Ord` を derive するか、明示的に sort key を定義する。

初期実装でこの変更を同時に行うのが大きい場合でも、少なくとも次を test で可視化する。

- 2 つの repo plugin が merge された場合、片方の `RepoMeta` 変更で merged plugin id が変わること。
- repo snapshot link は merge 対象外なので、この問題は CopyEachFile merge に限定されること。
- 同じ `RepoSnapshotIdentity` でも relative path が違う file は別 identity になること。

## 13. repo snapshot 用 `meta.json` を追加しない理由

初期設計では repo snapshot 用の `meta.json` を追加しない。

理由は以下。

- snapshot の同一性は `snapshot_key` で表現できる。
- revision の再現性は lock file が担当する。
- generation が参照する `_gen` entry は control plugin の `manifest.json` が担当する。
- `_gen` は成果物 DB であり、repo snapshot metadata を混ぜると責務が曖昧になる。
- 新しい metadata file を増やすと migration と cleanup の対象が増える。

後から必要になる可能性がある情報は以下だが、初期実装では必須ではない。

- source URL の変更履歴。
- `snapshot_key` の schema version。
- worktree 作成時刻。
- build 実行時刻。

これらが実際に必要になった場合だけ、`worktrees/<snapshot_key>/.rsplug-meta.json` のような worktree 局所の metadata を検討する。`_gen` 側には置かない。

## 14. migration / compatibility

### 14.1 既存 cache の扱い

既存の `repos/<repo_cache_dir>/` は通常 checkout であり、新仕様の `source.git` / `worktrees/` layout とは異なる。初期実装では破壊的 migration をしない。

互換方針は以下。

1. 新 layout が存在する場合は新 layout を使う。
2. 新 layout が存在しないが旧 checkout が存在する場合は、旧 checkout を source として読める範囲で対象 commit を確認する。
3. `--install` または `--update` が指定されていれば、新 layout を作成して移行する。
4. `--locked` で新 layout がなく、旧 checkout に対象 commit がない場合は、現行と同様に cache 不足として error にする。
5. 旧 checkout は自動削除しない。
6. 新しく生成する `_gen` symlink は旧 checkout root を指さない。

旧 checkout を自動で `source.git` に移動しない理由は、既存 directory が利用者に手で触られている可能性があるためである。初期版は coexist で十分。

### 14.2 lock file compatibility

lock file の形式は変えない。

- lock file には引き続き repository URL と full commit SHA を書く。
- `snapshot_key` や `input_hash` は lock file に書かない。
- build 入力が変わった場合は lock file ではなく `_gen` id と `worktrees/<snapshot_key>` が変わる。

これにより、既存の `rsplug.lock.json` はそのまま読める。

### 14.3 generation compatibility

`generations/` と root `init.lua` の挙動は変えない。

- 既存 generation loader は引き続き `pack/_gen/opt/<control_id>` を `packadd` する。
- `pack/_gen/opt/<plugin_id>` が symlink の場合、その symlink 先が旧 `repos/<repo>` から新 `repos/<repo>/worktrees/<snapshot_key>` に変わるだけである。
- 現行の `manifest.json` retention と `_gen` cleanup はそのまま使う。

既存 `_gen` entry の symlink が旧 checkout を指している場合、それは過去生成物として残る。新しく生成される `_gen` entry から新 snapshot layout を使う。

## 15. 実装順

実装は次の順に進める。

### 15.1 identity / hash の安全化

1. `RepoSnapshotIdentity` を追加する。初期実装では `RepoMeta` と統合してもよいが、absolute path を含めない。
2. repo 由来 file 用に `RepoFileIdentity { snapshot, relative_path }` を追加する。
3. generated file 用に `GeneratedFile { path, data_hash }` 相当の identity を追加する。
4. `FileItem` に logical identity を持たせる。
5. 現在の `SymlinkDirectory` 相当は `RepoSnapshotLink { target, identity }` のような repo snapshot 専用 variant にする。
6. `HowToPlaceFiles` / `FileItem` の hash は logical identity を通じて決まるようにし、`Arc<Path>` の absolute path を hash しない。
7. absolute path が `LoadedPlugin::plugin_id()` に影響しない unit test を追加する。
8. `RepoMeta` が `_gen` id ではなく repo identity component であることを code comment に反映する。
9. `repo_meta: Option<RepoMeta>` を `repo_metas` に変更するか、merge 時に全 repo meta を保持できる設計に変更する。
10. merged CopyEachFile plugin の identity が全 repo meta と relative path identity を反映する unit test を追加する。

### 15.2 path model の切り出し

次の小さな関数を追加する。

- `repo_root(cache_dir, repo) -> PathBuf`
- `source_git_dir(repo_root) -> PathBuf`
- `worktrees_dir(repo_root) -> PathBuf`
- `snapshot_root(repo_root, snapshot_key) -> PathBuf`
- `building_worktree_root(worktrees_dir) -> PathBuf`

`RepoSource::default_cachedir()` は維持する。

### 15.3 snapshot key の生成

1. `SnapshotKeyInput` を追加する。
2. `head_rev`、`dirty_diff`、`build`、`lua_build` を入力にする。
3. `util::hash::digest_hash_hex_string(&input)` で hash を作る。
4. filesystem-safe な ASCII 文字列にする。
5. unit test を追加する。
   - 同じ入力は同じ key。
   - commit が変わると別 key。
   - build command が変わると別 key。
   - `lua_build` が変わると別 key。
   - dirty diff が変わると別 key。

### 15.4 `util::git` の source / worktree 分離

1. `source.git` を初期化 / open する API を追加する。
2. 対象 commit を fetch する API を追加する。
3. `worktrees/<snapshot_key>` を対象 commit で作る API を追加する。
4. 既存 `Repository::fetch()` の checkout 依存を runtime worktree 側に閉じ込める。
5. bare `source.git` を採用する場合は、fetch と worktree 作成が bare repository から動くことを integration test で確認する。

### 15.5 `Plugin::load()` の分離

1. `proj_root` という単一概念を廃止し、`repo_root` / `source_git_dir` / `snapshot_root` に分ける。
2. Git fetch は `source.git` に対して行う。
3. file scan、build、`lua_post_update`、`lua_build`、copy、symlink は `snapshot_root` に対して行う。
4. `LoadedPlugin` には `_gen` placement に必要な情報と identity component を分けて持たせる。
5. `lock_info` は従来通り `(url, head_rev)` を返す。

### 15.6 repo snapshot link の参照先変更

1. `to_sym` plugin は `HowToPlaceFiles::RepoSnapshotLink { target: snapshot_root, identity }` 相当で表す。
2. `_gen/opt/<plugin_id>` が `worktrees/<snapshot_key>` を指すことを test する。
3. link target の absolute path が違っても、同じ `RepoSnapshotIdentity` なら `plugin_id` が変わらないことを test する。
4. update 後に過去 `_gen` symlink が別 commit に動かないことを integration test する。

### 15.7 `CopyEachFile` の参照元変更

1. `FileSource::Directory { path: snapshot_root }` にする。
2. 各 `FileItem` には `RepoFileIdentity { snapshot: RepoSnapshotIdentity, relative_path }` を持たせる。
3. `repository.ls_files()` は `snapshot_root` の repository から取る。
4. 既存 copy semantics と ignore / merge behavior が変わらないことを test する。
5. 全ファイル内容を hash しなくても、snapshot identity と relative path の変更で `plugin_id` が変わることを test する。

### 15.8 build 成功 marker の移動

1. `.rsplug_build_success` を `snapshot_root` に置く。
2. marker 内容は `repo_meta_id` または専用 `BuildCacheKey` の id にする。
3. 同じ snapshot + 同じ build 入力で build が skip されることを確認する。
4. build 入力変更で skip されないことを確認する。
5. build 後 dirty diff が変わる場合に final snapshot key が変わることを確認する。

### 15.9 dependency runtimepath の resolved snapshot 対応

1. `dependency_cachedirs` を runtimepath として使わない。
2. load 済み依存 plugin から resolved `snapshot_root` を渡す。
3. `lua_build` / `lua_post_update` の runtimepath に依存先 snapshot path を入れる。
4. script-only dependency は runtimepath なしとして扱う。
5. DAG 順が崩れていないことを test する。

### 15.10 migration fallback

1. 旧 checkout だけがある場合に install/update で新 layout を作る。
2. 旧 checkout を自動削除しない。
3. `--locked` で新 layout がなく旧 checkout に対象 commit がない場合は cache 不足 error にする。
4. 新 layout と旧 checkout が混在しても、新規 symlink が旧 checkout root を指さないことを test する。

### 15.11 integration test

最低限、次を追加する。

1. `--install` 後に `repos/<repo>/source.git` と `repos/<repo>/worktrees/<key>` が作られる。
2. `to_sym` plugin の `_gen` entry が snapshot worktree を指す。
3. 同じ lock file で再実行して同じ snapshot を再利用する。
4. `--update` 後も過去 symlink が指す snapshot は別 commit に動かない。
5. `CopyEachFile` plugin の出力が従来と同じである。
6. `lua_build` が依存先 snapshot runtimepath を見られる。
7. `generations/` と `init.lua` の出力が変わらない。
8. `plugin_id` が absolute cache path に依存しない。
9. CopyEachFile の id が `RepoSnapshotIdentity + relative_path` の変更に反応する。
10. merged CopyEachFile plugin の id が全 repo meta 変更に反応する。
11. script-only plugin の挙動が変わらない。

## 16. メリット

### 16.1 repo snapshot link の再現性が上がる

過去 generation が参照する `_gen` symlink は固定 snapshot worktree を指す。次回 update で repo が別 commit に動いても、過去 generation の plugin 実体は動かない。

さらに、identity は link target の absolute path ではなく `RepoSnapshotIdentity` で決まるため、cache root が異なる環境でも `_gen` id が不要に変わらない。

### 16.2 既存の出力モデルを壊さない

`generations/`、root `init.lua`、`pack/_gen/`、control plugin の `manifest.json` は維持する。利用者の Neovim 側 bootstrap も変わらない。

### 16.3 cache の責務が明確になる

- `repos/` は Git source と snapshot worktree。
- `_gen` は Neovim が読む成果物 DB。
- `generations/` は起動導線。
- lock file は repository revision の再現性。

この分離により、repo checkout の更新と generation retention が混ざらない。

### 16.4 snapshot 再利用が効く

同じ commit と同じ build 入力なら同じ worktree を使える。generation ごとに repository 実体を増やさない。

## 17. デメリット

### 17.1 Git 操作は複雑になる

現行の `1 repo = 1 checkout` より、`source.git` と `worktrees/` の分だけ Git 操作が増える。

### 17.2 dependency runtimepath の受け渡しを直す必要がある

現在の `dependency_cachedirs` は repo cache 相対 path で足りているが、新設計では依存先の resolved snapshot path が必要になる。ここは実装上の主な変更点になる。

### 17.3 cache 使用量は増える

同じ repo の複数 revision を保持できるようになるため、disk 使用量は増える。今回は `gc/` を作らないため、自動削除は `_gen` cleanup とは別問題として残る。

### 17.4 build が worktree を汚す plugin は扱いが難しい

build 成果物が worktree 内に残る場合、その dirty diff を snapshot identity に含める必要がある。build 前に snapshot key を確定すると、build 後の状態と key がずれる可能性がある。

このため、build 後 dirty diff を含める場合は、一時 worktree から final snapshot root への rename flow を採用する。

## 18. リスクと対策

### 18.1 snapshot key の衝突

リスク: `snapshot_key` の入力不足により、異なる成果物が同じ worktree を共有する。

対策:

- `head_rev`、`dirty_diff`、`build`、`lua_build` を含める。
- `SnapshotKeyInput` のような `#[derive(Hash)]` 型を使う。
- 手書き byte 連結を避ける。
- 必要なら schema byte を hash input に含める。

### 18.2 identity に absolute path が混入する

リスク: repo snapshot link の target path や dependency snapshot path が `LoadedPlugin::plugin_id()` に混入し、cache root が違う環境で `_gen` id が変わる。

対策:

- repo snapshot link は `target: Arc<Path>` と `identity: RepoSnapshotIdentity` を分ける。
- `CopyEachFile` の各 file は `RepoSnapshotIdentity + relative_path` を identity とする。
- generated file は `data_hash` を identity とする。
- `HowToPlaceFiles` / `FileItem` の hash には logical identity だけを入れ、placement path を入れない。
- absolute path 変更で `plugin_id` が変わらない unit test を追加する。

### 18.3 merge 後に repo identity が欠落する

リスク: CopyEachFile plugin を merge した時、片方の `RepoMeta` しか `LoadedPlugin` に残らず、もう片方の repo content 変更が `_gen` id に反映されない。

対策:

- `repo_meta: Option<RepoMeta>` を複数保持できる形にする。
- merge 時は全 repo meta を deterministic order で保持する。
- merge 後 plugin id が全 repo meta 変更に反応する test を追加する。

### 18.4 旧 checkout との混在

リスク: 旧 `repos/<repo>` checkout と新 `repos/<repo>/source.git` が同じ directory 階層に混在し、open 対象を誤る。

対策:

- 新 layout の marker は directory 構造で判定する。
- `source.git` が存在する場合だけ新 layout とみなす。
- 旧 checkout root を新規 runtime symlink 先にしない。

### 18.5 build 後 dirty state の扱い

リスク: build 後に生成された file が `repository.ls_files()` に含まれず、`CopyEachFile` では成果物が `_gen` にコピーされない。

対策:

- 現行と同じく tracked file を対象にする。
- build 成果物を runtime に含める必要がある plugin は `to_sym` を使うか、成果物を tracked path として扱う必要がある。
- この semantics は今回変更しない。

### 18.6 並列実行時の競合

リスク: 同じ snapshot を複数 rsplug process が同時に作ると worktree 作成や build marker 書き込みが競合する。

対策:

- 今回は `locks/` を追加しない。
- worktree 作成は一時 directory に作ってから final path へ atomic rename する。
- final path が既に存在する場合は、作成済み snapshot を再利用し、一時 directory を削除する。
- 作成途中で失敗した hidden building directory は次回 install/update で掃除できるようにする。
- process 間の完全排他が必要になった場合だけ、lock file ではなく target directory 上の atomic operation を検討する。

### 18.7 cleanup 方針がない

リスク: 古い `worktrees/` が増え続ける。

対策:

- 初期実装では自動 GC を入れない。
- 明示削除で十分とする。
- 自動削除が必要になった場合も `gc/` directory は作らず、既存 `worktrees/` を走査する command として検討する。

## 19. 未解決の決定事項

実装前または実装中に決める必要がある事項は以下。

1. `snapshot_key` の正確な schema。
   - 推奨: `<head_rev>` または `<head_rev>__v1_<input_hash>`。
   - `input_hash` に dirty diff、`build`、`lua_build` を含める。
2. build 前後の snapshot key 確定タイミング。
   - 推奨: build 後 dirty diff を含めるなら一時 worktree から final snapshot へ rename する。
   - build 後 dirty diff を含めないなら snapshot 作成は単純になるが、build 成果物込みの identity が弱くなる。
3. `lua_post_update` の実行条件。
   - 推奨: update 検知は `source.git` の fetch / resolved oid 変化で判断し、script 実行は plugin が実際に読む `snapshot_root` で行う。
   - 同じ snapshot を再利用する場合に `lua_post_update` を再実行するかは明確に決める。
4. dependency runtimepath の data flow。
   - 推奨: load 済み依存 plugin から `snapshot_root` を渡す。
   - repo path だけから推測しない。
5. `source.git` を bare repository にするか通常 repository にするか。
   - 推奨: runtime から読まないことを強制できる bare repository。
   - 既存 `util::git` の変更量を抑える必要があるなら段階的に通常 repository から始めてもよい。
6. `RepoMeta` の複数保持形式。
   - 推奨: deterministic order の `Vec<RepoMeta>` か `BTreeSet<RepoMeta>`。
   - merge 後 identity を壊さないことを優先する。

## 20. 初期スコープ

最初の実装は以下に絞る。

- identity に absolute path が混入しないようにする。
- repo snapshot link は `RepoSnapshotIdentity` で hash する。
- CopyEachFile は `RepoSnapshotIdentity + relative_path` で hash する。
- merged CopyEachFile plugin の repo identity 欠落を防ぐ。
- `repos/<repo>/source.git` と `repos/<repo>/worktrees/<snapshot_key>/` を導入する。
- 現在の `SymlinkDirectory` 相当を repo snapshot link として整理し、参照先を `worktrees/<snapshot_key>` にする。
- `CopyEachFile` の参照元も `worktrees/<snapshot_key>` にする。
- `.rsplug_build_success` は worktree 単位にする。
- dependency runtimepath は resolved snapshot path にする。
- `generations/`、root `init.lua`、`pack/_gen/`、control plugin の `manifest.json` は変更しない。
- lock file format は変更しない。
- root 直下の `tmp/`、`locks/`、`logs/`、`gc/` は追加しない。
- repo snapshot 用 `meta.json` は追加しない。

この範囲で、今回の主目的である「generation 切り替え時に `to_sym` plugin の実体が後続 update で動かない」を達成できる。

## 21. 実装時の完了条件

実装完了とみなす条件は以下。

1. `cargo fmt` が通る。
2. `cargo test` が通る。
3. 関連する clippy check が通る。
4. `to_sym` plugin で `_gen/opt/<plugin_id>` が `worktrees/<snapshot_key>` へ symlink される。
5. update 後も過去 generation の symlink target が変わらない。
6. cache root の absolute path を変えても、同じ content/build 入力なら `plugin_id` が変わらない。
7. CopyEachFile の plugin id が `RepoSnapshotIdentity + relative_path` を反映する。
8. repo snapshot link の plugin id が `RepoSnapshotIdentity` を反映する。
9. CopyEachFile merge 後の plugin id が全 repo meta を反映する。
10. `lua_build` / `lua_post_update` が依存先 snapshot runtimepath を使う。
11. 既存 lock file をそのまま読める。
12. 既存 `generations/` / `init.lua` / `_gen` retention の挙動が変わらない。

## 22. ひとことで言うと

`generations/` は起動導線、`_gen` は Neovim が読む成果物 DB、`repos/` は固定 snapshot cache として分ける。

今回変えるのは `repos/` から plugin 実体を読む方法と、そのために必要な identity/path 分離である。generation と `_gen` の基本設計は維持する。
