#!/usr/bin/env bash
# Enforce the runtime's branch-coverage floor from a cargo-llvm-cov LCOV file.
# The floor is intentionally a whole percentage: coverage is a ratchet against
# regressions, not a claim that every branch is exercised by unit tests.
set -euo pipefail

coverage_file="${1:-lcov.info}"
minimum_percent="${2:-44}"

if [[ ! -r "$coverage_file" ]]; then
    echo "coverage report is not readable: $coverage_file" >&2
    exit 1
fi

if [[ ! "$minimum_percent" =~ ^[0-9]+$ || "$minimum_percent" -gt 100 ]]; then
    echo "minimum coverage must be an integer from 0 to 100: $minimum_percent" >&2
    exit 1
fi

read -r branch_found branch_hit < <(
    awk -F: '
        /^BRF:/ { found += $2 }
        /^BRH:/ { hit += $2 }
        END { printf "%d %d\n", found, hit }
    ' "$coverage_file"
)

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
