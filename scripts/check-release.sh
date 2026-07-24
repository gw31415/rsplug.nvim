#!/usr/bin/env bash
# Verify release metadata without modifying the working tree.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:?Usage: $0 <version-without-v-prefix>}"
VERSION="${VERSION#v}"
if [[ ! "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
  echo "error: invalid release SemVer: $VERSION" >&2
  exit 2
fi

packages=(
  rsplug-adaptive-semaphore rsplug-dag rsplug-file-specifier
  rsplug-fts rsplug-walker rsplug
)
metadata="$(cd "$ROOT" && cargo metadata --locked --no-deps --format-version 1)"
for package in "${packages[@]}"; do
  actual="$(jq -r --arg name "$package" '.packages[] | select(.name == $name) | .version' <<<"$metadata")"
  if [[ "$actual" != "$VERSION" ]]; then
    echo "error: $package is $actual, expected $VERSION" >&2
    exit 1
  fi
done

for crate in adaptive_semaphore dag file_specifier walker fts; do
  actual="$(perl -ne 'if (/^'"$crate"' = \{.*version = "([^"]+)"/) { print $1 }' "$ROOT/Cargo.toml")"
  if [[ "$actual" != "$VERSION" ]]; then
    echo "error: workspace dependency $crate is $actual, expected $VERSION" >&2
    exit 1
  fi
done

for file in "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" \
  "$ROOT/crates/walker/Cargo.toml" \
  "$ROOT/crates/walker/vendor/fts/Cargo.toml" \
  "$ROOT/crates/walker/vendor/fts/Cargo.toml.orig"; do
  grep -Fq "version = \"$VERSION\"" "$file" || {
    echo "error: $file has no expected version $VERSION" >&2
    exit 1
  }
done
for file in "$ROOT/crates/walker/vendor/fts/README.md" \
  "$ROOT/crates/walker/vendor/fts/NOTICE.md"; do
  grep -Fq "rsplug-fts\", version = \"$VERSION\"" "$file" || {
    echo "error: $file has no expected rsplug-fts example" >&2
    exit 1
  }
done
echo "Release metadata is consistent for $VERSION"
