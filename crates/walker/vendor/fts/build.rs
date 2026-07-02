// Skeptic doc-tests are disabled for this vendored fork.
//
// The upstream fts-rs uses skeptic to compile-check README.md code examples.
// In this vendored package, the workspace produces multiple rlib fingerprints
// for the `fts` crate name (lib build + test build), causing skeptic's
// --extern resolution to fail with E0464 ("multiple candidates for rlib
// dependency `fts`"). Since the README examples document usage of the
// published `rsplug-fts` package (not the local workspace path), they are
// not meaningfully compilable here.
//
// If skeptic support is needed in the future, either:
//   - reference a single rlib path explicitly, or
//   - test against an installed `rsplug-fts` crate rather than the workspace.

fn main() {}
