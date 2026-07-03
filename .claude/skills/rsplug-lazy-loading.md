---
name: rsplug-lazy-loading
description: Use this skill when changing, reviewing, or debugging rsplug.nvim lazy-loading behavior, generated pack output, PlugCtl, plugin IDs, merge behavior, or Neovim runtime integration.
---

# rsplug.nvim lazy-loading investigation

Use this skill when the task involves `on_event`, `on_ft`, `on_cmd`, `on_map`, Lua `require`, `lua_before`, `lua_after`, `lua_start`, `PlugCtl`, `LoadedPlugin::plugin_id()`, package merging, or generated pack output.

## Repository context

- Read `AGENTS.md` first.
- `rsplug` is an external Rust binary, not a Neovim plugin.
- Lua runtime files are generated from Sailfish templates in `crates/rsplug/templates/`.
- `PlugCtl` in `crates/rsplug/src/rsplug/entities/plugctl.rs` wires lazy triggers.
- `LoadedPlugin` and merge/ID behavior live in `crates/rsplug/src/rsplug/entities/packpathstate.rs`.
- Config parsing and plugin construction live under `crates/rsplug/src/rsplug/entities/`.

## Investigation workflow

1. Check the diff against the requested base, usually `origin/main`:
   - `git diff --stat origin/main...HEAD`
   - `git diff --cached --stat` when changes are staged.
2. Read the touched Rust files and the relevant generated Lua templates before forming a theory.
3. Build the binary with `cargo build` or run `cargo check`; do not use `cargo check -q`.
4. Create a temporary fixture under `/tmp` or `.tmp/` with tiny local git repositories. Avoid network-dependent plugin repos for regression checks.
5. Run `rsplug --install --lockfile <tmp>/rsplug.lock.json <tmp>/config.toml` with `HOME=<tmp>/home` so output is isolated.
6. Inspect generated files under `<tmp>/home/.cache/rsplug/site/pack/_gen/` and `manifest.json`.
7. Open Neovim with Neowright using the generated pack path:
   - `NVIM_APPNAME=rsplug-lazy-test neowright open --name <name> -- --clean -u NONE -i NONE --cmd 'set packpath^=<tmp>/home/.cache/rsplug/site'`
   - APPNAME is useful for clean Neovim tests because it isolates config/state/cache directories from the user's real Neovim profile.
8. Exercise each relevant lazy trigger through Neowright:
   - command: `neowright exec --name <name> '<Command>'`
   - event: `neowright exec --name <name> 'doautocmd <Event>'`
   - filetype: `neowright exec --name <name> 'setfiletype <ft>'`
   - mapping: `neowright keys --name <name> '<key>'`
   - Lua require: `neowright eval --name <name> "return require('<module>')"`
9. Assert structured state with `neowright eval`, not only screenshots. Check `require('_rsplug').loaded`, global marker variables, commands, keymaps, and messages.
10. Close the Neowright session at the end.

## Verification expectations

- Run the focused Neowright regression harness.
- Run `cargo test` when Rust behavior or tests changed.
- Run `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` before finalizing code changes.
- If you create `.neowright/` artifacts or temporary fixture files, leave only intentional tracked files in the repo.

## Common pitfalls

- Lazy behavior can pass in headless Lua tests but fail in a real TUI; use Neowright for keymaps and commands.
- `on_map` needs real key input; prefer `neowright keys` and then inspect state.
- Package IDs can change when hashing or merging changes; do not hardcode IDs in tests unless the test derives them from generated output.
- Isolate `HOME` and `packpath` so the user's real Neovim config and plugin cache do not affect results.
