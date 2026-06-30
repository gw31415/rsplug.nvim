# rsplug-fts

`rsplug-fts` is a vendored fork of [`fts-rs`](https://github.com/dalance/fts-rs),
a Rust wrapper around libc `fts` for high-performance directory walking.

This package is published for use by [`rsplug.nvim`](https://github.com/gw31415/rsplug.nvim).
It is **not** the upstream `fts` crate and does not imply endorsement by the
upstream author.

## Acknowledgement

The original project is:

- Project: [`dalance/fts-rs`](https://github.com/dalance/fts-rs)
- crates.io: [`fts`](https://crates.io/crates/fts)
- Original author/copyright holder: `dalance`
- Original copyright notice: `Copyright (c) 2018 dalance`
- Original license: `MIT OR Apache-2.0`

The original license texts are preserved in this package as `LICENSE-MIT` and
`LICENSE-APACHE`. Additional attribution and change notes are in `NOTICE.md`.

## Why this package exists

`rsplug.nvim` vendors `fts-rs` so it can depend on the known vendored source
rather than the upstream `fts` package when publishing its workspace crates.
The crates.io package name is therefore changed to `rsplug-fts`, while the Rust
library crate name remains `fts` for source compatibility.

## Usage

```toml
[dependencies]
fts = { package = "rsplug-fts", version = "0.3.0" }
```

Then use the library as `fts` in Rust code:

```rust
use std::path::Path;
use fts::walkdir::{WalkDir, WalkDirConf};

let path = Path::new(".");
for entry in WalkDir::new(WalkDirConf::new(path)) {
    println!("{:?}", entry.unwrap());
}
```

Call `fts_*` functions directly:

```rust
use std::ffi::CString;
use fts::ffi::{fts_open, fts_read, fts_close, FTS_LOGICAL};

let path = CString::new(".").unwrap();
let paths = vec![path.as_ptr(), std::ptr::null()];
let fts = unsafe { fts_open(paths.as_ptr(), FTS_LOGICAL, None) };
let _ftsent = unsafe { fts_read(fts) };
let _ = unsafe { fts_close(fts) };
```

## Changes from upstream

See `NOTICE.md` for the maintained list of packaging changes. In short, this
fork changes package metadata/name for `rsplug.nvim` and keeps the library crate
name as `fts`.

### macOS-specific note

`rsplug-walker` uses this vendored package on Unix platforms, including macOS,
instead of depending on the upstream crates.io package named `fts`.

The dependency is intentionally written with a different package name but the
same Rust crate name:

```toml
fts = { package = "rsplug-fts", version = "0.3.0" }
```

This was done because the upstream `fts` registry package caused downstream
`rsplug` binary verification/linking on macOS to fail with unresolved libc
`fts_*` symbols, including `_fts_open$INODE64`, `_fts_read$INODE64`,
`_fts_set$INODE64`, and `_fts_close$INODE64`. Publishing the vendored source as
`rsplug-fts` makes that macOS-sensitive dependency explicit and keeps the
published dependency graph aligned with the source used by `rsplug.nvim`.

This is a packaging/forking difference for `rsplug.nvim`; it is not an upstream
claim or endorsement.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option, matching the upstream `fts-rs` license.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this vendored fork shall be dual licensed as above, without any
additional terms or conditions.
