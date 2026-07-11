#!/bin/sh
set -eu

TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-infra/terraform}"
REQUIRED_MACOS_TARGET="aarch64-apple-darwin"
REQUIRED_LINUX_TARGET="x86_64-unknown-linux-musl"
LOCK_KEY=".trueflow-release/publication.lock"
LEDGER_PREFIX=".trueflow-release/ledger"

usage() {
  cat <<'EOF'
Usage:
  deploy-downloads.sh --validate-only ARTIFACT_DIR
  deploy-downloads.sh --prepare-snapshot ARTIFACT_DIR SNAPSHOT_DIR
  deploy-downloads.sh --begin-publication SNAPSHOT_DIR RECEIPT
  deploy-downloads.sh --verify-publication-phase RECEIPT PHASE
  deploy-downloads.sh --mark-website-switched RECEIPT
  deploy-downloads.sh --finalize-publication SNAPSHOT_DIR RECEIPT
  deploy-downloads.sh --abort-publication RECEIPT

Download publication is an explicit, receipt-bound state machine. Prepare freezes
and validates exactly the two supported archives and their checksum manifest.
Begin uploads a verified release while retaining old downloads and leaves the
publication lock in PREPARED. A receipt-bound website deploy must switch it to
WEBSITE_SWITCHED before finalize can remove obsolete downloads. Abort only
releases an owned PREPARED publication; it never removes downloads.

A bare artifact directory is intentionally unsupported: there is no destructive
one-command download deployment mode.
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
  case "$value" in
    ''|*[!0123456789abcdef]*) return 1 ;;
  esac
  return 0
}

is_nonce() {
  value=$1
  [ "${#value}" -eq 32 ] || return 1
  case "$value" in
    ''|*[!0123456789abcdef]*) return 1 ;;
  esac
  return 0
}

validate_version() {
  value=$1
  case "$value" in
    v[0-9]*) ;;
    *) die "invalid release version: $value" ;;
  esac
  case "$value" in
    *[!A-Za-z0-9._-]*) die "invalid release version: $value" ;;
  esac
}

validate_release_metadata() {
  validate_version "$VERSION"
  expected_macos="trueflow-${VERSION}-${REQUIRED_MACOS_TARGET}.tar.gz"
  expected_linux="trueflow-${VERSION}-${REQUIRED_LINUX_TARGET}.tar.gz"
  expected_manifest="trueflow-${VERSION}-SHA256SUMS.txt"

  [ "$ARCHIVE_MACOS_NAME" = "$expected_macos" ] || die "unexpected macOS archive name"
  [ "$ARCHIVE_LINUX_NAME" = "$expected_linux" ] || die "unexpected Linux archive name"
  [ "$MANIFEST_NAME" = "$expected_manifest" ] || die "unexpected manifest name"
  is_lower_sha256 "$ARCHIVE_MACOS_SHA256" || die "invalid macOS archive digest"
  is_lower_sha256 "$ARCHIVE_LINUX_SHA256" || die "invalid Linux archive digest"
  is_lower_sha256 "$MANIFEST_SHA256" || die "invalid manifest digest"
}

validate_manifest() {
  manifest_path=$1

  awk \
    -v macos="$ARCHIVE_MACOS_NAME" \
    -v linux="$ARCHIVE_LINUX_NAME" '
      NF != 2 { bad = 1; next }
      length($1) != 64 || $1 ~ /[^0123456789abcdef]/ { bad = 1; next }
      $2 == macos { macos_count++; macos_digest = $1; next }
      $2 == linux { linux_count++; linux_digest = $1; next }
      { bad = 1 }
      END {
        if (bad || macos_count != 1 || linux_count != 1) exit 1
      }
    ' "$manifest_path" || die "manifest must contain exactly one lower-hex SHA-256 row for each required archive"

  MANIFEST_MACOS_SHA256=$(awk -v macos="$ARCHIVE_MACOS_NAME" '$2 == macos { print $1 }' "$manifest_path")
  MANIFEST_LINUX_SHA256=$(awk -v linux="$ARCHIVE_LINUX_NAME" '$2 == linux { print $1 }' "$manifest_path")
}

