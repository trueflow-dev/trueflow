#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CRATE_DIR="$REPO_ROOT/trueflow"
TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-$REPO_ROOT/infra/terraform}"
OUTPUT_BASE="${TRUEFLOW_RELEASE_OUTPUT_BASE:-$REPO_ROOT/.trueflow/release-artifacts}"
SNAPSHOT_BASE="${TRUEFLOW_RELEASE_SNAPSHOT_BASE:-$REPO_ROOT/.trueflow/release-snapshots}"
RECEIPT_BASE="${TRUEFLOW_RELEASE_RECEIPT_BASE:-$REPO_ROOT/.trueflow/release-receipts}"
VERSION=""
SKIP_BUILD=0
SKIP_INFRA_APPLY=0
SKIP_WEBSITE=0
SKIP_PACKAGE=0
SKIP_DOWNLOADS=0
AUTO_APPROVE=0
MACOS_BINARY=""
PUBLICATION_ACTIVE=0
WEBSITE_SWITCHED=0
RECEIPT=""

usage() {
  cat <<'EOF'
Usage: deploy-public-site.sh [options]

Run a safe public release switch for trueflow.dev. The normal order is:

1. package the macOS artifact; freeze an exact private two-target snapshot;
   validate, smoke-test the frozen macOS archive, and verify website version;
2. run OpenTofu init/fmt/validate/apply;
3. begin a receipt-bound download publication (old and new downloads coexist);
4. sync/invalidate the website while that publication lock remains held; and
5. finalize obsolete-download cleanup and release the lock.

Options:
  --version vX.Y.Z     Override the release version (default: read from Cargo.toml).
  --output-dir DIR     Base directory for versioned release artifacts (default: .trueflow/release-artifacts).
  --skip-build         Reuse the existing native macOS release binary when packaging.
  --macos-binary PATH  Package this supplied aarch64-apple-darwin binary instead of building locally.
  --skip-infra-apply   Skip tofu apply after init/fmt/validate.
  --skip-website       Prepare a validated download publication, then abort it without cleanup or website switch.
  --skip-package       Reuse the existing complete release artifact directory.
  --skip-downloads     Run only the standalone locked website deploy; it never cleans downloads.
  --auto-approve       Pass -auto-approve to tofu apply.
  -h, --help           Show this help text.

There is no direct destructive download deployment. A failure before the
receipt-bound website switch aborts the held PREPARED lock and retains both
release sets. A failure after the website switch leaves the verified lock for
explicit finalize/recovery rather than guessing at cleanup.
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

validate_version() {
  value=$1
  case "$value" in v[0-9]*) ;; *) die "invalid release version: $value" ;; esac
  case "$value" in *[!A-Za-z0-9._-]*) die "invalid release version: $value" ;; esac
}

website_release_version() {
  install_version=$(awk -F= '
    $1 == "DEFAULT_VERSION" {
      count++
      value = $2
      gsub(/"/, "", value)
      gsub(/[[:space:]]/, "", value)
    }
    END { if (count != 1 || value == "") exit 1; print value }
  ' "$REPO_ROOT/website/install.sh") || die "website/install.sh must define one DEFAULT_VERSION"
  links_version=$(awk '
    function record(version, kind) {
      if (release == "") release = version
      else if (release != version) bad = 1
      if (kind == "macos") macos++
      else if (kind == "linux") linux++
      else manifest++
    }
    {
      rest = $0
      while ((offset = index(rest, "/download/trueflow-")) != 0) {
        rest = substr(rest, offset + length("/download/trueflow-"))
        if (rest ~ /^v[0-9A-Za-z._-]*-aarch64-apple-darwin[.]tar[.]gz/) {
          candidate = rest; sub(/-aarch64-apple-darwin[.]tar[.]gz.*/, "", candidate); record(candidate, "macos")
        } else if (rest ~ /^v[0-9A-Za-z._-]*-x86_64-unknown-linux-musl[.]tar[.]gz/) {
          candidate = rest; sub(/-x86_64-unknown-linux-musl[.]tar[.]gz.*/, "", candidate); record(candidate, "linux")
        } else if (rest ~ /^v[0-9A-Za-z._-]*-SHA256SUMS[.]txt/) {
          candidate = rest; sub(/-SHA256SUMS[.]txt.*/, "", candidate); record(candidate, "manifest")
        }
        rest = substr(rest, 2)
      }
    }
    END { if (bad || macos < 1 || linux < 1 || manifest < 1 || release == "") exit 1; print release }
  ' "$REPO_ROOT/website/install/index.html") || die "website/install/index.html must name one consistent release version across its archive and manifest URLs"
  [ "$install_version" = "$links_version" ] || die "website installer and download links name different release versions"
  validate_version "$install_version"
  printf '%s\n' "$install_version"
}

new_nonce() {
  nonce=$(LC_ALL=C od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
  [ "${#nonce}" -eq 32 ] || die "failed to generate release nonce"
  case "$nonce" in *[!0123456789abcdef]*) die "failed to generate release nonce" ;; esac
  printf '%s\n' "$nonce"
}

run_infrastructure() {
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
}

abort_on_early_failure() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$PUBLICATION_ACTIVE" -eq 1 ] && [ "$WEBSITE_SWITCHED" -eq 0 ]; then
    if ! "$SCRIPT_DIR/deploy-downloads.sh" --abort-publication "$RECEIPT"; then
      printf 'error: publication remained locked after failed pre-switch flow: %s\n' "$RECEIPT" >&2
    fi
  fi
  exit "$status"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      shift; [ "$#" -gt 0 ] || die "--version requires a value"; VERSION=$1 ;;
    --output-dir)
      shift; [ "$#" -gt 0 ] || die "--output-dir requires a value"; OUTPUT_BASE=$1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --macos-binary)
      shift; [ "$#" -gt 0 ] || die "--macos-binary requires a value"; MACOS_BINARY=$1 ;;
    --skip-infra-apply) SKIP_INFRA_APPLY=1 ;;
    --skip-website) SKIP_WEBSITE=1 ;;
    --skip-package) SKIP_PACKAGE=1 ;;
    --skip-downloads) SKIP_DOWNLOADS=1 ;;
    --auto-approve) AUTO_APPROVE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
  shift
