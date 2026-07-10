# rsplug.nvim ExecPlans（統合）

本ファイルは `AGENTS.md`（ExecPlans セクション）の慣習に従う living document である。
2026-07-10 に旧 `PLANS.md`（lock/cache 同期）と `PLANS-copy-unification.md`（pack copy 統一）を
1ファイルに統合した。`§C` に `--gc` 拡張の未実装プランを追加した。

## 目次

- [§A — lock file / cache directory 同期](#a--lock-file--cache-directory-同期)
- [§B — sym 廃止 / pack copy 統一](#b--sym-廃止--pack-copy-統一)
- [§C — `--gc` 拡張（未実装プラン）](#c----gc-拡張未実装プラン)
- [Revision Notes](#revision-notes)

---

# §A — lock file / cache directory 同期

## Purpose / Big Picture

rsplug.nvim は `~/.cache/rsplug/repos/<repo>/worktrees/<snapshot_key>/` に snapshot を、JSON lock
（`rsplug.lock.json`）に repo URL → 解決 commit を保持する。lock は `--install`/`--update`/`--locked`
の副作用としてしか書かれなかったため、cache と drift する問題があった。

完了後の観測:
- デフォルト実行（flag 無し）でも lock の `rev` が on-disk snapshot と一致する。
- orphane snapshot は `--gc` で削除される。
- lock は存在しない snapshot を主張しない。

`repos/` は不変でない（build/lua_post_update/外部で snapshot worktree が変わりうる）。本 plan は不変性
を仮定しない範囲（on-disk 状態の読取りのみ）を実装し、不変性に依存する部分は §C / 将来に委ねる。

## Progress

- [x] (2026-07-07) Phase 1: lock を on-disk snapshot から再構築。`locked_map` を常にロックファイルから
  初期化（NotFound は空）。`Plugin::load` の `Ok(None)`（repo 有り・未インストール）URL を
  `urls_to_remove` に集めて lock から削除 → `lock_infos` を overlay。
- [x] (2026-07-07) Phase 2: `--gc` で orphane snapshot 削除。URL→cachedir 正方向変換
  （`rsplug::plugin::cachedir_from_url`）で lock と照合。`main.rs:gc_tests` で Acceptance 4/5 検証済み。
- [ ] Phase 3（deferred）: 不変 snapshot 設計（read-only 化）。

## Surprises & Discoveries

- デフォルト実行は `!locked` で lock を書くが、`locked_map` は `lock_infos`（fetch 時のみ）から埋まる。
  既インストールの cache 由来 plugin は `head_rev_str` を snapshot dir 名から設定して `lock_info` を返す
  （`plugin.rs:602`）。データは流れているが、`main.rs` が `--locked` 非設定時は file から `locked_map`
  を初期化しなかったため、設定に無い既存 plugin のエントリが欠落していた（Phase 1 で修正）。
- snapshot dir 名が commit hash を符号化: `<40-hex>` or `<40-hex>__v1_<hash>`（`packpathstate.rs:71`）。
  `latest_snapshot_oid`（`plugin.rs:928`）がこの prefix を parse する。git repo を開かずに commit が復元可能。
- **GC 逆変換バグ**: 初版 GC は dir→URL 逆変換だったが `walk_repos_for_gc` が絶対パスを渡すため
  `parts[0]==/` となり host 判定不可 → 全件不一致で何も削除しなかった。`default_cachedir` は `.git` 末尾を
  剥がすが lock key（`repo.url()`）は剥がさないため `.git` 付き URL でも不一致。正方向変換（URL→cachedir）
  で統一して解決。Evidence: `plugin.rs:default_cachedir`, `util.rs:github::url`, `main.rs:garbage_collect`。

## Decision Log

- Phase 1/2 は on-disk 状態の読取りのみで安全 → 即実装。Phase 3（不変性）は設計変更を要するため defer。
- `--gc` は opt-in（破壊的）。ユーザーが手動 snapshot を保持したい場合を考慮。
- GC は逆変換でなく URL→cachedir 正方向変換で照合する（`cachedir_from_url`）。逆変換は `.git`/scheme/auth
  扱いが脆弱。GC ロジックは `main.rs` に維持し、変換ヘルパのみライブラリに共有。

## Outcomes & Retrospective

- Phase 1/2 実装済み（`main.rs` + `plugin.rs`）。デフォルト実行後に lock `rev` が snapshot と一致し、
  設定にあって未インストールの repo は lock から除去される（Acceptance 1/2/3）。`--gc` は orphane snapshot
  を削除（Acceptance 4/5）。
- 検証: `cargo test --workspace`（`gc_tests` 5件追加）/ `clippy -D warnings` / `fmt --check` すべて緑。
- Retrospective: GC 逆変換バグはテスト不在で発見が遅れた。Acceptance をテストで先書きすべきだった。

## Context / Key files

- `main.rs` — `locked_map` 初期化（111-120）、load 収集（156-241）、lock 書込み（244-267）、`garbage_collect`（297-410）、`gc_tests`（413-561）。
- `plugin.rs` — `Plugin::load`、`latest_snapshot_oid`（928）、`cachedir_from_url`。
- `lockfile.rs` — `LockFile` read/write。
- `packpathstate.rs` — `RepoSnapshotIdentity`、`snapshot_key()`。

cache layout: `~/.cache/rsplug/repos/<cachedir>/{source.git/,worktrees/<snapshot_key>/}`。
lock 形式: `{"version":"1","locked":{"<url>":{"type":"git","rev":"<40hex>"}}}`。

## §A Validation / Acceptance

1. デフォルト実行後、各 plugin の lock `rev` が `repos/<repo>/worktrees/` の最新 snapshot dir 名の commit と一致。
2. 設定から削除済みだが cache 残存 plugin の lock エントリは保全される。
3. 設定に有るが未インストール（cache 無し）plugin のエントリは lock から削除される。
4. `--gc` が lock の `rev` に一致しない snapshot を削除する。
5. `--gc` が lock の `rev` に一致する snapshot を保全する。
6. `cargo test --workspace` / `clippy -D warnings` / `fmt --check` 通過。

---

# §B — sym 廃止 / pack copy 統一

## Purpose / Big Picture

rsplug は2系を持つ:
- **系A（出力）**: pack directory（`{packpath}/pack/_gen/opt/{id}/`, `generations/{ctl}.lua`, `init.lua`）。Neovim が読む。deterministic・portable・Nix 向き。
- **系B（キャッシュ）**: `repos/<repo>/{source.git/,worktrees/{snapshot_key}/}`, `rsplug.lock.json`。マシン固有。

かつて `to_sym`（`sym` 明示 or `build`/`lua_build`/`lua_post_update` で自動）のとき、系A の pack が系B の
snapshot を symlink 参照していた（`RepoSnapshotLink`）。これが pack の非自己完結・Nix 非安全・GC との
絡みを生んでいた。本 plan は `RepoSnapshotLink` を廃止し pack に常に copy 実体を置く。

## Progress（Phase 0-7）

- [x] Phase 0: `yank` の `hard_link` に ExDev で copy フォールバック追加。
- [x] Phase 1: `Repository::ls_files_with_untracked` 追加、`Plugin::load` が `has_build` で切替。
- [x] Phase 2: `RepoSnapshotLink`/`DirectoryExtractionType::Symlink`/`symlink_plugin_dir`/`to_sym` を一括廃止。常に `CopyEachFile`。
- [x] Phase 3: テスト整理・実機検証（blink.cmp でネイティブライブラリ pack 到達・lazy load・`:helptags` 正常）。
- [x] Phase 4: `dotgit` オプション実装。pack に `.git` 複製、dotgit=true は GitFetch 強制。
- [x] Phase 4b: `dotgit=true` だが snapshot に `.git` 無し → `PluginDotgitMissing` WARNING + skip（回復は `-u`）。
- [x] Phase 5: `clone_dir` プリミティブ（macOS `clonefile(2)`）。
- [x] Phase 6c: copy 戦略 `AtomicU8{reflink,hardlink,copy}`、`yank`→`place_path`→`copy_tree`/`copy_leaf`
  統一、`CopyEachFile` をディレクトリ・ファイル不分別辞書化、merge データロス修正（1段展開して子 key 化）、
  dotgit `.git` を通常 sealed-dir エントリ化。**plugin_id 非互換**（既存 pack/lock は再生成）。
- [x] (2026-07-10) Phase 7: FetchTarball の `.git` ワークアラウンド削除 + ハッシュ計算変更。**下記 Phase 7 詳細参照**。
- [x] (2026-07-10) doc 盗み復活: Phase 6a で列挙が read_dir（`doc` sealed-dir）になり盗みが no-op 化していたのを、`doc` を個別ファイルに展開して復元（`_rsplug:doc` start/control プラグインへ集約・helptags 1件）。
- [x] (2026-07-10) EEXIST 暫定対応: install copy を dst 既存在でマージ/上書きするよう堅牢化（Phase 8 で根本対応、本対応は safety net として残置）。
- [ ] (2026-07-10) Phase 8: doc 盗みをマージ前に移行 + マージの sealed/子混在を推移的に正規化。**下記 Phase 8 詳細参照**。

## Phase 7 詳細（2026-07-10 完了）

**Goal**: tarball（GitHub HTTPS + token）snapshot で `.git` を作らない（git バックエンドでないのに
`.git` が作られ誤解を招いていた）。identity/dirty を git でなく**ファイル内容ハッシュ**で計算。

**変更点**:
- `util.rs`: `TarballFetch::init_git_worktree`（git2 init + add -A + commit で `.git` を作っていた）を削除。
  `fetch_to_snapshot` は download + 展開のみに。`ls_files` は Phase 6c で既に削除済み。
- `plugin.rs`: `MaterializedRepo` enum 導入（`Git(util::git::Repository)` / `Plain`）。
  - `materialize` は tarball 成功 → `Plain`（`git::open` しない）、失敗時フォールバック / 非 tarball → `Git`。
  - snapshot 準備部: `repository` 型を `MaterializedRepo` に。再利用パスは `final_root/.git` 有無で
    `Git(git::open)` / `Plain` を切替（Phase7 前後の snapshot 両対応）。has_build パスは `is_plain` を記憶し
    rename 後に再構築。
  - `build_repo_snapshot_identity(&MaterializedRepo, snapshot_root, ...)`: `Git` は `is_dirty`/`diff_hash`
    （git diff）、`Plain` は build 有りなら `dirty_diff_from_content`、無ければ `None`（clean baseline）。
- `util.rs` 新設 `dirty_diff_from_content(root)`: ツリーをソート順再帰走査し各ファイルの
  （相対パス, 内容）を `Xxh3` に update して 128bit digest を返す。`.rsplug_build_success`（identity 由来で
  循環）と `.git`（メタデータ）は除外。git 版 `diff_hash` と同じ digest 手法。単体テスト
  `dirty_diff_from_content_is_deterministic_and_excludes_marker_and_git` で決定性・除外・内容感度を固定。
- **libc 採用**: `packpathstate.rs` の手書き `extern "C"`（macOS `clonefile` / Linux `ioctl`）とマジック定数
  `FICLONE = 0x40049409` を `libc::clonefile` / `libc::ioctl` / `libc::FICLONE` に置換。errno 定数
  （`EXDEV`/`ENOTSUP`/`ENOSYS`/`EOPNOTSUPP`/`ENOTTY`）も `libc::*` 化。libc は既存推移依存（git2/tokio）で
  コスト実質ゼロ。
- **E138 修正**: `packpathstate.rs` helptags と `plugin.rs` `lua_build_nvim_command` の nvim 起動に
  `-i NONE`（ShaDa 無効）を追加。並列 install で複数 nvim が同一 `main.shada` の書き込みを奪い合い
  `E138: All .../main.shada.tmp.X files exist` が出る問題の根本修正（`-u NONE` だけでは ShaDa は抑制されない）。
  helptags には `-n`（swap 無効）も追加。

**plugin_id 影響**: 非 build の tarball は dirty=None のまま不変。build 付き tarball は git diff → 内容ハッシュ
へ変化（ハッシュ変更の互換性考慮は不要・既存 pack/lock は再生成）。dotgit=true は GitFetch 強制で `Git` のため
`.git` 有り・git diff のまま（影響なし）。

**実機検証（隔離 HOME, GitHub HTTPS + token, tpope/vim-{repeat,surround,commentary,unimpaired}）**:
- 4 snapshot とも `.git` 無し、`source.git` 無し（tarball パス確認）。
- install 出力に E138/shada 無し（0件）。`~/.local/state/nvim` に `*.shada*` テンポラリ無し（`-i NONE` 効果）。
- pack 生成・`doc/tags`（helptags）生成・`generations/<ctl>.lua` + `init.lua` symlink 生成。
- 生成 init.lua を nvim headless でロード → messages 空（エラーなし）。
- `cargo test --workspace`（81+件）/ `clippy -D warnings` / `fmt --check` すべて緑。

## Phase 8 詳細（2026-07-10 実装）

**背景（EEXIST 回帰）**: doc 盗み復活で `doc` 衝突（マージ阻害要因）が消えた結果、`autoload/` 等の
sealed-dir を共有するプラグインが新たにマージするようになった。Phase 6c の `union_files`/
`expand_dir_union` の sealed-dir 展開は**非推移的**で、3+ プラグインのマージで同一 pack に
「sealed `autoload`」と「展開済み `autoload/子`」が混在し、install の `copy_tree` が既存 `autoload/` に
clonefile して `EEXIST (os error 17)` になっていた。

**変更1: doc 盗みをマージ前に移行**（doc を merge 対象から除外し、merge を clean に）。
- `LoadedPlugin::steal_doc(&mut self) -> BTreeMap<PathBuf, FileItem>` 新設: `self.files` から `doc/**`
  を抜き出し（`MergeType::Overwrite`）て返す。`PlugCtl::create` 内の盗み closure は削除。
- `main.rs`: `LoadedPlugin::merge` の**前**に全プラグインから doc を盗んで `doc_acc` に集約。
  マージは doc 無しの source プラグイン群で行う。`state.set_doc(doc_acc)` で注入 → `From<PlugCtl>` が
  `_rsplug:doc` プラグインを生成（従来通り rsplug 自身の `doc/rsplug.txt` と merge）。
- `PlugCtl.overwrite_files` を `BTreeMap<PluginID, HowToPlaceFiles>` → フラット `BTreeMap<PathBuf, FileItem>`
  に簡素化（`From<PlugCtl>` は元から `_id` を無視して flatten していたため）。
- **plugin_id 非互換**: source プラグインの id が doc 有り→無しで変化（既存 pack/lock は再生成）。

**変更2: マージの sealed/子混在を推移的に正規化**（autoload 系 EEXIST の根本対応）。
- `union_files` のマージ後、`normalize_sealed(&mut files)` を実行: sealed-dir `X` が同じ map 内に
  子孫 `X/...` を持つ場合、その sealed `X` を展開（子を個別エントリ化）して混在を解消。子孫を持つ
  sealed のみ展開し、nesting はループで処理。
- **IO は最低限**: 展開判定は BTreeMap range で O(log n)（IO 無し）。実際の展開（`read_dir`）は
  「子孫を持つ sealed」のみ。深さは当該 sealed の1階層（nesting は必要分だけループ）。
- `dirs_mergeable` は変更不要（sealed/子はキー不同のため衝突と見なされず、元から merge 可能。
  `union_files` が正しく処理すればよい）。

**safety net**: install copy の dst 既存在→マージ/上書き堅牢化（`copy_tree` は再帰 walk で既存 dst に merge、
`copy_file_with_strategy`/symlink は上書き）は残置。マージ正規化漏れや dotgit `.git` 等の例外でも
EEXIST で install が落ない保険。

**検証**: 隔離 HOME + ユーザ実設定（128 plugin）で再現していた EEXIST が解消（exit 0）。vim-gin pack の
`autoload/` が `gin edisch.vim mstdn` の完全 union になることを確認。doc 集約・tags 1件・help 参照は維持。
単体テスト `normalize_sealed_*`/`steal_doc_*`/`copy_tree_merges_into_existing_destination` 追加。

## Surprises & Discoveries

- `to_sym()` はユーザ明示だけでなく build/lua_build/lua_post_update で自動 true。sym の役割は build 成果物
  （untracked）を pack に伝えること（`CopyEachFile` が追跡ファイルのみだったため）。
- Lua runtime は pack 内相対パスのみで動作し `repos/cache` 非依存。sym 廃止で壊れない。
- `init.lua → generations/{ctl}.lua` の sym は系A内で閉じ（維持）。本 plan 対象は pack↔repos の sym のみ。
- **Phase 6a merge データロス**: `Plugin::load` が `ls_files`→`read_dir`（トップ sealed key）に切替えた結果、
  `Add` の `files.extend` が同 path を上書きしマージディレクトリの片側 source が消失していた。Phase 6c の
  「衝突時1段展開して子 key 化・再帰 union」で修正。
- Phase 6a/6c で `git ls-files`/`init_git_worktree` の git 依存が削除された結果、Phase 7 で tarball の
  `.git` を完全に外せるようになった（dirty も内容ハッシュで代替）。
- **Phase 6c sealed 展開の非推移性（Phase 8 で発見）**: `union_files`/`expand_dir_union` の sealed-dir 展開は
  pairwise のみ完全。3+ プラグインが `autoload/` 等を共有するマージで「sealed X」と「展開済み X/子」が
  同一 pack に混在し得る。長らく潜在していたが、doc 盗み復活で `doc` 衝突（マージ阻害要因）が消え、
  `autoload` 系プラグインが新たにマージするようになって顕在化（EEXIST）。Evidence: `packpathstate.rs`
  `union_files`/`expand_dir_union`/`dirs_mergeable`。

## Decision Log

- sym（`RepoSnapshotLink`）を廃止し常に copy 実体（案1）。pack を自己完結・Nix safe にするため。
- build 成果物 copy は `ls-files` ＋ `ls-files --others`（**.gitignore 無視**）。実機で `target/` が届かない
  問題を解決するため（旧 sym は worktree 全体参照で見えていた）。
- `dotgit` オプション（デフォ false）。true で snapshot の `.git` を pack に copy（blink.cmp 等の git 利用プラグイン用）。
  TarballFetch は `.git` を作れないため GitFetch 強制。Phase 6c で `.git` は通常 sealed-dir エントリ化
  （`LoadedPlugin.dotgit` は `-u` 回復のため残置）。
- copy 戦略は `AtomicU8{0=reflink,1=hardlink,2=copy}` 単調昇格。`EXDev` は hardlink も失敗するため copy まで jump。
- `init.lua → generations/{ctl}.lua` sym は維持（系A内完結）。
- Phase 7: tarball dirty を GitHub API hash でなく**ファイル内容ハッシュ**に。API hash は build 副作用を別途
  反映する手間が要るが、内容ハッシュは build 成果物を自然に取り込むため採用。`.git`/`.rsplug_build_success`
  は除外し決定論性を保全。

## Outcomes & Retrospective

- pack は `RepoSnapshotLink` を廃止し常に copy で自己完結（`find pack -type l` で `generations`/`init.lua` 以外 0件）。
- dotgit で blink.cmp の `.git` チェック問題（outdated → Lua fallback）を根本解決。
- Phase 7 で tarball snapshot の `.git` が消え、identity が git 非依存に。E138 併合修正。
- Retrospective: Phase 6a の sealed 列挙切替が merge データロスを招いた。Acceptance テストを同時追加すべき
  だった（Phase 6c/7 でテスト厚化）。Phase 7 の `dirty_diff_from_content` も即座に単体テストを添えた。

---

# §C — `--gc` 拡張（未実装プラン）

> **状態: 未実装（プランのみ）。** 2026-07-10 設計。実装時に本セクションを Progress/Outcomes に昇格させる。

## Purpose / Big Picture

現行 `--gc`（`main.rs:garbage_collect`）は「in-lock repo の orphane snapshot 削除」のみ。pack 側の
クリーンアップは `install` の副作用（`retained_manifest_entries` で現世代 + 過去 `RETAIN_GENERATIONS` 世代
の pack を保持、それ以外削除）でのみ行われ、`--gc` 単体では扱わない。

ユーザー要望: `--gc` を以下 **2 動作を同時に** 行うよう拡張する。
1. **generations に参照されていない pack のクリーンアップ**（pack GC）。
2. **使われていない repos のクリーンアップ**（repos GC）。

repos GC の削除範囲はユーザー確認済で **「両方」**:
- (a) **lock 外リポジトリ全体削除** — lock に含まれない repo ディレクトリ（worktrees + source.git 含む）を丸ごと削除。
- (b) **in-lock で snapshot 0 の source.git 削除** — GC 後 `worktrees/` が空（locked snapshot が disk に無い drift）なら `source.git` と空 repo ディレクトリを削除。

## Plan of Work

### 1. pack GC（generations 非参照 pack の削除）

- `pack/_gen/opt/*/manifest.json` をすべて読み、`entries`（`opt/<id>`）の**和集合** = 参照 pack 集合を構築。
  （`install` は現+過去 RETAIN_GENERATIONS 世代に pruning 済みなので、on-disk manifest の和集合が参照集合。）
- **安全策**: manifest が1つも無ければ pack GC をスキップ（全削除防止。空 lock refuse と同思想）。
- `pack/_gen/{start,opt}/<id>/` で参照集合に無いものを削除（`install` cleanup ループ
  `packpathstate.rs:1265-1293` と同等）。
- anchor pack が無い `generations/<id>.lua` を刈り取り（`packpathstate.rs:1301-1317` と同等）。

### 2. repos GC（`garbage_collect` 拡張）

- lock 空 → refuse（既存）。各 repo ディレクトリ（cachedir = `repos/` からの相対パス）:
  - **cachedir が lock に無い** → repo 全体（`worktrees/` + `source.git` + ディレクトリ）を削除。【新(a)】
  - **cachedir が lock にある** → 既存 orphane snapshot 削除（locked `rev` に一致しないもの）。GC 後
    `worktrees/` が空なら `source.git` と空 repo ディレクトリを削除。【新(b)】
- 両動作を1回の `--gc` で同時実行。削除対象はログで明示（破壊的のため）。

### 注意点（文書化必須）

- 複数 lockfile を同一 `~/.cache/rsplug` に切り替えて使う運用では、`--gc` 実行時の lockfile に無い repo
  （他設定由来）も (a) で削除されるリスクがある。`--gc` は「渡した lockfile が唯一の正」とみなす。
- pack GC / repos GC とも「参照情報が空」なら refuse / skip する安全設計を維持する。

## Validation / Acceptance（実装時）

- pack GC: orphane pack 削除 / 参照 pack 保全 / manifest 無しで skip。
- repos GC: lock 外 repo 全削除 / in-lock orphane snapshot / drift で source.git 削除 / 空 lock refuse。
- 既存 `gc_tests`（`main.rs:413`）を拡張し、pack GC 用の manifest/on-disk pack フィクスチャを追加。

## Concrete files（実装時）

- `main.rs` — `garbage_collect` 拡張（pack GC 追加・repos GC の (a)/(b) 追加）、`gc_tests` 拡張。
- pack GC の manifest 読取りは `packpathstate.rs` の `GenerationManifest`/`retained_manifest_entries`
  相当のロジックを `--gc` 向けに再構築（current manifest 無しで on-disk manifest 全走査）。

---

# Revision Notes

- 2025-07-07: §A 初版（Phase 1/2 safe-now、Phase 3 defer）。
- 2026-07-07: §A Phase 1/2 実装完了。GC 逆変換バグを正方向変換で根本修正し `gc_tests` 追加。
- 2026-07-07..09: §B Phase 0-6c 実装（sym 廃止・copy 統一・dotgit・`AtomicU8` copy 戦略）。
- 2026-07-10: **§B Phase 7 完了**（tarball `.git` 廃止・`MaterializedRepo` enum・`dirty_diff_from_content`
  内容ハッシュ）。併せて libc 採用（reflink FFI）と E138 修正（`-i NONE`）を実施。実機検証済み。
- 2026-07-10: 旧 `PLANS.md`（§A）と `PLANS-copy-unification.md`（§B）を本ファイルに統合。§B の
  「ExecPlan は別ファイルに置く」Decision（2026-07-07）は、ユーザー指示（1つの PLANS.md に集約）により
  **上書き**。§C（`--gc` 拡張）を未実装プランとして追加。
- 2026-07-10: doc 盗み復活（read_dir sealed-dir で no-op 化していたのを個別ファイル展開で修復）。
  その副作用で autoload 系マージの非推移性が顕在化し EEXIST。install 堅牢化（dst 既存在→マージ/上書き）
  で暫定対応後、**Phase 8** で根本対応（doc 盗みをマージ前に移行＋`union_files` の sealed/子混在を
  推移的正規化、IO 最小）。install 堅牢化は safety net として残置。
