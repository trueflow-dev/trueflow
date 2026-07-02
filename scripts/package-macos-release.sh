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
BINARY_SOURCE=""
usage() {
  cat <<'EOF'
Usage: package-macos-release.sh [--version vX.Y.Z] [--output-dir DIR] [--skip-build] [--binary PATH]

Build and package the Apple Silicon macOS trueflow binary into the versioned
artifact format expected by https://trueflow.dev/install.sh.

Options:
  --version VERSION   Override the version label (default: read from Cargo.toml).
  --output-dir DIR    Base directory for versioned artifacts (default: .trueflow/release-artifacts).
  --skip-build        Reuse the existing native release binary without rebuilding.
  --binary PATH       Package a supplied aarch64-apple-darwin binary instead of building locally.
  -h, --help          Show this help text.
EOF
}

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
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
    --binary)
      shift
      [ $# -gt 0 ] || die "--binary requires a value"
      BINARY_SOURCE=$1
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

if [ -n "$BINARY_SOURCE" ]; then
  printf '==> packaging supplied macOS binary %s\n' "$BINARY_SOURCE"
else
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64)
      ;;
    *)
      die "this script builds the local Apple Silicon macOS binary only on Apple Silicon macOS; pass --binary PATH to package a supplied macOS binary"
      ;;
  esac

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
fi

[ -f "$BINARY_SOURCE" ] || die "expected release binary at $BINARY_SOURCE"

set -- \
  --target "$TARGET" \
  --binary "$BINARY_SOURCE" \
  --output-dir "$OUTPUT_BASE"

if [ -n "$VERSION" ]; then
  set -- "$@" --version "$VERSION"
fi

"$REPO_ROOT/scripts/package-built-release.sh" "$@"
