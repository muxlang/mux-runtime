#!/usr/bin/env bash
# Enforce line and branch floors from an LCOV report.
set -euo pipefail

coverage_file="${1:-lcov.info}"
minimum_line="${2:-50}"
minimum_branch="${3:-50}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

for value in "$minimum_line" "$minimum_branch"; do
    if [[ ! "$value" =~ ^[0-9]+$ || "$value" -gt 100 ]]; then
        echo "coverage floors must be integers from 0 to 100" >&2
        exit 1
    fi
done

if [[ ! -r "$coverage_file" ]]; then
    echo "coverage report is not readable: $coverage_file" >&2
    exit 1
fi

if ! metrics="$(awk -f "$script_dir/parse-lcov-metrics.awk" "$coverage_file")"; then
    echo "coverage report is malformed: $coverage_file" >&2
    exit 1
fi
read -r line_found line_hit branch_found branch_hit <<< "$metrics"

if (( line_found == 0 || branch_found == 0 )); then
    echo "coverage report must contain line and branch records: $coverage_file" >&2
    exit 1
fi

line_percent=$((100 * line_hit / line_found))
branch_percent=$((100 * branch_hit / branch_found))
if (( line_percent < minimum_line )); then
    printf 'line coverage %d/%d (%d%%) is below the %d%% floor\n' \
        "$line_hit" "$line_found" "$line_percent" "$minimum_line" >&2
    exit 1
fi
if (( branch_percent < minimum_branch )); then
    printf 'branch coverage %d/%d (%d%%) is below the %d%% floor\n' \
        "$branch_hit" "$branch_found" "$branch_percent" "$minimum_branch" >&2
    exit 1
fi

printf 'line coverage %d/%d (%d%%), branch coverage %d/%d (%d%%)\n' \
    "$line_hit" "$line_found" "$line_percent" \
    "$branch_hit" "$branch_found" "$branch_percent"
