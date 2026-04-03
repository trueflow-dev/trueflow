#!/usr/bin/env bash
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
config_file="${workspace_root}/config/review.toml"

if [[ ! -f "${config_file}" ]]; then
  echo "missing config: ${config_file}" >&2
  exit 1
fi

mkdir -p "${workspace_root}/tmp"
cp "${config_file}" "${workspace_root}/tmp/review.toml"

echo "bootstrapped review bench workspace"
echo "root=${workspace_root}"
echo "config=${config_file}"
