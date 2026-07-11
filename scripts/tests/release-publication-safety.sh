#!/bin/sh
# Network-free release publication contracts. The release scripts are expected to
# use only the public AWS/OpenTofu CLIs exercised by the fail-closed shims below.
# This suite intentionally fails until the publication protocol in issue #26 is
# implemented; it never contacts AWS, CloudFront, or OpenTofu.
set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
BASE_PATH=$PATH

case "${TMPDIR:-/tmp}" in
  /*) TEMP_BASE=${TMPDIR:-/tmp} ;;
  *) TEMP_BASE=/tmp ;;
esac
WORK=$(mktemp -d "$TEMP_BASE/trueflow-release-publication-safety.XXXXXX")
KEEP_WORKSPACE=${TRUEFLOW_KEEP_RELEASE_TEST_WORKSPACE:-0}
CASE_SEQUENCE=0
TOTAL=0
FAILED=0

cleanup() {
  if [ "$KEEP_WORKSPACE" = 1 ]; then
    printf 'release safety workspace retained at %s\n' "$WORK" >&2
    return
  fi

  # `trash` makes accidental inspection/recovery possible. Do not remove paths
  # recursively: this suite must never turn a cleanup error into data loss.
  if command -v trash >/dev/null 2>&1; then
    trash "$WORK" >/dev/null 2>&1 || printf 'release safety workspace retained at %s\n' "$WORK" >&2
  else
    printf 'release safety workspace retained at %s (install trash or set TRUEFLOW_KEEP_RELEASE_TEST_WORKSPACE=1 to inspect)\n' "$WORK" >&2
  fi
}
trap cleanup EXIT HUP INT TERM

SHIM_DIR=$WORK/shims
mkdir -p "$SHIM_DIR" "$WORK/cases"

if command -v shasum >/dev/null 2>&1; then
  REAL_SHASUM=$(command -v shasum)
elif command -v sha256sum >/dev/null 2>&1; then
  REAL_SHASUM=$(command -v sha256sum)
else
  printf 'release safety test requires shasum or sha256sum\n' >&2
  exit 2
fi
REAL_TAR=$(command -v tar)
TAB=$(printf '\t')
export TF_REAL_SHASUM=$REAL_SHASUM TF_REAL_TAR=$REAL_TAR

cat >"$SHIM_DIR/shasum" <<'SHIM'
#!/bin/sh
set -u
if [ "${TF_SHIM_FAIL_SHASUM:-0}" = 1 ]; then
  printf 'injected shasum failure\n' >&2
  exit 91
fi
exec "$TF_REAL_SHASUM" "$@"
SHIM
chmod 0755 "$SHIM_DIR/shasum"

cat >"$SHIM_DIR/tar" <<'SHIM'
#!/bin/sh
set -u
if [ "${TF_SHIM_FAIL_TAR:-0}" = 1 ]; then
  # Reproduce the legacy direct-final-path hazard without relying on a timing or
  # resource-exhaustion failure. A safe packager passes a temporary path here.
  previous=
  target=
  for argument in "$@"; do
    if [ "$previous" = -czf ]; then
      target=$argument
      break
    fi
    case "$argument" in
      -czf) previous=-czf ;;
      -czf*) target=${argument#-czf}; break ;;
    esac
  done
  if [ -n "$target" ]; then
    : >"$target"
  fi
  printf 'injected tar failure\n' >&2
  exit 90
fi
exec "$TF_REAL_TAR" "$@"
SHIM
chmod 0755 "$SHIM_DIR/tar"

cat >"$SHIM_DIR/tofu" <<'SHIM'
#!/bin/sh
set -u
: "${TF_TOFU_LOG:?TF_TOFU_LOG is required}"
printf 'tofu' >>"$TF_TOFU_LOG"
for argument in "$@"; do
  printf ' %s' "$argument" >>"$TF_TOFU_LOG"
done
printf '\n' >>"$TF_TOFU_LOG"

while [ $# -gt 0 ]; do
  case "$1" in
    -chdir=*) shift ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || { printf 'fake tofu: missing command\n' >&2; exit 127; }
case "$1" in
  output)
    [ "${2:-}" = -raw ] || { printf 'fake tofu: only output -raw is supported\n' >&2; exit 127; }
    case "${3:-}" in
      site_bucket_name) printf '%s\n' "${TF_FAKE_BUCKET:-trueflow-test-bucket}" ;;
      site_distribution_id) printf '%s\n' "${TF_FAKE_DISTRIBUTION:-TESTDIST}" ;;
      *) printf 'fake tofu: unknown output %s\n' "${3:-}" >&2; exit 127 ;;
    esac
    ;;
  init|fmt|validate|apply) ;;
  *) printf 'fake tofu: unsupported command %s\n' "$1" >&2; exit 127 ;;
esac
SHIM
chmod 0755 "$SHIM_DIR/tofu"

cat >"$SHIM_DIR/aws" <<'SHIM'
#!/bin/sh
# A deliberately small versioned-S3 model. It rejects every invocation outside
# the contract below so an accidental real-AWS spelling cannot silently pass.
set -u
: "${TF_FAKE_S3_ROOT:?TF_FAKE_S3_ROOT is required}"
: "${TF_AWS_LOG:?TF_AWS_LOG is required}"
ROOT=$TF_FAKE_S3_ROOT
mkdir -p "$ROOT/current" "$ROOT/current-meta" "$ROOT/versions" "$ROOT/removed" "$ROOT/tmp"
TAB=$(printf '\t')
[ -f "$ROOT/sequence" ] || printf '0\n' >"$ROOT/sequence"

record() {
  printf '%s\n' "$*" >>"$TF_AWS_LOG"
}

fail_if_requested() {
  point=$1
  case ",${TF_AWS_FAIL:-}," in
    *",$point,"*) printf 'injected AWS failure at %s\n' "$point" >&2; exit 88 ;;
  esac
}

next_version() {
  sequence=$(cat "$ROOT/sequence")
  sequence=$((sequence + 1))
  printf '%s\n' "$sequence" >"$ROOT/sequence"
  printf 'v%06d\n' "$sequence"
}

sha256() {
  if [ -n "${TF_REAL_SHASUM:-}" ]; then
    "$TF_REAL_SHASUM" -a 256 "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

object_file() {
  printf '%s/current/%s/%s\n' "$ROOT" "$1" "$2"
}

meta_file() {
  printf '%s/current-meta/%s/%s\n' "$ROOT" "$1" "$2"
}

version_dir() {
  printf '%s/versions/%s/%s\n' "$ROOT" "$1" "$2"
}

current_state() {
  meta=$(meta_file "$1" "$2")
  [ -f "$meta" ] || return 1
  IFS=$TAB read -r CURRENT_KIND CURRENT_VERSION CURRENT_ETAG <"$meta"
  [ "$CURRENT_KIND" = OBJECT ] || return 1
  return 0
}

write_object_version() {
  bucket=$1
  key=$2
  body=$3
  version=$(next_version)
  etag=$(sha256 "$body")
  directory=$(version_dir "$bucket" "$key")
  mkdir -p "$directory" "$(dirname "$(object_file "$bucket" "$key")")" "$(dirname "$(meta_file "$bucket" "$key")")"
  cp "$body" "$directory/$version.body"
  printf 'OBJECT\t%s\t%s\n' "$version" "$etag" >"$directory/$version.meta"
  cp "$body" "$(object_file "$bucket" "$key")"
  printf 'OBJECT\t%s\t%s\n' "$version" "$etag" >"$(meta_file "$bucket" "$key")"
  NEW_VERSION=$version
  NEW_ETAG=$etag
}

write_delete_marker() {
  bucket=$1
  key=$2
  version=$(next_version)
  directory=$(version_dir "$bucket" "$key")
  mkdir -p "$directory" "$(dirname "$(meta_file "$bucket" "$key")")"
  : >"$directory/$version.delete"
  printf 'DELETE\t%s\tdelete-%s\n' "$version" "$version" >"$directory/$version.meta"
  object=$(object_file "$bucket" "$key")
  if [ -f "$object" ]; then
    mv "$object" "$ROOT/removed/current-$version.body"
  fi
  printf 'DELETE\t%s\tdelete-%s\n' "$version" "$version" >"$(meta_file "$bucket" "$key")"
  NEW_VERSION=$version
  NEW_ETAG=delete-$version
}

restore_latest_version() {
  bucket=$1
  key=$2
  directory=$(version_dir "$bucket" "$key")
  latest=
  for candidate in "$directory"/*.meta; do
    [ -f "$candidate" ] || continue
    case "$candidate" in *.removed.meta) continue ;; esac
    latest=$candidate
  done
  meta=$(meta_file "$bucket" "$key")
  object=$(object_file "$bucket" "$key")
  if [ -z "$latest" ]; then
    if [ -f "$object" ]; then
      mv "$object" "$ROOT/removed/no-current.body"
    fi
    if [ -f "$meta" ]; then
      mv "$meta" "$ROOT/removed/no-current.meta"
    fi
    return
  fi
  IFS=$TAB read -r kind version etag <"$latest"
  mkdir -p "$(dirname "$meta")" "$(dirname "$object")"
  if [ "$kind" = DELETE ]; then
    if [ -f "$object" ]; then
      mv "$object" "$ROOT/removed/resurfaced-$version.body"
    fi
    printf 'DELETE\t%s\t%s\n' "$version" "$etag" >"$meta"
  else
    cp "${latest%.meta}.body" "$object"
    printf 'OBJECT\t%s\t%s\n' "$version" "$etag" >"$meta"
  fi
}

emit_object_result() {
  query=$1
  output=$2
  version=$3
  etag=$4
  if [ "$output" = text ]; then
    case "$query" in
      VersionId) printf '%s\n' "$version" ;;
      ETag) printf '%s\n' "$etag" ;;
      *) printf '%s\t%s\n' "$etag" "$version" ;;
    esac
  else
    printf '{"ETag":"%s","VersionId":"%s"}\n' "$etag" "$version"
  fi
}

parse_s3_uri() {
  uri=${1#s3://}
  case "$uri" in
    */*) URI_BUCKET=${uri%%/*}; URI_KEY=${uri#*/} ;;
    *) URI_BUCKET=$uri; URI_KEY= ;;
  esac
}

