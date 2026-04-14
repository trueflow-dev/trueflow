#!/bin/sh
set -eu

TARGET="aarch64-apple-darwin"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CRATE_DIR="$REPO_ROOT/trueflow"
DEFAULT_OUTPUT_BASE="$REPO_ROOT/.trueflow/release-artifacts"
SKIP_BUILD=0
OUTPUT_BASE=$DEFAULT_OUTPUT_BASE
VERSION=""

usage() {
  cat <<'EOF'
Usage: package-macos-release.sh [--version vX.Y.Z] [--output-dir DIR] [--skip-build]

Build and package the Apple Silicon macOS trueflow binary into the versioned
artifact format expected by https://trueflow.dev/install.sh.

Options:
  --version VERSION   Override the version label (default: read from Cargo.toml).
  --output-dir DIR    Base directory for versioned artifacts (default: .trueflow/release-artifacts).
  --skip-build        Reuse the existing release binary without rebuilding.
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
    --skip-build)
      SKIP_BUILD=1
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

case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64)
    ;;
  *)
    die "this script packages the local Apple Silicon macOS binary; run it on Apple Silicon macOS"
    ;;
esac

if [ -z "$VERSION" ]; then
  VERSION=$(read_default_version)
fi

ARTIFACT_DIR="$OUTPUT_BASE/$VERSION"
ARCHIVE_NAME="trueflow-${VERSION}-${TARGET}.tar.gz"
CHECKSUM_NAME="trueflow-${VERSION}-SHA256SUMS.txt"
ARCHIVE_PATH="$ARTIFACT_DIR/$ARCHIVE_NAME"
CHECKSUM_PATH="$ARTIFACT_DIR/$CHECKSUM_NAME"
BINARY_SOURCE="$CRATE_DIR/target/release/trueflow"

if [ "$SKIP_BUILD" -eq 0 ]; then
  printf '==> building trueflow release binary\n'
  (
    cd "$CRATE_DIR"
    cargo build --release --locked
  )
else
  printf '==> skipping build and reusing existing release binary\n'
fi

[ -f "$BINARY_SOURCE" ] || die "expected release binary at $BINARY_SOURCE"

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
  sha256_line "$ARCHIVE_NAME" > "$CHECKSUM_NAME"
)

printf '==> wrote %s\n' "$ARCHIVE_PATH"
printf '==> wrote %s\n' "$CHECKSUM_PATH"
printf '==> next: %s/scripts/deploy-downloads.sh %s\n' "$REPO_ROOT" "$ARTIFACT_DIR"
