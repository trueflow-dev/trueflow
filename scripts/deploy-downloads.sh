#!/bin/sh
set -eu

if [ $# -ne 1 ]; then
  printf 'usage: %s ARTIFACT_DIR\n' "$0" >&2
  exit 1
fi

ARTIFACT_DIR=$1
TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-infra/terraform}"
REQUIRED_TARGETS="aarch64-apple-darwin x86_64-unknown-linux-musl"

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

validate_artifact_dir() {
  [ -d "$ARTIFACT_DIR" ] || die "artifact directory not found: $ARTIFACT_DIR"

  set -- "$ARTIFACT_DIR"/trueflow-*-SHA256SUMS.txt
  [ "$#" -eq 1 ] && [ -f "$1" ] || die "expected exactly one trueflow-vX.Y.Z-SHA256SUMS.txt file in $ARTIFACT_DIR"

  CHECKSUM_PATH=$1
  CHECKSUM_NAME=$(basename "$CHECKSUM_PATH")
  VERSION=${CHECKSUM_NAME#trueflow-}
  VERSION=${VERSION%-SHA256SUMS.txt}
  [ -n "$VERSION" ] && [ "$VERSION" != "$CHECKSUM_NAME" ] || die "invalid checksum filename: $CHECKSUM_NAME"

  for target in $REQUIRED_TARGETS; do
    archive_name="trueflow-${VERSION}-${target}.tar.gz"
    archive_path="$ARTIFACT_DIR/$archive_name"

    [ -f "$archive_path" ] || die "missing required release artifact: $archive_name"
    awk -v target="$archive_name" '$2 == target { found = 1 } END { exit found ? 0 : 1 }' "$CHECKSUM_PATH" \
      || die "missing checksum entry for required release artifact: $archive_name"
  done
}

infra_output() {
  output_key=$1
  "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" output -raw "$output_key"
}

validate_artifact_dir

SITE_BUCKET="${TRUEFLOW_SITE_BUCKET:-$(infra_output site_bucket_name)}"
DISTRIBUTION_ID="${TRUEFLOW_DISTRIBUTION_ID:-$(infra_output site_distribution_id)}"

printf '==> syncing %s to s3://%s/download/\n' "$ARTIFACT_DIR" "$SITE_BUCKET"
aws s3 sync "$ARTIFACT_DIR/" "s3://${SITE_BUCKET}/download/" --delete

printf '==> invalidating CloudFront distribution %s\n' "$DISTRIBUTION_ID"
aws cloudfront create-invalidation \
  --distribution-id "$DISTRIBUTION_ID" \
  --paths '/download/*' '/install.sh' >/dev/null

printf '==> download deploy queued\n'
