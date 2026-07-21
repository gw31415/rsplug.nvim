# rsplug.nvim Claude context

This repository is rsplug.nvim, an external Rust binary that generates a Neovim pack directory and Lua runtime integration for fast deterministic plugin installs.

## Project rules

- Follow `AGENTS.md`; it is the canonical project context.
- Respond to the user in Japanese unless they explicitly request another language.
- Do not commit, push, or rewrite history unless explicitly asked.
- Do not read or print secrets.
- Prefer root-cause investigation over workaround fixes.
- Touch only the files needed for the task.

## Important commands

- Build/check: `cargo check`
- Tests: `cargo test`
- Clippy: `cargo clippy --workspace --all-targets -- -D warnings`
- Format check: `cargo fmt --all -- --check`
- Do not use `cargo check -q`.

## Neovim / Neowright

- Neowright is installed for this repository under `.agents/skills/neowright/SKILL.md` and mirrored for Claude under `.claude/skills/neowright.md`.
- Use Neowright for real Neovim TUI behavior, especially lazy-loading triggers, mappings, commands, autocmds, filetypes, Lua `require`, messages, and snapshots.
- For clean Neovim configuration isolation, prefer `NVIM_APPNAME=<throwaway-name>` together with `--clean -u NONE -i NONE`; APPNAME keeps config/state/cache paths separate from the user's real Neovim profile.
- Prefer named sessions: `neowright open --name <name> -- <nvim-args>`.
- Use `neowright eval`, `exec`, `keys`, `wait`, and `snapshot`; close sessions with `neowright close --name <name>`.
- Avoid headed mode unless explicitly requested.

## Lazy-loading model reminders

- `LazyRegistration` generates runtime control files and maps plugin IDs to lazy triggers.
- Supported triggers: `on_event`, `on_ft`, `require`, `on_cmd`, `on_map`, `lua_after`, `lua_before`, `lua_start`, `build`.
- Lazy plugin IDs are derived from `LoadedPlugin::plugin_id()`; changes to hashing/merge behavior can affect generated package names and trigger maps.
- When investigating lazy-loading changes, compare generated pack output and then exercise the behavior in a real Neovim session with Neowright.
