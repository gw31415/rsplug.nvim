---
name: rust-verification
description: Use this skill when validating Rust changes in rsplug.nvim before reporting completion.
---

# Rust verification for rsplug.nvim

Use these checks for Rust changes in this repository.

## Commands

- Format check: `cargo fmt --all -- --check`
- Build/check: `cargo check`
- Tests: `cargo test`
- Lints: `cargo clippy --workspace --all-targets -- -D warnings`

Do not use `cargo check -q`.

## Practices

- Re-run `git status --short` before final reporting.
- Report real command output, not assumed success.
- If a command fails, read the exact error and fix the root cause if it is in scope.
- Do not commit unless the user explicitly asks.
