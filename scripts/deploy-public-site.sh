#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CRATE_DIR="$REPO_ROOT/trueflow"
TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-$REPO_ROOT/infra/terraform}"
OUTPUT_BASE="${TRUEFLOW_RELEASE_OUTPUT_BASE:-$REPO_ROOT/.trueflow/release-artifacts}"
VERSION=""
SKIP_BUILD=0
SKIP_INFRA_APPLY=0
SKIP_WEBSITE=0
SKIP_PACKAGE=0
SKIP_DOWNLOADS=0
AUTO_APPROVE=0

usage() {
  cat <<'EOF'
Usage: deploy-public-site.sh [options]

Run the full public-site deployment flow for trueflow.dev:

1. OpenTofu init/fmt/validate/apply for infra/terraform
2. Upload website/ to the site bucket and invalidate CloudFront
3. Build/package the Apple Silicon macOS binary artifact
4. Upload /download artifacts and invalidate CloudFront download paths

Options:
  --version vX.Y.Z     Override the release version (default: read from Cargo.toml).
  --output-dir DIR     Base directory for versioned release artifacts (default: .trueflow/release-artifacts).
  --skip-build         Reuse the existing release binary when packaging.
  --skip-infra-apply   Skip tofu apply after init/fmt/validate.
  --skip-website       Skip syncing website/.
  --skip-package       Skip packaging the macOS release artifact.
  --skip-downloads     Skip syncing /download artifacts.
  --auto-approve       Pass -auto-approve to tofu apply.
  -h, --help           Show this help text.
EOF
}

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

read_default_version() {
  cargo_version=$(awk -F '"' '/^version = "/ { print $2; exit }' "$CRATE_DIR/Cargo.toml")
  [ -n "$cargo_version" ] || die "failed to read version from $CRATE_DIR/Cargo.toml"
  printf 'v%s\n' "$cargo_version"
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
    --skip-infra-apply)
      SKIP_INFRA_APPLY=1
      ;;
    --skip-website)
      SKIP_WEBSITE=1
      ;;
    --skip-package)
      SKIP_PACKAGE=1
      ;;
    --skip-downloads)
      SKIP_DOWNLOADS=1
      ;;
    --auto-approve)
      AUTO_APPROVE=1
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

if [ -z "$VERSION" ]; then
  VERSION=$(read_default_version)
fi

ARTIFACT_DIR="$OUTPUT_BASE/$VERSION"

printf '==> tofu init\n'
"$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" init
printf '==> tofu fmt -check\n'
"$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" fmt -check -recursive
printf '==> tofu validate\n'
"$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" validate

if [ "$SKIP_INFRA_APPLY" -eq 0 ]; then
  printf '==> tofu apply\n'
  if [ "$AUTO_APPROVE" -eq 1 ]; then
    "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" apply -auto-approve
  else
    "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" apply
  fi
else
  printf '==> skipping tofu apply\n'
fi

if [ "$SKIP_WEBSITE" -eq 0 ]; then
  printf '==> deploying website content\n'
  "$REPO_ROOT/scripts/deploy-website.sh"
else
  printf '==> skipping website deploy\n'
fi

if [ "$SKIP_PACKAGE" -eq 0 ]; then
  printf '==> packaging macOS release artifact\n'
  if [ "$SKIP_BUILD" -eq 1 ]; then
    "$REPO_ROOT/scripts/package-macos-release.sh" --version "$VERSION" --output-dir "$OUTPUT_BASE" --skip-build
  else
    "$REPO_ROOT/scripts/package-macos-release.sh" --version "$VERSION" --output-dir "$OUTPUT_BASE"
  fi
else
  printf '==> skipping macOS packaging\n'
fi

if [ "$SKIP_DOWNLOADS" -eq 0 ]; then
  [ -d "$ARTIFACT_DIR" ] || die "artifact directory not found: $ARTIFACT_DIR"
  printf '==> deploying download artifacts from %s\n' "$ARTIFACT_DIR"
  "$REPO_ROOT/scripts/deploy-downloads.sh" "$ARTIFACT_DIR"
else
  printf '==> skipping download deploy\n'
fi

printf '==> public site deployment flow complete\n'
printf '==> site: https://trueflow.dev/\n'
printf '==> install: https://trueflow.dev/install/\n'
printf '==> downloads: https://trueflow.dev/download/\n'
