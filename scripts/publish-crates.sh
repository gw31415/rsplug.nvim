#!/usr/bin/env bash
# Publish the workspace crates in registry dependency order.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:?Usage: $0 <version>}">
API="https://crates.io/api/v1/crates"

wait_visible() {
  local crate="$1" version="$2" attempt=0 delay=2 status
  while (( attempt < 8 )); do
    status="$(curl -sS -o /dev/null -w '%{http_code}' \
      -A 'rsplug.nvim release workflow' "$API/$crate/$version" || true)"
    [[ "$status" == 200 ]] && return 0
    [[ "$status" == 404 || "$status" == 429 || "$status" =~ ^5 ]] || {
      echo "error: unexpected crates.io status $status for $crate $version" >&2
      return 1
    }
    attempt=$((attempt + 1))
    sleep "$delay"
    if (( delay < 30 )); then
      delay=$((delay * 2))
    fi
  done
  echo "error: crates.io did not expose $crate $version before timeout" >&2
  return 1
}

publish_one() {
  local crate="$1" status local_archive remote_archive local_sha remote_sha
  echo "→ Packaging $crate $VERSION"
  (cd "$ROOT" && cargo package --locked -p "$crate")
  local_archive="$ROOT/target/package/$crate-$VERSION.crate"
  [[ -f "$local_archive" ]] || {
    echo "error: missing package archive $local_archive" >&2
    return 1
  }

  status="$(curl -sS -o /dev/null -w '%{http_code}' \
    -A 'rsplug.nvim release workflow' "$API/$crate/$VERSION" || true)"
  case "$status" in
    404)
      echo "→ Publishing $crate $VERSION"
      (cd "$ROOT" && cargo publish --locked -p "$crate")
      wait_visible "$crate" "$VERSION"
      ;;
    200)
      remote_archive="$(mktemp "${TMPDIR:-/tmp}/rsplug-crate.XXXXXX")"
      curl -fsSL -A 'rsplug.nvim release workflow' \
        "$API/$crate/$VERSION/download" -o "$remote_archive"
      local_sha="$(sha256sum "$local_archive" | awk '{print $1}')"
      remote_sha="$(sha256sum "$remote_archive" | awk '{print $1}')"
      rm -f "$remote_archive"
      if [[ "$local_sha" != "$remote_sha" ]]; then
        echo "error: published $crate $VERSION differs from this source" >&2
        return 1
      fi
      echo "✓ $crate $VERSION already published with matching bytes"
      ;;
    *)
      echo "error: crates.io returned $status for $crate $VERSION" >&2
      return 1
      ;;
  esac
}

for crate in \
  rsplug-adaptive-semaphore rsplug-dag rsplug-file-specifier rsplug-fts \
  rsplug-walker rsplug; do
  publish_one "$crate"
done
