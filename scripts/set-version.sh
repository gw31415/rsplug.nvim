#!/usr/bin/env bash
# Compatibility entry point. Version preparation is a pre-tag operation.
set -euo pipefail
exec "$(cd "$(dirname "$0")" && pwd)/prepare-release.sh" "$@"
