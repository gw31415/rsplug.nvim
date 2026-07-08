# sym 廃止 / pack copy 統一 ExecPlan

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document follows the repository-level `PLANS.md` convention described in `AGENTS.md` (ExecPlans section). It is a separate ExecPlan from `PLANS.md` (lock/cache synchronization) because it concerns a different subsystem.


## Purpose / Big Picture

rsplug.nvim は2つの系を持つ:

- **系A（出力）**: pack directory（`{packpath}/pack/_gen/opt/{id}/`, `generations/{ctl}.lua`, `init.lua`）。Neovim が読む。`AGENTS.md` が謳う「deterministic・portable・Nix 向き」な出力。
- **系B（キャッシュ）**: `repos/<repo>/source.git/`（bare object store）＋ `repos/<repo>/worktrees/{snapshot_key}/`（git worktree な snapshot）、`rsplug.lock.json`。マシン固有のキャッシュ・状態。

現在、`to_sym`（TOML `sym` 明示、または `build`/`lua_build`/`lua_post_update` による自動有効化）のとき、**系A の pack が系B の snapshot を symlink で参照する**（`HowToPlaceFiles::RepoSnapshotLink`）。これが「両者を繋ぐ sym」である。

問題:

- pack は本来ポータブルな出力なのに、実体が系B（`~/.cache/rsplug/repos/...`）を symlink 参照し、自己完結しない。
- Nix store に pack を置くとリンク先が store の外で切れる。
- lock/repos の同期・GC が sym を通じて pack と絡む（snapshot を GC すると pack 側が dangling になりうる）。

本 plan の目的: **`RepoSnapshotLink` を廃止し、pack に常にファイル copy（実体）を置く**ことで pack と repos を完全に分離し、pack を自己完結・Nix safe にする。worktree 方式が導入済み（snapshot は独立 worktree）なので、copy を効率化しつつ sym を外せる。


## Progress

- [x] (2026-07-07) 現状分析: `HowToPlaceFiles::RepoSnapshotLink`（`packpathstate.rs:141`）, `to_sym()`（`config.rs:65-70`）, `DirectoryExtractionType::Symlink`, `symlink_plugin_dir` を特定。
- [x] (2026-07-07) 設計検討: sym 削除の3案（copy 統一 / pack 直 worktree / copy 維持）を比較 → **案1（copy 統一）**を採用。
- [x] (2026-07-07) 核心課題特定: `to_sym()` が `build`/`lua_build`/`lua_post_update` で自動 true。理由は `CopyEachFile` が `git ls_files`（追跡ファイルのみ）で build 成果物（untracked）を含まないため。sym は worktree 全体を見せて成果物を伝えていた。
- [x] (2026-07-07) 成果物 copy 方針決定: `git ls-files` ＋ `git ls-files --others --exclude-standard`（.gitignore 尊重）。
- [x] (2026-07-07) 影響なし確認: Lua runtime（`templates/*.lua`, `*.stpl`）は `snapshot`/`repos`/`cache` に非依存・pack 内完結。`init.lua → generations/{ctl}.lua` sym は系A内で閉じる（維持）。`to_sym` フィールド削除も serde（`deny_unknown_fields` 無し）で既存 TOML は無視される。
- [x] (2026-07-07) Phase 0: `yank` の `hard_link` に ExDev で copy フォールバック追加（`packpathstate.rs`）。commit b272ade。
- [x] (2026-07-07) Phase 1: `Repository::ls_files_with_untracked`（`ls-files` + `--others --exclude-standard`）追加。`Plugin::load` が `has_build` で切り替え。
- [x] (2026-07-07) Phase 2: `RepoSnapshotLink`/`DirectoryExtractionType::Symlink`/`symlink_plugin_dir`/`collect_doc_files_from_root`/`to_sym`/`manually_to_sym` を一括廃止。`Plugin::load` は常に `CopyEachFile`。commit a2ffd7f。
- [ ] Phase 3: テスト整理・実機検証（Neowright）。コード側テスト（`snapshot_link_id_*`/`to_sym` 系）は Phase 2 で削除済み。build 成果物 copy の実機検証が残り。


## Surprises & Discoveries

