#!/usr/bin/env bash
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
version=${1:-"0.1.0"}
tag="review-bench-v${version}"

echo "preparing release ${tag}"

if [[ ! -f "${workspace_root}/Cargo.toml" ]]; then
  echo "workspace is missing Cargo.toml" >&2
  exit 1
fi

artifacts=(
  "target/release/review-bench"
  "target/release/review-bench.dSYM"
)

for artifact in "${artifacts[@]}"; do
  echo "would publish ${artifact}"
done

echo "release plan complete"
