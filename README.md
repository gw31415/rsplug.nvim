# rsplug.nvim

> A blazingly fast Neovim plugin manager written in Rust

[![Crates.io](https://img.shields.io/crates/v/rsplug.svg)](https://crates.io/crates/rsplug)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Overview

**rsplug.nvim** is a modern Neovim plugin manager that takes a different approach: it's implemented as an external Rust binary to build a Vim pack package rather than a Vimscript/Lua plugin. Instead of installing plugins directly, rsplug **synchronizes Vim pack packages from TOML configuration files**, enabling fast, parallel Git operations, deterministic builds. In the future, rsplug will provide seamless integration with Nix-based workflows.

### Why rsplug.nvim?

- **External binary architecture**: Say goodbye to launching Neovim just to manage plugins
- **Blazingly fast**: Parallel shallow Git clones with depth=1 for rapid installation and updates
- **Deterministic and reproducible**: Lock file support ensures consistent plugin versions across machines
- **Lazy-loading first**: Sophisticated lazy-loading system with autocmds, filetypes, commands, and keymaps
- **Minimal runtime overhead**: Merges compatible plugins to reduce Neovim's `runtimepath` size
- **TOML configuration**: Clean, readable configuration format with version wildcards and dependency support

## Quick Start

### Installation

First, you need to add one liner to your Neovim configuration.

```lua
dofile(vim.fn.expand '~/.cache/rsplug/init.lua')
```

Then, install the `rsplug` binary:

#### From Source

Requires Rust 1.89+ and Git:

```bash
git clone https://github.com/gw31415/rsplug.nvim.git
cd rsplug.nvim
cargo build --release
# Binary will be at target/release/rsplug
```

#### Using Nix Flakes

```bash
nix build github:gw31415/rsplug.nvim
# Binary will be in result/bin/rsplug
```

Or add to your flake:

```nix
{
  inputs.rsplug.url = "github:gw31415/rsplug.nvim";
  # ...
}
```

### Basic Usage

1. **Create a configuration file** (e.g., `~/.config/nvim/rsplug.toml`):

```toml
[[plugins]]
repo = "nvim-lua/plenary.nvim"

[[plugins]]
repo = "neovim/nvim-lspconfig"
on_event = "BufReadPre"
lua_after = """
require('lspconfig').rust_analyzer.setup{}
"""

[[plugins]]
repo = "hrsh7th/nvim-cmp"
on_event = ["InsertEnter", "CmdlineEnter"]
lua_after = "require('cmp').setup{}"
```

2. **Synchronize plugins from the TOML file**:

>[!WARNING]
> rsplug synchronizes pack packages based on the configuration file(s) you provide. If you change which configuration file you specify as an argument, plugins defined in other files will no longer be loaded. Always use the same configuration file pattern or use the `RSPLUG_CONFIG_FILES` environment variable to ensure consistency.

```bash
rsplug -iu ~/.config/nvim/rsplug.toml
```

>[!TIP]
> **Use environment variable for convenience!**
> Instead of specifying the config file every time, set `RSPLUG_CONFIG_FILES` in your shell profile:

```bash
export RSPLUG_CONFIG_FILES="~/.config/nvim/rsplug.toml"
# Or use glob patterns:
export RSPLUG_CONFIG_FILES="~/.config/nvim/plugins/*.toml"
# Run rsplug without positional arguments:
rsplug -iu
```

#### CLI Options

- To install new plugins: `-i` or `--install`
  ```bash
  rsplug --install ~/.config/nvim/rsplug.toml
  rsplug -i ~/.config/nvim/rsplug.toml
  ```
- To update existing plugins: `-u` or `--update`
  ```bash
  rsplug --update ~/.config/nvim/rsplug.toml
  rsplug -u ~/.config/nvim/rsplug.toml
  ```
- To sync hooks or scripts only: (no git operations)
  ```bash
  rsplug ~/.config/nvim/rsplug.toml
  ```
- To use the lock file for exact versions: `--locked`
  ```bash
  rsplug --locked ~/.config/nvim/rsplug.toml
  ```

## Key Features

### Lazy Loading

rsplug.nvim provides comprehensive lazy-loading triggers:

```toml
[[plugins]]
repo = "nvim-telescope/telescope.nvim"
on_cmd = "Telescope"                    # Load on command
on_map = { n = "<leader>ff" }           # Load on keymap
on_event = "VimEnter"                   # Load on autocmd event
on_ft = ["lua", "vim"]                  # Load on filetype
```

Lua modules are automatically detected and loaded on `require`:

```toml
[[plugins]]
repo = "nvim-lua/plenary.nvim"
# Will auto-load when you call require('plenary')
```

### Lifecycle Hooks

Execute Lua code / subprocess before/after plugin load or install:

```toml
[[plugins]]
repo = "folke/which-key.nvim"
lua_before = "vim.g.which_key_timeout = 300"  # Before plugin loads
lua_after = "require('which-key').setup{}"    # After plugin loads

[[plugins]]
repo = "yetone/avante.nvim"
build = ["make"]                              # Run after install/update
lua_build = "vim.fn.system({'make', 'docs'})" # Run Lua after install/update
```

### Dependencies

Declare plugin dependencies that load together:

```toml
[[plugins]]
repo = "nvim-telescope/telescope.nvim"
depends = ["plenary.nvim"]             # Load plenary.nvim simultaneously
on_cmd = "Telescope"
```

### Repository Sources

#### GitHub (shorthand)

```toml
[[plugins]]
repo = "owner/repo"               # Default branch
repo = "owner/repo@main"          # Specific branch
repo = "owner/repo@v1.2.0"        # Exact tag
repo = "owner/repo@v*"            # Latest matching tag (wildcard)
```

#### General Git URL

Any repository accessible via `git` can be specified by providing a full URL containing `://`:

```toml
[[plugins]]
repo = "https://gitlab.com/owner/plugin"          # Default branch
repo = "https://gitlab.com/owner/plugin@main"     # Branch / tag / commit
repo = "https://codeberg.org/owner/plugin@v1.0"  # Any hosting
```

The `@rev` suffix works the same as for GitHub shorthand — it accepts branch names, tags, and wildcard patterns.

The cache directory is derived from the URL's host and path (scheme, authentication, port, and `.git` suffix are stripped), e.g.:

```
https://gitlab.com/owner/plugin  →  ~/.cache/rsplug/repos/gitlab.com/owner/plugin
```

### Version Control

Lock to specific versions or use wildcards:

```toml
[[plugins]]
repo = "j-hui/fidget.nvim@v1.2.0"      # Exact version

[[plugins]]
repo = "j-hui/fidget.nvim@v*"          # Latest v* tag

[[plugins]]
repo = "j-hui/fidget.nvim@main"        # Specific branch
```

### Lock File for Reproducibility

Every time you run `rsplug` a lock file is generated at `~/.cache/rsplug/rsplug.lock.json` by default.
This file records the exact commit hashes of all installed plugins.

To sync plugins the exact versions from the lock file, use the `--locked` flag:

```bash
rsplug --locked
```

### Multiple Configuration Files

Combine multiple configuration files using glob patterns:

```bash
rsplug '~/.config/nvim/plugins/*.toml'
# Or with multiple patterns:
rsplug '~/.config/nvim/base.toml:~/.config/nvim/plugins/*.toml'
```

Or set via environment variable:

```bash
export RSPLUG_CONFIG_FILES="~/.config/nvim/plugins/*.toml"
rsplug
```

## Configuration Reference

### Plugin Fields

| Field | Type | Description |
|-------|------|-------------|
| `repo` | String | `owner/repo[@rev]` (GitHub) or any Git URL (`https://…[@rev]`) |
| `start` | Boolean | If `true`, always load at startup (default: `false`) |
| `on_event` | String/Array | Autocmd event(s) to trigger lazy-load |
| `on_cmd` | String/Array | User command(s) to trigger lazy-load |
| `on_ft` | String/Array | Filetype(s) to trigger lazy-load |
| `on_map` | String/Table | Keymap(s) to trigger lazy-load |
| `depends` | Array | Plugin dependencies loaded simultaneously |
| `lua_start` | String | Lua code to run at Neovim startup |
| `lua_before` | String | Lua code to run before plugin loads |
| `lua_after` | String | Lua code to run after plugin loads |
| `build` | Array | Subprocess to run after install/update |
| `lua_build` | String | Lua script to run after install/update |
| `name` | String | Custom plugin name (default: repo name) |
| `sym` | Boolean | Use symlink instead of file copy |
| `ignore` | String | Gitignore-style patterns for files to exclude |

### Key Mapping Syntax

```toml
# Simple (`nxo` modes)
on_map = "<leader>f"

# Single mode
on_map = { n = "<leader>f" }

# Multiple modes
on_map = { nx = "<leader>f" }

# Multiple keys
on_map = { n = ["<leader>f", "<leader>g"] }
```

## Command-Line Interface

```
Vim plugin manager written in Rust

Usage: rsplug [OPTIONS] <CONFIG_FILES>...

Arguments:
  <CONFIG_FILES>...  Glob-patterns of the config files. Split by ':' to specify multiple patterns [env: RSPLUG_CONFIG_FILES]

Options:
  -i, --install              Install plugins which are not installed yet
  -u, --update               Access remote and update repositories
      --locked               Fix the repo version with rev in the lockfile
      --lockfile <LOCKFILE>  Specify the lockfile path
  -h, --help                 Print help
```

## How It Works

rsplug.nvim operates in two phases:

### 1. Build Phase (CLI)

rsplug **synchronizes the pack packages** from your TOML configuration:

1. Parses TOML configuration file(s)
2. Resolves plugin dependencies using DAG (Directed Acyclic Graph)
3. Clones/updates Git repositories to `~/.cache/rsplug/repos/`
  - Clone new plugins if it provided the option `--install`
  - Update repos if it provided the option `--update`
  - Synchronizes to specific commit from lock file if `--locked` is provided
4. Writes lock file with exact commit hashes
5. Runs build commands / `lua_build` scripts if specified
6. Generates `~/.cache/rsplug/init.lua`, writes the generated control plugin under `~/.cache/rsplug/pack/_gen/opt/`, and stores that generation's `manifest.json` inside the control plugin

**Important:** The pack directory reflects exactly what's in your current configuration file(s). If you change which configuration file you pass as an argument, the pack directory will be re-synchronized to match only those plugins.

### 2. Runtime Phase (Neovim)

- `~/.cache/rsplug/init.lua` prepends the generated packpath and explicitly `packadd`s the current generated control plugin
- `lua_start` scripts run at startup before controlled startup plugin loads
- `start = true` plugins are placed under `pack/_gen/opt/` and loaded during startup via rsplug's controlled `:packadd!` path
- Registers lazy-loading triggers (autocmds, commands, keymaps)
- On startup or lazy trigger, loads plugin via `:packadd` with before/after hooks

### v0.2.0 Breaking Change

rsplug v0.2.0 changes the Neovim bootstrap line and the generated package layout
so `start = true` plugins can run `lua_before` and `lua_after` deterministically.

Update your `init.lua` from:

```lua
vim.opt.packpath:prepend("~/.cache/rsplug")
```

to:

```lua
dofile(vim.fn.expand("~/.cache/rsplug/init.lua"))
```

Managed plugins, including `start = true` plugins, are now placed under
`pack/_gen/opt/` and loaded by rsplug's generated runtime.

To preserve deterministic `lua_before` / `lua_after` ordering, managed
`start = true` plugins are not merged with other managed startup plugins.

## Advanced Topics

### Plugin Merging

rsplug automatically merges plugins with the same lazy-loading trigger when their files don't conflict. This reduces `runtimepath` size and improves startup performance.

### Build Caching

Build commands are cached using a hash of:
- Git commit SHA
- Working directory changes
- Build command itself (`build` and `lua_build`)

Rebuilds only occur when necessary, speeding up subsequent runs.

### Startup Generation Manifests

rsplug writes the generated control plugin under `pack/_gen/opt/<hash>/` and
loads the current control plugin from `init.lua` with `:packadd <hash>`. Older
retained control plugins are not native startup packages, so they are not loaded
by `packloadall` or startup package scanning.

Each control plugin contains a `manifest.json` listing the shared `_gen` entries
that generation references. The runtime exposes this data as `_rsplug.manifest`.
rsplug keeps the latest few control manifests and only cleans `_gen` plugin
directories when no retained manifest references them. This avoids deleting Lua
hook/runtime modules still referenced by a Neovim instance that started before a
later rsplug update.

## Who Is This For?

rsplug.nvim is ideal for:

- **Advanced Neovim users** who want precise control over plugin loading
- **Performance enthusiasts** seeking fast startup times
- **Rust developers** who prefer Rust tooling
- **Configuration hackers** who enjoy TOML over Lua for data

## Documentation

- **Quick reference**: This README
- **Detailed documentation**: `:help rsplug` (see `doc/rsplug.txt`)
- **Example configuration**: See `example.toml` in this repository
- **Repository**: https://github.com/gw31415/rsplug.nvim

## Known Limitations

- Plugin merging may occur between sibling dependencies without clear ordering control

## Contributing

Issues and pull requests are welcome! See the [issue tracker](https://github.com/gw31415/rsplug.nvim/issues) for current tasks and known issues.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

---

**Note**: This is an early-stage project under active development. APIs and behaviors may change.