- Observation: `to_sym()` はユーザ明示（`sym`）だけでなく、`build`/`lua_build`/`lua_post_update` のいずれかがあれば自動的に true。つまり build を使うプラグインは強制的に sym。Evidence: `config.rs:65-70`。
- Discovery: sym の本来の役割は「build 成果物（untracked）を pack に伝えること」。`CopyEachFile` は `git ls_files`（追跡のみ）なので build 成果物が含まれず、sym が worktree 全体を見せて代替していた。Evidence: `plugin.rs:557`（`ls_files`）, `plugin.rs:576-580`（`if to_sym`）。
- Observation: Lua runtime は pack 内の相対パスのみで動作し、`snapshot`/`repos`/`cache` への依存ゼロ（`templates/` 配下に該当参照なし）。つまり sym 廃止で Lua は壊れない。
- Observation: `init.lua → generations/{ctl}.lua` の sym は系A内で閉じ、resolve+`:h:h` で pack 内完結する（テスト `init_template_resolves_symlink_and_goes_up_two_levels`）。これは「pack↔repos を繋ぐ sym」ではなく維持対象。
- Observation: `yank`（`packpathstate.rs:440-447`）は非 macOS で `hard_link`、フォールバックなし。pack が Nix store 等 別FS だと失敗する。copy 統一のポータビリティ前提としてフォールバックが必須。
- Observation: `CacheConfig`（`config.rs:51`）に `#[serde(deny_unknown_fields)]` が無い。よって `to_sym`/`manually_to_sym` を削除しても、既存 TOML の `sym = true` は無視されエラーにならない。


## Decision Log

- Decision: sym（`RepoSnapshotLink`）を廃止し、pack に常に copy 実体を置く（案1 copy 統一）。
  Rationale: pack を自己完結・ポータブル・Nix safe にする。`AGENTS.md` の「deterministic portable output」に合致。worktree は系B（キャッシュ）の効率化に専念させる。案2（pack 直 worktree）は pack に `.git` が入り Nix read-only と衝突、案3（copy 維持・sym のみ廃止）は `hard_link` 別FS問題が残るため不採用。
  Date: 2026-07-07.

- Decision: build 成果物（untracked）の pack copy は `git ls-files` ＋ `git ls-files --others`（**.gitignore 無視**、ignored ディレクトリは再帰列挙）で列挙する。
  Rationale: 当初は `--exclude-standard`（.gitignore 尊重）だったが、実機検証（blink.cmp）で build 成果物が `.gitignore` 対象（`target/`）にあり pack に届かないことが判明。旧 sym 版は worktree 全体参照で見えていたため、copy 版でも `.gitignore` 無視で同等にする（2026-07-07 の `--exclude-standard` Decision は取り消し）。重量 copy は clonefile/hard_link で軽減。実行時の変更は pack でなく Neovim の XDG パス（`~/.local/share/nvim`, `~/.cache/nvim`）が標準。
  Date: 2026-07-08.

- Decision: `yank` の `hard_link` に ExDev（別FS）検出で copy フォールバックを入れる。
  Rationale: copy 統一のポータビリティ（Nix store 配置）を成立させる前提。これが無いと別FSで install が失敗する。
  Date: 2026-07-07.

- Decision: `init.lua → generations/{ctl}.lua` sym は維持する。
  Rationale: 系A内で閉じ、ポータビリティを損なわない。本 plan の対象（pack↔repos の sym）ではない。
  Date: 2026-07-07.

- Decision: 本 ExecPlan は `PLANS.md`（lock/cache 同期）とは別ファイル `PLANS-copy-unification.md` に置く。
  Rationale: トピックが異なり、1ファイルに混ぜると構造が乱れる。
  Date: 2026-07-07.

- Decision: macOS の pack copy は自前 `clonefile(2)` でなく `tokio::fs::copy`（`std::fs::copy`）に任せる。一時的に自前 clonefile を入れたが revert した。
  Rationale: 最近の Rust では `std::fs::copy` が macOS/APFS で `fclonefileat`/`clonefile`（CoW）を使用し、HFS+ では `fcopyfile` にフォールバックする（std が自動切替）。自前 clonefile と同等で、フォールバック含め std に委ねる方が保守負荷が低い。非 macOS は `hard_link` → `copy`（ExDev）を維持（hard_link が走るのは非 macOS 同一FS のみ）。
  Date: 2026-07-07.


## Outcomes & Retrospective

