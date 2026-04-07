#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/measure-checks.sh [--profile PROFILE] [--output-dir DIR] [--run-name NAME] [stage ...]
  scripts/measure-checks.sh --list-profiles
  scripts/measure-checks.sh --list-stages

Profiles:
  check           Default local gate (tests, lint, fmt)
  check-fast      Faster no-test local gate (compile, lint, fmt)
  check-heavy     Heavyweight code-only checks (audit, doc, coverage)
  check-code      Broad local code verification (tests/examples/lint/docs/coverage; benches excluded)
  check-packaging Separate host-default Nix package verification
  current-check   Legacy alias for check-code
  check-full      Legacy alias for check-code
  local-minimum   Legacy alias for check-fast
  local-dev       Legacy alias for check

Examples:
  scripts/measure-checks.sh --profile check
  scripts/measure-checks.sh --profile check-fast
  scripts/measure-checks.sh --profile check-code
EOF
}

list_profiles() {
  cat <<'EOF'
check
check-fast
check-heavy
check-code
check-packaging
current-check
check-full
local-minimum
local-dev
EOF
}

list_stages() {
  cat <<'EOF'
compile-check
compile-check-code
compile-check-all-targets
test
test-code
test-full
lint
lint-code
lint-all-targets
fmt-check
audit
doc
coverage-check
nix-check
nix-check-native
nix-check-default
nix-check-static
nix-check-release
nix-check-native-with-tests
nix-check-default-with-tests
nix-check-static-with-tests
nix-check-release-with-tests
EOF
}

stage_command() {
  case "$1" in
    compile-check)
      printf '%s\n' 'cd trueflow && cargo check --features tui-test-support --lib --bins --tests'
      ;;
    compile-check-code|compile-check-all-targets)
      printf '%s\n' 'cd trueflow && cargo check --features tui-test-support --lib --bins --tests --examples'
      ;;
    test)
      printf '%s\n' 'cd trueflow && cargo nextest run --features tui-test-support'
      ;;
    test-code|test-full)
      printf '%s\n' 'cd trueflow && cargo nextest run --features tui-test-support --lib --bins --tests --examples'
      ;;
    lint)
      printf '%s\n' 'cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests -- -D warnings'
      ;;
    lint-code|lint-all-targets)
      printf '%s\n' 'cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples -- -D warnings'
      ;;
    fmt-check)
      printf '%s\n' 'cd trueflow && cargo fmt --check --all'
      ;;
    audit)
      printf '%s\n' 'cd trueflow && cargo audit'
      ;;
    doc)
      printf '%s\n' 'cd trueflow && cargo doc --features tui-test-support --no-deps'
      ;;
    coverage-check)
      printf '%s\n' 'cd trueflow && cargo llvm-cov --features tui-test-support --lib --bins --tests --summary-only --ignore-filename-regex "src/commands/tui.rs" --fail-under-lines 80'
      ;;
    nix-check)
      printf '%s\n' 'nix build --no-link .#default'
      ;;
    nix-check-native)
      printf '%s\n' 'nix build --no-link .#native'
      ;;
    nix-check-default)
      printf '%s\n' 'nix build --no-link .#default'
      ;;
    nix-check-static)
      printf '%s\n' 'nix build --no-link .#static'
      ;;
    nix-check-release)
      printf '%s\n' 'nix build --no-link .#release'
      ;;
    nix-check-native-with-tests)
      printf '%s\n' 'nix build --no-link .#native-with-tests'
      ;;
    nix-check-default-with-tests)
      printf '%s\n' 'nix build --no-link .#default-with-tests'
      ;;
    nix-check-static-with-tests)
      printf '%s\n' 'nix build --no-link .#static-with-tests'
      ;;
    nix-check-release-with-tests)
      printf '%s\n' 'nix build --no-link .#release-with-tests'
      ;;
    *)
      echo "unknown stage: $1" >&2
      exit 2
      ;;
  esac
}

profile_stages() {
  case "$1" in
    check|local-dev)
      printf '%s\n' test lint fmt-check
      ;;
    check-fast|local-minimum)
      printf '%s\n' compile-check lint fmt-check
      ;;
    check-heavy)
      printf '%s\n' audit doc coverage-check
      ;;
    check-packaging)
      printf '%s\n' nix-check
      ;;
    check-code|check-full|current-check)
      printf '%s\n' test-code lint-code fmt-check audit doc coverage-check
      ;;
    *)
      echo "unknown profile: $1" >&2
      exit 2
      ;;
  esac
}

format_duration() {
  local seconds="$1"
  local hours=$((seconds / 3600))
  local minutes=$(((seconds % 3600) / 60))
  local remainder=$((seconds % 60))

  if [ "$hours" -gt 0 ]; then
    printf '%dh %dm %ds' "$hours" "$minutes" "$remainder"
  elif [ "$minutes" -gt 0 ]; then
    printf '%dm %ds' "$minutes" "$remainder"
  else
    printf '%ds' "$remainder"
  fi
}

