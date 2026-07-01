# Notices for rsplug-fts

`rsplug-fts` is a vendored fork of `fts-rs`.

- Original project: <https://github.com/dalance/fts-rs>
- Original crates.io package: <https://crates.io/crates/fts>
- Original author/copyright holder: dalance
- Original copyright notice: Copyright (c) 2018 dalance
- Original license: dual-licensed under either MIT or Apache-2.0, at the user's option

The original MIT and Apache-2.0 license texts are included in this package as
`LICENSE-MIT` and `LICENSE-APACHE`.

## Why this fork exists

This package is published under the name `rsplug-fts` so `rsplug.nvim` can depend
on a vendored copy of the `fts` library without taking ownership of, or implying
affiliation with, the upstream `fts` crate name.

## Changes from upstream

Compared with the upstream `fts-rs` package, this vendored package has been
adjusted for use inside `rsplug.nvim`:

- crates.io package name changed from `fts` to `rsplug-fts`;
- Rust library crate name intentionally remains `fts` for source compatibility;
- package metadata points to the `rsplug.nvim` repository;
- Cargo package excludes generated/vendor artifacts that crates.io rejects;
- dependency metadata may differ from the upstream release as required for the
  vendored build.
- `test_data/` fixtures restored from upstream git (excluded from the crates.io
  package, absent in the vendored source);
- skeptic doc-tests disabled (`build.rs` no longer calls
  `skeptic::generate_doc_tests`; `skeptic` removed from dev/build dependencies);
  the workspace produces multiple rlib fingerprints for the `fts` crate name
  causing E0464 in skeptic's `--extern` resolution.

## macOS note

This vendored package is used by `rsplug-walker` on Unix platforms, including
macOS. The upstream crates.io package name `fts` is not used by `rsplug-walker`:
`rsplug-walker` depends on this package as:

```toml
fts = { package = "rsplug-fts", version = "0.3.0" }
```

while the Rust library crate name remains `fts`.

This distinction matters on macOS because using the upstream `fts` registry
package as a transitive dependency of the published `rsplug-walker` caused
downstream `rsplug` binary verification/linking to fail on macOS with unresolved
libc `fts_*` symbols such as `_fts_open$INODE64`, `_fts_read$INODE64`,
`_fts_set$INODE64`, and `_fts_close$INODE64`. The `rsplug-fts` package exists so
the published dependency graph uses the same vendored source and package
metadata that `rsplug.nvim` builds with locally.

No upstream endorsement is implied by this macOS packaging change.

No endorsement by the upstream author is implied.
