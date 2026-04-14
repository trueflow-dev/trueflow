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

ARTIFACT_DIR="$OUTPUT_BASE/$VERSION"
ARCHIVE_NAME="trueflow-${VERSION}-${TARGET}.tar.gz"
CHECKSUM_NAME="trueflow-${VERSION}-SHA256SUMS.txt"
ARCHIVE_PATH="$ARTIFACT_DIR/$ARCHIVE_NAME"
CHECKSUM_PATH="$ARTIFACT_DIR/$CHECKSUM_NAME"

mkdir -p "$ARTIFACT_DIR"
STAGE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-package.XXXXXX")
trap cleanup EXIT INT TERM
PACKAGE_DIR="$STAGE_DIR/package"
mkdir -p "$PACKAGE_DIR"

copy_executable "$BINARY_SOURCE" "$PACKAGE_DIR/trueflow"
cp "$REPO_ROOT/LICENSE" "$PACKAGE_DIR/"
cp "$REPO_ROOT/README.md" "$PACKAGE_DIR/"

printf '==> packaging %s\n' "$ARCHIVE_NAME"
tar -C "$PACKAGE_DIR" -czf "$ARCHIVE_PATH" trueflow LICENSE README.md

(
  cd "$ARTIFACT_DIR"
  : > "$CHECKSUM_NAME"
  found_archive=0
  for archive in trueflow-"${VERSION}"-*.tar.gz; do
    [ -f "$archive" ] || continue
    sha256_line "$archive" >> "$CHECKSUM_NAME"
    found_archive=1
  done
  [ "$found_archive" -eq 1 ] || die "no packaged archives found in $ARTIFACT_DIR"
)

printf '==> wrote %s\n' "$ARCHIVE_PATH"
printf '==> wrote %s\n' "$CHECKSUM_PATH"
printf '==> next: %s/scripts/deploy-downloads.sh %s\n' "$REPO_ROOT" "$ARTIFACT_DIR"
