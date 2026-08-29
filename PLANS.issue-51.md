# Issue #51: replace clap with usage-rs

## Goal

Replace the `rsplug` binary's clap-derived parser with the `usage-rs` facade while preserving the established public CLI contract: flags, required config inputs, environment fallback and colon splitting, strict unknown/duplicate handling, conflicts, and help/version/error exit semantics.

## Plan

1. Characterize the existing clap behavior with focused parser/process tests and record the current resolved dependency footprint plus stripped release binary size.
2. Pin one exact `usage-rs` 6.x facade version, translate the derive/attributes mechanically, and let compiler diagnostics drive any syntax corrections without changing application logic.
3. Run focused CLI tests, format, check, full tests, and clippy; manually compare help/version and representative parse failures.
4. Record before/after dependency and release-binary measurements in the implementing PR, then merge the verified PR.

## Compatibility invariants

- `-i`/`--install`, `-u`/`--update`, `--locked`, and `--lockfile <PATH>` retain their meanings.
- `--update` conflicts with `--locked`.
- `<CONFIG_FILES>...` remains required unless `RSPLUG_CONFIG_FILES` supplies it.
- `:` splits config patterns from both argv and the environment fallback; argv wins over the environment fallback.
- Unknown flags and repeated scalar flags remain errors.
- Help/version exit successfully; parse failures exit with status 2.
- Environment values are not exposed in help/diagnostics.
- No resolved clap dependency remains for the `rsplug` crate after migration.

## Measurements and validation

- Baseline (`7eb37fe`): 230 unique normal dependency-tree lines; stripped release binary 10,048,160 bytes.
- Migrated: 224 unique normal dependency-tree lines; stripped release binary 10,130,528 bytes (+82,368 bytes, about +0.82%).
- `clap` is absent from the migrated resolved normal dependency graph.
- Manual compatibility checks preserve `rsplug` in help/version output and status 2 for missing input, flag conflicts, and unknown flags.
- Focused CLI compatibility tests: 11/11 passing.
- Repository validation passing: `cargo fmt --all -- --check`, `cargo check`, `cargo test`, and `cargo clippy --workspace --all-targets -- -D warnings`.
- One existing log progress test transiently failed under parallel execution during validation; it passed alone, the rsplug unit suite then passed 5/5 consecutive parallel runs, and the final unmodified `cargo test` command passed.

## Progress

- [x] Baseline CLI behavior characterized against the clap implementation.
- [x] Baseline dependency and stripped release-binary measurements recorded.
- [x] Parser migrated to exact-pinned `usage-rs` 6.5.0.
- [x] Focused and full validation pass.
- [ ] Before/after measurements recorded in PR and PR merged.
