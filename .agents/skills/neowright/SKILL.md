---
name: neowright
description: Use this skill when debugging, reproducing, automating, or inspecting Neovim UI behavior with Neowright. Trigger whenever the user mentions Neovim TUI issues, plugin or config debugging, floating windows, completion menus, keymaps, diagnostics, snapshots, sessions, or asks an agent to drive or inspect a real Neovim instance.
---

# Neowright

Use Neowright to automate and inspect a real Neovim TUI session from outside Neovim. Neowright is a standalone CLI harness, not a Neovim plugin, MCP server, or general-purpose terminal automation framework.

## When To Use

Use this skill when the task depends on real interactive Neovim behavior:

- Reproducing plugin or configuration issues that only appear in the TUI.
- Inspecting floating windows, completion menus, diagnostics, messages, splits, or layout behavior.
- Driving mappings or key sequences with Neovim-style key notation.
- Waiting for asynchronous UI state before inspecting results.
- Capturing a Snapshot of the visible terminal grid for later review.

Do not use Neowright for tasks that can be answered by reading files or running headless commands alone.

## Core Workflow

Open a named Session so later commands can target it reliably:

```bash
neowright open --name debug -- <nvim-args>
```

Send Neovim-style keys:

```bash
neowright keys --name debug "<leader>ff"
```

Use direct PTY input only as an escape hatch when Neovim is blocked and cannot answer RPC, for example to dismiss a hit-enter prompt:

```bash
neowright keys --name debug --pty "<CR>"
```

`keys --pty` is not full Neovim key notation. It supports plain text plus terminal-level notation such as `<Esc>`, `<CR>`, `<Tab>`, `<BS>`, `<C-c>`, and `<M-x>`, and rejects unsupported notation instead of guessing.

Run an Ex command:

```bash
neowright exec --name debug "messages"
```

Inspect or mutate Neovim state with Lua:

```bash
neowright eval --name debug "return vim.api.nvim_get_current_line()"
```

Wait for UI or editor state instead of sleeping:

```bash
neowright wait --name debug "return vim.fn.mode() == 'n'"
```

Capture the visible TUI grid:

```bash
neowright snapshot --name debug
```

Attach a headed visible UI only when the user explicitly asks for headed mode, a visible UI, a terminal window, or to watch/interact with the same Session. Do not infer headed mode from a debugging task, snapshot request, or need to inspect UI state; use headless Neowright commands and snapshots instead. If the user explicitly asks for headed mode, you must attach a visible UI.

```bash
neowright attach --name debug
neowright attach --name debug --terminal-preset <preset>
neowright attach --name debug --terminal-cmd "<terminal-command>"
```

`attach` can auto-detect known terminal presets from the current terminal environment. Use `neowright attach -h` when you need the current preset flag help, `--terminal-preset` to force a known launch command, or `--terminal-cmd` when the user provides a custom terminal command.

Use `--print-command` when you want the exact manual attach command instead of asking Neowright to launch a terminal:

```bash
neowright attach --name debug --print-command
```

Close Sessions opened for the task when the workflow is complete:

```bash
neowright close --name debug
```

## Working Practices

- Be explicit about which Session is being driven.
- Use `--name` for repeatable targeting across commands.
- Prefer small, step-by-step interactions over long key sequences sent all at once. Send the next key or short sequence only after confirming the previous step produced the expected UI or editor state.
- Investigate between steps with `snapshot`, `wait`, `eval`, or `exec` rather than assuming what Neovim did. Interactive UI state can diverge because of modes, prompts, plugin latency, mappings, or completion menus.
- Do not use headed mode unless the user explicitly asks for it. When they do ask for headed mode, attach a visible UI; do not substitute snapshots-only inspection.
- Use `-h` on any command or subcommand when you need exact arguments, for example `neowright eval -h`.
- Read Neowright output as Agent-Readable Markdown; important values such as Session IDs, paths, and results are reported as structured Markdown fields.
- Use Snapshot artifact paths from command output when referring to saved captures.
- Snapshots are saved as project-local artifacts under `.neowright/`, which may appear as untracked files in the target project.
- Prefer `wait` for state changes that may be asynchronous, such as plugin startup, diagnostics, completion, or UI redraws.
- Treat attached visible UIs as optional clients. The PTY-backed Session remains authoritative for `keys`, `eval`, `wait`, `resize`, and `snapshot`.
- Remember that attached UIs share editor state. Smaller terminals can affect layout and snapshots, human input can race with agent input, and `:qa!` from any UI exits the shared Neovim instance.
- Close Sessions the agent opened when the task is finished.

## Common Patterns

Open a file and capture the initial UI:

```bash
neowright open --name inspect -- path/to/file.lua
neowright wait --name inspect "return vim.api.nvim_buf_get_name(0):match('file%.lua') ~= nil"
neowright snapshot --name inspect
```

Trigger a mapping that opens a split and inspect the result:

```bash
neowright keys --name inspect "<leader>x"
neowright wait --name inspect "return vim.fn.mode() == 'n' and #vim.api.nvim_list_wins() > 1"
neowright snapshot --name inspect
```

Check structured Neovim state:

```bash
neowright eval --name inspect "return vim.inspect(vim.api.nvim_list_wins())"
```