select_release_set() {
  release_dir=$1
  [ -d "$release_dir" ] || die "artifact directory not found: $release_dir"

  set -- "$release_dir"/trueflow-*-SHA256SUMS.txt
  [ "$#" -eq 1 ] && [ -f "$1" ] || die "expected exactly one trueflow-vX.Y.Z-SHA256SUMS.txt file in $release_dir"

  MANIFEST_PATH=$1
  MANIFEST_NAME=$(basename "$MANIFEST_PATH")
  VERSION=${MANIFEST_NAME#trueflow-}
  VERSION=${VERSION%-SHA256SUMS.txt}
  [ -n "$VERSION" ] && [ "$VERSION" != "$MANIFEST_NAME" ] || die "invalid checksum filename: $MANIFEST_NAME"
  validate_version "$VERSION"

  ARCHIVE_MACOS_NAME="trueflow-${VERSION}-${REQUIRED_MACOS_TARGET}.tar.gz"
  ARCHIVE_LINUX_NAME="trueflow-${VERSION}-${REQUIRED_LINUX_TARGET}.tar.gz"
  ARCHIVE_MACOS_PATH="$release_dir/$ARCHIVE_MACOS_NAME"
  ARCHIVE_LINUX_PATH="$release_dir/$ARCHIVE_LINUX_NAME"
  [ -f "$ARCHIVE_MACOS_PATH" ] || die "missing required release artifact: $ARCHIVE_MACOS_NAME"
  [ -f "$ARCHIVE_LINUX_PATH" ] || die "missing required release artifact: $ARCHIVE_LINUX_NAME"

  # Digests are assigned after strict row validation so an invalid manifest cannot
  # influence which files are accepted.
  ARCHIVE_MACOS_SHA256=$(sha256_of "$ARCHIVE_MACOS_PATH")
  ARCHIVE_LINUX_SHA256=$(sha256_of "$ARCHIVE_LINUX_PATH")
  MANIFEST_SHA256=$(sha256_of "$MANIFEST_PATH")
  is_lower_sha256 "$ARCHIVE_MACOS_SHA256" || die "failed to calculate macOS archive SHA-256"
  is_lower_sha256 "$ARCHIVE_LINUX_SHA256" || die "failed to calculate Linux archive SHA-256"
  is_lower_sha256 "$MANIFEST_SHA256" || die "failed to calculate manifest SHA-256"

  validate_manifest "$MANIFEST_PATH"
  [ "$MANIFEST_MACOS_SHA256" = "$ARCHIVE_MACOS_SHA256" ] || die "macOS archive digest does not match manifest"
  [ "$MANIFEST_LINUX_SHA256" = "$ARCHIVE_LINUX_SHA256" ] || die "Linux archive digest does not match manifest"
  validate_release_metadata
}

validate_snapshot_exact() {
  snapshot_dir=$1
  select_release_set "$snapshot_dir"

  macos_seen=0
  linux_seen=0
  manifest_seen=0
  file_count=0
  for snapshot_entry in "$snapshot_dir"/* "$snapshot_dir"/.[!.]* "$snapshot_dir"/..?*; do
    [ -e "$snapshot_entry" ] || continue
    [ ! -L "$snapshot_entry" ] || die "snapshot must not contain symbolic links"
    [ -f "$snapshot_entry" ] || die "snapshot must contain only regular files"
    file_count=$((file_count + 1))
    snapshot_name=$(basename "$snapshot_entry")
    case "$snapshot_name" in
      "$ARCHIVE_MACOS_NAME") macos_seen=$((macos_seen + 1)) ;;
      "$ARCHIVE_LINUX_NAME") linux_seen=$((linux_seen + 1)) ;;
      "$MANIFEST_NAME") manifest_seen=$((manifest_seen + 1)) ;;
      *) die "snapshot contains an unexpected file: $snapshot_name" ;;
    esac
  done
  [ "$file_count" -eq 3 ] && [ "$macos_seen" -eq 1 ] && [ "$linux_seen" -eq 1 ] && [ "$manifest_seen" -eq 1 ] \
    || die "snapshot must contain exactly two required archives and one manifest"
}

prepare_snapshot() {
  artifact_dir=$1
  snapshot_dir=$2
  if [ -e "$snapshot_dir" ] || [ -L "$snapshot_dir" ]; then
    [ -d "$snapshot_dir" ] && [ ! -L "$snapshot_dir" ] || die "snapshot path is not a directory: $snapshot_dir"
    for snapshot_entry in "$snapshot_dir"/* "$snapshot_dir"/.[!.]* "$snapshot_dir"/..?*; do
      [ -e "$snapshot_entry" ] || continue
      die "snapshot directory must be empty: $snapshot_dir"
    done
  else
    mkdir -m 700 "$snapshot_dir" || die "failed to create private snapshot directory: $snapshot_dir"
  fi
  if snapshot_mode=$(stat -f '%Lp' "$snapshot_dir" 2>/dev/null); then
    :
  else
    snapshot_mode=$(stat -c '%a' "$snapshot_dir" 2>/dev/null) || die "failed to read snapshot directory mode"
  fi
  [ "$snapshot_mode" = 700 ] || die "snapshot directory must have mode 0700: $snapshot_dir"

  # Validate paths and bytes before making the private snapshot, then validate the
  # copied bytes again. No later phase opens ARTIFACT_DIR.
  select_release_set "$artifact_dir"
  cp "$ARCHIVE_MACOS_PATH" "$snapshot_dir/$ARCHIVE_MACOS_NAME"
  cp "$ARCHIVE_LINUX_PATH" "$snapshot_dir/$ARCHIVE_LINUX_NAME"
  cp "$MANIFEST_PATH" "$snapshot_dir/$MANIFEST_NAME"
  validate_snapshot_exact "$snapshot_dir"
  printf '==> prepared frozen release snapshot %s\n' "$snapshot_dir"
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
  S3_HEAD_ERROR=$(mktemp "${TMPDIR:-/tmp}/trueflow-s3-head.XXXXXX") || die "failed to allocate S3 error file"
  if S3_HEAD_OUTPUT=$(aws s3api head-object \
    --bucket "$head_bucket" \
    --key "$head_key" \
    --query '[ETag, VersionId]' \
    --output text 2>"$S3_HEAD_ERROR"); then
    set -- $S3_HEAD_OUTPUT
    [ "$#" -eq 2 ] || die "ambiguous S3 HEAD metadata for $head_key"
    S3_ETAG=$1
    S3_VERSION=$2
    case "$S3_ETAG:$S3_VERSION" in
      :*|*:|*:None|*:null) die "missing S3 ETag or VersionId for $head_key" ;;
    esac
    return 0
  fi

  S3_HEAD_FAILURE=$(cat "$S3_HEAD_ERROR")
  case "$S3_HEAD_FAILURE" in
    *404*|*NoSuchKey*|*NotFound*|*'Not Found'*|*'not found'*|*'object absent'*) return 1 ;;
    *) die "ambiguous S3 HEAD failure for $head_key: $S3_HEAD_FAILURE" ;;
  esac
}
get_current_object() {
  get_bucket=$1
  get_key=$2
  get_path=$3
  aws s3api get-object --bucket "$get_bucket" --key "$get_key" "$get_path" >/dev/null
}

make_ledger() {
  ledger_path=$1
  cat > "$ledger_path" <<EOF
version=$VERSION
archive_macos_name=$ARCHIVE_MACOS_NAME
archive_macos_sha256=$ARCHIVE_MACOS_SHA256
archive_linux_name=$ARCHIVE_LINUX_NAME
archive_linux_sha256=$ARCHIVE_LINUX_SHA256
manifest_name=$MANIFEST_NAME
manifest_sha256=$MANIFEST_SHA256
EOF
}

make_lock_body() {
  lock_path=$1
  lock_phase=$2
  cat > "$lock_path" <<EOF
version=$VERSION
nonce=$OWNER_NONCE
phase=$lock_phase
archive_macos_name=$ARCHIVE_MACOS_NAME
archive_macos_sha256=$ARCHIVE_MACOS_SHA256
archive_linux_name=$ARCHIVE_LINUX_NAME
archive_linux_sha256=$ARCHIVE_LINUX_SHA256
manifest_name=$MANIFEST_NAME
manifest_sha256=$MANIFEST_SHA256
EOF
}

new_nonce() {
  nonce=$(LC_ALL=C od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
  is_nonce "$nonce" || die "failed to generate publication nonce"
  printf '%s\n' "$nonce"
}

write_receipt() {
  receipt_path=$1
  receipt_dir=$(dirname "$receipt_path")
  [ -d "$receipt_dir" ] || die "receipt parent directory not found: $receipt_dir"
  receipt_tmp=$(umask 077; mktemp "$receipt_path.tmp.XXXXXX") || die "failed to allocate receipt temporary file"
  umask 077
  cat > "$receipt_tmp" <<EOF
bucket=$SITE_BUCKET
distribution=$DISTRIBUTION_ID
version=$VERSION
archive_macos_name=$ARCHIVE_MACOS_NAME
archive_macos_sha256=$ARCHIVE_MACOS_SHA256
archive_linux_name=$ARCHIVE_LINUX_NAME
archive_linux_sha256=$ARCHIVE_LINUX_SHA256
manifest_name=$MANIFEST_NAME
manifest_sha256=$MANIFEST_SHA256
nonce=$OWNER_NONCE
lock_key=$LOCK_KEY
lock_etag=$LOCK_ETAG
lock_version=$LOCK_VERSION
phase=$PUBLICATION_PHASE
EOF
  chmod 600 "$receipt_tmp"
  mv -f "$receipt_tmp" "$receipt_path"
}

load_receipt() {
  receipt_path=$1
  [ -f "$receipt_path" ] && [ ! -L "$receipt_path" ] || die "receipt is not a regular file: $receipt_path"

  REC_BUCKET= REC_DISTRIBUTION= REC_VERSION= REC_ARCHIVE_MACOS_NAME= REC_ARCHIVE_MACOS_SHA256=
  REC_ARCHIVE_LINUX_NAME= REC_ARCHIVE_LINUX_SHA256= REC_MANIFEST_NAME= REC_MANIFEST_SHA256=
  REC_NONCE= REC_LOCK_KEY= REC_LOCK_ETAG= REC_LOCK_VERSION= REC_PHASE=
  REC_BUCKET_SEEN=0 REC_DISTRIBUTION_SEEN=0 REC_VERSION_SEEN=0 REC_ARCHIVE_MACOS_NAME_SEEN=0 REC_ARCHIVE_MACOS_SHA256_SEEN=0
  REC_ARCHIVE_LINUX_NAME_SEEN=0 REC_ARCHIVE_LINUX_SHA256_SEEN=0 REC_MANIFEST_NAME_SEEN=0 REC_MANIFEST_SHA256_SEEN=0
  REC_NONCE_SEEN=0 REC_LOCK_KEY_SEEN=0 REC_LOCK_ETAG_SEEN=0 REC_LOCK_VERSION_SEEN=0 REC_PHASE_SEEN=0

  while IFS= read -r receipt_line || [ -n "$receipt_line" ]; do
    case "$receipt_line" in *=*) ;; *) die "malformed publication receipt" ;; esac
    receipt_key=${receipt_line%%=*}
    receipt_value=${receipt_line#*=}
    [ -n "$receipt_value" ] || die "empty publication receipt field: $receipt_key"
    case "$receipt_value" in *' '*|*'\t'*|*=*) die "unsafe publication receipt value: $receipt_key" ;; esac
    case "$receipt_key" in
      bucket) [ "$REC_BUCKET_SEEN" -eq 0 ] || die "duplicate receipt field: bucket"; REC_BUCKET=$receipt_value; REC_BUCKET_SEEN=1 ;;
      distribution) [ "$REC_DISTRIBUTION_SEEN" -eq 0 ] || die "duplicate receipt field: distribution"; REC_DISTRIBUTION=$receipt_value; REC_DISTRIBUTION_SEEN=1 ;;
      version) [ "$REC_VERSION_SEEN" -eq 0 ] || die "duplicate receipt field: version"; REC_VERSION=$receipt_value; REC_VERSION_SEEN=1 ;;
      archive_macos_name) [ "$REC_ARCHIVE_MACOS_NAME_SEEN" -eq 0 ] || die "duplicate receipt field: archive_macos_name"; REC_ARCHIVE_MACOS_NAME=$receipt_value; REC_ARCHIVE_MACOS_NAME_SEEN=1 ;;
      archive_macos_sha256) [ "$REC_ARCHIVE_MACOS_SHA256_SEEN" -eq 0 ] || die "duplicate receipt field: archive_macos_sha256"; REC_ARCHIVE_MACOS_SHA256=$receipt_value; REC_ARCHIVE_MACOS_SHA256_SEEN=1 ;;
      archive_linux_name) [ "$REC_ARCHIVE_LINUX_NAME_SEEN" -eq 0 ] || die "duplicate receipt field: archive_linux_name"; REC_ARCHIVE_LINUX_NAME=$receipt_value; REC_ARCHIVE_LINUX_NAME_SEEN=1 ;;
      archive_linux_sha256) [ "$REC_ARCHIVE_LINUX_SHA256_SEEN" -eq 0 ] || die "duplicate receipt field: archive_linux_sha256"; REC_ARCHIVE_LINUX_SHA256=$receipt_value; REC_ARCHIVE_LINUX_SHA256_SEEN=1 ;;
      manifest_name) [ "$REC_MANIFEST_NAME_SEEN" -eq 0 ] || die "duplicate receipt field: manifest_name"; REC_MANIFEST_NAME=$receipt_value; REC_MANIFEST_NAME_SEEN=1 ;;
      manifest_sha256) [ "$REC_MANIFEST_SHA256_SEEN" -eq 0 ] || die "duplicate receipt field: manifest_sha256"; REC_MANIFEST_SHA256=$receipt_value; REC_MANIFEST_SHA256_SEEN=1 ;;
      nonce) [ "$REC_NONCE_SEEN" -eq 0 ] || die "duplicate receipt field: nonce"; REC_NONCE=$receipt_value; REC_NONCE_SEEN=1 ;;
      lock_key) [ "$REC_LOCK_KEY_SEEN" -eq 0 ] || die "duplicate receipt field: lock_key"; REC_LOCK_KEY=$receipt_value; REC_LOCK_KEY_SEEN=1 ;;
      lock_etag) [ "$REC_LOCK_ETAG_SEEN" -eq 0 ] || die "duplicate receipt field: lock_etag"; REC_LOCK_ETAG=$receipt_value; REC_LOCK_ETAG_SEEN=1 ;;
      lock_version) [ "$REC_LOCK_VERSION_SEEN" -eq 0 ] || die "duplicate receipt field: lock_version"; REC_LOCK_VERSION=$receipt_value; REC_LOCK_VERSION_SEEN=1 ;;
      phase) [ "$REC_PHASE_SEEN" -eq 0 ] || die "duplicate receipt field: phase"; REC_PHASE=$receipt_value; REC_PHASE_SEEN=1 ;;
      *) die "unknown publication receipt field: $receipt_key" ;;
    esac
  done < "$receipt_path"

  [ "$REC_BUCKET_SEEN" -eq 1 ] && [ "$REC_DISTRIBUTION_SEEN" -eq 1 ] && [ "$REC_VERSION_SEEN" -eq 1 ] && \
    [ "$REC_ARCHIVE_MACOS_NAME_SEEN" -eq 1 ] && [ "$REC_ARCHIVE_MACOS_SHA256_SEEN" -eq 1 ] && \
    [ "$REC_ARCHIVE_LINUX_NAME_SEEN" -eq 1 ] && [ "$REC_ARCHIVE_LINUX_SHA256_SEEN" -eq 1 ] && \
    [ "$REC_MANIFEST_NAME_SEEN" -eq 1 ] && [ "$REC_MANIFEST_SHA256_SEEN" -eq 1 ] && \
    [ "$REC_NONCE_SEEN" -eq 1 ] && [ "$REC_LOCK_KEY_SEEN" -eq 1 ] && [ "$REC_LOCK_ETAG_SEEN" -eq 1 ] && \
    [ "$REC_LOCK_VERSION_SEEN" -eq 1 ] && [ "$REC_PHASE_SEEN" -eq 1 ] || die "incomplete publication receipt"

  OWNER_NONCE=$REC_NONCE
  VERSION=$REC_VERSION
  ARCHIVE_MACOS_NAME=$REC_ARCHIVE_MACOS_NAME
  ARCHIVE_MACOS_SHA256=$REC_ARCHIVE_MACOS_SHA256
  ARCHIVE_LINUX_NAME=$REC_ARCHIVE_LINUX_NAME
  ARCHIVE_LINUX_SHA256=$REC_ARCHIVE_LINUX_SHA256
  MANIFEST_NAME=$REC_MANIFEST_NAME
  MANIFEST_SHA256=$REC_MANIFEST_SHA256
  validate_release_metadata
  is_nonce "$OWNER_NONCE" || die "invalid receipt nonce"
  [ "$REC_LOCK_KEY" = "$LOCK_KEY" ] || die "unexpected receipt lock key"
  case "$REC_PHASE" in ACQUIRED|PREPARED|WEBSITE_SWITCHED) ;; *) die "invalid receipt phase" ;; esac
}

load_lock_body() {
  lock_path=$1
  LOCK_VERSION_FIELD= LOCK_NONCE= LOCK_PHASE= LOCK_ARCHIVE_MACOS_NAME= LOCK_ARCHIVE_MACOS_SHA256=
  LOCK_ARCHIVE_LINUX_NAME= LOCK_ARCHIVE_LINUX_SHA256= LOCK_MANIFEST_NAME= LOCK_MANIFEST_SHA256=
  LOCK_VERSION_FIELD_SEEN=0 LOCK_NONCE_SEEN=0 LOCK_PHASE_SEEN=0 LOCK_ARCHIVE_MACOS_NAME_SEEN=0 LOCK_ARCHIVE_MACOS_SHA256_SEEN=0
  LOCK_ARCHIVE_LINUX_NAME_SEEN=0 LOCK_ARCHIVE_LINUX_SHA256_SEEN=0 LOCK_MANIFEST_NAME_SEEN=0 LOCK_MANIFEST_SHA256_SEEN=0

  while IFS= read -r lock_line || [ -n "$lock_line" ]; do
    case "$lock_line" in *=*) ;; *) die "malformed publication lock" ;; esac
    lock_field=${lock_line%%=*}
    lock_value=${lock_line#*=}
    [ -n "$lock_value" ] || die "empty publication lock field: $lock_field"
    case "$lock_value" in *' '*|*'\t'*|*=*) die "unsafe publication lock value: $lock_field" ;; esac
    case "$lock_field" in
      version) [ "$LOCK_VERSION_FIELD_SEEN" -eq 0 ] || die "duplicate lock field: version"; LOCK_VERSION_FIELD=$lock_value; LOCK_VERSION_FIELD_SEEN=1 ;;
      nonce) [ "$LOCK_NONCE_SEEN" -eq 0 ] || die "duplicate lock field: nonce"; LOCK_NONCE=$lock_value; LOCK_NONCE_SEEN=1 ;;
      phase) [ "$LOCK_PHASE_SEEN" -eq 0 ] || die "duplicate lock field: phase"; LOCK_PHASE=$lock_value; LOCK_PHASE_SEEN=1 ;;
      archive_macos_name) [ "$LOCK_ARCHIVE_MACOS_NAME_SEEN" -eq 0 ] || die "duplicate lock field: archive_macos_name"; LOCK_ARCHIVE_MACOS_NAME=$lock_value; LOCK_ARCHIVE_MACOS_NAME_SEEN=1 ;;
      archive_macos_sha256) [ "$LOCK_ARCHIVE_MACOS_SHA256_SEEN" -eq 0 ] || die "duplicate lock field: archive_macos_sha256"; LOCK_ARCHIVE_MACOS_SHA256=$lock_value; LOCK_ARCHIVE_MACOS_SHA256_SEEN=1 ;;
      archive_linux_name) [ "$LOCK_ARCHIVE_LINUX_NAME_SEEN" -eq 0 ] || die "duplicate lock field: archive_linux_name"; LOCK_ARCHIVE_LINUX_NAME=$lock_value; LOCK_ARCHIVE_LINUX_NAME_SEEN=1 ;;
      archive_linux_sha256) [ "$LOCK_ARCHIVE_LINUX_SHA256_SEEN" -eq 0 ] || die "duplicate lock field: archive_linux_sha256"; LOCK_ARCHIVE_LINUX_SHA256=$lock_value; LOCK_ARCHIVE_LINUX_SHA256_SEEN=1 ;;
      manifest_name) [ "$LOCK_MANIFEST_NAME_SEEN" -eq 0 ] || die "duplicate lock field: manifest_name"; LOCK_MANIFEST_NAME=$lock_value; LOCK_MANIFEST_NAME_SEEN=1 ;;
      manifest_sha256) [ "$LOCK_MANIFEST_SHA256_SEEN" -eq 0 ] || die "duplicate lock field: manifest_sha256"; LOCK_MANIFEST_SHA256=$lock_value; LOCK_MANIFEST_SHA256_SEEN=1 ;;
      *) die "unknown publication lock field: $lock_field" ;;
    esac
  done < "$lock_path"

  [ "$LOCK_VERSION_FIELD_SEEN" -eq 1 ] && [ "$LOCK_NONCE_SEEN" -eq 1 ] && [ "$LOCK_PHASE_SEEN" -eq 1 ] && \
    [ "$LOCK_ARCHIVE_MACOS_NAME_SEEN" -eq 1 ] && [ "$LOCK_ARCHIVE_MACOS_SHA256_SEEN" -eq 1 ] && \
    [ "$LOCK_ARCHIVE_LINUX_NAME_SEEN" -eq 1 ] && [ "$LOCK_ARCHIVE_LINUX_SHA256_SEEN" -eq 1 ] && \
    [ "$LOCK_MANIFEST_NAME_SEEN" -eq 1 ] && [ "$LOCK_MANIFEST_SHA256_SEEN" -eq 1 ] || die "incomplete publication lock"

  saved_version=$VERSION
  saved_macos_name=$ARCHIVE_MACOS_NAME
  saved_macos_sha=$ARCHIVE_MACOS_SHA256
  saved_linux_name=$ARCHIVE_LINUX_NAME
  saved_linux_sha=$ARCHIVE_LINUX_SHA256
  saved_manifest_name=$MANIFEST_NAME
  saved_manifest_sha=$MANIFEST_SHA256
  VERSION=$LOCK_VERSION_FIELD
  ARCHIVE_MACOS_NAME=$LOCK_ARCHIVE_MACOS_NAME
  ARCHIVE_MACOS_SHA256=$LOCK_ARCHIVE_MACOS_SHA256
  ARCHIVE_LINUX_NAME=$LOCK_ARCHIVE_LINUX_NAME
  ARCHIVE_LINUX_SHA256=$LOCK_ARCHIVE_LINUX_SHA256
  MANIFEST_NAME=$LOCK_MANIFEST_NAME
  MANIFEST_SHA256=$LOCK_MANIFEST_SHA256
  validate_release_metadata
  VERSION=$saved_version
  ARCHIVE_MACOS_NAME=$saved_macos_name
  ARCHIVE_MACOS_SHA256=$saved_macos_sha
  ARCHIVE_LINUX_NAME=$saved_linux_name
  ARCHIVE_LINUX_SHA256=$saved_linux_sha
  MANIFEST_NAME=$saved_manifest_name
  MANIFEST_SHA256=$saved_manifest_sha
  is_nonce "$LOCK_NONCE" || die "invalid lock nonce"
  case "$LOCK_PHASE" in ACQUIRED|PREPARED|WEBSITE_SWITCHED) ;; *) die "invalid lock phase" ;; esac
}

load_current_lock() {
  lock_bucket=$1
  lock_key=$2
  if s3_head "$lock_bucket" "$lock_key"; then
    LOCK_ETAG=$S3_ETAG
    LOCK_VERSION=$S3_VERSION
  else
    die "publication lock is absent"
  fi
  lock_body_path=$(mktemp "${TMPDIR:-/tmp}/trueflow-publication-lock.XXXXXX") || die "failed to allocate lock file"
  get_current_object "$lock_bucket" "$lock_key" "$lock_body_path"
  if s3_head "$lock_bucket" "$lock_key"; then
    [ "$LOCK_ETAG" = "$S3_ETAG" ] && [ "$LOCK_VERSION" = "$S3_VERSION" ] || die "publication lock changed while being read"
  else
    die "publication lock disappeared while being read"
  fi
  load_lock_body "$lock_body_path"
}

verify_receipt_lock() {
  receipt_path=$1
  expected_phase=$2
  load_receipt "$receipt_path"
  [ "$REC_PHASE" = "$expected_phase" ] || die "receipt phase is $REC_PHASE, expected $expected_phase"
  load_current_lock "$REC_BUCKET" "$REC_LOCK_KEY"
  [ "$LOCK_ETAG" = "$REC_LOCK_ETAG" ] || die "receipt lock ETag is stale"
  [ "$LOCK_VERSION" = "$REC_LOCK_VERSION" ] || die "receipt lock VersionId is stale"
  [ "$LOCK_VERSION_FIELD" = "$REC_VERSION" ] || die "receipt lock version does not match"
  [ "$LOCK_NONCE" = "$REC_NONCE" ] || die "receipt lock nonce does not match"
  [ "$LOCK_PHASE" = "$REC_PHASE" ] || die "receipt lock phase does not match"
  [ "$LOCK_ARCHIVE_MACOS_NAME" = "$REC_ARCHIVE_MACOS_NAME" ] && [ "$LOCK_ARCHIVE_MACOS_SHA256" = "$REC_ARCHIVE_MACOS_SHA256" ] || die "receipt macOS archive does not match lock"
  [ "$LOCK_ARCHIVE_LINUX_NAME" = "$REC_ARCHIVE_LINUX_NAME" ] && [ "$LOCK_ARCHIVE_LINUX_SHA256" = "$REC_ARCHIVE_LINUX_SHA256" ] || die "receipt Linux archive does not match lock"
  [ "$LOCK_MANIFEST_NAME" = "$REC_MANIFEST_NAME" ] && [ "$LOCK_MANIFEST_SHA256" = "$REC_MANIFEST_SHA256" ] || die "receipt manifest does not match lock"
}

assert_snapshot_matches_receipt() {
  snapshot_dir=$1
  validate_snapshot_exact "$snapshot_dir"
  [ "$VERSION" = "$REC_VERSION" ] || die "snapshot version does not match receipt"
  [ "$ARCHIVE_MACOS_NAME" = "$REC_ARCHIVE_MACOS_NAME" ] && [ "$ARCHIVE_MACOS_SHA256" = "$REC_ARCHIVE_MACOS_SHA256" ] || die "snapshot macOS archive does not match receipt"
  [ "$ARCHIVE_LINUX_NAME" = "$REC_ARCHIVE_LINUX_NAME" ] && [ "$ARCHIVE_LINUX_SHA256" = "$REC_ARCHIVE_LINUX_SHA256" ] || die "snapshot Linux archive does not match receipt"
  [ "$MANIFEST_NAME" = "$REC_MANIFEST_NAME" ] && [ "$MANIFEST_SHA256" = "$REC_MANIFEST_SHA256" ] || die "snapshot manifest does not match receipt"
}

verify_or_create_ledger() {
  allow_create=${1:-1}
  ledger_key="$LEDGER_PREFIX/$VERSION"
  ledger_expected=$(mktemp "${TMPDIR:-/tmp}/trueflow-ledger-expected.XXXXXX") || die "failed to allocate ledger file"
  make_ledger "$ledger_expected"
  ledger_actual=$(mktemp "${TMPDIR:-/tmp}/trueflow-ledger-actual.XXXXXX") || die "failed to allocate ledger file"

  if s3_head "$SITE_BUCKET" "$ledger_key"; then
    get_current_object "$SITE_BUCKET" "$ledger_key" "$ledger_actual"
    cmp -s "$ledger_expected" "$ledger_actual" || die "immutable ledger bytes differ for $VERSION"
    return
  fi

  [ "$allow_create" = 1 ] || die "immutable ledger is absent for $VERSION"
  aws s3api put-object \
    --bucket "$SITE_BUCKET" \
    --key "$ledger_key" \
    --body "$ledger_expected" \
    --if-none-match '*' \
    --content-type text/plain >/dev/null
  if s3_head "$SITE_BUCKET" "$ledger_key"; then
    get_current_object "$SITE_BUCKET" "$ledger_key" "$ledger_actual"
    cmp -s "$ledger_expected" "$ledger_actual" || die "immutable ledger bytes differ after create for $VERSION"
  else
    die "immutable ledger did not become current"
  fi
}

verify_remote_object() {
  remote_key=$1
  expected_sha=$2
  remote_path=$(mktemp "${TMPDIR:-/tmp}/trueflow-remote-object.XXXXXX") || die "failed to allocate remote verification file"
  get_current_object "$SITE_BUCKET" "$remote_key" "$remote_path"
  remote_sha=$(sha256_of "$remote_path")
  [ "$remote_sha" = "$expected_sha" ] || die "remote object digest differs: $remote_key"
}

ensure_remote_object() {
  local_path=$1
  object_name=$2
  expected_sha=$3
  checksum_mode=$4
  remote_key="download/$object_name"

  if s3_head "$SITE_BUCKET" "$remote_key"; then
    verify_remote_object "$remote_key" "$expected_sha"
    return
  fi

  if [ "$checksum_mode" = manifest ]; then
    have_command base64 || die "base64 is required for manifest request checksum"
    object_checksum=$(base64 < "$local_path" | tr -d '\n')
    aws s3api put-object \
      --bucket "$SITE_BUCKET" \
      --key "$remote_key" \
      --body "$local_path" \
      --if-none-match '*' \
      --checksum-algorithm SHA256 \
      --checksum-sha256 "$object_checksum" >/dev/null
  else
    aws s3api put-object \
      --bucket "$SITE_BUCKET" \
      --key "$remote_key" \
      --body "$local_path" \
      --if-none-match '*' >/dev/null
  fi
  verify_remote_object "$remote_key" "$expected_sha"
}

transition_lock() {
  receipt_path=$1
  from_phase=$2
  to_phase=$3
  verify_receipt_lock "$receipt_path" "$from_phase"

  SITE_BUCKET=$REC_BUCKET
  DISTRIBUTION_ID=$REC_DISTRIBUTION
  OWNER_NONCE=$REC_NONCE
  VERSION=$REC_VERSION
  ARCHIVE_MACOS_NAME=$REC_ARCHIVE_MACOS_NAME
  ARCHIVE_MACOS_SHA256=$REC_ARCHIVE_MACOS_SHA256
  ARCHIVE_LINUX_NAME=$REC_ARCHIVE_LINUX_NAME
  ARCHIVE_LINUX_SHA256=$REC_ARCHIVE_LINUX_SHA256
  MANIFEST_NAME=$REC_MANIFEST_NAME
  MANIFEST_SHA256=$REC_MANIFEST_SHA256
  lock_next=$(mktemp "${TMPDIR:-/tmp}/trueflow-publication-next.XXXXXX") || die "failed to allocate lock file"
  make_lock_body "$lock_next" "$to_phase"
  aws s3api put-object \
    --bucket "$SITE_BUCKET" \
    --key "$REC_LOCK_KEY" \
    --body "$lock_next" \
    --if-match "$REC_LOCK_ETAG" \
    --content-type text/plain >/dev/null

  load_current_lock "$SITE_BUCKET" "$REC_LOCK_KEY"
  [ "$LOCK_VERSION_FIELD" = "$VERSION" ] && [ "$LOCK_NONCE" = "$OWNER_NONCE" ] && [ "$LOCK_PHASE" = "$to_phase" ] || die "publication lock transition did not commit expected state"
  PUBLICATION_PHASE=$to_phase
  write_receipt "$receipt_path"
}

release_current_lock() {
  receipt_path=$1
  expected_phase=$2
  verify_receipt_lock "$receipt_path" "$expected_phase"
  run_release_barrier BEFORE_UNLOCK
  aws s3api delete-object \
    --bucket "$REC_BUCKET" \
    --key "$REC_LOCK_KEY" \
    --if-match "$REC_LOCK_ETAG" >/dev/null
  if s3_head "$REC_BUCKET" "$REC_LOCK_KEY"; then
    die "publication lock is still current after release"
  fi
}

begin_publication() {
  snapshot_dir=$1
  receipt_path=$2
  [ ! -e "$receipt_path" ] && [ ! -L "$receipt_path" ] || die "refusing to overwrite existing receipt: $receipt_path"
  validate_snapshot_exact "$snapshot_dir"
  resolve_infrastructure

  OWNER_NONCE=$(new_nonce)
  lock_initial=$(mktemp "${TMPDIR:-/tmp}/trueflow-publication-initial.XXXXXX") || die "failed to allocate lock file"
  make_lock_body "$lock_initial" ACQUIRED
  aws s3api put-object \
    --bucket "$SITE_BUCKET" \
    --key "$LOCK_KEY" \
    --body "$lock_initial" \
    --if-none-match '*' \
    --content-type text/plain >/dev/null
  load_current_lock "$SITE_BUCKET" "$LOCK_KEY"
  [ "$LOCK_VERSION_FIELD" = "$VERSION" ] && [ "$LOCK_NONCE" = "$OWNER_NONCE" ] && [ "$LOCK_PHASE" = ACQUIRED ] || die "publication lock acquisition did not commit expected state"
  PUBLICATION_PHASE=ACQUIRED
  write_receipt "$receipt_path"

  verify_or_create_ledger
  ensure_remote_object "$ARCHIVE_MACOS_PATH" "$ARCHIVE_MACOS_NAME" "$ARCHIVE_MACOS_SHA256" archive
  ensure_remote_object "$ARCHIVE_LINUX_PATH" "$ARCHIVE_LINUX_NAME" "$ARCHIVE_LINUX_SHA256" archive
  ensure_remote_object "$MANIFEST_PATH" "$MANIFEST_NAME" "$MANIFEST_SHA256" manifest
  transition_lock "$receipt_path" ACQUIRED PREPARED
  run_release_barrier PREPARED
  printf '==> publication prepared; old downloads remain until receipt-bound website switch and finalize\n'
}

finalize_publication() {
  snapshot_dir=$1
  receipt_path=$2
  verify_receipt_lock "$receipt_path" WEBSITE_SWITCHED
  assert_snapshot_matches_receipt "$snapshot_dir"
  SITE_BUCKET=$REC_BUCKET
  DISTRIBUTION_ID=$REC_DISTRIBUTION
  verify_or_create_ledger 0
  verify_remote_object "download/$ARCHIVE_MACOS_NAME" "$ARCHIVE_MACOS_SHA256"
  verify_remote_object "download/$ARCHIVE_LINUX_NAME" "$ARCHIVE_LINUX_SHA256"
  verify_remote_object "download/$MANIFEST_NAME" "$MANIFEST_SHA256"

  run_release_barrier BEFORE_CLEANUP
  empty_dir=$(mktemp -d "${TMPDIR:-/tmp}/trueflow-download-cleanup.XXXXXX") || die "failed to allocate empty cleanup directory"
  printf '==> removing obsolete downloads after website switch\n'
  aws s3 sync "$empty_dir/" "s3://${SITE_BUCKET}/download/" --delete \
    --exclude "$ARCHIVE_MACOS_NAME" \
    --exclude "$ARCHIVE_LINUX_NAME" \
    --exclude "$MANIFEST_NAME"
  printf '==> invalidating CloudFront download paths for %s\n' "$DISTRIBUTION_ID"
  aws cloudfront create-invalidation \
    --distribution-id "$DISTRIBUTION_ID" \
    --paths '/download/*' '/install.sh' >/dev/null
  release_current_lock "$receipt_path" WEBSITE_SWITCHED
  printf '==> publication finalized\n'
}

abort_publication() {
  receipt_path=$1
  load_receipt "$receipt_path"
  case "$REC_PHASE" in ACQUIRED|PREPARED) ;; *) die "abort is only allowed for an owned ACQUIRED or PREPARED receipt" ;; esac
  release_current_lock "$receipt_path" "$REC_PHASE"
  printf '==> publication aborted; downloads were not removed\n'
}

[ "$#" -ge 1 ] || {
  usage >&2
  exit 1
}

case "$1" in
  --validate-only)
    [ "$#" -eq 2 ] || { usage >&2; exit 1; }
    select_release_set "$2"
    printf '==> artifact directory is valid for %s\n' "$VERSION"
    ;;
  --prepare-snapshot)
    [ "$#" -eq 3 ] || { usage >&2; exit 1; }
    prepare_snapshot "$2" "$3"
    ;;
  --begin-publication)
    [ "$#" -eq 3 ] || { usage >&2; exit 1; }
    begin_publication "$2" "$3"
    ;;
  --verify-publication-phase)
    [ "$#" -eq 3 ] || { usage >&2; exit 1; }
    case "$3" in ACQUIRED|PREPARED|WEBSITE_SWITCHED) ;; *) die "invalid publication phase: $3" ;; esac
    verify_receipt_lock "$2" "$3"
    printf '==> receipt owns current publication lock in %s\n' "$3"
    ;;
  --mark-website-switched)
    [ "$#" -eq 2 ] || { usage >&2; exit 1; }
    transition_lock "$2" PREPARED WEBSITE_SWITCHED
    run_release_barrier WEBSITE_SWITCHED
    printf '==> website switch recorded; finalize may now remove obsolete downloads\n'
    ;;
  --finalize-publication)
    [ "$#" -eq 3 ] || { usage >&2; exit 1; }
    finalize_publication "$2" "$3"
    ;;
  --abort-publication)
    [ "$#" -eq 2 ] || { usage >&2; exit 1; }
    abort_publication "$2"
    ;;
  -h|--help)
    [ "$#" -eq 1 ] || { usage >&2; exit 1; }
    usage
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
