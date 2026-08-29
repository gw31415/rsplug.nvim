# Add `--man` and `--completion` generation modes

## Goal

Expose documentation generation directly from rsplug's usage-rs declaration without requiring config input or running plugin-management side effects.

The public additions are:

- `rsplug --man`: write the rsplug(1) manpage to stdout.
- `rsplug --completion [SHELL]`: write a completion script to stdout for `bash`, `zsh`, `fish`, `nu`, or `powershell`; when omitted, detect the shell from `$SHELL`.

Both are generation modes, not normal plugin-manager execution. Each must be exclusive with every other explicit user argument, including `CONFIG_FILES`, and with the other generation mode. The `RSPLUG_CONFIG_FILES` environment fallback is ignored in generation modes. `CONFIG_FILES` remains required for normal execution.

## Decisions

- Use usage-rs `exclusive` on both generation flags instead of enumerating current conflicts. This makes the invariant automatically apply to future CLI arguments as well.
- Make `CONFIG_FILES` conditionally required with `required_unless = ["--completion", "--man"]`.
- Enable usage-rs's `completions` feature and `#[usage(completion)]`, so generated scripts call rsplug's own hidden completion protocol rather than an external `usage` executable.
- Generate the manpage from `Args::to_kdl()` through `usage-lib`'s manpage renderer. The usage-rs declaration remains the single source of truth.
- Generation writes only to stdout and returns before cache/lock/config initialization.

## Validation

- [x] `--help` advertises both options and supported completion shell values.
- [x] `--man` succeeds without config and emits a section-1 roff manpage for rsplug.
- [x] all five completion shells succeed without config and emit scripts for rsplug.
- [x] generated completion protocol returns candidates from rsplug's current usage-rs metadata.
- [x] unsupported explicit completion shell values are parse errors; omitted shell values are detected from `$SHELL`.
- [x] both generation modes conflict with all normal options, positional config input, and each other.
- [x] normal no-input invocation still fails with parse status 2.
- [x] `cargo fmt --all -- --check`, `cargo check`, `cargo test`, and clippy pass.
