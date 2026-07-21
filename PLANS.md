# Runtime hot paths ExecPlan

Only unimplemented work belongs here. Delete a phase's recipe after it lands;
Git history owns completed implementation details.

## Goal

Optimize the generated Neovim runtime without changing lazy-load semantics:

- `on_event`: replace the old×current autocmd comparison with O(A) indexing.
- `on_ft`: use generation-manifest paths; a v2 manifest performs zero
  `nvim_get_runtime_file` calls and no global path diff.
- Lua `require`: keep O(1) root lookup, store nothing for unrelated modules,
  and remove rsplug's searcher after every registered root is satisfied.
- `on_map`: keep the existing reverse indices, but remove the permanent
  `ModeChanged` watcher after all reachable configured modes are initialized.
- A package is loaded at most once. A package loaded by another trigger has no
  ftplugin sourced again.
- Identical input produces deterministic output. Control package IDs may change
  because generated Lua and the manifest schema intentionally change.

## Read first; do not reimplement completed work

- Trigger maps and rendering:
  `crates/rsplug/src/rsplug/entities/lazy_registration.rs`, especially
  `From<LazyRegistration> for Vec<LoadedPlugin>`.
- Quadratic autocmd helper:
  `templates/lua/_rsplug/init.stpl::get_new_autocmds`; callers are
  `on_event.stpl` and `on_ft.lua`.
- Six global ft lookups and quadratic path diff:
  `templates/lua/_rsplug/on_ft.lua` and `get_ft_runtime_file`.
- Permanent require searcher: `templates/plugin/on_lua.lua`. The required
  root→package index already exists as `luam2pkgid`.
- Permanent map watcher: `templates/plugin/on_map.lua`. `on_map/init.lua`
  already has `setup_done`, `pattern_modes`, `pattern_ids`, and `id_patterns`;
  preserve them.
- Generation manifest creation/write:
  `pack_plan.rs::GenerationManifest` and `PackPlan::install`.
- Event wrappers already use `once = true`; mapping reverse lookup is already
  implemented. Neither is a task in this plan.

## Invariants

- Generated Lua/JSON contains no build-machine absolute path. Resolve paths
  relative to the loaded `_rsplug` script so packs remain movable/Nix-safe.
- Render/serialize with BTree order, explicit sorting, and stable deduplication.
- Preserve `packadd` idempotence, hook/dependency/`on_source` order, `User`
  lookup through `ctx.match`, and the `InsertCharPre` and
  `BufNew`/`BufReadCmd` workarounds.
- Preserve ft order: exact `ftplugin/{ft}.{vim,lua}`, then
  `ftplugin/{ft}_*.{vim,lua}`, then immediate `.vim`/`.lua` children of
  `ftplugin/{ft}/`.
- Keep `GenerationManifest.entries` unchanged; retention/cleanup uses it.
- Keep the current Neovim baseline and `package.loaders`. Do not redesign
  install/fetch scheduling, plugin IDs, mapping semantics, or config validation.
- `parse_mode` can reach only `''`, `n`, `o`, `x`, `v`, `s`, `i`, `c`, and `t`.
  Unsupported configured characters remain a separate validation issue and
  must not keep a watcher alive.

## R0 — Characterize before editing

Add the headless fixture described under Validation first. On current code,
record behavior for shared events, all ft path forms, root/submodule require,
and duplicate maps. Keep semantic assertions; API-count assertions may initially
describe the slow behavior and must be tightened in the relevant phase.

## R1 — Generation manifest v2

Files: `pack_plan.rs`, `lazy_registration.rs`, `_rsplug/init.stpl`.

Use this logical private model (`#[serde(default)]` on `runtime` so retained v1
manifests still deserialize):

    {
      "version": 2,
      "entries": ["opt/<id>", ...],
      "runtime": {
        "ftplugin": {
          "lua": {
            "<id>": [
              "opt/<id>/ftplugin/lua.vim",
              "opt/<id>/ftplugin/lua_extra.lua",
              "opt/<id>/ftplugin/lua/settings.lua"
            ]
          }
        }
      }
    }

Rust type for `ftplugin`:
`BTreeMap<String, BTreeMap<String, Vec<String>>>`.

Implementation steps:

1. Add a narrow `LazyRegistration` method that clones `ft2pkgid` into string
   keys before `PackPlan::install` consumes `self.ctl`; expose no other maps.
2. Finish package copy and rename new packages into `pack/_gen/opt` first. Then,
   before writing control manifests or swapping `init.lua`, build the index from
   `pack/_gen/opt/<id>` for only registered `(ft,id)` pairs. This location works
   for new and reused packages.
