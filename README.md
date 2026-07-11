# rsplug.nvim

> A fast, reproducible Neovim plugin manager that builds a standard Vim pack
> with an external Rust binary.

[![Crates.io](https://img.shields.io/crates/v/rsplug.svg)](https://crates.io/crates/rsplug)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## What it does

rsplug reads one or more TOML files, resolves plugin dependencies, fetches Git
repositories in parallel, and generates a self-contained pack under
`~/.cache/rsplug/`. Neovim loads that pack through a generated `init.lua`.
Plugin management therefore happens from the shell or a build system; Neovim
does not need to be running.

The generated output contains only the selected revisions and runtime files.
The repository cache and the generated pack are separate, so the pack can be
copied into a Nix or other reproducible build.

Highlights:

- shallow, parallel Git operations and a fast GitHub tarball path;
- JSON lockfile with exact commit revisions;
- lazy loading by event, command, filetype, function, mapping, source, or
  automatically detected Lua `require`;
- `lua_before`, `lua_after`, startup, build, and post-update hooks;
- dependency co-loading and conflict-aware merging to reduce `runtimepath`;
- configuration-only entries for startup or setup scripts.

## Installation

Neovim must load the generated bootstrap file. Add this once to `init.lua`:

```lua
dofile(vim.fn.expand '~/.cache/rsplug/init.lua')
```

Install the binary with mise:

```bash
mise use -g github:gw31415/rsplug.nvim
```

Or with Rust binstall:

```bash
cargo binstall rsplug
```

Or with Nix:

```bash
nix build github:gw31415/rsplug.nvim
```

## Quick start

Create `~/.config/nvim/rsplug.toml`:

```toml
[[plugins]]
repo = "nvim-lua/plenary.nvim"

[[plugins]]
repo = "neovim/nvim-lspconfig"
on_event = "BufReadPre"
lua_after = "require 'lspconfig'.rust_analyzer.setup {}"

[[plugins]]
repo = "nvim-telescope/telescope.nvim"
on_cmd = "Telescope"
depends = "plenary.nvim"
```

Install missing repositories and generate the pack:

```bash
rsplug --install ~/.config/nvim/rsplug.toml
```

Update existing repositories with:

```bash
rsplug --update ~/.config/nvim/rsplug.toml
```

With neither flag, rsplug reuses the cached revisions and regenerates hooks and
pack output without accessing remotes. Use `--locked` in CI or another
reproducible build:

```bash
rsplug --locked ~/.config/nvim/rsplug.toml
```

The configuration file list is part of the desired output. Keep it stable: a
run with a different list synchronizes the pack to that list and can remove
plugins that were defined only by the previous list. For convenience, use the
`RSPLUG_CONFIG_FILES` environment variable:

```bash
export RSPLUG_CONFIG_FILES="$HOME/.config/nvim/plugins/*.toml"
rsplug --install
```

Patterns can be separated by `:`. Multiple files are read in deterministic path
order.

## Repository and lock files

`repo` accepts GitHub shorthand or any URL containing `://`:

```toml
repo = "owner/plugin"                    # default branch
repo = "owner/plugin@main"               # branch
repo = "owner/plugin@v1.2.0"             # tag or commit
repo = "owner/plugin@v*"                 # matching tag
repo = "https://gitlab.com/owner/plugin"
repo = "https://codeberg.org/owner/plugin@main"
```

The optional `@revision` is taken from the URL path; `@` in URL userinfo is not
treated as a revision separator. URL repositories are cached below
`~/.cache/rsplug/repos/` under a canonical identity shared with the lockfile:
scheme and userinfo are removed, the host is lowercased, default ports
(80/443/22/9418) are dropped but non-default ports are kept, and trailing
`.git` is removed.

The default lockfile is `~/.cache/rsplug/rsplug.lock.json`. Set another path
with `--lockfile`. It records the resolved Git commit for each repository:

```json
{
  "version": "2",
  "locked": {
    "github.com/owner/plugin": {
      "type": "git",
      "rev": "40-character-commit-sha"
    }
  }
}
```

`--locked` requires every configured repository to have a lock entry and does
not contact remotes. `--update` and `--locked` cannot be combined.

## Configuration

Each `[[plugins]]` entry may represent a repository or only configuration Lua.
If `repo` is omitted, the entry is a script-only entry; its `lua_start`,
`lua_before`, or `lua_after` still participates in the generated runtime.

### Loading

Plugins are lazy by default. `start = true` makes an entry load during startup;
when both `start` and lazy triggers are present, `start` wins and the triggers
are ignored.

```toml
[[plugins]]
repo = "folke/which-key.nvim"
start = true
lua_before = "vim.g.which_key_timeout = 300"
lua_after = "require 'which-key'.setup {}"

[[plugins]]
repo = "nvim-telescope/telescope.nvim"
on_event = ["BufReadPre", "InsertEnter"]
on_cmd = "Telescope"
on_ft = ["lua", "vim"]
on_func = "TelescopeFindFiles"
on_map = { n = "<leader>ff" }
```

Supported triggers are `on_event`, `on_cmd`, `on_ft`, `on_func`, `on_map`, and
`on_source`. A plugin's `lua/*.lua` module paths are also detected so a plain
`require 'module'` can load it automatically.

`on_map` accepts a key for all modes, a mode table, or arrays of keys. Mode
letters follow Neovim conventions, for example `{ nx = ["<leader>f", "<leader>g"] }`.

### Names and dependencies

`name` is the public name used by `depends` and `on_source`; by default it is
the repository basename. It is useful when two repositories have the same
basename. Anonymous script-only entries are allowed and receive an internal
content-derived identity, but cannot be referenced by name.

```toml
[[plugins]]
repo = "nvim-telescope/telescope.nvim"
name = "telescope.nvim"
depends = ["plenary.nvim"]
on_source = "some-host.nvim"
```

Dependencies load together with the plugin that triggered them. They must be
defined in the configuration and may be transitive, but cycles are invalid.

### Hooks and materialization

```toml
[[plugins]]
repo = "yetone/avante.nvim"
build = ["make"]
lua_build = "vim.fn.system({'make', 'docs'})"
lua_post_update = "require 'avante'.post_update()"
ignore = "tests/\n*.md\n.github/"
merge = false
```

- `lua_start` runs at startup before controlled startup loads.
- `lua_before` runs immediately before `:packadd`.
- `lua_after` runs immediately after `:packadd`.
- `build` is an argument array executed in the repository directory after
  install/update; it is not a shell command string.
- `lua_build` runs in headless Neovim after install/update.
- `lua_post_update` runs in headless Neovim only when an existing repository
  receives a new revision during `--update`.
- `ignore` contains Gitignore-style patterns.
- `merge` defaults to `true`; `false` keeps the entry separate from compatible
  user plugins in both startup and lazy output.

## How loading works

The CLI builds a generation under a private staging directory and publishes it
into `pack/_gen/opt/` only after all files, manifests, and the loader succeed,
swapping `init.lua` atomically — so a failed run leaves the previous generation
bootable. It writes a generated control package and `init.lua`, and retains a
small set of previous generations. The bootstrap prepends the generated
packpath and explicitly loads the current control package. At runtime, a
trigger runs `lua_before`, loads the plugin, then runs `lua_after`.

Compatible entries with non-conflicting files can share one pack entry. Help
files are collected for a single helptags pass. Snapshot manifests make the
merge and copy decisions without repeatedly walking repository trees; they are
only a cache and filesystem fallback preserves correctness.

To boot a retained generation, list `~/.cache/rsplug/generations/` and pass its
32-character ID:

```bash
RSPLUG_GENERATION=<id> nvim
```

An invalid or pruned ID falls back to the latest generation.

## CLI reference

```text
rsplug [OPTIONS] <CONFIG_FILES>...

-i, --install              Install repositories not present in the cache
-u, --update               Fetch and update repositories
    --locked               Use exact revisions from the lockfile
    --lockfile <LOCKFILE>  Override the lockfile path
-h, --help                 Show help
```

Default paths below `~/.cache/rsplug/` are `init.lua`, `repos/`,
`pack/_gen/`, and `rsplug.lock.json`.

## Further documentation

- `:help rsplug` — the complete Vim help reference;
- [`example.toml`](example.toml) — a working configuration example;
- [GitHub issues](https://github.com/gw31415/rsplug.nvim/issues) — known work
  and discussions.

## Updates

The current release includes bounded parallel work, staged GitHub tarball
downloads with Git fallback, snapshot manifests, anonymous script-only entries,
preserved `on_source` names after merging, consistent `merge = false`
semantics for startup and lazy plugins, a canonical repository identity
shared by the lockfile and cache path so URL variants of one repository no
longer split into separate entries, atomic generation publication (each
generation is built in a staging directory and published via an atomic
`init.lua` swap, so a failed run cannot leave the pack half-written), and
batched GitHub rev resolution (`--update`/`--install` resolves GitHub repos'
revisions in one GraphQL query — including Git-backend `https://github.com/...`
URLs — instead of one REST request per repository; other repos resolve as
before). The generated bootstrap is required for
the v0.2 package layout; older configurations using only
`vim.opt.packpath:prepend '~/.cache/rsplug'` should switch to the `dofile` line
shown above.

## License

Apache License 2.0. See [LICENSE](LICENSE).