is_excluded() {
  full_key=$1
  relative_key=$2
  shift 2
  for excluded in "$@"; do
    case "$full_key" in $excluded) return 0 ;; esac
    case "$relative_key" in $excluded) return 0 ;; esac
  done
  return 1
}

put_object() {
  bucket=$1
  key=$2
  body=$3
  condition_none=$4
  condition_etag=$5
  current_state "$bucket" "$key" && exists=1 || exists=0
  if [ "$condition_none" = 1 ] && [ "$exists" = 1 ]; then
    printf 'fake s3: conditional create failed for %s\n' "$key" >&2
    return 45
  fi
  if [ -n "$condition_etag" ]; then
    [ "$exists" = 1 ] && [ "$CURRENT_ETAG" = "$condition_etag" ] || {
      printf 'fake s3: conditional update failed for %s\n' "$key" >&2
      return 46
    }
  fi
  write_object_version "$bucket" "$key" "$body"
  record "put bucket=$bucket key=$key version=$NEW_VERSION etag=$NEW_ETAG"
}

delete_current() {
  bucket=$1
  key=$2
  condition_etag=$3
  current_state "$bucket" "$key" && exists=1 || exists=0
  if [ -n "$condition_etag" ]; then
    [ "$exists" = 1 ] && [ "$CURRENT_ETAG" = "$condition_etag" ] || {
      printf 'fake s3: conditional delete failed for %s\n' "$key" >&2
      return 47
    }
  fi
  write_delete_marker "$bucket" "$key"
  record "delete-current bucket=$bucket key=$key version=$NEW_VERSION"
}

s3_sync() {
  source=$1
  destination=$2
  shift 2
  delete=0
  excludes=
  while [ $# -gt 0 ]; do
    case "$1" in
      --delete) delete=1 ;;
      --exclude) shift; [ $# -gt 0 ] || exit 127; excludes="$excludes ${1}" ;;
      --no-progress|--only-show-errors|--exact-timestamps) ;;
      *) printf 'fake aws: unsupported s3 sync option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  case "$destination" in s3://*) ;; *) printf 'fake aws: only local-to-s3 sync is supported\n' >&2; exit 127 ;; esac
  parse_s3_uri "$destination"
  bucket=$URI_BUCKET
  prefix=$URI_KEY
  case "$prefix" in '') ;; */) ;; *) prefix=$prefix/ ;; esac
  [ -d "$source" ] || { printf 'fake aws: sync source missing: %s\n' "$source" >&2; exit 127; }
  fail_if_requested s3-sync-before
  seen="$ROOT/tmp/seen-$(next_version)"
  : >"$seen"
  copy_count=0
  (
    cd "$source" || exit 127
    find . -type f -print
  ) | while IFS= read -r relative; do
    relative=${relative#./}
    key=$prefix$relative
    if is_excluded "$key" "$relative" $excludes; then
      record "sync-excluded bucket=$bucket key=$key"
      continue
    fi
    put_object "$bucket" "$key" "$source/$relative" 0 "" || exit $?
    printf '%s\n' "$key" >>"$seen"
    copy_count=$((copy_count + 1))
    if [ "${TF_AWS_FAIL_SYNC_AFTER:-0}" -eq "$copy_count" ]; then
      printf 'injected AWS sync copy failure\n' >&2
      exit 88
    fi
  done
  sync_status=$?
  [ "$sync_status" -eq 0 ] || exit "$sync_status"
  if [ "$delete" = 1 ]; then
    current_root="$ROOT/current/$bucket"
    if [ -d "$current_root" ]; then
      find "$current_root" -type f -print | while IFS= read -r current; do
        key=${current#"$current_root/"}
        case "$key" in "$prefix"*) ;; *) continue ;; esac
        relative_key=${key#"$prefix"}
        if is_excluded "$key" "$relative_key" $excludes; then
          record "sync-delete-excluded bucket=$bucket key=$key"
          continue
        fi
        if grep -F -x "$key" "$seen" >/dev/null 2>&1; then
          continue
        fi
        delete_current "$bucket" "$key" "" || exit $?
      done
    fi
  fi
  record "sync-complete bucket=$bucket prefix=$prefix delete=$delete"
  fail_if_requested s3-sync-after
}

