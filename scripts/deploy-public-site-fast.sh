#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

usage() {
  cat <<'EOF'
Usage: deploy-public-site-fast.sh [options]

Fast path for redeploying trueflow.dev content after infra already exists.

This wrapper calls deploy-public-site.sh with --skip-infra-apply, so it will:

1. run tofu init/fmt/validate
2. skip tofu apply
3. upload website/
4. package the Apple Silicon macOS binary artifact
5. upload /download artifacts

All other flags are forwarded through.

Examples:
  ./scripts/deploy-public-site-fast.sh
  ./scripts/deploy-public-site-fast.sh --skip-build
  ./scripts/deploy-public-site-fast.sh --version v0.1.0
EOF
}

if [ $# -gt 0 ]; then
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
  esac
fi

exec "$SCRIPT_DIR/deploy-public-site.sh" --skip-infra-apply "$@"
