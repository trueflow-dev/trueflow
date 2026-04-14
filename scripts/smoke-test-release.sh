#!/bin/sh
set -eu

if [ $# -ne 1 ]; then
  printf 'usage: %s ARTIFACT_TARBALL\n' "$0" >&2
  exit 1
fi

ARTIFACT_TARBALL=$1
[ -f "$ARTIFACT_TARBALL" ] || {
  printf 'error: artifact not found: %s\n' "$ARTIFACT_TARBALL" >&2
  exit 1
}

have_command() {
  command -v "$1" >/dev/null 2>&1
}

cleanup() {
  if [ -n "${STAGE_DIR:-}" ] && [ -d "$STAGE_DIR" ] && have_command trash; then
    trash "$STAGE_DIR" >/dev/null 2>&1 || true
  fi
}

STAGE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-release-smoke.XXXXXX")
trap cleanup EXIT INT TERM
PACKAGE_DIR="$STAGE_DIR/package"
REPO_DIR="$STAGE_DIR/repo"
HOME_DIR="$STAGE_DIR/home"
OUTPUT_JSON="$STAGE_DIR/review.json"
mkdir -p "$PACKAGE_DIR" "$REPO_DIR/src" "$HOME_DIR"

tar -xzf "$ARTIFACT_TARBALL" -C "$PACKAGE_DIR"

BINARY_PATH="$PACKAGE_DIR/trueflow"
README_PATH="$PACKAGE_DIR/README.md"
LICENSE_PATH="$PACKAGE_DIR/LICENSE"

[ -f "$BINARY_PATH" ] || {
  printf 'error: packaged binary missing at %s\n' "$BINARY_PATH" >&2
  exit 1
}
[ -f "$README_PATH" ] || {
  printf 'error: packaged README missing at %s\n' "$README_PATH" >&2
  exit 1
}
[ -f "$LICENSE_PATH" ] || {
  printf 'error: packaged LICENSE missing at %s\n' "$LICENSE_PATH" >&2
  exit 1
}

printf '==> checking trueflow --version\n'
HOME="$HOME_DIR" "$BINARY_PATH" --version >/dev/null

cat > "$REPO_DIR/src/lib.rs" <<'EOF'
pub struct Widget {
    pub name: String,
}

impl Widget {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}
EOF

printf '==> checking trueflow review --all --json\n'
(
  cd "$REPO_DIR"
  HOME="$HOME_DIR" "$BINARY_PATH" review --all --json > "$OUTPUT_JSON"
)

grep -q '"blocks"' "$OUTPUT_JSON" || {
  printf 'error: review output did not contain block data\n' >&2
  exit 1
}

printf '==> smoke test passed for %s\n' "$ARTIFACT_TARBALL"
