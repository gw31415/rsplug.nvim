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

## Validation

Use actual rendered templates, not copied Lua implementations:

- Add a `#[cfg(test)]` fixture renderer in `lazy_registration.rs`. It writes a
  control package plus small fake packages to `tempfile`.
- Add `crates/rsplug/tests/runtime_hot_paths.lua` for behavior assertions.
- A Rust test runs `nvim --headless --clean -i NONE -n -c 'luafile ...'
  -c 'qa!'` (with XDG dirs isolated under the pack temp root) and prints
  stdout/stderr on failure. Missing `nvim` is a clear test failure, not a silent
  skip. `--clean` is used instead of `-u NONE` because `-u NONE` disables
  Neovim's filetype→ftplugin chain that `on_ft` relies on; macOS `/var`↔
  `/private/var` packpaths are canonicalized so the control package is not
  double-registered on `runtimepath`.
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

- [x] R0 characterization fixture exists and passes.
- [x] R1 manifest v2 and deterministic ft index pass Rust tests.
- [x] R2 linear autocmd discovery/owned loaders pass behavior tests.
- [x] R3 manifest-driven ft loading passes behavior tests.
- [x] R4 root reconciliation/searcher retirement passes behavior tests.
- [x] R5 mapping observer retirement passes behavior tests.
- [x] Workspace tests, clippy, fmt, structural gates, and benchmark report pass.