profile="check"
output_dir=""
run_name=""
custom_stages=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile)
      profile="$2"
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
      shift 2
      ;;
    --run-name)
      run_name="$2"
      shift 2
      ;;
    --list-profiles)
      list_profiles
      exit 0
      ;;
    --list-stages)
      list_stages
      exit 0
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      while [ "$#" -gt 0 ]; do
        custom_stages+=("$1")
        shift
      done
      ;;
    *)
      custom_stages+=("$1")
      shift
      ;;
  esac
done

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
cd "$repo_root"

if [ -z "$output_dir" ]; then
  output_dir="$repo_root/.trueflow/measurements"
fi
mkdir -p "$output_dir"

timestamp=$(date -u +"%Y%m%dT%H%M%SZ")
if [ -z "$run_name" ]; then
  run_name="${profile}-${timestamp}"
fi
run_dir="$output_dir/$run_name"
stages_dir="$run_dir/stages"
mkdir -p "$stages_dir"

stages=()
if [ "${#custom_stages[@]}" -gt 0 ]; then
  stages=("${custom_stages[@]}")
else
  while IFS= read -r stage; do
    [ -n "$stage" ] && stages+=("$stage")
  done < <(profile_stages "$profile")
fi

summary_tsv="$run_dir/summary.tsv"
summary_md="$run_dir/summary.md"
env_txt="$run_dir/env.txt"

head_commit=$(git rev-parse HEAD)
if git diff --quiet && git diff --cached --quiet; then
  dirty_state="clean"
else
  dirty_state="dirty"
fi

{
  printf 'timestamp_utc=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf 'profile=%s\n' "$profile"
  printf 'run_name=%s\n' "$run_name"
  printf 'head_commit=%s\n' "$head_commit"
  printf 'git_state=%s\n' "$dirty_state"
  printf 'uname=%s\n' "$(uname -a)"
  if command -v rustc >/dev/null 2>&1; then
    printf 'rustc=%s\n' "$(rustc --version)"
  fi
  if command -v cargo >/dev/null 2>&1; then
    printf 'cargo=%s\n' "$(cargo --version)"
  fi
  if command -v nix >/dev/null 2>&1; then
    printf 'nix=%s\n' "$(nix --version 2>/dev/null || true)"
  fi
} > "$env_txt"

printf 'stage\tstatus\tduration_seconds\tstart_utc\tend_utc\tcommand\n' > "$summary_tsv"

recorded_stages=()
recorded_statuses=()
recorded_durations=()
recorded_commands=()

failed=0
total_duration=0

for stage in "${stages[@]}"; do
  command_text=$(stage_command "$stage")
  stage_dir="$stages_dir/$stage"
  mkdir -p "$stage_dir"
  printf '%s\n' "$command_text" > "$stage_dir/command.txt"

  start_utc=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  SECONDS=0
  set +e
  bash -lc "cd '$repo_root' && set -euo pipefail && $command_text" \
    > "$stage_dir/stdout.log" \
    2> "$stage_dir/stderr.log"
  status=$?
  set -e
  duration=$SECONDS
  end_utc=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

  total_duration=$((total_duration + duration))
  if [ "$status" -ne 0 ]; then
    failed=1
  fi

  recorded_stages+=("$stage")
  recorded_statuses+=("$status")
  recorded_durations+=("$duration")
  recorded_commands+=("$command_text")

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$stage" "$status" "$duration" "$start_utc" "$end_utc" "$command_text" \
    >> "$summary_tsv"

done

{
  printf '# Check timing run\n\n'
  printf -- '- profile: `%s`\n' "$profile"
  printf -- '- run name: `%s`\n' "$run_name"
  printf -- '- head commit: `%s`\n' "$head_commit"
  printf -- '- git state: `%s`\n' "$dirty_state"
  printf -- '- total duration: `%s`\n' "$(format_duration "$total_duration")"
  printf -- '- output dir: `%s`\n\n' "$run_dir"
  printf '| Stage | Exit | Duration |
'
  printf '|---|---:|---:|
'
  for i in "${!recorded_stages[@]}"; do
    printf '| `%s` | `%s` | `%s` |\n' \
      "${recorded_stages[$i]}" \
      "${recorded_statuses[$i]}" \
      "$(format_duration "${recorded_durations[$i]}")"
  done
  printf '\nArtifacts for each stage are under `stages/<stage>/`.\n'
} > "$summary_md"

cat "$summary_md"

if [ "$failed" -ne 0 ]; then
  echo
  echo "One or more stages failed. See $summary_tsv and per-stage logs under $stages_dir." >&2
  exit 1
fi
