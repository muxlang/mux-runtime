#!/usr/bin/env bash
# Render the aggregate line and branch coverage in a small Markdown report.
#
# This deliberately consumes LCOV rather than scraping cargo-llvm-cov's human
# output.  LCOV is the artifact the coverage job already gates and uploads, so
# the report and the gate cannot disagree because one of them changed format.
#
# Usage: summarize-coverage.sh [lcov-file] [markdown-file]
set -euo pipefail

coverage_file="${1:-lcov.info}"
output_file="${2:-coverage-summary.md}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

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
if (( line_hit < 0 || line_hit > line_found || branch_hit < 0 || branch_hit > branch_found )); then
    echo "coverage report contains invalid covered/total counts: $coverage_file" >&2
    exit 1
fi

line_percent="$(awk -v hit="$line_hit" -v found="$line_found" 'BEGIN { printf "%.2f", 100 * hit / found }')"
branch_percent="$(awk -v hit="$branch_hit" -v found="$branch_found" 'BEGIN { printf "%.2f", 100 * hit / found }')"

tmp_file="${output_file}.tmp.$$"
trap 'rm -f "$tmp_file"' EXIT
{
    printf '# Runtime coverage\n\n'
    printf '| Metric | Covered | Total | Coverage |\n'
    printf '| --- | ---: | ---: | ---: |\n'
    printf '| Lines | %d | %d | %s%% |\n' "$line_hit" "$line_found" "$line_percent"
    printf '| Branches | %d | %d | %s%% |\n' "$branch_hit" "$branch_found" "$branch_percent"
} > "$tmp_file"
mv "$tmp_file" "$output_file"
trap - EXIT

cat "$output_file"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    cat "$output_file" >> "$GITHUB_STEP_SUMMARY"
fi
