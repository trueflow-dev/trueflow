#!/usr/bin/env bash
set -euo pipefail

readonly MAX_RETRIES=3

collect_until() {
	local limit="$1"
	local current=0
	local values=()

	while [[ "$current" -lt "$limit" ]]; do
		values+=("$current")
		current=$((current + 1))
	done

	printf '%s\n' "${values[@]}"
}

process_values() {
	local factor="$1"
	shift
	local output=()
	local value

	for value in "$@"; do
		output+=("$((value * factor))")
	done

	printf '%s\n' "${output[@]}"
}

describe() {
	local label="$1"
	shift
	"$@"
}

it() {
	local label="$1"
	shift
	"$@"
}

test_collect_until() {
	local values
	values=$(collect_until 2 | tr "\n" " ")
	if [[ "$values" != "0 1 " ]]; then
		echo "collect_until failed" >&2
		exit 1
	fi
}

main() {
	local name="sample"
	local threshold=4

	describe "collector" test_collect_until
	it "collects values" test_collect_until

	mapfile -t values < <(collect_until "$threshold")
	mapfile -t processed < <(process_values 2 "${values[@]}")

	for ((attempt = 0; attempt < MAX_RETRIES; attempt++)); do
		echo "attempt ${attempt}"
	done

	case "${processed[*]}" in
	*"0"*)
		echo "${name}: ${processed[*]}"
		;;
	*)
		echo "${name}: empty"
		;;
	esac
}

main "$@"
