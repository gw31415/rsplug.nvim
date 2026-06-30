#!/usr/bin/env bash
# Set the workspace version across all Cargo.toml files.
# Usage: scripts/set-version.sh <version>
#   <version> must NOT have a leading 'v' (e.g. "0.2.0", not "v0.2.0")
set -euo pipefail

VERSION="${1:?Usage: $0 <version-without-v-prefix>}"
# Strip leading 'v' if accidentally passed
VERSION="${VERSION#v}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 1. Workspace [workspace.package].version
sed -i.bak -E "s/^version = \".*\"/version = \"${VERSION}\"/" \
    "${ROOT}/Cargo.toml"
# On macOS sed, the temp file ends up on the same line — remove backup
rm -f "${ROOT}/Cargo.toml.bak"

# 2. Each internal crate's version in [workspace.dependencies]
for crate in adaptive_semaphore dag file_specifier walker; do
    # Match lines like: adaptive_semaphore = { package = "rsplug-adaptive-semaphore", path = "adaptive_semaphore", version = "0.1.0" }
    sed -i.bak -E "/^${crate} = \{ .*path = / s/version = \"[^\"]+\"/version = \"${VERSION}\"/" \
        "${ROOT}/Cargo.toml"
    rm -f "${ROOT}/Cargo.toml.bak"
done

# rsplug-walker 0.1.0 was already published with a broken upstream fts dependency.
# For the rsplug v0.1.0 release, root must depend on rsplug-walker 0.1.1.
WALKER_VERSION="${VERSION}"
if [ "${VERSION}" = "0.1.0" ]; then
    WALKER_VERSION="0.1.1"
fi

sed -i.bak -E "/^walker = \{ .*path = / s/version = \"[^\"]+\"/version = \"${WALKER_VERSION}\"/" \
    "${ROOT}/Cargo.toml"
rm -f "${ROOT}/Cargo.toml.bak"

sed -i.bak -E "s/^version = \".*\"/version = \"${WALKER_VERSION}\"/" \
    "${ROOT}/crates/walker/Cargo.toml"
rm -f "${ROOT}/crates/walker/Cargo.toml.bak"

echo "✓ Set workspace version to ${VERSION} (rsplug-walker ${WALKER_VERSION})"
