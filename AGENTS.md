# AGENTS

This repository is for **rsplug.nvim**, a Neovim plugin manager that is implemented
as an external binary (not a Vim/Neovim plugin). The project focuses on fast,
parallel Git-based installs/updates and deterministic, portable outputs that can
fit into Nix-based workflows.

## Project intent

- External binary instead of a plugin to avoid restart-required reload issues and
  to make updates independent of running Neovim.
- Fast installs/updates via shallow (depth=1) Git operations in parallel.
- Deterministic and portable output layout: input TOML + lock file(s) -> output
  pack directory (single package).
- Designed to be usable as a build tool in Nix workflows.
- Merge simultaneously loaded plugins to reduce `runtimepath` size.
- Allow configuration-only scripts (no plugin) to be embedded as dependencies.

## Implementation languages

- Rust for the core implementation.
- Lua for runtime integration in Neovim.
- Lua script templates are generated using the `sailfish` template engine.

## Configuration & output model

- Inputs: one or more TOML files plus a lock file.
- Output: a single Neovim `pack` directory (single package).
- Lock file management (read/write) is part of the workflow.
- Lock file format is **JSON** (`rsplug.lock.json`) with a `version` field and
  per-repository locked resources (`type`, `rev`).

## Implemented status (synced with resolved GitHub issues + current log implementation)

- TOML parsing.
- Parallel install/update.
- Lock file read/write.
- Support for config-only scripts (no plugin).
- Loading related scripts for plugins.
- `on_cmd`, `on_map`, `on_ft`, `on_event`, and Lua `require` lazy-loading.
- `build` hooks executed after install/update with build-success caching.
- Dependency co-loading via `with`.
- Helptags generation during install.
- Merge behavior for simultaneously loaded plugins to reduce `runtimepath`.
- Fixes for historical issues in lazy map loading and duplicate mapping behavior.
- Fixes for edge-case deadlocks (including zero-plugin install paths).
- Rich installer/log UX:
  - grouped config discovery summaries,
  - fetch stage/progress reporting,
  - build output progress lines,
  - improved install/yank/help log cleanup and formatting.

## Lazy-loading model

A `PlugCtl` structure aggregates all plugin settings and generates a single
plugin that:

- Adds runtime paths and loads scripts (e.g. `plugin/`, `ftdetect/`) on events.
- Executes `:packadd` when needed.
- Wires plugin-related scripts and dependencies for lazy loading.

Supported lazy-loading triggers and scripts:

- `on_event`: autocmd events
- `on_ft`: filetype
- `require`: auto-load on Lua `require`
- `on_cmd`: command execution
- `on_map`: key mapping
- `lua_after`: run after plugin load
- `lua_before`: run before plugin load
- `lua_start`: run at Neovim startup
- `build`: run subprocess after install

## Resolved design direction

- The lock file is not TOML-embedded config. Current direction is:
  TOML config as source of intent + JSON lock file for reproducible revisions.

## Deferred / non-goals (current)

- Dedicated `ftplugin` configuration field support is intentionally deferred
  for now (workaround: put files under `after/ftplugin` directly).

## Notes

- Don't run `cargo check -q`
