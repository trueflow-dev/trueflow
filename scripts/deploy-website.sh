#!/bin/sh
set -eu

TRUEFLOW_INFRA_CLI="${TRUEFLOW_INFRA_CLI:-tofu}"
TRUEFLOW_INFRA_DIR="${TRUEFLOW_INFRA_DIR:-infra/terraform}"

infra_output() {
  output_key=$1
  "$TRUEFLOW_INFRA_CLI" -chdir="$TRUEFLOW_INFRA_DIR" output -raw "$output_key"
}

SITE_BUCKET="${TRUEFLOW_SITE_BUCKET:-$(infra_output site_bucket_name)}"
DISTRIBUTION_ID="${TRUEFLOW_DISTRIBUTION_ID:-$(infra_output site_distribution_id)}"

printf '==> syncing website/ to s3://%s\n' "$SITE_BUCKET"
aws s3 sync website/ "s3://${SITE_BUCKET}/" --delete

printf '==> invalidating CloudFront distribution %s\n' "$DISTRIBUTION_ID"
aws cloudfront create-invalidation \
  --distribution-id "$DISTRIBUTION_ID" \
  --paths '/*' >/dev/null

printf '==> website deploy queued\n'