s3_cp() {
  source=$1
  destination=$2
  shift 2
  while [ $# -gt 0 ]; do
    case "$1" in
      --checksum-algorithm|--checksum-mode|--expected-bucket-owner) shift; [ $# -gt 0 ] || exit 127 ;;
      --only-show-errors|--no-progress) ;;
      *) printf 'fake aws: unsupported s3 cp option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  case "$destination" in
    s3://*)
      parse_s3_uri "$destination"
      put_object "$URI_BUCKET" "$URI_KEY" "$source" 0 "" || exit $?
      ;;
    *)
      case "$source" in s3://*) ;; *) printf 'fake aws: unsupported cp direction\n' >&2; exit 127 ;; esac
      parse_s3_uri "$source"
      current_state "$URI_BUCKET" "$URI_KEY" || exit 44
      cp "$(object_file "$URI_BUCKET" "$URI_KEY")" "$destination"
      record "get bucket=$URI_BUCKET key=$URI_KEY"
      ;;
  esac
}

s3api_put() {
  bucket=
  key=
  body=
  none=0
  etag=
  checksum_algorithm=
  checksum_sha256=
  query=
  output=json
  while [ $# -gt 0 ]; do
    case "$1" in
      --bucket) shift; bucket=${1:-} ;;
      --key) shift; key=${1:-} ;;
      --body) shift; body=${1:-} ;;
      --if-none-match) shift; [ "${1:-}" = '*' ] || { printf 'fake aws: unsupported if-none-match\n' >&2; exit 127; }; none=1 ;;
      --if-match) shift; etag=${1:-} ;;
      --checksum-algorithm) shift; checksum_algorithm=${1:-} ;;
      --checksum-sha256) shift; checksum_sha256=${1:-} ;;
      --expected-bucket-owner|--content-type) shift; [ $# -gt 0 ] || exit 127 ;;
      --query) shift; query=${1:-} ;;
      --output) shift; output=${1:-} ;;
      --no-cli-pager) ;;
      *) printf 'fake aws: unsupported put-object option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  [ -n "$bucket" ] && [ -n "$key" ] && [ -n "$body" ] || { printf 'fake aws: put-object needs bucket/key/body\n' >&2; exit 127; }
  if [ -n "$checksum_sha256" ]; then
    [ "$checksum_algorithm" = SHA256 ] || { printf 'fake aws: checksum-sha256 requires SHA256 algorithm\n' >&2; exit 127; }
    decoded_length=$(printf '%s' "$checksum_sha256" | base64 -d 2>/dev/null | wc -c | tr -d ' ')
    if [ "$decoded_length" != 32 ]; then
      decoded_length=$(printf '%s' "$checksum_sha256" | base64 -D 2>/dev/null | wc -c | tr -d ' ')
    fi
    [ "$decoded_length" = 32 ] || { printf 'fake aws: invalid checksum-sha256 payload\n' >&2; exit 127; }
  fi
  fail_if_requested s3api-put
  put_object "$bucket" "$key" "$body" "$none" "$etag" || exit $?
  emit_object_result "$query" "$output" "$NEW_VERSION" "$NEW_ETAG"
}

s3api_head() {
  bucket=
  key=
  query=
  output=json
  while [ $# -gt 0 ]; do
    case "$1" in
      --bucket) shift; bucket=${1:-} ;;
      --key) shift; key=${1:-} ;;
      --if-match) shift; requested_etag=${1:-} ;;
      --query) shift; query=${1:-} ;;
      --output) shift; output=${1:-} ;;
      --expected-bucket-owner|--checksum-mode) shift; [ $# -gt 0 ] || exit 127 ;;
      --no-cli-pager) ;;
      *) printf 'fake aws: unsupported head-object option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  current_state "$bucket" "$key" || { printf 'fake s3: object absent: %s\n' "$key" >&2; exit 44; }
  if [ -n "${requested_etag:-}" ] && [ "$requested_etag" != "$CURRENT_ETAG" ]; then
    printf 'fake s3: head etag mismatch\n' >&2
    exit 46
  fi
  record "head bucket=$bucket key=$key version=$CURRENT_VERSION etag=$CURRENT_ETAG"
  emit_object_result "$query" "$output" "$CURRENT_VERSION" "$CURRENT_ETAG"
}

s3api_get() {
  bucket=
  key=
  destination=
  while [ $# -gt 0 ]; do
    case "$1" in
      --bucket) shift; bucket=${1:-} ;;
      --key) shift; key=${1:-} ;;
      --checksum-mode|--expected-bucket-owner) shift; [ $# -gt 0 ] || exit 127 ;;
      --no-cli-pager) ;;
      --*) printf 'fake aws: unsupported get-object option %s\n' "$1" >&2; exit 127 ;;
      *) [ -z "$destination" ] || { printf 'fake aws: too many get-object paths\n' >&2; exit 127; }; destination=$1 ;;
    esac
    shift
  done
  current_state "$bucket" "$key" || exit 44
  cp "$(object_file "$bucket" "$key")" "$destination"
  record "get bucket=$bucket key=$key version=$CURRENT_VERSION"
  printf '{"ETag":"%s","VersionId":"%s"}\n' "$CURRENT_ETAG" "$CURRENT_VERSION"
}

s3api_delete() {
  bucket=
  key=
  etag=
  version_id=
  query=
  output=json
  while [ $# -gt 0 ]; do
    case "$1" in
      --bucket) shift; bucket=${1:-} ;;
      --key) shift; key=${1:-} ;;
      --if-match) shift; etag=${1:-} ;;
      --version-id) shift; version_id=${1:-} ;;
      --expected-bucket-owner) shift; [ $# -gt 0 ] || exit 127 ;;
      --query) shift; query=${1:-} ;;
      --output) shift; output=${1:-} ;;
      --no-cli-pager) ;;
      *) printf 'fake aws: unsupported delete-object option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  [ -n "$bucket" ] && [ -n "$key" ] || { printf 'fake aws: delete-object needs bucket/key\n' >&2; exit 127; }
  if [ -n "$version_id" ]; then
    directory=$(version_dir "$bucket" "$key")
    [ -f "$directory/$version_id.meta" ] || { printf 'fake s3: version missing\n' >&2; exit 44; }
    mv "$directory/$version_id.meta" "$directory/$version_id.removed.meta"
    if [ -f "$directory/$version_id.body" ]; then
      mv "$directory/$version_id.body" "$directory/$version_id.removed.body"
    fi
    if [ -f "$directory/$version_id.delete" ]; then
      mv "$directory/$version_id.delete" "$directory/$version_id.removed.delete"
    fi
    restore_latest_version "$bucket" "$key"
    record "delete-version bucket=$bucket key=$key version=$version_id"
    printf '{"VersionId":"%s"}\n' "$version_id"
    return
  fi
  delete_current "$bucket" "$key" "$etag" || exit $?
  emit_object_result "$query" "$output" "$NEW_VERSION" "$NEW_ETAG"
}

