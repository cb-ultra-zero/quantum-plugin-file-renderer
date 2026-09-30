#!/usr/bin/env bash
#
# tools/bundle.sh — build the release cdylib, assemble the `docs/external-plugins.md` §2
# bundle and tar it.
#
#   manifest.toml      D21 static data (validated by kernel::registry::validate_manifest)
#   plugin.json        the runtime descriptor, with `entry`/`entry_sha256` stamped here
#   lib/<entry>        the cdylib named by plugin.json
#
# Output: dist/quantum-plugin-file-renderer-<version>-<target>.tar.gz plus the tarball's
# SHA-256. The last five stdout lines are machine-readable for a release workflow:
#
#   TARGET=<rustc host triple>
#   ENTRY=<path inside the bundle>
#   ENTRY_SHA256=<sha256 of the entry library>
#   BUNDLE=<path to the tarball>
#   BUNDLE_SHA256=<sha256 of the tarball>
#
# Usage: tools/bundle.sh [--out DIR] [--no-stamp-repo]
#
# `--no-stamp-repo` leaves the committed plugin.json untouched (useful in CI); by default
# the committed descriptor is restamped too, so `git status` after a bundle is the drift
# check between the repo and the artifact.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT_DIR="dist"
STAMP_REPO=1
while [ $# -gt 0 ]; do
  case "$1" in
    --out)
      OUT_DIR="${2:?--out needs a directory}"
      shift 2
      ;;
    --no-stamp-repo)
      STAMP_REPO=0
      shift
      ;;
    -h | --help)
      sed -n '2,30p' "$0"
      exit 0
      ;;
    *)
      echo "bundle: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

die() {
  echo "bundle: $*" >&2
  exit 1
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "no sha256sum/shasum on PATH"
  fi
}

# Rewrite the two per-artifact fields of plugin.json. The renderer list is never touched:
# it is the source of truth the runtime `describe()` is checked against.
stamp_plugin_json() {
  local src="$1" dst="$2" entry="$3" sha="$4" tmp
  tmp="$(mktemp)"
  sed -e "s|\"entry\": \".*\"|\"entry\": \"${entry}\"|" \
    -e "s|\"entry_sha256\": \".*\"|\"entry_sha256\": \"${sha}\"|" \
    "$src" >"$tmp"
  grep -q "\"entry\": \"${entry}\"" "$tmp" || die "could not stamp \`entry\` into $dst"
  grep -q "\"entry_sha256\": \"${sha}\"" "$tmp" || die "could not stamp \`entry_sha256\` into $dst"
  mv "$tmp" "$dst"
}

TARGET="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$TARGET" ] || die "could not read the host triple from rustc -vV"

case "$TARGET" in
  *windows*) LIB_PREFIX=""; EXT="dll" ;;
  *darwin*) LIB_PREFIX="lib"; EXT="dylib" ;;
  *) LIB_PREFIX="lib"; EXT="so" ;;
esac
ENTRY="lib/${LIB_PREFIX}quantum_plugin_file_renderer.${EXT}"

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
[ -n "$VERSION" ] || die "could not read the version from Cargo.toml"

echo "bundle: quantum-plugin-file-renderer ${VERSION} for ${TARGET}"
cargo build --release --locked -p quantum_plugin_file_renderer

BUILT="target/release/${LIB_PREFIX}quantum_plugin_file_renderer.${EXT}"
[ -f "$BUILT" ] || die "the release library is missing: $BUILT"
ENTRY_SHA256="$(sha256_of "$BUILT")"

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/qfr-bundle.XXXXXX")"
VERIFY="$(mktemp -d "${TMPDIR:-/tmp}/qfr-verify.XXXXXX")"
trap 'rm -rf "$STAGE" "$VERIFY"' EXIT

mkdir -p "$STAGE/lib"
cp "$BUILT" "$STAGE/$ENTRY"
cp manifest.toml "$STAGE/manifest.toml"
stamp_plugin_json plugin.json "$STAGE/plugin.json" "$ENTRY" "$ENTRY_SHA256"

if [ "$STAMP_REPO" = 1 ]; then
  stamp_plugin_json plugin.json plugin.json "$ENTRY" "$ENTRY_SHA256"
fi

chmod 0644 "$STAGE/plugin.json"
chmod 0644 "$STAGE/manifest.toml"
mkdir -p "$OUT_DIR"
TARBALL="$OUT_DIR/quantum-plugin-file-renderer-${VERSION}-${TARGET}.tar.gz"
tar -czf "$TARBALL" -C "$STAGE" manifest.toml plugin.json lib
BUNDLE_SHA256="$(sha256_of "$TARBALL")"

# Self-check: the artifact must be the thing we think it is (bundle root, entry digest,
# descriptor digest) before it is ever published.
tar -xzf "$TARBALL" -C "$VERIFY"
for required in manifest.toml plugin.json "$ENTRY"; do
  [ -e "$VERIFY/$required" ] || die "the tarball is missing $required"
done
[ "$(sha256_of "$VERIFY/$ENTRY")" = "$ENTRY_SHA256" ] ||
  die "the entry digest inside the tarball does not match $ENTRY_SHA256"
grep -q "$ENTRY_SHA256" "$VERIFY/plugin.json" ||
  die "the bundled plugin.json does not carry the entry digest"
grep -q "id = \"file-renderer\"" "$VERIFY/manifest.toml" ||
  die "the bundled manifest.toml is not this plugin's"

echo "bundle: files in $TARBALL"
tar -tzf "$TARBALL" | sed 's/^/  /'
echo "TARGET=$TARGET"
echo "ENTRY=$ENTRY"
echo "ENTRY_SHA256=$ENTRY_SHA256"
echo "BUNDLE=$TARBALL"
echo "BUNDLE_SHA256=$BUNDLE_SHA256"
