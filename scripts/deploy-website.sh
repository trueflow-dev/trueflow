#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
DOWNLOADS_SCRIPT="$SCRIPT_DIR/deploy-downloads.sh"
TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-$REPO_ROOT/infra/terraform}"
LOCK_KEY=".trueflow-release/publication.lock"
LEDGER_PREFIX=".trueflow-release/ledger"

usage() {
  cat <<'EOF'
Usage: deploy-website.sh [--publication-receipt RECEIPT]

Synchronize website-owned bucket keys and invalidate the website cache.

Without a receipt, this is a standalone locked website deployment: it derives
one exact website release version, verifies that release against its immutable
ledger and public files, then syncs website-owned keys without download cleanup.

With --publication-receipt, the receipt must currently own a PREPARED download
publication for the version named by the website. The script keeps that lock
through website sync and invalidation, then records WEBSITE_SWITCHED. It never
releases the lock or removes downloads; finalize-publication owns that step.
EOF
}

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

have_command() {
  command -v "$1" >/dev/null 2>&1
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

is_lower_sha256() {
  value=$1
  [ "${#value}" -eq 64 ] || return 1
  case "$value" in ''|*[!0123456789abcdef]*) return 1 ;; esac
  return 0
}

is_nonce() {
  value=$1
  [ "${#value}" -eq 32 ] || return 1
  case "$value" in ''|*[!0123456789abcdef]*) return 1 ;; esac
  return 0
}

validate_version() {
  value=$1
  case "$value" in v[0-9]*) ;; *) die "invalid website release version: $value" ;; esac
  case "$value" in *[!A-Za-z0-9._-]*) die "invalid website release version: $value" ;; esac
}

website_release_version() {
  install_version=$(awk -F= '
    $1 == "DEFAULT_VERSION" {
      count++
      value = $2
      gsub(/"/, "", value)
      gsub(/[[:space:]]/, "", value)
    }
    END {
      if (count != 1 || value == "") exit 1
      print value
    }
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
          candidate = rest
          sub(/-aarch64-apple-darwin[.]tar[.]gz.*/, "", candidate)
          record(candidate, "macos")
        } else if (rest ~ /^v[0-9A-Za-z._-]*-x86_64-unknown-linux-musl[.]tar[.]gz/) {
          candidate = rest
          sub(/-x86_64-unknown-linux-musl[.]tar[.]gz.*/, "", candidate)
          record(candidate, "linux")
        } else if (rest ~ /^v[0-9A-Za-z._-]*-SHA256SUMS[.]txt/) {
          candidate = rest
          sub(/-SHA256SUMS[.]txt.*/, "", candidate)
          record(candidate, "manifest")
        }
        rest = substr(rest, 2)
      }
    }
    END {
      if (bad || macos < 1 || linux < 1 || manifest < 1 || release == "") exit 1
      print release
    }
  ' "$REPO_ROOT/website/install/index.html") || die "website/install/index.html must name one consistent release version across its archive and manifest URLs"

  [ "$install_version" = "$links_version" ] || die "website installer and download links name different release versions"
  validate_version "$install_version"
  printf '%s\n' "$install_version"
}

infra_output() {
  output_key=$1
  "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" output -raw "$output_key"
}

resolve_infrastructure() {
  SITE_BUCKET="${TRUEFLOW_SITE_BUCKET:-$(infra_output site_bucket_name)}"
  DISTRIBUTION_ID="${TRUEFLOW_DISTRIBUTION_ID:-$(infra_output site_distribution_id)}"
  [ -n "$SITE_BUCKET" ] || die "empty site bucket"
  [ -n "$DISTRIBUTION_ID" ] || die "empty site distribution"
}

run_release_barrier() {
  phase=$1
  if [ -n "${TRUEFLOW_RELEASE_BARRIER:-}" ]; then
    [ -x "$TRUEFLOW_RELEASE_BARRIER" ] || die "TRUEFLOW_RELEASE_BARRIER is not executable"
    "$TRUEFLOW_RELEASE_BARRIER" "$phase"
  fi
}

s3_head() {
  head_bucket=$1
  head_key=$2
  S3_HEAD_ERROR=$(mktemp "${TMPDIR:-/tmp}/trueflow-website-head.XXXXXX") || die "failed to allocate S3 error file"
  if S3_HEAD_OUTPUT=$(aws s3api head-object \
    --bucket "$head_bucket" \
    --key "$head_key" \
    --query '[ETag, VersionId]' \
    --output text 2>"$S3_HEAD_ERROR"); then
    set -- $S3_HEAD_OUTPUT
    [ "$#" -eq 2 ] || die "ambiguous S3 HEAD metadata for $head_key"
    S3_ETAG=$1
    S3_VERSION=$2
    case "$S3_ETAG:$S3_VERSION" in :*|*:|*:None|*:null) die "missing S3 ETag or VersionId for $head_key" ;; esac
    return 0
  fi
  S3_HEAD_FAILURE=$(cat "$S3_HEAD_ERROR")
  case "$S3_HEAD_FAILURE" in
    *404*|*NoSuchKey*|*NotFound*|*'Not Found'*|*'not found'*|*'object absent'*) return 1 ;;
    *) die "ambiguous S3 HEAD failure for $head_key: $S3_HEAD_FAILURE" ;;
  esac
}