s3api_list_versions() {
  bucket=
  prefix=
  while [ $# -gt 0 ]; do
    case "$1" in
      --bucket) shift; bucket=${1:-} ;;
      --prefix) shift; prefix=${1:-} ;;
      --output|--query) shift; [ $# -gt 0 ] || exit 127 ;;
      --no-cli-pager) ;;
      *) printf 'fake aws: unsupported list-object-versions option %s\n' "$1" >&2; exit 127 ;;
    esac
    shift
  done
  record "list-versions bucket=$bucket prefix=$prefix"
  printf '{"Versions":[],"DeleteMarkers":[]}\n'
}

[ $# -gt 0 ] || { printf 'fake aws: missing service\n' >&2; exit 127; }
service=$1
shift
case "$service" in
  s3)
    [ "${1:-}" = sync ] && { shift; s3_sync "$@"; exit $?; }
    [ "${1:-}" = cp ] && { shift; s3_cp "$@"; exit $?; }
    printf 'fake aws: unsupported s3 command %s\n' "${1:-}" >&2
    exit 127
    ;;
  s3api)
    command=${1:-}
    shift || true
    case "$command" in
      put-object) s3api_put "$@" ;;
      head-object) s3api_head "$@" ;;
      get-object) s3api_get "$@" ;;
      delete-object) s3api_delete "$@" ;;
      list-object-versions) s3api_list_versions "$@" ;;
      *) printf 'fake aws: unsupported s3api command %s\n' "$command" >&2; exit 127 ;;
    esac
    ;;
  cloudfront)
    [ "${1:-}" = create-invalidation ] || { printf 'fake aws: unsupported cloudfront command\n' >&2; exit 127; }
    shift
    distribution=
    paths=
    while [ $# -gt 0 ]; do
      case "$1" in
        --distribution-id) shift; distribution=${1:-} ;;
        --paths)
          shift
          paths_seen=0
          while [ $# -gt 0 ]; do
            case "$1" in
              --*) break ;;
              *) paths="$paths $1"; paths_seen=1; shift ;;
            esac
          done
          [ "$paths_seen" -eq 1 ] || { printf 'fake aws: --paths needs one or more values\n' >&2; exit 127; }
          continue
          ;;
        --caller-reference) shift; [ $# -gt 0 ] || exit 127 ;;
        --no-cli-pager) ;;
        *) printf 'fake aws: unsupported invalidation option %s\n' "$1" >&2; exit 127 ;;
      esac
      shift
    done
    fail_if_requested cloudfront-invalidation
    record "invalidation distribution=$distribution paths=$paths"
    printf '{"Invalidation":{"Id":"test-invalidation"}}\n'
    ;;
  *) printf 'fake aws: unsupported service %s\n' "$service" >&2; exit 127 ;;
esac
SHIM
chmod 0755 "$SHIM_DIR/aws"

cat >"$SHIM_DIR/release-barrier" <<'SHIM'
#!/bin/sh
set -u
: "${TF_BARRIER_LOG:?TF_BARRIER_LOG is required}"
[ $# -eq 1 ] || { printf 'fake release barrier: expected one phase\n' >&2; exit 127; }
case "$1" in
  SNAPSHOT_VALIDATED|SMOKE_TESTED|INFRA_APPLIED|LOCK_ACQUIRED|LEDGER_RESERVED|ARCHIVES_COMMITTED|MANIFEST_COMMITTED|PREPARED|WEBSITE_SYNCED|WEBSITE_SWITCHED|BEFORE_CLEANUP|BEFORE_UNLOCK|ABORT) ;;
  *) printf 'fake release barrier: unknown phase %s\n' "$1" >&2; exit 127 ;;
esac
printf '%s\n' "$1" >>"$TF_BARRIER_LOG"
case ",${TF_BARRIER_FAIL:-}," in
  *",$1,"*) printf 'injected release barrier failure at %s\n' "$1" >&2; exit 89 ;;
esac
SHIM
chmod 0755 "$SHIM_DIR/release-barrier"


new_case() {
  CASE_SEQUENCE=$((CASE_SEQUENCE + 1))
  CASE_DIR=$WORK/cases/$CASE_SEQUENCE
  TEST_BARRIER=
  FAKE_S3=$CASE_DIR/fake-s3
  AWS_LOG=$CASE_DIR/aws.log
  TOFU_LOG=$CASE_DIR/tofu.log
  BARRIER_LOG=$CASE_DIR/barrier.log
  mkdir -p "$CASE_DIR" "$FAKE_S3"
  : >"$AWS_LOG"
  : >"$TOFU_LOG"
  : >"$BARRIER_LOG"
}

with_release_environment() {
  PATH=$SHIM_DIR:$BASE_PATH \
  TF_FAKE_S3_ROOT=$FAKE_S3 \
  TF_AWS_LOG=$AWS_LOG \
  TF_TOFU_LOG=$TOFU_LOG \
  TF_BARRIER_LOG=$BARRIER_LOG \
  TF_FAKE_BUCKET=trueflow-test-bucket \
  TF_FAKE_DISTRIBUTION=TESTDIST \
  TRUEFLOW_INFRA_CLI=$SHIM_DIR/tofu \
  TRUEFLOW_SITE_BUCKET=trueflow-test-bucket \
  TRUEFLOW_DISTRIBUTION_ID=TESTDIST \
  TRUEFLOW_RELEASE_BARRIER=${TEST_BARRIER:-} \
  "$@"
}

sha256_file() {
  "$REAL_SHASUM" -a 256 "$1" | awk '{print $1}'
}

archive_name() {
  printf 'trueflow-%s-%s.tar.gz\n' "$1" "$2"
}

manifest_name() {
  printf 'trueflow-%s-SHA256SUMS.txt\n' "$1"
}

make_release() (
  directory=$1
  version=$2
  arm_body=$3
  linux_body=$4
  mkdir -p "$directory"
  arm=$(archive_name "$version" aarch64-apple-darwin)
  linux=$(archive_name "$version" x86_64-unknown-linux-musl)
  printf '%s\n' "$arm_body" >"$directory/$arm"
  printf '%s\n' "$linux_body" >"$directory/$linux"
  {
    printf '%s  %s\n' "$(sha256_file "$directory/$arm")" "$arm"
    printf '%s  %s\n' "$(sha256_file "$directory/$linux")" "$linux"
  } >"$directory/$(manifest_name "$version")"
)

seed_object() {
  bucket=$1
  key=$2
  body=$3
  file=$CASE_DIR/seed-$(basename "$key")-$CASE_SEQUENCE
  printf '%s\n' "$body" >"$file"
  with_release_environment "$SHIM_DIR/aws" s3api put-object --bucket "$bucket" --key "$key" --body "$file" >/dev/null
}

current_path() {
  printf '%s/current/%s/%s\n' "$FAKE_S3" "$1" "$2"
}

current_meta_path() {
  printf '%s/current-meta/%s/%s\n' "$FAKE_S3" "$1" "$2"
}

assert_file_equals() {
  actual=$1
  expected=$2
  description=$3
  if ! cmp -s "$actual" "$expected"; then
    printf 'assertion failed: %s\n' "$description" >&2
    return 1
  fi
}