- (2026-07-07) Phase 0-2 実装完了。`pack` は `RepoSnapshotLink` を廃止し常にファイル copy で自己完結（`repos/` への symlink なし）。`init.lua → generations/<id>.lua` の pack 内 sym のみ維持。
- (2026-07-08) `.gitignore` 無視に変更（実機検証で blink.cmp の `target/` が pack に届かない問題を解決）。`ls_files_with_untracked` は ignored ディレクトリ（`target/` 等）の中身を再帰 copy。
- (2026-07-08) 実機検証（隔離 HOME, cmp.toml, `--locked`, build 済み snapshot 再利用）: `find pack/_gen -type l` = **0件**（sym 廃止確認）。blink.cmp の `target/release/libblink_cmp_fuzzy.dylib` が pack に copy されることを確認。
- 検証: `cargo test --workspace` 全パス（71件）、`cargo clippy --workspace --all-targets -D warnings` warning なし、`cargo fmt --check` クリーン。
- 残課題: Neowright での Neovim 実機確認（`:helptags`・lazy load が壊れないこと）。ネイティブライブラリが pack に届いたため動作期待大。


## Context and Orientation

Key terms:

- **系A（pack 出力）**: `{packpath}/pack/_gen/opt/{id}/`（プラグイン実体）, `generations/{ctl}.lua`（世代ローダー）, `init.lua`（→ `generations` の sym）。
- **系B（キャッシュ）**: `repos/<repo>/source.git/`（bare object store）, `repos/<repo>/worktrees/{snapshot_key}/`（git worktree snapshot）, `rsplug.lock.json`。
- **`HowToPlaceFiles`** (`packpathstate.rs:136`): `CopyEachFile`（ファイル copy）と `RepoSnapshotLink`（snapshot へ symlink）。本 plan は `RepoSnapshotLink` を廃止し `CopyEachFile` に統一。
- **`to_sym()`** (`config.rs:65`): `manually_to_sym || build || lua_build || lua_post_update`。`RepoSnapshotLink` を選ぶ判定。
- **`DirectoryExtractionType`** (`packpathstate.rs:471`): `Files`（copy）と `Symlink`（sym）。本 plan は `Symlink` を廃止。

Key files:

- `crates/rsplug/src/rsplug/entities/packpathstate.rs` — `HowToPlaceFiles`（136）, `DirectoryExtractionType`（471）, `LoadedPlugin::snapshot_root`（213）, `PackPathState::install`（661）, `symlink_plugin_dir`（496）, `yank` の copy/`hard_link`（440-447）。
- `crates/rsplug/src/rsplug/entities/plugin.rs` — `Plugin::load` の `if to_sym`（576-580）, `ls_files`（557）。
- `crates/rsplug/src/rsplug/entities/config.rs` — `CacheConfig.manually_to_sym`（55）, `to_sym()`（65）。
- `crates/rsplug/src/rsplug/entities/plugctl.rs` — `overwrite_files` の `RepoSnapshotLink` arm（622-627）, `collect_doc_files_from_root`（27）。


## Plan of Work

### Phase 0 — `hard_link` copy フォールバック（前提）

**Goal**: `yank` が別FS（Nix store 等）で `hard_link` 失敗時に copy にフォールバックし、pack 実体化が常に成功するようにする。

`packpathstate.rs:440-447` の `copy` を、`hard_link` 失敗（`ErrorKind::CrossesDevices` / `ExDev` 等）時に `tokio::fs::copy` にフォールバックするよう変更する。この Phase は sym と無関係に単独で有用（現状でも別FSで壊れる）。

### Phase 1 — build 成果物 copy（sym の代替基盤）

**Goal**: build プラグイン（`build`/`lua_build`/`lua_post_update`）の pack copy が、未追跡の build 成果物を含むようにする。

`plugin.rs:557` の `repository.ls_files()` を、`ls-files` ＋ `ls-files --others --exclude-standard` に拡張する（`util.rs` の git ヘルパにメソッド追加）。この Phase では `RepoSnapshotLink` と並存させ、build プラグインを copy に切り替えても成果物が届くことを先に確認する。

注意: `snapshot_key` は `dirty_diff` を identity（ひいては `plugin_id`）に含むため、build 成果物の差分は `plugin_id` で区別される。copy 時に untracked を含めれば、identity と実体が一致する。