3. Inspect only: exact `.vim` then `.lua`; immediate `<ft>_*` `.vim`/`.lua`
   children sorted by relative path; immediate `.vim`/`.lua` children of the
   `<ft>/` directory sorted by relative path. Append in those three groups and
   stable-dedup. Compare literal path components in Rust, not Lua patterns.
   A missing `ftplugin/` or `<ft>/` directory means no matches. Bounded
   file/one-directory symlink following is allowed; any other read error aborts
   publication rather than creating an incomplete v2 manifest.
4. Move manifest serialization from its current pre-copy location to after the
   index build. Keep `entries` sorting and the later publication flow.
5. In `_rsplug/init.stpl`, lazily cache decoded manifest, generation root, and
   an `entries` set. Add `get_ft_runtime_files(ids, ft)`. For v2, return only
   indexed paths for those IDs, joined to generation root. Reject absolute
   paths, `..`, and paths whose `opt/<id>` is absent from `entries`. Derive the
   generation root as `fnamemodify(manifest_path, ':h:h:h')` because the file is
   `<generation>/opt/<control-id>/manifest.json`.
6. Retain the current three-query function, clearly named as the v1/corrupt
   manifest compatibility fallback. A valid v2 manifest never calls it.

Tests: v2 JSON, retained-v1 deserialize, new+reused packages, all three match
groups, unrelated extensions, relative-path validation, sort/dedup, symlinks,
and scan failure before `init.lua` publication.

## R2 — Linear autocmd discovery and owned event loaders

Files: `plugin/on_event.stpl`, `lua/_rsplug/on_event.stpl`,
`lua/_rsplug/init.stpl`, `lua/_rsplug/on_ft.lua`, `lazy_registration.rs`.

1. Replace `get_new_autocmds` with:
   - `index_autocmds(items, excluded_groups)`: one pass producing
     `by_id[id] = true` and the set of pre-existing group IDs;
   - `new_autocmds(items, before, excluded_groups)`: one pass dropping old IDs
     and rsplug-owned groups while preserving Neovim order.
   Neovim 0.9 autocmd records have IDs; delete the old property-comparison loop.
2. Keep one `nvim_get_autocmds` call before and one after `packadd`. Neovim has
   no execute-by-autocmd-ID API; the goal is O(A) Lua diffing, not zero queries.
3. Put generated event loaders in deterministic augroup
   `rsplug.runtime.on_event`, give them descriptions, retain `once = true`, and
   register every returned autocmd ID in the on-event module.
4. Callback entry must `pcall(nvim_del_autocmd, ctx.id)` and remove its registry
   entry before `packadd`, preventing nested delivery of the same trigger.
5. Group newly discovered autocmds by group ID and replay each new group once
   with the existing context. Never replay rsplug's group. For a groupless
   autocmd or one added to a pre-existing group, do not globally replay all
   handlers (that duplicates unrelated callbacks). If it cannot be executed
   individually, defer it to the next natural event and name the test to state
   this limitation.
6. `on_ft.lua` must reuse these helpers for `Syntax`, `BufEnter`, and
   `BufWinEnter`; no second diff implementation.
7. Move triggered IDs to a local and delete their `event2pkgid` entry before
   loading. Preserve `ctx.match`/`ctx.event`, context fields, and workarounds.

Tests: 100 old autocmds, a new group, a pre-existing group, a groupless handler,
nested event delivery, `User`, both workarounds, no unrelated double execution,
and removal of the rsplug loader ID.

## R3 — Manifest-driven filetype load

Files: `pack_plan.rs`, `lazy_registration.rs`, `_rsplug/init.stpl`, `on_ft.lua`.

After R1 and R2, implement `on_ft.load(pkgids, ft)` exactly as follows:

1. `local ctl = require '_rsplug'`.
2. Mark each on-ft-local unseen ID processed. Add it to `new_ids` only if
   `ctl.loaded[id]` was false before loading. This prevents double source when
   another trigger loaded the package first.
3. Return if `new_ids` is empty.
4. Snapshot replay-event autocmds with the R2 helper.
5. Resolve `paths = ctl.get_ft_runtime_files(new_ids, ft)` before `packadd`.
6. `ctl.packadd` each new ID in order.
7. Source each path once using
   `vim.cmd('source ' .. vim.fn.fnameescape(path))`; pack roots may contain
   spaces or command separators.
8. Discover/replay only new `Syntax`, `BufEnter`, and `BufWinEnter` groups.

Remove global runtime lookup and list diff from the v2 path. Keep the processed
ID table so the second buffer is constant-time.

Tests: exact/suffix/subdirectory `.vim` and `.lua`, no match, multiple IDs, a
merged package, `merge = false`, a pack path with spaces, second buffer, and a
package preloaded through another trigger.

## R4 — Retire the Lua-module searcher

Files: `lazy_registration.rs`, `on_lua.stpl`, `plugin/on_lua.lua`,
`_rsplug/init.stpl`.

1. Keep `luam2pkgid`. Derive deterministic `pkgid2luam` while rendering;
   sort/dedup each ID's roots. Render both indices.