assert_text_equals() {
  actual=$1
  expected=$2
  description=$3
  if [ "$actual" != "$expected" ]; then
    printf 'assertion failed: %s (expected %s, got %s)\n' "$description" "$expected" "$actual" >&2
    return 1
  fi
}

assert_present() {
  path=$1
  description=$2
  if [ ! -f "$path" ]; then
    printf 'assertion failed: expected %s\n' "$description" >&2
    return 1
  fi
}

assert_absent() {
  path=$1
  description=$2
  if [ -f "$path" ]; then
    printf 'assertion failed: unexpected %s\n' "$description" >&2
    return 1
  fi
}

assert_empty_file() {
  path=$1
  description=$2
  if [ -s "$path" ]; then
    printf 'assertion failed: expected no %s\n' "$description" >&2
    return 1
  fi
}

assert_contains() {
  needle=$1
  path=$2
  description=$3
  if ! grep -F -- "$needle" "$path" >/dev/null 2>&1; then
    printf 'assertion failed: expected %s\n' "$description" >&2
    return 1
  fi
}

assert_not_contains() {
  needle=$1
  path=$2
  description=$3
  if grep -F -- "$needle" "$path" >/dev/null 2>&1; then
    printf 'assertion failed: unexpected %s\n' "$description" >&2
    return 1
  fi
}

assert_current_object() {
  bucket=$1
  key=$2
  description=$3
  assert_present "$(current_path "$bucket" "$key")" "$description"
}

assert_no_current_object() {
  bucket=$1
  key=$2
  description=$3
  assert_absent "$(current_path "$bucket" "$key")" "$description"
}

assert_delete_marker() {
  bucket=$1
  key=$2
  description=$3
  meta=$(current_meta_path "$bucket" "$key")
  assert_present "$meta" "$description metadata" || return 1
  IFS=$TAB read -r kind version etag <"$meta"
  if [ "$kind" != DELETE ]; then
    printf 'assertion failed: expected delete marker for %s\n' "$description" >&2
    return 1
  fi
}

assert_strict_manifest() {
  directory=$1
  version=$2
  manifest=$directory/$(manifest_name "$version")
  arm=$(archive_name "$version" aarch64-apple-darwin)
  linux=$(archive_name "$version" x86_64-unknown-linux-musl)
  assert_present "$manifest" "manifest $manifest" || return 1
  if ! awk -v arm="$arm" -v linux="$linux" '
    NF != 2 { exit 1 }
    length($1) != 64 || $1 ~ /[^0-9a-f]/ { exit 1 }
    $2 != arm && $2 != linux { exit 1 }
    seen[$2]++ != 0 { exit 1 }
    { rows++ }
    END { exit rows == 2 && seen[arm] == 1 && seen[linux] == 1 ? 0 : 1 }
  ' "$manifest"; then
    printf 'assertion failed: manifest is not an exact two-row release set\n' >&2
    return 1
  fi
  while IFS='  ' read -r digest filename; do
    actual=$(sha256_file "$directory/$filename")
    if [ "$actual" != "$digest" ]; then
      printf 'assertion failed: manifest digest does not match %s\n' "$filename" >&2
      return 1
    fi
  done <"$manifest"
}

assert_exact_snapshot() {
  directory=$1
  version=$2
  expected_files=3
  actual_files=$(find "$directory" -type f | wc -l | tr -d ' ')
  assert_text_equals "$actual_files" "$expected_files" 'snapshot has exactly three files' || return 1
  assert_present "$directory/$(archive_name "$version" aarch64-apple-darwin)" 'snapshot Apple archive' || return 1
  assert_present "$directory/$(archive_name "$version" x86_64-unknown-linux-musl)" 'snapshot Linux archive' || return 1
  assert_strict_manifest "$directory" "$version"
}

run_website() {
  (
    cd "$REPO_ROOT" || exit 1
    with_release_environment "$REPO_ROOT/scripts/deploy-website.sh" "$@"
  ) >"$CASE_DIR/website.out" 2>&1
}

run_downloads() {
  (
    cd "$REPO_ROOT" || exit 1
    with_release_environment "$REPO_ROOT/scripts/deploy-downloads.sh" "$@"
  ) >"$CASE_DIR/downloads.out" 2>&1
}

run_packager() {
  PATH=$SHIM_DIR:$BASE_PATH "$REPO_ROOT/scripts/package-built-release.sh" "$@" >"$CASE_DIR/packager.out" 2>&1
}

website_release_version() {
  awk -F= '/^DEFAULT_VERSION=/{ gsub(/"/, "", $2); print $2; exit }' "$REPO_ROOT/website/install.sh"
}

expect_failure() {
  "$@" && {
    printf 'assertion failed: command unexpectedly succeeded: %s\n' "$*" >&2
    return 1
  }
  return 0
}

reset_remote_logs() {
  : >"$AWS_LOG"
  : >"$TOFU_LOG"
  : >"$BARRIER_LOG"
}

assert_barrier_order() {
  barrier_file=$1
  shift
  previous_line=0
  for expected_phase in "$@"; do
    phase_line=$(awk -v phase="$expected_phase" '$0 == phase { print NR; exit }' "$barrier_file")
    if [ -z "$phase_line" ] || [ "$phase_line" -le "$previous_line" ]; then
      printf 'assertion failed: missing or out-of-order release barrier %s\n' "$expected_phase" >&2
      return 1
    fi
    previous_line=$phase_line
  done
}

test_fake_s3_delete_marker_model() {
  new_case
  lock_body=$CASE_DIR/lock-body
  printf 'ACQUIRED\n' >"$lock_body"
  with_release_environment "$SHIM_DIR/aws" s3api put-object --bucket trueflow-test-bucket --key .trueflow-release/publication.lock --body "$lock_body" --if-none-match '*' >/dev/null || return 1
  meta=$(current_meta_path trueflow-test-bucket .trueflow-release/publication.lock)
  IFS=$TAB read -r kind acquired_version acquired_etag <"$meta"
  printf 'PREPARED\n' >"$lock_body"
  with_release_environment "$SHIM_DIR/aws" s3api put-object --bucket trueflow-test-bucket --key .trueflow-release/publication.lock --body "$lock_body" --if-match "$acquired_etag" >/dev/null || return 1
  IFS=$TAB read -r kind prepared_version prepared_etag <"$meta"
  with_release_environment "$SHIM_DIR/aws" s3api delete-object --bucket trueflow-test-bucket --key .trueflow-release/publication.lock --version-id "$prepared_version" >/dev/null || return 1
  assert_text_equals "$(cat "$(current_path trueflow-test-bucket .trueflow-release/publication.lock)")" ACQUIRED 'exact-version delete resurfaces older lock body' || return 1
  IFS=$TAB read -r kind resurfaced_version resurfaced_etag <"$meta"
  with_release_environment "$SHIM_DIR/aws" s3api delete-object --bucket trueflow-test-bucket --key .trueflow-release/publication.lock --if-match "$resurfaced_etag" >/dev/null || return 1
  assert_no_current_object trueflow-test-bucket .trueflow-release/publication.lock 'lock hidden behind current delete marker' || return 1
  assert_delete_marker trueflow-test-bucket .trueflow-release/publication.lock 'lock current marker' || return 1
  printf 'NEXT\n' >"$lock_body"
  with_release_environment "$SHIM_DIR/aws" s3api put-object --bucket trueflow-test-bucket --key .trueflow-release/publication.lock --body "$lock_body" --if-none-match '*' >/dev/null || return 1
  assert_text_equals "$(cat "$(current_path trueflow-test-bucket .trueflow-release/publication.lock)")" NEXT 'conditional acquisition after delete marker' || return 1
}

