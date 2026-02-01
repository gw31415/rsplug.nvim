# rsplug.nvim

> A blazingly fast Neovim plugin manager written in Rust

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Overview

**rsplug.nvim** is a modern Neovim plugin manager that takes a different approach: it's implemented as an external Rust binary rather than a Vimscript/Lua plugin. This architectural decision enables fast, parallel Git operations, deterministic builds, and seamless integration with Nix-based workflows.

### Why rsplug.nvim?

- **External binary architecture**: No Neovim restart required to update the plugin manager itself
- **Blazingly fast**: Parallel shallow Git clones with depth=1 for rapid installation and updates
- **Deterministic and reproducible**: Lock file support ensures consistent plugin versions across machines
- **Lazy-loading first**: Sophisticated lazy-loading system with autocmds, filetypes, commands, and keymaps
- **Nix-friendly**: Designed to work as a build tool in Nix workflows with deterministic outputs
- **Minimal runtime overhead**: Merges compatible plugins to reduce Neovim's `runtimepath` size
- **TOML configuration**: Clean, readable configuration format with version wildcards and dependency support

## Quick Start

### Installation

#### From Source

Requires Rust 1.80+ and Git:

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

2. **Install plugins**:

```bash
rsplug --install ~/.config/nvim/rsplug.toml
```

3. **Add to your Neovim init.lua**:

```lua
-- Set packpath to rsplug's output directory
vim.opt.packpath:prepend(vim.fn.expand("~/.cache/rsplug/_gen"))

-- Load lazy-loading infrastructure
require("_rsplug")
```

4. **Update plugins**:

```bash
rsplug --update ~/.config/nvim/rsplug.toml
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

Execute Lua code at different stages:

```toml
[[plugins]]
repo = "folke/which-key.nvim"
lua_before = "vim.g.which_key_timeout = 300"  # Before plugin loads
lua_after = "require('which-key').setup{}"    # After plugin loads

[[plugins]]
repo = "nvim-treesitter/nvim-treesitter"
build = ["TSUpdate"]                           # Run after install/update
```

### Dependencies

Declare plugin dependencies that load together:

```toml
[[plugins]]
repo = "nvim-telescope/telescope.nvim"
with = ["plenary.nvim"]                # Load plenary.nvim simultaneously
on_cmd = "Telescope"
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

Generate a lock file to ensure consistent plugin versions:

```bash
# Create/update lock file
rsplug --update ~/.config/nvim/rsplug.toml

# Use exact versions from lock file
rsplug --locked --install ~/.config/nvim/rsplug.toml
```

### Multiple Configuration Files

Combine multiple configuration files using glob patterns:

```bash
rsplug --install '~/.config/nvim/plugins/*.toml'
# Or with multiple patterns:
rsplug --install '~/.config/nvim/base.toml:~/.config/nvim/plugins/*.toml'
```

Or set via environment variable:

```bash
export RSPLUG_CONFIG_FILES="~/.config/nvim/plugins/*.toml"
rsplug --install
```

## Configuration Reference

### Plugin Fields

| Field | Type | Description |
|-------|------|-------------|
| `repo` | String | Repository in `owner/repo[@version]` format (GitHub) |
| `start` | Boolean | If `true`, always load at startup (default: `false`) |
| `on_event` | String/Array | Autocmd event(s) to trigger lazy-load |
| `on_cmd` | String/Array | User command(s) to trigger lazy-load |
| `on_ft` | String/Array | Filetype(s) to trigger lazy-load |
| `on_map` | String/Table | Keymap(s) to trigger lazy-load |
| `with` | Array | Plugin dependencies loaded simultaneously |
| `lua_before` | String | Lua code to run before plugin loads |
| `lua_after` | String | Lua code to run after plugin loads |
| `build` | Array | Shell commands to run after install/update |
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
Usage: rsplug [OPTIONS] <CONFIG_FILES>...

Arguments:
  <CONFIG_FILES>...  Glob-patterns of config files (split by ':' for multiple)

Options:
  -i, --install              Install plugins not yet installed
  -u, --update               Fetch from remote and update repositories
      --locked               Use exact revisions from lock file (conflicts with --update)
      --lockfile <LOCKFILE>  Specify lock file path (default: ~/.cache/rsplug/rsplug.lock.json)
  -h, --help                 Print help
```

**Environment Variables:**
- `RSPLUG_CONFIG_FILES`: Default config file pattern(s)

## How It Works

rsplug.nvim operates in two phases:

### 1. Build Phase (CLI)

```
rsplug --install config.toml
```

1. Parses TOML configuration file(s)
2. Resolves plugin dependencies using DAG (Directed Acyclic Graph)
3. Clones/updates Git repositories to `~/.cache/rsplug/repos/`
4. Runs build commands if specified
5. Generates plugin structure in `~/.cache/rsplug/_gen/pack/_gen/`
6. Creates lazy-loading infrastructure in `~/.cache/rsplug/_gen/_rsplug/`
7. Writes lock file with exact commit hashes

### 2. Runtime Phase (Neovim)

```lua
require("_rsplug")
```

- Registers lazy-loading triggers (autocmds, commands, keymaps)
- On trigger, loads plugin via `:packadd` with before/after hooks
- Transparent to the user - plugins load exactly when needed

## Advanced Topics

The deterministic output (config + lock file → plugin directory) makes it ideal for Nix-based Neovim configurations.

### Plugin Merging

rsplug automatically merges plugins with the same lazy-loading trigger when their files don't conflict. This reduces `runtimepath` size and improves startup performance.

### Build Caching

Build commands are cached using a hash of:
- Git commit SHA
- Working directory changes
- Build command itself

Rebuilds only occur when necessary, speeding up subsequent runs.

## Who Is This For?

rsplug.nvim is ideal for:

- **Advanced Neovim users** who want precise control over plugin loading
- **Performance enthusiasts** seeking fast startup times
- **Nix users** who need reproducible, declarative configurations
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