2. The data module owns `pending_roots`, `remaining_roots`, `pkgid2luam`, and
   optional `on_packadd`. A root is satisfied only when every mapped ID exists
   in `_rsplug.loaded`; decrement its count once.
3. The searcher keeps its own function reference. It extracts the root, returns
   `nil` immediately for a non-pending root without mutating state, guards only
   registered recursive loads, packadds the root's IDs, then reconciles it.
   Preserve the current `package.loaded[mod_name]` closure; otherwise return
   `nil` so standard loaders find the newly added runtime path. Clear the
   recursion guard on both success and error, then rethrow the original error.
4. Install `state.on_packadd(id)`. At the successful end of `_rsplug.packadd`,
   call it only when `package.loaded['_rsplug/on_lua']` already exposes it; do
   not require the module for notification. It checks only
   `pkgid2luam[id]`. Reconcile once when installing the searcher for IDs loaded
   earlier by another trigger.
5. At zero roots, schedule removal once with `vim.schedule`. In that callback,
   find the exact function identity in `package.loaders`, remove it, and clear
   `on_packadd`. Never remove by saved numeric position or mutate the loader
   list during the current `require` iteration.

Tests: unrelated names without state growth, root/submodule, recursion during
packadd, one ID with multiple roots, one root with multiple IDs, other-trigger
satisfaction, unknown-module error, successful final require, and exact
searcher removal after scheduled work drains.

## R5 — Retire mapping observers

Files: `lazy_registration.rs`, `plugin/on_map.lua` (make it `.stpl` if useful),
`lua/_rsplug/on_map/init.lua`.

1. Render configured modes. Build `pending_modes` from only the reachable modes
   listed in Invariants, so an unsupported config character cannot pin the
   watcher forever.
2. Create augroup `rsplug.runtime.on_map`; retain returned `ModeChanged` and
   `VimEnter` IDs. Set `VimEnter` to `once = true`.
3. On `ModeChanged`, parse only `vim.v.event.new_mode`, inspect at most the
   returned mode characters in `pending_modes`, and return without requiring a
   mode module when none match. Never iterate every mapping/configured mode.
4. First setup of a reachable mode removes it from `pending_modes`. At zero,
   delete the owned augroup by ID; this removes `ModeChanged` and any not-yet-
   fired `VimEnter` loader together. Cleanup is `pcall`-guarded and idempotent.
5. Preserve `parse_mode`, expr returns, termcode/key replay flags, and existing
   reverse indices. Centralize pattern removal so it clears `pattern_modes`,
   `pattern_ids`, and all affected `id_patterns` entries.

Tests: normal/operator overlap, visual/select families, insert/command/terminal,
all-modes `''`, shared pattern across modes/IDs, preloaded ID, special-key replay,
unrelated transitions, and observer absence after reachable modes are set up.

## Validation

Use actual rendered templates, not copied Lua implementations:

- Add a `#[cfg(test)]` fixture renderer in `lazy_registration.rs`. It writes a
  control package plus small fake packages to `tempfile`.
- Add `crates/rsplug/tests/runtime_hot_paths.lua` for behavior assertions.
- A Rust test runs `nvim --headless -u NONE -i NONE -n -c 'luafile ...'
  -c 'qa!'` and prints stdout/stderr on failure. Missing `nvim` is a clear test
  failure, not a silent skip.
- Use Neowright only if an interactive/TUI failure cannot be explained
  headlessly; use a named session and close it afterward.

Stable pass/fail instrumentation:

- v2 `on_ft`: zero `nvim_get_runtime_file` calls;
- event diff: one before and one after autocmd query, no nested comparison;
- 10,000 unrelated requires: pending/in-progress sizes unchanged;
- all roots satisfied: rsplug searcher absent after `vim.wait`;
- unrelated mode: no mode module loaded; all reachable modes set up:
  rsplug `ModeChanged` autocmd absent.

Add a non-gating `vim.uv.hrtime` mode: five samples for 1,000 autocmds, 1,000
ft files, 10,000 unrelated requires, and 10,000 unrelated mode changes. Write
scale, iterations, median, p95, and API counts as JSON under `target/`. Compare
on one machine; CI uses structural gates, not flaky wall-clock thresholds.

After every phase:

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

Never run `cargo check -q`.

## Progress

- [ ] R0 characterization fixture exists and passes.
- [ ] R1 manifest v2 and deterministic ft index pass Rust tests.
- [ ] R2 linear autocmd discovery/owned loaders pass behavior tests.
- [ ] R3 manifest-driven ft loading passes behavior tests.
- [ ] R4 root reconciliation/searcher retirement passes behavior tests.
- [ ] R5 mapping observer retirement passes behavior tests.
- [ ] Workspace tests, clippy, fmt, structural gates, and benchmark report pass.