done

if [ -z "$VERSION" ]; then
  VERSION=$(read_default_version)
fi
validate_version "$VERSION"
ARTIFACT_DIR="$OUTPUT_BASE/$VERSION"

if [ "$SKIP_DOWNLOADS" -eq 1 ]; then
  [ "$SKIP_WEBSITE" -eq 0 ] || die "--skip-downloads and --skip-website leave no coherent public operation"
  printf '==> website-only deployment: verifying the website release under its standalone lock\n'
  website_version=$(website_release_version)
  [ "$website_version" = "$VERSION" ] || die "website version $website_version does not match requested version $VERSION"
  run_infrastructure
  "$SCRIPT_DIR/deploy-website.sh"
  printf '==> standalone website deployment complete; downloads were not changed\n'
  exit 0
fi

if [ "$SKIP_PACKAGE" -eq 0 ]; then
  set -- --version "$VERSION" --output-dir "$OUTPUT_BASE"
  if [ -n "$MACOS_BINARY" ]; then
    set -- "$@" --binary "$MACOS_BINARY"
  elif [ "$SKIP_BUILD" -eq 1 ]; then
    set -- "$@" --skip-build
  fi
  printf '==> packaging macOS release artifact\n'
  "$SCRIPT_DIR/package-macos-release.sh" "$@"
else
  printf '==> reusing existing release artifacts\n'
fi
[ -d "$ARTIFACT_DIR" ] || die "artifact directory not found: $ARTIFACT_DIR"

# Snapshot preparation revalidates all source bytes and creates a mode-0700 exact
# copy. Every later smoke/publish/finalize operation uses only this path.
umask 077
mkdir -p "$SNAPSHOT_BASE" "$RECEIPT_BASE"
snapshot_nonce=$(new_nonce)
SNAPSHOT_DIR="$SNAPSHOT_BASE/${VERSION}-${snapshot_nonce}"
RECEIPT="$RECEIPT_BASE/${VERSION}-${snapshot_nonce}.receipt"
printf '==> freezing exact release snapshot\n'
"$SCRIPT_DIR/deploy-downloads.sh" --prepare-snapshot "$ARTIFACT_DIR" "$SNAPSHOT_DIR"
"$SCRIPT_DIR/deploy-downloads.sh" --validate-only "$SNAPSHOT_DIR"

MACOS_ARCHIVE="$SNAPSHOT_DIR/trueflow-${VERSION}-aarch64-apple-darwin.tar.gz"
printf '==> smoke-testing frozen macOS archive\n'
"$SCRIPT_DIR/smoke-test-release.sh" "$MACOS_ARCHIVE"
website_version=$(website_release_version)
[ "$website_version" = "$VERSION" ] || die "website version $website_version does not match frozen release version $VERSION"
printf '==> website version matches frozen release %s\n' "$VERSION"

run_infrastructure
printf '==> beginning receipt-bound download publication\n'
"$SCRIPT_DIR/deploy-downloads.sh" --begin-publication "$SNAPSHOT_DIR" "$RECEIPT"
PUBLICATION_ACTIVE=1
trap abort_on_early_failure EXIT
trap 'exit 1' HUP INT TERM

if [ "$SKIP_WEBSITE" -eq 1 ]; then
  printf '==> website switch skipped; aborting prepared publication without download cleanup\n'
  "$SCRIPT_DIR/deploy-downloads.sh" --abort-publication "$RECEIPT"
  PUBLICATION_ACTIVE=0
  trap - EXIT HUP INT TERM
  printf '==> validated prepare-only publication aborted; old downloads remain\n'
  exit 0
fi

printf '==> switching website under publication receipt\n'
"$SCRIPT_DIR/deploy-website.sh" --publication-receipt "$RECEIPT"
WEBSITE_SWITCHED=1
printf '==> finalizing old-download cleanup after website switch\n'
"$SCRIPT_DIR/deploy-downloads.sh" --finalize-publication "$SNAPSHOT_DIR" "$RECEIPT"
PUBLICATION_ACTIVE=0
trap - EXIT HUP INT TERM

printf '==> public site deployment flow complete\n'
printf '==> site: https://trueflow.dev/\n'
printf '==> install: https://trueflow.dev/install/\n'
printf '==> downloads: https://trueflow.dev/download/\n'
