#!/usr/bin/env bash
# Emit the release contract as JSON without changing repository files.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:?Usage: $0 <version-without-v-prefix>}"
VERSION="${VERSION#v}"
"$ROOT/scripts/check-release.sh" "$VERSION" >/dev/null
metadata="$(cd "$ROOT" && cargo metadata --locked --no-deps --format-version 1)"

packages_json="$(jq '[.packages[] | select(.name == "rsplug-adaptive-semaphore" or .name == "rsplug-dag" or .name == "rsplug-file-specifier" or .name == "rsplug-fts" or .name == "rsplug-walker" or .name == "rsplug") | {name, version}]' <<<"$metadata")"
jq -n \
  --arg tag "v$VERSION" \
  --arg version "$VERSION" \
  --argjson packages "$packages_json" \
  '{
    tag: $tag,
    version: $version,
    prerelease: ($version | contains("-")),
    packages: $packages,
    dependency_levels: [
      ["rsplug-adaptive-semaphore", "rsplug-dag", "rsplug-file-specifier", "rsplug-fts"],
      ["rsplug-walker"],
      ["rsplug"]
    ],
    assets: [
      ("rsplug-aarch64-apple-darwin-" + $version + ".tar.gz"),
      ("rsplug-aarch64-unknown-linux-gnu-" + $version + ".tar.gz"),
      ("rsplug-x86_64-apple-darwin-" + $version + ".tar.gz"),
      ("rsplug-x86_64-pc-windows-msvc-" + $version + ".zip"),
      ("rsplug-x86_64-unknown-linux-gnu-" + $version + ".tar.gz"),
      "SHA256SUMS"
    ]
  }'
