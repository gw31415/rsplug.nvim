#!/usr/bin/env bash
# Prepare a release commit. CI must never invoke this after a tag is created.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:?Usage: $0 <version-without-v-prefix>}"
VERSION="${VERSION#v}"

if [[ ! "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
  echo "error: invalid release SemVer: $VERSION" >&2
  exit 2
fi
export RELEASE_VERSION="$VERSION"

replace_once() {
  local file="$1" pattern="$2"
  perl -0pi -e '
    my $n = s{'"$pattern"'}{$1 . $ENV{RELEASE_VERSION} . $2}mge;
    die "expected exactly one version field in $ARGV, found $n\n" if $n != 1;
  ' "$file"
}

replace_fts_examples() {
  local file="$1"
  perl -0pi -e '
    my $n = s{(rsplug-fts", version = ")[^"]+}{$1 . $ENV{RELEASE_VERSION}}mge;
    die "missing rsplug-fts examples in $ARGV\n" if $n == 0;
  ' "$file"
}

replace_once "$ROOT/Cargo.toml" '(^\[workspace\.package\][\s\S]*?\nversion = ")[^"]+(")'
for crate in adaptive_semaphore dag file_specifier walker fts; do
  replace_once "$ROOT/Cargo.toml" '(^'"$crate"' = \{[^\n]*?version = ")[^"]+(")'
done
replace_once "$ROOT/crates/walker/Cargo.toml" '(^\[package\][\s\S]*?\nversion = ")[^"]+(")'
replace_once "$ROOT/crates/walker/vendor/fts/Cargo.toml" '(^\[package\][\s\S]*?\nversion = ")[^"]+(")'
replace_once "$ROOT/crates/walker/vendor/fts/Cargo.toml.orig" '(^\[package\][\s\S]*?\nversion = ")[^"]+(")'
replace_fts_examples "$ROOT/crates/walker/vendor/fts/README.md"
replace_fts_examples "$ROOT/crates/walker/vendor/fts/NOTICE.md"

for package in \
  rsplug-adaptive-semaphore rsplug-dag rsplug-file-specifier \
  rsplug-fts rsplug-walker rsplug; do
  RELEASE_PACKAGE="$package" perl -0pi -e '
    my $name = $ENV{RELEASE_PACKAGE};
    my $n = s{(name = "\Q$name\E"\nversion = ")[^"]+}{$1 . $ENV{RELEASE_VERSION}}e;
    die "missing or duplicate lock package $name in $ARGV\n" if $n != 1;
  ' "$ROOT/Cargo.lock"
done

"$ROOT/scripts/check-release.sh" "$VERSION"
echo "Prepared release $VERSION. Review and commit the diff before creating v$VERSION."