get_current_object() {
  aws s3api get-object --bucket "$1" --key "$2" "$3" >/dev/null
}

receipt_field() {
  receipt_path=$1
  expected_field=$2
  [ -f "$receipt_path" ] && [ ! -L "$receipt_path" ] || die "receipt is not a regular file: $receipt_path"
  field_value=$(awk -v expected="$expected_field" '
    index($0, expected "=") == 1 { count++; value = substr($0, length(expected) + 2) }
    END { if (count != 1 || value == "") exit 1; print value }
  ' "$receipt_path") || die "receipt is missing an unambiguous $expected_field field"
  case "$field_value" in *' '*|*'\t'*|*=*) die "unsafe receipt field: $expected_field" ;; esac
  printf '%s\n' "$field_value"
}

read_ledger() {
  ledger_path=$1
  LEDGER_VERSION= LEDGER_ARCHIVE_MACOS_NAME= LEDGER_ARCHIVE_MACOS_SHA256=
  LEDGER_ARCHIVE_LINUX_NAME= LEDGER_ARCHIVE_LINUX_SHA256= LEDGER_MANIFEST_NAME= LEDGER_MANIFEST_SHA256=
  LEDGER_VERSION_SEEN=0 LEDGER_ARCHIVE_MACOS_NAME_SEEN=0 LEDGER_ARCHIVE_MACOS_SHA256_SEEN=0
  LEDGER_ARCHIVE_LINUX_NAME_SEEN=0 LEDGER_ARCHIVE_LINUX_SHA256_SEEN=0 LEDGER_MANIFEST_NAME_SEEN=0 LEDGER_MANIFEST_SHA256_SEEN=0
  while IFS= read -r ledger_line || [ -n "$ledger_line" ]; do
    case "$ledger_line" in *=*) ;; *) die "malformed immutable release ledger" ;; esac
    ledger_key=${ledger_line%%=*}
    ledger_value=${ledger_line#*=}
    [ -n "$ledger_value" ] || die "empty immutable release ledger field"
    case "$ledger_value" in *' '*|*'\t'*|*=*) die "unsafe immutable release ledger value" ;; esac
    case "$ledger_key" in
      version) [ "$LEDGER_VERSION_SEEN" -eq 0 ] || die "duplicate immutable release ledger version"; LEDGER_VERSION=$ledger_value; LEDGER_VERSION_SEEN=1 ;;
      archive_macos_name) [ "$LEDGER_ARCHIVE_MACOS_NAME_SEEN" -eq 0 ] || die "duplicate immutable release ledger macOS name"; LEDGER_ARCHIVE_MACOS_NAME=$ledger_value; LEDGER_ARCHIVE_MACOS_NAME_SEEN=1 ;;
      archive_macos_sha256) [ "$LEDGER_ARCHIVE_MACOS_SHA256_SEEN" -eq 0 ] || die "duplicate immutable release ledger macOS digest"; LEDGER_ARCHIVE_MACOS_SHA256=$ledger_value; LEDGER_ARCHIVE_MACOS_SHA256_SEEN=1 ;;
      archive_linux_name) [ "$LEDGER_ARCHIVE_LINUX_NAME_SEEN" -eq 0 ] || die "duplicate immutable release ledger Linux name"; LEDGER_ARCHIVE_LINUX_NAME=$ledger_value; LEDGER_ARCHIVE_LINUX_NAME_SEEN=1 ;;
      archive_linux_sha256) [ "$LEDGER_ARCHIVE_LINUX_SHA256_SEEN" -eq 0 ] || die "duplicate immutable release ledger Linux digest"; LEDGER_ARCHIVE_LINUX_SHA256=$ledger_value; LEDGER_ARCHIVE_LINUX_SHA256_SEEN=1 ;;
      manifest_name) [ "$LEDGER_MANIFEST_NAME_SEEN" -eq 0 ] || die "duplicate immutable release ledger manifest name"; LEDGER_MANIFEST_NAME=$ledger_value; LEDGER_MANIFEST_NAME_SEEN=1 ;;
      manifest_sha256) [ "$LEDGER_MANIFEST_SHA256_SEEN" -eq 0 ] || die "duplicate immutable release ledger manifest digest"; LEDGER_MANIFEST_SHA256=$ledger_value; LEDGER_MANIFEST_SHA256_SEEN=1 ;;
      *) die "unknown immutable release ledger field: $ledger_key" ;;
    esac
  done < "$ledger_path"
  [ "$LEDGER_VERSION_SEEN" -eq 1 ] && [ "$LEDGER_ARCHIVE_MACOS_NAME_SEEN" -eq 1 ] && [ "$LEDGER_ARCHIVE_MACOS_SHA256_SEEN" -eq 1 ] && \
    [ "$LEDGER_ARCHIVE_LINUX_NAME_SEEN" -eq 1 ] && [ "$LEDGER_ARCHIVE_LINUX_SHA256_SEEN" -eq 1 ] && \
    [ "$LEDGER_MANIFEST_NAME_SEEN" -eq 1 ] && [ "$LEDGER_MANIFEST_SHA256_SEEN" -eq 1 ] || die "incomplete immutable release ledger"
  [ "$LEDGER_VERSION" = "$WEBSITE_VERSION" ] || die "immutable ledger version does not match website"
  [ "$LEDGER_ARCHIVE_MACOS_NAME" = "trueflow-${WEBSITE_VERSION}-aarch64-apple-darwin.tar.gz" ] || die "immutable ledger macOS name does not match website"
  [ "$LEDGER_ARCHIVE_LINUX_NAME" = "trueflow-${WEBSITE_VERSION}-x86_64-unknown-linux-musl.tar.gz" ] || die "immutable ledger Linux name does not match website"
  [ "$LEDGER_MANIFEST_NAME" = "trueflow-${WEBSITE_VERSION}-SHA256SUMS.txt" ] || die "immutable ledger manifest name does not match website"
  is_lower_sha256 "$LEDGER_ARCHIVE_MACOS_SHA256" || die "invalid immutable ledger macOS digest"
  is_lower_sha256 "$LEDGER_ARCHIVE_LINUX_SHA256" || die "invalid immutable ledger Linux digest"
  is_lower_sha256 "$LEDGER_MANIFEST_SHA256" || die "invalid immutable ledger manifest digest"
}

