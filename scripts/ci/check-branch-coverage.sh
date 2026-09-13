#!/usr/bin/env bash
# Enforce the runtime's branch-coverage floor from a cargo-llvm-cov LCOV file.
# The floor is intentionally a whole percentage: coverage is a ratchet against
# regressions, not a claim that every branch is exercised by unit tests.
set -euo pipefail

coverage_file="${1:-lcov.info}"
minimum_percent="${2:-44}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

if [[ ! -r "$coverage_file" ]]; then
    echo "coverage report is not readable: $coverage_file" >&2
    exit 1
fi

if [[ ! "$minimum_percent" =~ ^[0-9]+$ || "$minimum_percent" -gt 100 ]]; then
    echo "minimum coverage must be an integer from 0 to 100: $minimum_percent" >&2
    exit 1
fi

if ! metrics="$(awk -f "$script_dir/parse-lcov-metrics.awk" "$coverage_file")"; then
    echo "coverage report is malformed: $coverage_file" >&2
    exit 1
fi
read -r _line_found _line_hit branch_found branch_hit <<< "$metrics"

if (( branch_found == 0 )); then
    echo "coverage report contains no branch records: $coverage_file" >&2
    exit 1
fi

if (( branch_hit * 100 < branch_found * minimum_percent )); then
    printf 'branch coverage %d/%d (%.2f%%) is below the %d%% floor\n' \
        "$branch_hit" "$branch_found" \
        "$(awk -v hit="$branch_hit" -v found="$branch_found" 'BEGIN { printf "%.2f", 100 * hit / found }')" \
        "$minimum_percent" >&2
    exit 1
fi

printf 'branch coverage %d/%d (%.2f%%), minimum %d%%\n' \
    "$branch_hit" "$branch_found" \
    "$(awk -v hit="$branch_hit" -v found="$branch_found" 'BEGIN { printf "%.2f", 100 * hit / found }')" \
    "$minimum_percent"
