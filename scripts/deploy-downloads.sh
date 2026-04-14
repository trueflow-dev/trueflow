#!/bin/sh
set -eu

if [ $# -ne 1 ]; then
  printf 'usage: %s ARTIFACT_DIR\n' "$0" >&2
  exit 1
fi

ARTIFACT_DIR=$1
TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-infra/terraform}"

infra_output() {
  output_key=$1
  "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" output -raw "$output_key"
}

SITE_BUCKET="${TRUEFLOW_SITE_BUCKET:-$(infra_output site_bucket_name)}"
DISTRIBUTION_ID="${TRUEFLOW_DISTRIBUTION_ID:-$(infra_output site_distribution_id)}"

printf '==> syncing %s to s3://%s/download/\n' "$ARTIFACT_DIR" "$SITE_BUCKET"
aws s3 sync "$ARTIFACT_DIR/" "s3://${SITE_BUCKET}/download/" --delete

printf '==> invalidating CloudFront distribution %s\n' "$DISTRIBUTION_ID"
aws cloudfront create-invalidation \
  --distribution-id "$DISTRIBUTION_ID" \
  --paths '/download/*' '/install.sh' >/dev/null

printf '==> download deploy queued\n'