verify_standalone_release() {
  release_dir=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-website-release.XXXXXX") || die "failed to allocate release verification directory"
  get_current_object "$SITE_BUCKET" "$LEDGER_PREFIX/$WEBSITE_VERSION" "$release_dir/ledger"
  read_ledger "$release_dir/ledger"
  get_current_object "$SITE_BUCKET" "download/$LEDGER_ARCHIVE_MACOS_NAME" "$release_dir/$LEDGER_ARCHIVE_MACOS_NAME"
  get_current_object "$SITE_BUCKET" "download/$LEDGER_ARCHIVE_LINUX_NAME" "$release_dir/$LEDGER_ARCHIVE_LINUX_NAME"
  get_current_object "$SITE_BUCKET" "download/$LEDGER_MANIFEST_NAME" "$release_dir/$LEDGER_MANIFEST_NAME"
  [ "$(sha256_of "$release_dir/$LEDGER_ARCHIVE_MACOS_NAME")" = "$LEDGER_ARCHIVE_MACOS_SHA256" ] || die "public macOS archive differs from immutable ledger"
  [ "$(sha256_of "$release_dir/$LEDGER_ARCHIVE_LINUX_NAME")" = "$LEDGER_ARCHIVE_LINUX_SHA256" ] || die "public Linux archive differs from immutable ledger"
  [ "$(sha256_of "$release_dir/$LEDGER_MANIFEST_NAME")" = "$LEDGER_MANIFEST_SHA256" ] || die "public manifest differs from immutable ledger"
  awk \
    -v macos="$LEDGER_ARCHIVE_MACOS_NAME" \
    -v linux="$LEDGER_ARCHIVE_LINUX_NAME" \
    -v macos_digest="$LEDGER_ARCHIVE_MACOS_SHA256" \
    -v linux_digest="$LEDGER_ARCHIVE_LINUX_SHA256" '
      NF != 2 { bad = 1; next }
      $2 == macos && $1 == macos_digest { macos_count++; next }
      $2 == linux && $1 == linux_digest { linux_count++; next }
      { bad = 1 }
      END { if (bad || macos_count != 1 || linux_count != 1) exit 1 }
    ' "$release_dir/$LEDGER_MANIFEST_NAME" || die "public manifest is not the immutable exact release set"
}

