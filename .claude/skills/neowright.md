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

When testing Neovim with a clean configuration, set a throwaway APPNAME so config/state/cache are isolated from the user's real profile:

```bash
NVIM_APPNAME=rsplug-test neowright open --name debug -- --clean -u NONE -i NONE <nvim-args>
```

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

Attach a headed visible UI only when the user explicitly asks for headed mode, a visible UI, a terminal window, or to watch/interact with the same Session. Do not infer headed mode from a debugging task, snapshot request, or need to inspect UI state; use headless Neowright commands and snapshots instead. If the user explicitly asks for headed mode, attach a visible UI.

```bash
neowright attach --name debug
neowright attach --name debug --terminal-preset <preset>
neowright attach --name debug --terminal-cmd "<terminal-command>"
```

Close Sessions opened for the task when the workflow is complete:

```bash
neowright close --name debug
```

## Working Practices

- Be explicit about which Session is being driven.
- Use `--name` for repeatable targeting across commands.
- Prefer `NVIM_APPNAME=<throwaway-name>` for clean-session tests, especially when checking plugin manager output, so the user's normal Neovim config and state cannot affect results.
- Prefer small, step-by-step interactions over long key sequences sent all at once.
- Investigate between steps with `snapshot`, `wait`, `eval`, or `exec` rather than assuming what Neovim did.
- Do not use headed mode unless the user explicitly asks for it.
- Use `-h` on any command or subcommand when you need exact arguments.
- Read Neowright output as Agent-Readable Markdown; important values such as Session IDs, paths, and results are reported as structured Markdown fields.
- Snapshots are saved as project-local artifacts under `.neowright/`, which may appear as untracked files.
- Prefer `wait` for asynchronous state changes.
- Close Sessions the agent opened when the task is finished.
