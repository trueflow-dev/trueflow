#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CRATE_DIR="$REPO_ROOT/trueflow"
DEFAULT_OUTPUT_BASE="$REPO_ROOT/.trueflow/release-artifacts"
OUTPUT_BASE=$DEFAULT_OUTPUT_BASE
VERSION=""
TARGET=""
BINARY_SOURCE=""

usage() {
  cat <<'EOF'
Usage: package-built-release.sh --target TARGET --binary PATH [--version vX.Y.Z] [--output-dir DIR]

Package an already-built trueflow binary into the versioned artifact format
used under https://trueflow.dev/download/.

Options:
  --target TARGET     Rust target triple for the packaged artifact.
  --binary PATH       Path to an already-built trueflow binary.
  --version VERSION   Override the version label (default: read from Cargo.toml).
  --output-dir DIR    Base directory for versioned artifacts (default: .trueflow/release-artifacts).
  -h, --help          Show this help text.
EOF
}

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

have_command() {
  command -v "$1" >/dev/null 2>&1
}
validate_filename_component() {
  value=$1
  label=$2

  case "$value" in
    ""|.|..|*[!0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz._+-]*)
      die "$label must contain only letters, digits, '.', '_', '+', or '-'"
      ;;
  esac
}


read_default_version() {
  cargo_version=$(awk -F '"' '/^version = "/ { print $2; exit }' "$CRATE_DIR/Cargo.toml")
  [ -n "$cargo_version" ] || die "failed to read version from $CRATE_DIR/Cargo.toml"
  printf 'v%s\n' "$cargo_version"
}

sha256_line() {
  archive_name=$1

  if have_command shasum; then
    shasum -a 256 "$archive_name"
    return
  fi

  if have_command sha256sum; then
    sha256sum "$archive_name"
    return
  fi

  die "shasum or sha256sum is required"
}
sha256_digest() {
  archive_path=$1
  hash_line=$(sha256_line "$archive_path") || return 1
  digest=$(printf '%s\n' "$hash_line" | awk 'NR == 1 { print $1; exit }')

  printf '%s\n' "$digest" |
    awk 'length($0) == 64 && $0 ~ /^[0-9a-f]+$/ { valid = 1 } END { exit valid ? 0 : 1 }' ||
    die "failed to calculate SHA-256 for $archive_path"
  printf '%s\n' "$digest"
}

append_checksum_entry() {
  archive_path=$1
  archive_name=$2

  case "$MANIFEST_ARCHIVES" in
    *"
$archive_name
"*)
      die "duplicate archive basename: $archive_name"
      ;;
  esac

  MANIFEST_ARCHIVES="${MANIFEST_ARCHIVES}${archive_name}
"
  digest=$(sha256_digest "$archive_path")
  printf '%s  %s\n' "$digest" "$archive_name" >> "$CHECKSUM_TMP"
}


copy_executable() {
  source_path=$1
  destination_path=$2

  if have_command install; then
    install -m 0755 "$source_path" "$destination_path"
  else
    cp "$source_path" "$destination_path"
    chmod 0755 "$destination_path"
  fi
}

cleanup() {
  if [ -n "${ARCHIVE_TMP:-}" ] && [ -e "$ARCHIVE_TMP" ]; then
    rm -f "$ARCHIVE_TMP" || true
  fi

  if [ -n "${CHECKSUM_TMP:-}" ] && [ -e "$CHECKSUM_TMP" ]; then
    rm -f "$CHECKSUM_TMP" || true
  fi

  if [ -n "${STAGE_DIR:-}" ] && [ -d "$STAGE_DIR" ] && have_command trash; then
    trash "$STAGE_DIR" >/dev/null 2>&1 || true
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --target)
      shift
      [ $# -gt 0 ] || die "--target requires a value"
      TARGET=$1
      ;;
    --binary)
      shift
      [ $# -gt 0 ] || die "--binary requires a value"
      BINARY_SOURCE=$1
      ;;
    --version)
      shift
      [ $# -gt 0 ] || die "--version requires a value"
      VERSION=$1
      ;;
    --output-dir)
      shift
      [ $# -gt 0 ] || die "--output-dir requires a value"
      OUTPUT_BASE=$1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
  shift
done

[ -n "$TARGET" ] || die "--target is required"
[ -n "$BINARY_SOURCE" ] || die "--binary is required"
[ -f "$BINARY_SOURCE" ] || die "expected built binary at $BINARY_SOURCE"

if [ -z "$VERSION" ]; then
  VERSION=$(read_default_version)
fi

validate_filename_component "$VERSION" "version"
validate_filename_component "$TARGET" "target"

ARTIFACT_DIR="$OUTPUT_BASE/$VERSION"
ARCHIVE_NAME="trueflow-${VERSION}-${TARGET}.tar.gz"
CHECKSUM_NAME="trueflow-${VERSION}-SHA256SUMS.txt"
ARCHIVE_PATH="$ARTIFACT_DIR/$ARCHIVE_NAME"
CHECKSUM_PATH="$ARTIFACT_DIR/$CHECKSUM_NAME"

STAGE_DIR=
ARCHIVE_TMP=
CHECKSUM_TMP=
trap cleanup EXIT HUP INT TERM

mkdir -p "$ARTIFACT_DIR"
ARCHIVE_TMP=$(mktemp "$ARTIFACT_DIR/.${ARCHIVE_NAME}.XXXXXX")
CHECKSUM_TMP=$(mktemp "$ARTIFACT_DIR/.${CHECKSUM_NAME}.XXXXXX")
STAGE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-package.XXXXXX")
PACKAGE_DIR="$STAGE_DIR/package"
mkdir -p "$PACKAGE_DIR"

copy_executable "$BINARY_SOURCE" "$PACKAGE_DIR/trueflow"
cp "$REPO_ROOT/LICENSE" "$PACKAGE_DIR/"
cp "$REPO_ROOT/README.md" "$PACKAGE_DIR/"

printf '==> packaging %s\n' "$ARCHIVE_NAME"
tar -C "$PACKAGE_DIR" -czf "$ARCHIVE_TMP" trueflow LICENSE README.md
tar -tzf "$ARCHIVE_TMP" >/dev/null

MANIFEST_ARCHIVES='
'

for archive_path in "$ARTIFACT_DIR"/trueflow-"${VERSION}"-*.tar.gz; do
  [ -f "$archive_path" ] || continue
  archive_name=${archive_path##*/}
  [ "$archive_name" = "$ARCHIVE_NAME" ] && continue
  append_checksum_entry "$archive_path" "$archive_name"
done

append_checksum_entry "$ARCHIVE_TMP" "$ARCHIVE_NAME"

[ -s "$CHECKSUM_TMP" ] || die "checksum manifest generation produced no data"
mv -f "$ARCHIVE_TMP" "$ARCHIVE_PATH"
ARCHIVE_TMP=
mv -f "$CHECKSUM_TMP" "$CHECKSUM_PATH"
CHECKSUM_TMP=

printf '==> wrote %s\n' "$ARCHIVE_PATH"
printf '==> wrote %s\n' "$CHECKSUM_PATH"
