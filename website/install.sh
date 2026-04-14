#!/bin/sh
set -eu

BASE_URL="${TRUEFLOW_BASE_URL:-https://trueflow.dev}"
DEFAULT_VERSION="v0.1.0"
DEFAULT_INSTALL_DIR="${HOME}/.local/bin"
VERSION="${TRUEFLOW_VERSION:-$DEFAULT_VERSION}"
INSTALL_DIR="${TRUEFLOW_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"

usage() {
  cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--to DIR]

Installs the supported trueflow binary from trueflow.dev.

Options:
  --version VERSION  Install a specific versioned artifact.
  --to DIR           Install into DIR instead of ~/.local/bin.
  -h, --help         Show this help text.
EOF
}

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

have_command() {
  command -v "$1" >/dev/null 2>&1
}

download_to() {
  url=$1
  destination=$2

  if have_command curl; then
    curl -fsSL "$url" -o "$destination"
    return
  fi

  if have_command wget; then
    wget -qO "$destination" "$url"
    return
  fi

  die "curl or wget is required"
}

sha256_of() {
  path=$1

  if have_command shasum; then
    shasum -a 256 "$path" | awk '{ print $1 }'
    return
  fi

  if have_command sha256sum; then
    sha256sum "$path" | awk '{ print $1 }'
    return
  fi

  die "shasum or sha256sum is required"
}

verify_checksum() {
  archive_path=$1
  checksum_path=$2
  archive_name=$(basename "$archive_path")
  expected_checksum=$(awk -v target="$archive_name" '$2 == target { print $1 }' "$checksum_path")

  [ -n "$expected_checksum" ] || die "no checksum entry found for ${archive_name}"

  actual_checksum=$(sha256_of "$archive_path")
  [ "$actual_checksum" = "$expected_checksum" ] || die "checksum mismatch for ${archive_name}"
}

detect_target() {
  os_name=$(uname -s)
  arch_name=$(uname -m)

  case "$os_name:$arch_name" in
    Darwin:arm64|Darwin:aarch64)
      printf '%s\n' 'aarch64-apple-darwin'
      ;;
    *)
      return 1
      ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      shift
      [ $# -gt 0 ] || die "--version requires a value"
      VERSION=$1
      ;;
    --to)
      shift
      [ $# -gt 0 ] || die "--to requires a value"
      INSTALL_DIR=$1
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

TARGET=$(detect_target) || die "unsupported platform; current draft support is Apple Silicon macOS only. See ${BASE_URL}/install/"
ARCHIVE_NAME="trueflow-${VERSION}-${TARGET}.tar.gz"
CHECKSUM_NAME="trueflow-${VERSION}-SHA256SUMS.txt"
ARCHIVE_URL="${BASE_URL}/download/${ARCHIVE_NAME}"
CHECKSUM_URL="${BASE_URL}/download/${CHECKSUM_NAME}"
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-install.XXXXXX")
ARCHIVE_PATH="${TEMP_DIR}/${ARCHIVE_NAME}"
CHECKSUM_PATH="${TEMP_DIR}/${CHECKSUM_NAME}"

printf '==> downloading %s\n' "$ARCHIVE_URL"
download_to "$ARCHIVE_URL" "$ARCHIVE_PATH"
printf '==> downloading %s\n' "$CHECKSUM_URL"
download_to "$CHECKSUM_URL" "$CHECKSUM_PATH"
printf '==> verifying checksum\n'
verify_checksum "$ARCHIVE_PATH" "$CHECKSUM_PATH"

mkdir -p "$INSTALL_DIR"
tar -xzf "$ARCHIVE_PATH" -C "$TEMP_DIR"
BINARY_PATH="${TEMP_DIR}/trueflow"
[ -f "$BINARY_PATH" ] || die "archive did not contain a trueflow binary at the top level"

if have_command install; then
  install -m 0755 "$BINARY_PATH" "$INSTALL_DIR/trueflow"
else
  cp "$BINARY_PATH" "$INSTALL_DIR/trueflow"
  chmod 0755 "$INSTALL_DIR/trueflow"
fi

printf '==> installed trueflow to %s/trueflow\n' "$INSTALL_DIR"
case ":$PATH:" in
  *:"$INSTALL_DIR":*)
    ;;
  *)
    printf 'note: add %s to PATH if needed\n' "$INSTALL_DIR"
    ;;
esac
printf '==> run %s/trueflow --version\n' "$INSTALL_DIR"
