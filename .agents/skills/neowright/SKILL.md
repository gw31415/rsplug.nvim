---
name: neowright
description: Reproduce or inspect interactive Neovim TUI behavior using Neowright when live UI observation is needed.
---

# Neowright

Use the installed CLI help for current flags. Use a unique named session and close sessions opened for the task, including after failures.

For isolated rsplug tests, set `NVIM_APPNAME=<throwaway-name>` and use `--clean -u NONE -i NONE`; use the fixture's generated `packpath` when testing generated packs.

- Open: `neowright open --name <name> -- <nvim-args>`
- Keys: `neowright keys --name <name> "<keys>"`
- Ex command: `neowright exec --name <name> "<command>"`
- Lua state: `neowright eval --name <name> "return <expression>"`
- Wait for state: `neowright wait --name <name> "return <condition>"`
- Capture: `neowright snapshot --name <name>`
- Close: `neowright close --name <name>`

`keys --pty` is an escape hatch for blocked RPC; it accepts terminal notation, not all Neovim key notation. Use headless sessions and snapshots by default. Attach a visible UI when the user asks; attached clients share the editor state and can affect its dimensions or input.