### Phase 2 — `RepoSnapshotLink` / `to_sym` 廃止

**Goal**: sym 関連のコードを一括削除し、常に `CopyEachFile` に統一する。

削除対象:

- `HowToPlaceFiles::RepoSnapshotLink`（`packpathstate.rs:141`）と、`PartialEq`/`Eq`/`Hash`（153-181）, `snapshot_root`（213-226）, `Add`/merge（389-394）, `insert`（646-655）, `install`（791-808）の各 arm。
- `DirectoryExtractionType::Symlink`（473）と `install` の分岐（718-722, 761-810）。
- `symlink_plugin_dir`（496-503）。
- `to_sym()`（`config.rs:65-66`）, `manually_to_sym`（55）, TOML `sym`（rename）。
- `collect_doc_files_from_root`（`plugctl.rs:27`）と `overwrite_files` の `RepoSnapshotLink` arm（622-627）, panic arm（480-481）。
- `plugin.rs:576-580` の `if to_sym`（常に `CopyEachFile`）。

### Phase 3 — テスト整理・実機検証

- `snapshot_link_id_is_independent_of_absolute_target`（`packpathstate.rs:1051-1075`）削除/変更。
- `to_sym` 系テスト（`config.rs:380`, `plugin.rs:1310`）更新。
- build 成果物 copy の実機検証: Neowright で build プラグイン（`make` 等）の成果物が pack に届き、`:helptags`・lazy load が壊れないことを確認。
- Acceptance 1-6 を検証。


## Concrete Steps

Unless noted, all commands run from the repository root.

実装時に各 Phase の具体的編集箇所・コマンドを埋める。各 Phase 後に `cargo test --workspace` / `cargo clippy --workspace --all-targets` / `cargo fmt --check` を実施（`AGENTS.md`: `cargo check -q` 禁止）。


## Validation and Acceptance

1. `to_sym=true`（TOML `sym`）を指定しても copy になる（sym が作られない）。pack 配下にプラグイン実体の symlink が無い。
2. `build`/`lua_build`/`lua_post_update` を持つプラグインの pack copy に、build 成果物（`.gitignore` 対象の `target/` 等を含む全 untracked）が含まれる。`:helptags` で help が生成される。
3. pack を別FS（tmpdir 等で模擬）に install しても `hard_link` 失敗で copy にフォールバックし成功する。
4. pack が `repos/` 配下を一切参照しない（`find pack -type l` で `generations`/`init.lua` 以外に symlink が無い）。
5. `rsplug.lock.json` と repos の同期・`--gc` が、pack への影響なく機能する（`PLANS.md` lock/cache の Acceptance と両立）。
6. `cargo test --workspace` / `cargo clippy --workspace --all-targets -D warnings` / `cargo fmt --check` 通過。


## Idempotence and Recovery

- copy 統一はべき等。複数回実行しても同じ pack ができる（cache 状態不変なら）。
- `to_sym` フィールド削除後、既存 TOML の `sym = true` は無視される（エラーなし）。ユーザが sym を期待していた場合は無言で copy になる（必要なら警告を検討）。
- 復元: sym 廃止前に pack が repos を symlink 参照していた場合、廃止後は copy 実体になる。古い symlink pack は `install` の既存ロジック（`remove_dir_all` → copy）で置き換えられる。


## Interfaces and Dependencies

- `HowToPlaceFiles` は `CopyEachFile` のみに単純化。`RepoSnapshotLink` 廃止。
- `DirectoryExtractionType` は `Files` のみ。`Symlink` 廃止。
- `LoadedPlugin::snapshot_root` は `CopyEachFile` の `FileSource::Directory` から取得（`RepoSnapshotLink` 廃止後も機能）。
- `Plugin::load` は常に `CopyEachFile` を返す。
- `CacheConfig` から `manually_to_sym`/`to_sym` 削除。
- `PackPathState::install` の symlink 分岐削除。

外部依存の変更なし。Lua runtime への影響なし（pack 内完結）。


## Revision Notes

- 2026-07-07: 初版。設計検討完了（案1 copy 統一、`ls-files`+untracked、`hard_link` フォールバック）。Phase 0/1 を safe-now、Phase 2 を廃止、Phase 3 を実機検証とする。実装は未着手。