test_root_sync_preserves_release_owned_prefixes() {
  new_case
  website_version=$(website_release_version)
  website_release=$CASE_DIR/website-release
  make_release "$website_release" "$website_version" public-apple public-linux
  for release_file in "$website_release"/*; do
    with_release_environment "$SHIM_DIR/aws" s3api put-object \
      --bucket trueflow-test-bucket \
      --key "download/$(basename "$release_file")" \
      --body "$release_file" >/dev/null || return 1
  done
  website_arm=$(archive_name "$website_version" aarch64-apple-darwin)
  website_linux=$(archive_name "$website_version" x86_64-unknown-linux-musl)
  website_manifest=$(manifest_name "$website_version")
  website_ledger=$CASE_DIR/website-ledger
  cat >"$website_ledger" <<EOF
version=$website_version
archive_macos_name=$website_arm
archive_macos_sha256=$(sha256_file "$website_release/$website_arm")
archive_linux_name=$website_linux
archive_linux_sha256=$(sha256_file "$website_release/$website_linux")
manifest_name=$website_manifest
manifest_sha256=$(sha256_file "$website_release/$website_manifest")
EOF
  with_release_environment "$SHIM_DIR/aws" s3api put-object \
    --bucket trueflow-test-bucket \
    --key ".trueflow-release/ledger/$website_version" \
    --body "$website_ledger" >/dev/null || return 1
  seed_object trueflow-test-bucket download/trueflow-v1-aarch64-apple-darwin.tar.gz old-download
  seed_object trueflow-test-bucket .trueflow-release/ledger/v1 immutable-ledger
  seed_object trueflow-test-bucket retired-page.html obsolete-page
  reset_remote_logs
  run_website || return 1
  assert_current_object trueflow-test-bucket download/trueflow-v1-aarch64-apple-darwin.tar.gz 'release-owned download after root website sync' || return 1
  assert_current_object trueflow-test-bucket .trueflow-release/ledger/v1 'release ledger after root website sync' || return 1
  assert_no_current_object trueflow-test-bucket retired-page.html 'obsolete website-owned key after root website sync'
}

test_strict_manifest_validation() {
  new_case
  version=v9.8.7
  artifacts=$CASE_DIR/artifacts
  make_release "$artifacts" "$version" apple linux
  reset_remote_logs
  run_downloads --validate-only "$artifacts" || {
    printf 'assertion failed: exact valid manifest must validate before any remote action\n' >&2
    return 1
  }
  assert_empty_file "$AWS_LOG" 'AWS calls from validate-only' || return 1
  assert_empty_file "$TOFU_LOG" 'OpenTofu calls from validate-only' || return 1

  manifest=$artifacts/$(manifest_name "$version")
  cp "$manifest" "$CASE_DIR/good-manifest"
  arm=$(archive_name "$version" aarch64-apple-darwin)
  linux=$(archive_name "$version" x86_64-unknown-linux-musl)

  cp "$CASE_DIR/good-manifest" "$manifest"
  printf '%s  %s\n' "$(sha256_file "$artifacts/$arm")" "$arm" >>"$manifest"
  reset_remote_logs
  expect_failure run_downloads --validate-only "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls for duplicate manifest row' || return 1
  assert_empty_file "$TOFU_LOG" 'OpenTofu calls for duplicate manifest row' || return 1

  cp "$CASE_DIR/good-manifest" "$manifest"
  printf '%064d  unexpected.tar.gz\n' 0 >>"$manifest"
  reset_remote_logs
  expect_failure run_downloads --validate-only "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls for unsupported manifest row' || return 1

  cp "$CASE_DIR/good-manifest" "$manifest"
  printf 'not-a-sha  %s\n' "$arm" >"$manifest"
  reset_remote_logs
  expect_failure run_downloads --validate-only "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls for malformed manifest row' || return 1

  cp "$CASE_DIR/good-manifest" "$manifest"
  printf '%064d  %s\n' 0 "$arm" >"$manifest"
  printf '%s  %s\n' "$(sha256_file "$artifacts/$linux")" "$linux" >>"$manifest"
  reset_remote_logs
  expect_failure run_downloads --validate-only "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls for checksum mismatch' || return 1

  cp "$CASE_DIR/good-manifest" "$manifest"
  printf '\n' >>"$manifest"
  reset_remote_logs
  expect_failure run_downloads --validate-only "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls for blank manifest row' || return 1
}

test_legacy_download_deploy_is_rejected() {
  new_case
  artifacts=$CASE_DIR/artifacts
  make_release "$artifacts" v1.2.3 apple linux
  reset_remote_logs
  expect_failure run_downloads "$artifacts" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls from removed legacy one-directory deploy' || return 1
  assert_empty_file "$TOFU_LOG" 'OpenTofu calls from removed legacy one-directory deploy'
}

test_snapshot_is_frozen_before_remote_actions() {
  new_case
  version=v2.3.4
  source=$CASE_DIR/source
  snapshot=$CASE_DIR/snapshot
  receipt=$CASE_DIR/receipt
  make_release "$source" "$version" original-apple original-linux
  mkdir "$snapshot"
  chmod 0700 "$snapshot"
  reset_remote_logs
  run_downloads --prepare-snapshot "$source" "$snapshot" || return 1
  assert_exact_snapshot "$snapshot" "$version" || return 1
  assert_empty_file "$AWS_LOG" 'AWS calls while preparing local snapshot' || return 1
  assert_empty_file "$TOFU_LOG" 'OpenTofu calls while preparing local snapshot' || return 1

  arm=$(archive_name "$version" aarch64-apple-darwin)
  linux=$(archive_name "$version" x86_64-unknown-linux-musl)
  printf 'replaced-apple\n' >"$source/$arm"
  printf 'replaced-linux\n' >"$source/$linux"
  {
    printf '%s  %s\n' "$(sha256_file "$source/$arm")" "$arm"
    printf '%s  %s\n' "$(sha256_file "$source/$linux")" "$linux"
  } >"$source/$(manifest_name "$version")"

  run_downloads --begin-publication "$snapshot" "$receipt" || return 1
  assert_file_equals "$(current_path trueflow-test-bucket download/$arm)" "$snapshot/$arm" 'uploaded Apple archive equals frozen snapshot' || return 1
  assert_file_equals "$(current_path trueflow-test-bucket download/$linux)" "$snapshot/$linux" 'uploaded Linux archive equals frozen snapshot' || return 1
  assert_file_equals "$(current_path trueflow-test-bucket download/$(manifest_name "$version"))" "$snapshot/$(manifest_name "$version")" 'uploaded manifest equals frozen snapshot' || return 1
  run_downloads --abort-publication "$receipt" || return 1
}

test_packager_tar_failure_is_atomic() {
  new_case
  version=v3.4.5
  output=$CASE_DIR/output
  binary=$CASE_DIR/trueflow
  printf '#!/bin/sh\nprintf first\n' >"$binary"
  chmod 0755 "$binary"
  run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output" || return 1
  run_packager --target x86_64-unknown-linux-musl --binary "$binary" --version "$version" --output-dir "$output" || return 1
  artifact_dir=$output/$version
  archive=$artifact_dir/$(archive_name "$version" aarch64-apple-darwin)
  manifest=$artifact_dir/$(manifest_name "$version")
  cp "$archive" "$CASE_DIR/archive-before"
  cp "$manifest" "$CASE_DIR/manifest-before"
  printf '#!/bin/sh\nprintf replacement\n' >"$binary"
  if TF_SHIM_FAIL_TAR=1 run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output"; then
    printf 'assertion failed: injected tar failure unexpectedly succeeded\n' >&2
    return 1
  fi
  assert_file_equals "$archive" "$CASE_DIR/archive-before" 'final archive survives tar failure' || return 1
  assert_file_equals "$manifest" "$CASE_DIR/manifest-before" 'final manifest survives tar failure'
}

test_packager_checksum_failure_is_atomic() {
  new_case
  version=v3.4.6
  output=$CASE_DIR/output
  binary=$CASE_DIR/trueflow
  printf '#!/bin/sh\nprintf original\n' >"$binary"
  chmod 0755 "$binary"
  run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output" || return 1
  run_packager --target x86_64-unknown-linux-musl --binary "$binary" --version "$version" --output-dir "$output" || return 1
  artifact_dir=$output/$version
  manifest=$artifact_dir/$(manifest_name "$version")
  cp "$manifest" "$CASE_DIR/manifest-before"
  printf '#!/bin/sh\nprintf replacement\n' >"$binary"
  if TF_SHIM_FAIL_SHASUM=1 run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output"; then
    printf 'assertion failed: injected checksum failure unexpectedly succeeded\n' >&2
    return 1
  fi
  assert_file_equals "$manifest" "$CASE_DIR/manifest-before" 'final manifest survives checksum failure'
}

test_packager_replaces_same_target_once() {
  new_case
  version=v3.4.7
  output=$CASE_DIR/output
  binary=$CASE_DIR/trueflow
  printf '#!/bin/sh\nprintf first\n' >"$binary"
  chmod 0755 "$binary"
  run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output" || return 1
  run_packager --target x86_64-unknown-linux-musl --binary "$binary" --version "$version" --output-dir "$output" || return 1
  printf '#!/bin/sh\nprintf replacement\n' >"$binary"
  run_packager --target aarch64-apple-darwin --binary "$binary" --version "$version" --output-dir "$output" || return 1
  assert_strict_manifest "$output/$version" "$version"
}

prepare_release_snapshot() {
  source=$1
  snapshot=$2
  version=$3
  make_release "$source" "$version" "$version-apple" "$version-linux"
  mkdir "$snapshot"
  chmod 0700 "$snapshot"
  run_downloads --prepare-snapshot "$source" "$snapshot"
}

test_lock_abort_creates_current_delete_marker() {
  new_case
  version=v4.5.6
  source=$CASE_DIR/source
  snapshot=$CASE_DIR/snapshot
  receipt=$CASE_DIR/receipt
  prepare_release_snapshot "$source" "$snapshot" "$version" || return 1
  run_downloads --begin-publication "$snapshot" "$receipt" || return 1
  assert_current_object trueflow-test-bucket .trueflow-release/publication.lock 'held publication lock' || return 1
  assert_contains PREPARED "$(current_path trueflow-test-bucket .trueflow-release/publication.lock)" 'prepared lock body' || return 1
  run_downloads --abort-publication "$receipt" || return 1
  assert_no_current_object trueflow-test-bucket .trueflow-release/publication.lock 'released lock current body' || return 1
  assert_delete_marker trueflow-test-bucket .trueflow-release/publication.lock 'released lock delete marker' || return 1
  assert_not_contains '--version-id' "$AWS_LOG" 'version-specific lock deletion'
}

website_failure_case() {
  failure_point=$1
  failure_after=${2:-0}
  new_case
  version=$(website_release_version)
  [ -n "$version" ] || { printf 'assertion failed: website has no DEFAULT_VERSION\n' >&2; return 1; }
  old_version=v0.0.1
  old_dir=$CASE_DIR/old
  source=$CASE_DIR/source
  snapshot=$CASE_DIR/snapshot
  receipt=$CASE_DIR/receipt
  make_release "$old_dir" "$old_version" old-apple old-linux
  for file in "$old_dir"/*; do
    seed_object trueflow-test-bucket "download/$(basename "$file")" "$(cat "$file")"
  done
  reset_remote_logs
  prepare_release_snapshot "$source" "$snapshot" "$version" || return 1
  run_downloads --begin-publication "$snapshot" "$receipt" || return 1
  old_arm=$(archive_name "$old_version" aarch64-apple-darwin)
  old_manifest=$(manifest_name "$old_version")
  new_arm=$(archive_name "$version" aarch64-apple-darwin)
  new_manifest=$(manifest_name "$version")
  assert_current_object trueflow-test-bucket "download/$old_arm" 'old download before website switch' || return 1
  assert_current_object trueflow-test-bucket "download/$old_manifest" 'old manifest before website switch' || return 1
  assert_current_object trueflow-test-bucket "download/$new_arm" 'new download before website switch' || return 1
  assert_current_object trueflow-test-bucket "download/$new_manifest" 'new manifest before website switch' || return 1

  case "$failure_point" in
    s3-sync-before)
      if TF_AWS_FAIL=s3-sync-before run_website --publication-receipt "$receipt"; then
        printf 'assertion failed: injected website sync failure unexpectedly succeeded\n' >&2
        return 1
      fi
      ;;
    s3-sync-after)
      if TF_AWS_FAIL_SYNC_AFTER="$failure_after" run_website --publication-receipt "$receipt"; then
        printf 'assertion failed: injected partial website sync unexpectedly succeeded\n' >&2
        return 1
      fi
      ;;
    cloudfront-invalidation)
      if TF_AWS_FAIL=cloudfront-invalidation run_website --publication-receipt "$receipt"; then
        printf 'assertion failed: injected website invalidation failure unexpectedly succeeded\n' >&2
        return 1
      fi
      ;;
    *)
      printf 'test bug: unsupported website failure point %s\n' "$failure_point" >&2
      return 1
      ;;
  esac
  assert_current_object trueflow-test-bucket "download/$old_arm" 'old download after website failure' || return 1
  assert_current_object trueflow-test-bucket "download/$old_manifest" 'old manifest after website failure' || return 1
  assert_current_object trueflow-test-bucket "download/$new_arm" 'new download after website failure' || return 1
  assert_current_object trueflow-test-bucket "download/$new_manifest" 'new manifest after website failure' || return 1
  assert_contains PREPARED "$(current_path trueflow-test-bucket .trueflow-release/publication.lock)" 'prepared lock retained after website failure' || return 1
  assert_not_contains "delete-current bucket=trueflow-test-bucket key=download/$old_arm" "$AWS_LOG" 'old cleanup after website failure' || return 1
  assert_not_contains 'paths= /download/*' "$AWS_LOG" 'download invalidation after website failure' || return 1
  run_downloads --abort-publication "$receipt"
}

test_website_failure_keeps_old_and_new_downloads() {
  printf '  failure barrier: before root sync\n'
  website_failure_case s3-sync-before || return 1
  printf '  failure barrier: after first copied website object\n'
  website_failure_case s3-sync-after 1 || return 1
  printf '  failure barrier: website invalidation\n'
  website_failure_case cloudfront-invalidation
}

test_phase_order_and_finalize_gate() {
  new_case
  TEST_BARRIER=$SHIM_DIR/release-barrier
  version=$(website_release_version)
  [ -n "$version" ] || return 1
  old_version=v0.0.2
  old_dir=$CASE_DIR/old
  source=$CASE_DIR/source
  snapshot=$CASE_DIR/snapshot
  receipt=$CASE_DIR/receipt
  make_release "$old_dir" "$old_version" old-apple old-linux
  for file in "$old_dir"/*; do
    seed_object trueflow-test-bucket "download/$(basename "$file")" "$(cat "$file")"
  done
  reset_remote_logs
  prepare_release_snapshot "$source" "$snapshot" "$version" || return 1
  run_downloads --begin-publication "$snapshot" "$receipt" || return 1
  old_arm=$(archive_name "$old_version" aarch64-apple-darwin)
  assert_current_object trueflow-test-bucket "download/$old_arm" 'old download retained at PREPARED' || return 1
  if run_downloads --finalize-publication "$snapshot" "$receipt"; then
    printf 'assertion failed: finalize before website switch unexpectedly succeeded\n' >&2
    return 1
  fi
  assert_current_object trueflow-test-bucket "download/$old_arm" 'old download after rejected early finalize' || return 1
  run_website --publication-receipt "$receipt" || return 1
  assert_contains WEBSITE_SWITCHED "$(current_path trueflow-test-bucket .trueflow-release/publication.lock)" 'lock after receipt-bound website sync' || return 1
  run_downloads --finalize-publication "$snapshot" "$receipt" || return 1
  assert_no_current_object trueflow-test-bucket "download/$old_arm" 'old download after post-switch finalize' || return 1
  assert_delete_marker trueflow-test-bucket "download/$old_arm" 'old download delete marker after finalize' || return 1
  assert_contains 'invalidation distribution=TESTDIST paths= /download/*' "$AWS_LOG" 'download invalidation after cleanup' || return 1
  assert_no_current_object trueflow-test-bucket .trueflow-release/publication.lock 'lock after finalized release' || return 1
  assert_delete_marker trueflow-test-bucket .trueflow-release/publication.lock 'lock marker after finalized release' || return 1
  assert_barrier_order "$BARRIER_LOG" PREPARED WEBSITE_SWITCHED BEFORE_CLEANUP BEFORE_UNLOCK
}

test_concurrent_and_replayed_receipts_cannot_publish() {
  new_case
  first_version=v5.0.1
  second_version=v5.0.2
  first_source=$CASE_DIR/first-source
  first_snapshot=$CASE_DIR/first-snapshot
  first_receipt=$CASE_DIR/first-receipt
  second_source=$CASE_DIR/second-source
  second_snapshot=$CASE_DIR/second-snapshot
  second_receipt=$CASE_DIR/second-receipt
  prepare_release_snapshot "$first_source" "$first_snapshot" "$first_version" || return 1
  prepare_release_snapshot "$second_source" "$second_snapshot" "$second_version" || return 1
  reset_remote_logs
  run_downloads --begin-publication "$first_snapshot" "$first_receipt" || return 1
  second_arm=$(archive_name "$second_version" aarch64-apple-darwin)
  if run_downloads --begin-publication "$second_snapshot" "$second_receipt"; then
    printf 'assertion failed: concurrent begin unexpectedly acquired held lock\n' >&2
    return 1
  fi
  assert_no_current_object trueflow-test-bucket "download/$second_arm" 'second release archive while first owns lock' || return 1
  assert_not_contains "key=.trueflow-release/ledger/$second_version" "$AWS_LOG" 'second ledger mutation while first owns lock' || return 1
  run_downloads --abort-publication "$first_receipt" || return 1
  run_downloads --begin-publication "$second_snapshot" "$second_receipt" || return 1
  if run_downloads --verify-publication-phase "$first_receipt" PREPARED; then
    printf 'assertion failed: replayed first receipt verified against newer lock\n' >&2
    return 1
  fi
  if run_downloads --finalize-publication "$first_snapshot" "$first_receipt"; then
    printf 'assertion failed: replayed first receipt finalized newer publication\n' >&2
    return 1
  fi
  assert_current_object trueflow-test-bucket "download/$(archive_name "$first_version" aarch64-apple-darwin)" 'first release archive after rejected stale finalize' || return 1
  run_downloads --abort-publication "$second_receipt"
}

run_test() {
  label=$1
  function_name=$2
  TOTAL=$((TOTAL + 1))
  printf '\n== %s\n' "$label"
  if "$function_name"; then
    printf 'ok %s\n' "$label"
  else
    printf 'not ok %s\n' "$label" >&2
    FAILED=$((FAILED + 1))
  fi
}

printf 'release publication safety contracts (network-free)\n'
run_test 'fake S3 distinguishes version deletion from a current delete marker' test_fake_s3_delete_marker_model
run_test 'root website sync preserves download and release-control ownership' test_root_sync_preserves_release_owned_prefixes
run_test 'download validation accepts only an exact two-row digest manifest before remote work' test_strict_manifest_validation
run_test 'legacy one-directory download deploy is rejected without side effects' test_legacy_download_deploy_is_rejected
run_test 'snapshot bytes remain the uploaded bytes after original paths are replaced' test_snapshot_is_frozen_before_remote_actions
run_test 'packager preserves final files when archive creation fails' test_packager_tar_failure_is_atomic
run_test 'packager preserves final manifest when checksum creation fails' test_packager_checksum_failure_is_atomic
run_test 'packager regenerates a same-target manifest with one logical row per target' test_packager_replaces_same_target_once
run_test 'abort removes only the current lock through a delete marker' test_lock_abort_creates_current_delete_marker
run_test 'website failure barriers retain both old and new downloadable releases' test_website_failure_keeps_old_and_new_downloads
run_test 'only a receipt-bound website switch permits old-download cleanup' test_phase_order_and_finalize_gate
run_test 'held locks and replayed receipts cannot publish or clean another release' test_concurrent_and_replayed_receipts_cannot_publish

if [ "$FAILED" -ne 0 ]; then
  printf '\n%d of %d release publication safety contracts failed\n' "$FAILED" "$TOTAL" >&2
  exit 1
fi
printf '\nall %d release publication safety contracts passed\n' "$TOTAL"
