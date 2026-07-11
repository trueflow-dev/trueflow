#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

usage() {
  cat <<'EOF'
Usage: deploy-public-site-fast.sh [options]

Fast path for a safe public release switch after infrastructure already exists.
It forwards all options to deploy-public-site.sh with --skip-infra-apply.

Normal inherited ordering is: package; freeze and strictly validate a private
snapshot; smoke-test that frozen macOS archive; verify website version; run tofu
init/fmt/validate without apply; begin the held download publication; perform
receipt-bound website sync/invalidation; then finalize download cleanup.

--skip-downloads selects the inherited standalone locked website mode and never
removes downloads. --skip-website prepares then aborts a publication without
cleanup. A bare artifact directory is never a destructive deployment command.

Examples:
  ./scripts/deploy-public-site-fast.sh
  ./scripts/deploy-public-site-fast.sh --skip-build
  ./scripts/deploy-public-site-fast.sh --version v0.1.1
  ./scripts/deploy-public-site-fast.sh --macos-binary path/to/aarch64-apple-darwin/trueflow
EOF
}

if [ "$#" -gt 0 ]; then
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
  esac
fi

exec "$SCRIPT_DIR/deploy-public-site.sh" --skip-infra-apply "$@"
