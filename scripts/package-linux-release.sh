#!/bin/sh
set -eu

TARGET="x86_64-unknown-linux-musl"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
DEFAULT_OUTPUT_BASE="$REPO_ROOT/.trueflow/release-artifacts"
OUTPUT_BASE=$DEFAULT_OUTPUT_BASE
VERSION=""
BINARY_SOURCE=""

usage() {
  cat <<'EOF'
Usage: package-linux-release.sh [--version vX.Y.Z] [--output-dir DIR] [--binary PATH]

Build and package the Linux x86_64 musl trueflow binary on native Linux x86_64.
By default this script builds via `nix build .#release` and then packages the
result into the artifact format used under https://trueflow.dev/download/.

Options:
  --version VERSION   Override the version label (default: read from Cargo.toml).
  --output-dir DIR    Base directory for versioned artifacts (default: .trueflow/release-artifacts).
  --binary PATH       Package an already-built binary instead of running nix build.
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

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64)
    ;;
  *)
    die "this script packages the native Linux x86_64 musl release; run it on Linux x86_64"
    ;;
esac

if [ -z "$BINARY_SOURCE" ]; then
  have_command nix || die "nix is required unless --binary is provided"
  printf '==> building trueflow Linux x86_64 musl release with nix\n'
  BUILD_OUTPUT=$(cd "$REPO_ROOT" && nix build --no-link --print-out-paths .#release)
  [ -n "$BUILD_OUTPUT" ] || die "nix build did not print an output path"
  BINARY_SOURCE="$BUILD_OUTPUT/bin/trueflow"
else
  printf '==> packaging prebuilt binary %s\n' "$BINARY_SOURCE"
fi

set -- \
  --target "$TARGET" \
  --binary "$BINARY_SOURCE" \
  --output-dir "$OUTPUT_BASE"

if [ -n "$VERSION" ]; then
  set -- "$@" --version "$VERSION"
fi

"$REPO_ROOT/scripts/package-built-release.sh" "$@"
