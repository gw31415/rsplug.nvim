#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/rsplug-release-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

tar -cf "$tmp/repo.tar" --exclude=target --exclude=.git -C "$ROOT" .
mkdir "$tmp/repo"
tar -xf "$tmp/repo.tar" -C "$tmp/repo"
chmod +x "$tmp/repo/scripts"/*.sh

repo="$tmp/repo"
current="$(cargo metadata --manifest-path "$repo/Cargo.toml" --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "rsplug") | .version')"
metadata="$("$repo/scripts/release-metadata.sh" "$current")"
[[ "$(jq -r .tag <<<"$metadata")" == v$current ]]
[[ "$(jq '.packages | length' <<<"$metadata")" -eq 6 ]]
"$repo/scripts/check-release.sh" "$current"
before="$(sha256sum "$repo/Cargo.toml" "$repo/Cargo.lock" \
  "$repo/crates/walker/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml.orig" \
  "$repo/crates/walker/vendor/fts/README.md" \
  "$repo/crates/walker/vendor/fts/NOTICE.md")"

"$repo/scripts/prepare-release.sh" 9.9.9
"$repo/scripts/check-release.sh" v9.9.9
after_first="$(sha256sum "$repo/Cargo.toml" "$repo/Cargo.lock" \
  "$repo/crates/walker/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml.orig" \
  "$repo/crates/walker/vendor/fts/README.md" \
  "$repo/crates/walker/vendor/fts/NOTICE.md")"
"$repo/scripts/prepare-release.sh" 0.3.1 >/dev/null
after_second="$(sha256sum "$repo/Cargo.toml" "$repo/Cargo.lock" \
  "$repo/crates/walker/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml" \
  "$repo/crates/walker/vendor/fts/Cargo.toml.orig" \
  "$repo/crates/walker/vendor/fts/README.md" \
  "$repo/crates/walker/vendor/fts/NOTICE.md")"
[[ "$after_first" == "$after_second" ]]
[[ "$before" != "$after_first" ]]

if "$repo/scripts/check-release.sh" 1.2 >/dev/null 2>&1; then
  echo "invalid version was accepted" >&2
  exit 1
fi
if "$repo/scripts/check-release.sh" '1.2.3+build' >/dev/null 2>&1; then
  echo "build metadata was accepted" >&2
  exit 1
fi

echo "release script tests passed"