new_nonce() {
  nonce=$(LC_ALL=C od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
  is_nonce "$nonce" || die "failed to generate website publication nonce"
  printf '%s\n' "$nonce"
}

make_standalone_lock() {
  lock_path=$1
  cat > "$lock_path" <<EOF
version=$WEBSITE_VERSION
nonce=$STANDALONE_NONCE
phase=WEBSITE_ONLY
EOF
}

verify_standalone_lock() {
  if s3_head "$SITE_BUCKET" "$LOCK_KEY"; then
    [ "$S3_ETAG" = "$STANDALONE_LOCK_ETAG" ] && [ "$S3_VERSION" = "$STANDALONE_LOCK_VERSION" ] || die "standalone website lock changed"
  else
    die "standalone website lock is absent"
  fi
  lock_body=$(mktemp "${TMPDIR:-/tmp}/trueflow-website-lock.XXXXXX") || die "failed to allocate website lock body"
  get_current_object "$SITE_BUCKET" "$LOCK_KEY" "$lock_body"
  awk -v version="$WEBSITE_VERSION" -v nonce="$STANDALONE_NONCE" '
    $0 == "version=" version { version_count++; next }
    $0 == "nonce=" nonce { nonce_count++; next }
    $0 == "phase=WEBSITE_ONLY" { phase_count++; next }
    { bad = 1 }
    END { if (bad || version_count != 1 || nonce_count != 1 || phase_count != 1) exit 1 }
  ' "$lock_body" || die "standalone website lock body changed"
}

release_standalone_lock() {
  verify_standalone_lock
  run_release_barrier BEFORE_UNLOCK
  aws s3api delete-object \
    --bucket "$SITE_BUCKET" \
    --key "$LOCK_KEY" \
    --if-match "$STANDALONE_LOCK_ETAG" >/dev/null
  if s3_head "$SITE_BUCKET" "$LOCK_KEY"; then
    die "standalone website lock remained current after release"
  fi
}

sync_website() {
  printf '==> syncing website/ to s3://%s\n' "$SITE_BUCKET"
  aws s3 sync "$REPO_ROOT/website/" "s3://${SITE_BUCKET}/" --delete \
    --exclude 'download/*' \
    --exclude '.trueflow-release/*'
  run_release_barrier WEBSITE_SYNCED
  printf '==> invalidating CloudFront distribution %s\n' "$DISTRIBUTION_ID"
  aws cloudfront create-invalidation \
    --distribution-id "$DISTRIBUTION_ID" \
    --paths '/*' >/dev/null
}

standalone_deploy() {
  resolve_infrastructure
  STANDALONE_NONCE=$(new_nonce)
  standalone_lock=$(mktemp "${TMPDIR:-/tmp}/trueflow-website-lock-new.XXXXXX") || die "failed to allocate website lock"
  make_standalone_lock "$standalone_lock"
  aws s3api put-object \
    --bucket "$SITE_BUCKET" \
    --key "$LOCK_KEY" \
    --body "$standalone_lock" \
    --if-none-match '*' \
    --content-type text/plain >/dev/null
  if s3_head "$SITE_BUCKET" "$LOCK_KEY"; then
    STANDALONE_LOCK_ETAG=$S3_ETAG
    STANDALONE_LOCK_VERSION=$S3_VERSION
  else
    die "standalone website lock did not become current"
  fi
  verify_standalone_lock
  if ! verify_standalone_release; then
    release_standalone_lock || printf 'error: standalone website lock left held after release verification failure\n' >&2
    exit 1
  fi
  if ! sync_website; then
    release_standalone_lock || printf 'error: standalone website lock left held after failed sync\n' >&2
    exit 1
  fi
  release_standalone_lock
}

receipt_deploy() {
  receipt_path=$1
  "$DOWNLOADS_SCRIPT" --verify-publication-phase "$receipt_path" PREPARED
  receipt_version=$(receipt_field "$receipt_path" version)
  SITE_BUCKET=$(receipt_field "$receipt_path" bucket)
  DISTRIBUTION_ID=$(receipt_field "$receipt_path" distribution)
  [ "$receipt_version" = "$WEBSITE_VERSION" ] || die "website version does not match publication receipt"
  sync_website
  "$DOWNLOADS_SCRIPT" --mark-website-switched "$receipt_path"
}

RECEIPT_PATH=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --publication-receipt)
      shift
      [ "$#" -gt 0 ] || die "--publication-receipt requires a value"
      [ -z "$RECEIPT_PATH" ] || die "--publication-receipt may be specified once"
      RECEIPT_PATH=$1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift
done

WEBSITE_VERSION=$(website_release_version)
printf '==> website declares release version %s\n' "$WEBSITE_VERSION"
if [ -n "$RECEIPT_PATH" ]; then
  receipt_deploy "$RECEIPT_PATH"
  printf '==> receipt-bound website deploy queued; publication lock remains held\n'
else
  standalone_deploy
  printf '==> standalone website deploy queued\n'
fi
