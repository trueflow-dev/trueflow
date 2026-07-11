#!/bin/sh
# Network-free behavioral contracts for website/install.sh. Every external
# release interaction is fail-closed and backed by a per-case fixture.
set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
INSTALLER=$REPO_ROOT/website/install.sh
BASE_PATH=$PATH
TEST_BASE_URL=https://installer-fixture.invalid
GOOD_SUM=1111111111111111111111111111111111111111111111111111111111111111
BAD_SUM=2222222222222222222222222222222222222222222222222222222222222222

case "${TMPDIR:-/tmp}" in
  /*) TEMP_BASE=${TMPDIR:-/tmp} ;;
  *) TEMP_BASE=/tmp ;;
esac
WORK=$(mktemp -d "$TEMP_BASE/trueflow-installer-safety.XXXXXX")
KEEP_WORKSPACE=${TRUEFLOW_KEEP_INSTALLER_TEST_WORKSPACE:-0}
TOTAL=0
FAILED=0

cleanup() {
  if [ "$KEEP_WORKSPACE" = 1 ]; then
    printf 'installer safety workspace retained at %s\n' "$WORK" >&2
    return
  fi

  if command -v trash >/dev/null 2>&1; then
    trash "$WORK" >/dev/null 2>&1 || printf 'installer safety workspace retained at %s\n' "$WORK" >&2
  else
    printf 'installer safety workspace retained at %s (install trash or set TRUEFLOW_KEEP_INSTALLER_TEST_WORKSPACE=1 to inspect)\n' "$WORK" >&2
  fi
}
trap cleanup 0 1 2 15

SHIM_DIR=$WORK/shims
mkdir -p "$SHIM_DIR" "$WORK/cases"

cat >"$SHIM_DIR/uname" <<'SHIM'
#!/bin/sh
set -u
: "${TF_INSTALLER_STATE:?TF_INSTALLER_STATE is required}"
case "$#:$1" in
  1:-s)
    printf '%s\n' '-s' >>"$TF_INSTALLER_STATE/uname.log"
    printf '%s\n' "${TF_INSTALLER_UNAME_S:?TF_INSTALLER_UNAME_S is required}"
    ;;
  1:-m)
    printf '%s\n' '-m' >>"$TF_INSTALLER_STATE/uname.log"
    printf '%s\n' "${TF_INSTALLER_UNAME_M:?TF_INSTALLER_UNAME_M is required}"
    ;;
  *)
    printf 'fake uname: unsupported invocation' >&2
    for argument in "$@"; do printf ' <%s>' "$argument" >&2; done
    printf '\n' >&2
    exit 127
    ;;
esac
SHIM

cat >"$SHIM_DIR/curl" <<'SHIM'
#!/bin/sh
set -u
: "${TF_INSTALLER_STATE:?TF_INSTALLER_STATE is required}"
: "${TF_INSTALLER_ARCHIVE_FIXTURE:?TF_INSTALLER_ARCHIVE_FIXTURE is required}"
: "${TF_INSTALLER_ARCHIVE_NAME:?TF_INSTALLER_ARCHIVE_NAME is required}"
: "${TF_INSTALLER_CHECKSUM_MODE:?TF_INSTALLER_CHECKSUM_MODE is required}"
: "${TF_INSTALLER_GOOD_SUM:?TF_INSTALLER_GOOD_SUM is required}"
: "${TF_INSTALLER_BAD_SUM:?TF_INSTALLER_BAD_SUM is required}"
if [ "$#" -ne 4 ] || [ "$1" != -fsSL ] || [ "$3" != -o ]; then
  printf 'fake curl: unsupported invocation' >&2
  for argument in "$@"; do printf ' <%s>' "$argument" >&2; done
  printf '\n' >&2
  exit 127
fi
url=$2
destination=$4
printf '%s\n' "$url" >>"$TF_INSTALLER_STATE/curl.log"
case "$url" in
  *.tar.gz)
    cp "$TF_INSTALLER_ARCHIVE_FIXTURE" "$destination"
    ;;
  *-SHA256SUMS.txt)
    case "$TF_INSTALLER_CHECKSUM_MODE" in
      success)
        printf '%s  %s\n' "$TF_INSTALLER_GOOD_SUM" "$TF_INSTALLER_ARCHIVE_NAME" >"$destination"
        ;;
      missing)
        printf '%s  %s\n' "$TF_INSTALLER_GOOD_SUM" 'some-other-artifact.tar.gz' >"$destination"
        ;;
      mismatch)
        printf '%s  %s\n' "$TF_INSTALLER_BAD_SUM" "$TF_INSTALLER_ARCHIVE_NAME" >"$destination"
        ;;
      *)
        printf 'fake curl: unsupported checksum mode <%s>\n' "$TF_INSTALLER_CHECKSUM_MODE" >&2
        exit 127
        ;;
    esac
    ;;
  *)
    printf 'fake curl: unsupported URL <%s>\n' "$url" >&2
    exit 127
    ;;
esac
SHIM

cat >"$SHIM_DIR/shasum" <<'SHIM'
#!/bin/sh
set -u
: "${TF_INSTALLER_STATE:?TF_INSTALLER_STATE is required}"
: "${TF_INSTALLER_ARCHIVE_FIXTURE:?TF_INSTALLER_ARCHIVE_FIXTURE is required}"
: "${TF_INSTALLER_GOOD_SUM:?TF_INSTALLER_GOOD_SUM is required}"
if [ "$#" -ne 3 ] || [ "$1" != -a ] || [ "$2" != 256 ]; then
  printf 'fake shasum: unsupported invocation' >&2
  for argument in "$@"; do printf ' <%s>' "$argument" >&2; done
  printf '\n' >&2
  exit 127
fi
cmp -s "$TF_INSTALLER_ARCHIVE_FIXTURE" "$3" || {
  printf 'fake shasum: archive bytes differ from downloaded fixture\n' >&2
  exit 126
}
printf '%s\n' "$3" >"$TF_INSTALLER_STATE/shasum.log"
printf '%s  %s\n' "$TF_INSTALLER_GOOD_SUM" "$3"
SHIM

cat >"$SHIM_DIR/tar" <<'SHIM'
#!/bin/sh
set -u
: "${TF_INSTALLER_STATE:?TF_INSTALLER_STATE is required}"
: "${TF_INSTALLER_ARCHIVE_FIXTURE:?TF_INSTALLER_ARCHIVE_FIXTURE is required}"
: "${TF_INSTALLER_BINARY_FIXTURE:?TF_INSTALLER_BINARY_FIXTURE is required}"
if [ "$#" -ne 4 ] || [ "$1" != -xzf ] || [ "$3" != -C ]; then
  printf 'fake tar: unsupported invocation' >&2
  for argument in "$@"; do printf ' <%s>' "$argument" >&2; done
  printf '\n' >&2
  exit 127
fi
cmp -s "$TF_INSTALLER_ARCHIVE_FIXTURE" "$2" || {
  printf 'fake tar: archive bytes differ from downloaded fixture\n' >&2
  exit 126
}
[ -d "$4" ] || {
  printf 'fake tar: extraction directory does not exist <%s>\n' "$4" >&2
  exit 126
}
printf '%s\n' "$2" >"$TF_INSTALLER_STATE/tar.log"
cp "$TF_INSTALLER_BINARY_FIXTURE" "$4/trueflow"
chmod 0644 "$4/trueflow"
SHIM

cat >"$SHIM_DIR/install" <<'SHIM'
#!/bin/sh
set -u
: "${TF_INSTALLER_STATE:?TF_INSTALLER_STATE is required}"
: "${TF_INSTALLER_BINARY_FIXTURE:?TF_INSTALLER_BINARY_FIXTURE is required}"
if [ "$#" -ne 4 ] || [ "$1" != -m ] || [ "$2" != 0755 ]; then
  printf 'fake install: unsupported invocation' >&2
  for argument in "$@"; do printf ' <%s>' "$argument" >&2; done
  printf '\n' >&2
  exit 127
fi
cmp -s "$TF_INSTALLER_BINARY_FIXTURE" "$3" || {
  printf 'fake install: extracted binary bytes differ from fixture\n' >&2
  exit 126
}
printf '%s\n' "$4" >"$TF_INSTALLER_STATE/install.log"
cp "$3" "$4"
chmod 0755 "$4"
SHIM

chmod 0755 "$SHIM_DIR/uname" "$SHIM_DIR/curl" "$SHIM_DIR/shasum" "$SHIM_DIR/tar" "$SHIM_DIR/install"

fail() {
  printf '\n  FAIL: %s\n' "$1" >&2
  exit 1
}

assert_equal() {
  actual=$1
  expected=$2
  label=$3
  if [ "$actual" != "$expected" ]; then
    printf '\n  FAIL: %s\n    expected: <%s>\n      actual: <%s>\n' "$label" "$expected" "$actual" >&2
    exit 1
  fi
}

assert_files_equal() {
  expected=$1
  actual=$2
  label=$3
  cmp -s "$expected" "$actual" || fail "$label"
}

assert_file_absent() {
  path=$1
  label=$2
  [ ! -f "$path" ] || fail "$label"
}

mode_of() {
  mode=$(stat -f '%Lp' "$1" 2>/dev/null) || mode=
  case "$mode" in
    [0-7][0-7][0-7]|[0-7][0-7][0-7][0-7]) ;;
    *) mode=$(stat -c '%a' "$1" 2>/dev/null) || fail "could not read mode for $1" ;;
  esac
  printf '%s\n' "$mode"
}

assert_mode() {
  path=$1
  expected=$2
  label=$3
  actual=$(mode_of "$path")
  assert_equal "$actual" "$expected" "$label"
}

setup_case() {
  case_name=$1
  CASE_DIR=$WORK/cases/$case_name
  TF_INSTALLER_STATE=$CASE_DIR/state
  mkdir -p "$TF_INSTALLER_STATE" "$CASE_DIR/tmp" "$CASE_DIR/home"
  TF_INSTALLER_ARCHIVE_FIXTURE=$CASE_DIR/archive.fixture
  TF_INSTALLER_BINARY_FIXTURE=$CASE_DIR/binary.fixture
  printf 'deterministic archive fixture for %s\n' "$case_name" >"$TF_INSTALLER_ARCHIVE_FIXTURE"
  printf '#!/bin/sh\nprintf "fixture trueflow: %s\\n"\n' "$case_name" >"$TF_INSTALLER_BINARY_FIXTURE"
  chmod 0644 "$TF_INSTALLER_BINARY_FIXTURE"
  TF_INSTALLER_CHECKSUM_MODE=success
  TF_INSTALLER_UNAME_S=Darwin
  TF_INSTALLER_UNAME_M=arm64
  export TF_INSTALLER_STATE TF_INSTALLER_ARCHIVE_FIXTURE TF_INSTALLER_BINARY_FIXTURE
  export TF_INSTALLER_CHECKSUM_MODE TF_INSTALLER_UNAME_S TF_INSTALLER_UNAME_M
  export TF_INSTALLER_GOOD_SUM=$GOOD_SUM TF_INSTALLER_BAD_SUM=$BAD_SUM
}

select_artifact() {
  version=$1
  target=$2
  TF_INSTALLER_ARCHIVE_NAME=trueflow-${version}-${target}.tar.gz
  TF_INSTALLER_CHECKSUM_NAME=trueflow-${version}-SHA256SUMS.txt
  export TF_INSTALLER_ARCHIVE_NAME TF_INSTALLER_CHECKSUM_NAME
}

write_expected_urls() {
  expected_file=$1
  printf '%s/download/%s\n%s/download/%s\n' \
    "$TEST_BASE_URL" "$TF_INSTALLER_ARCHIVE_NAME" \
    "$TEST_BASE_URL" "$TF_INSTALLER_CHECKSUM_NAME" >"$expected_file"
}

run_installer() {
  PATH="$SHIM_DIR:$BASE_PATH" \
  HOME="$CASE_DIR/home" \
  TMPDIR="$CASE_DIR/tmp" \
  TRUEFLOW_BASE_URL="$TEST_BASE_URL" \
  TRUEFLOW_VERSION=v0.0.0-env-should-be-overridden \
  TRUEFLOW_INSTALL_DIR="$CASE_DIR/env-destination-should-be-overridden" \
    sh "$INSTALLER" "$@"
}

case_darwin_arm64() (
  setup_case darwin-arm64
  version=v9.8.7
  target=aarch64-apple-darwin
  destination=$CASE_DIR/custom-darwin-bin
  select_artifact "$version" "$target"
  expected_urls=$CASE_DIR/expected.urls
  write_expected_urls "$expected_urls"

  if ! run_installer --version "$version" --to "$destination" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"; then
    fail 'Darwin/arm64 installer invocation failed'
  fi
  assert_files_equal "$expected_urls" "$TF_INSTALLER_STATE/curl.log" 'Darwin/arm64 archive and manifest URLs were not exact'
  assert_files_equal "$TF_INSTALLER_BINARY_FIXTURE" "$destination/trueflow" 'Darwin/arm64 installed bytes differ from archive fixture'
  assert_mode "$destination/trueflow" 755 'Darwin/arm64 installed mode is not 0755'
  [ -x "$destination/trueflow" ] || fail 'Darwin/arm64 installed binary is not executable'
  assert_equal "$(cat "$TF_INSTALLER_STATE/uname.log")" "$(printf '%s\n%s' -s -m)" 'Darwin/arm64 platform probes differ'
  assert_equal "$(cat "$TF_INSTALLER_STATE/install.log")" "$destination/trueflow" 'Darwin/arm64 install destination differs'
  assert_file_absent "$CASE_DIR/env-destination-should-be-overridden/trueflow" '--to did not override TRUEFLOW_INSTALL_DIR on Darwin/arm64'
)

case_linux_x86_64() (
  setup_case linux-x86-64
  TF_INSTALLER_UNAME_S=Linux
  TF_INSTALLER_UNAME_M=x86_64
  export TF_INSTALLER_UNAME_S TF_INSTALLER_UNAME_M
  version=v3.2.1
  target=x86_64-unknown-linux-musl
  destination=$CASE_DIR/custom-linux-bin
  select_artifact "$version" "$target"
  expected_urls=$CASE_DIR/expected.urls
  write_expected_urls "$expected_urls"

  if ! run_installer --to "$destination" --version "$version" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"; then
    fail 'Linux/x86_64 installer invocation failed'
  fi
  assert_files_equal "$expected_urls" "$TF_INSTALLER_STATE/curl.log" 'Linux/x86_64 archive and manifest URLs were not exact'
  assert_files_equal "$TF_INSTALLER_BINARY_FIXTURE" "$destination/trueflow" 'Linux/x86_64 installed bytes differ from archive fixture'
  assert_mode "$destination/trueflow" 755 'Linux/x86_64 installed mode is not 0755'
  [ -x "$destination/trueflow" ] || fail 'Linux/x86_64 installed binary is not executable'
  assert_equal "$(cat "$TF_INSTALLER_STATE/uname.log")" "$(printf '%s\n%s' -s -m)" 'Linux/x86_64 platform probes differ'
  assert_equal "$(cat "$TF_INSTALLER_STATE/install.log")" "$destination/trueflow" 'Linux/x86_64 install destination differs'
  assert_file_absent "$CASE_DIR/env-destination-should-be-overridden/trueflow" '--to did not override TRUEFLOW_INSTALL_DIR on Linux/x86_64'
)

case_unsupported_platform() (
  setup_case unsupported-platform
  TF_INSTALLER_UNAME_S=Linux
  TF_INSTALLER_UNAME_M=aarch64
  export TF_INSTALLER_UNAME_S TF_INSTALLER_UNAME_M
  destination=$CASE_DIR/unsupported-bin

  if run_installer --version v1.2.3 --to "$destination" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"; then
    fail 'unsupported platform unexpectedly succeeded'
  fi
  expected_error="error: unsupported platform; current support is Apple Silicon macOS and Linux x86_64. See $TEST_BASE_URL/install/"
  assert_equal "$(cat "$CASE_DIR/stderr")" "$expected_error" 'unsupported platform diagnostic differs'
  assert_equal "$(cat "$TF_INSTALLER_STATE/uname.log")" "$(printf '%s\n%s' -s -m)" 'unsupported platform probes differ'
  assert_file_absent "$TF_INSTALLER_STATE/curl.log" 'unsupported platform attempted a download'
  [ ! -d "$destination" ] || fail 'unsupported platform created the destination directory'
)

case_missing_manifest_row() (
  setup_case missing-manifest-row
  version=v4.5.6
  target=aarch64-apple-darwin
  destination=$CASE_DIR/existing-bin
  select_artifact "$version" "$target"
  TF_INSTALLER_CHECKSUM_MODE=missing
  export TF_INSTALLER_CHECKSUM_MODE
  mkdir -p "$destination"
  printf 'preexisting destination: missing row\n' >"$destination/trueflow"
  chmod 0711 "$destination/trueflow"
  cp "$destination/trueflow" "$CASE_DIR/original-destination"
  expected_urls=$CASE_DIR/expected.urls
  write_expected_urls "$expected_urls"

  if run_installer --version "$version" --to "$destination" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"; then
    fail 'manifest without the selected archive row unexpectedly succeeded'
  fi
  expected_error="error: no checksum entry found for $TF_INSTALLER_ARCHIVE_NAME"
  assert_equal "$(cat "$CASE_DIR/stderr")" "$expected_error" 'missing manifest row diagnostic differs'
  assert_files_equal "$expected_urls" "$TF_INSTALLER_STATE/curl.log" 'missing-row case archive and manifest URLs were not exact'
  assert_files_equal "$CASE_DIR/original-destination" "$destination/trueflow" 'missing manifest row replaced the preexisting destination'
  assert_mode "$destination/trueflow" 711 'missing manifest row changed the preexisting destination mode'
  assert_file_absent "$TF_INSTALLER_STATE/shasum.log" 'missing manifest row computed an archive checksum'
  assert_file_absent "$TF_INSTALLER_STATE/tar.log" 'missing manifest row extracted the archive'
  assert_file_absent "$TF_INSTALLER_STATE/install.log" 'missing manifest row invoked install'
)

case_checksum_mismatch() (
  setup_case checksum-mismatch
  TF_INSTALLER_UNAME_S=Linux
  TF_INSTALLER_UNAME_M=x86_64
  export TF_INSTALLER_UNAME_S TF_INSTALLER_UNAME_M
  version=v7.6.5
  target=x86_64-unknown-linux-musl
  destination=$CASE_DIR/existing-bin
  select_artifact "$version" "$target"
  TF_INSTALLER_CHECKSUM_MODE=mismatch
  export TF_INSTALLER_CHECKSUM_MODE
  mkdir -p "$destination"
  printf 'preexisting destination: checksum mismatch\n' >"$destination/trueflow"
  chmod 0711 "$destination/trueflow"
  cp "$destination/trueflow" "$CASE_DIR/original-destination"
  expected_urls=$CASE_DIR/expected.urls
  write_expected_urls "$expected_urls"

  if run_installer --to "$destination" --version "$version" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"; then
    fail 'checksum mismatch unexpectedly succeeded'
  fi
  expected_error="error: checksum mismatch for $TF_INSTALLER_ARCHIVE_NAME"
  assert_equal "$(cat "$CASE_DIR/stderr")" "$expected_error" 'checksum mismatch diagnostic differs'
  assert_files_equal "$expected_urls" "$TF_INSTALLER_STATE/curl.log" 'mismatch case archive and manifest URLs were not exact'
  assert_files_equal "$CASE_DIR/original-destination" "$destination/trueflow" 'checksum mismatch replaced the preexisting destination'
  assert_mode "$destination/trueflow" 711 'checksum mismatch changed the preexisting destination mode'
  [ -f "$TF_INSTALLER_STATE/shasum.log" ] || fail 'checksum mismatch did not compute the archive checksum'
  assert_file_absent "$TF_INSTALLER_STATE/tar.log" 'checksum mismatch extracted the archive'
  assert_file_absent "$TF_INSTALLER_STATE/install.log" 'checksum mismatch invoked install'
)

run_case() {
  label=$1
  function_name=$2
  TOTAL=$((TOTAL + 1))
  printf 'case %d - %s: ' "$TOTAL" "$label"
  if "$function_name"; then
    printf 'PASS\n'
  else
    FAILED=$((FAILED + 1))
    printf 'FAIL\n'
  fi
}

run_case 'Darwin/arm64 selects, verifies, and installs the requested artifact' case_darwin_arm64
run_case 'Linux/x86_64 selects, verifies, and installs the requested artifact' case_linux_x86_64
run_case 'unsupported platforms fail before download or destination creation' case_unsupported_platform
run_case 'missing manifest rows preserve a preexisting destination' case_missing_manifest_row
run_case 'checksum mismatches preserve a preexisting destination' case_checksum_mismatch

PASSED=$((TOTAL - FAILED))
printf 'summary: %d cases, %d passed, %d failed\n' "$TOTAL" "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
