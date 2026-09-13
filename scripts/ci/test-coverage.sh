#!/usr/bin/env bash
# Exercise the LCOV parser, floors, and summary used by the coverage job.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
test_dir="$(mktemp -d "${TMPDIR:-/tmp}/mux-coverage-test.XXXXXX")"
cleanup() {
    rm -rf -- "$test_dir"
}
trap cleanup EXIT

valid="$test_dir/valid.info"
cat > "$valid" <<'LCOV'
TN:
SF:src/lib.rs
LF:10
LH:10
BRF:2
BRH:2
end_of_record
LCOV

"$script_dir/check-branch-coverage.sh" "$valid" 50 >/dev/null
"$script_dir/check-coverage.sh" "$valid" 50 50 >/dev/null
"$script_dir/summarize-coverage.sh" "$valid" "$test_dir/summary.md" >/dev/null
grep -Fq '| Lines | 10 | 10 | 100.00% |' "$test_dir/summary.md"
grep -Fq '| Branches | 2 | 2 | 100.00% |' "$test_dir/summary.md"

assert_rejected() {
    local name="$1"
    local report="$2"
    if "$script_dir/check-branch-coverage.sh" "$report" 50 >/dev/null 2>&1; then
        echo "coverage check accepted $name" >&2
        return 1
    fi
    if "$script_dir/check-coverage.sh" "$report" 50 50 >/dev/null 2>&1; then
        echo "line and branch coverage check accepted $name" >&2
        return 1
    fi
    if "$script_dir/summarize-coverage.sh" "$report" "$test_dir/summary.md" >/dev/null 2>&1; then
        echo "coverage summary accepted $name" >&2
        return 1
    fi
}

assert_floor_rejected() {
    local name="$1"
    local report="$2"
    if "$script_dir/check-coverage.sh" "$report" 50 50 >/dev/null 2>&1; then
        echo "coverage floor accepted $name" >&2
        return 1
    fi
}

low_lines="$test_dir/low-lines.info"
cat > "$low_lines" <<'LCOV'
TN:
SF:src/lib.rs
LF:10
LH:4
BRF:2
BRH:2
end_of_record
LCOV
assert_floor_rejected "line coverage below the floor" "$low_lines"

low_branches="$test_dir/low-branches.info"
cat > "$low_branches" <<'LCOV'
TN:
SF:src/lib.rs
LF:10
LH:10
BRF:4
BRH:1
end_of_record
LCOV
assert_floor_rejected "branch coverage below the floor" "$low_branches"

missing_counter="$test_dir/missing-counter.info"
cat > "$missing_counter" <<'LCOV'
TN:
SF:src/lib.rs
LF:10
BRF:2
BRH:2
end_of_record
LCOV
assert_rejected "a record without LH" "$missing_counter"

invalid_counter="$test_dir/invalid-counter.info"
cat > "$invalid_counter" <<'LCOV'
TN:
SF:src/lib.rs
LF:10
LH:not-a-count
BRF:2
BRH:2
end_of_record
LCOV
assert_rejected "a non-numeric counter" "$invalid_counter"

incomplete_record="$test_dir/incomplete-record.info"
cat > "$incomplete_record" <<'LCOV'
TN:
SF:src/first.rs
LF:10
LH:10
BRF:2
BRH:2
end_of_record
SF:src/second.rs
LF:10
BRF:2
BRH:2
end_of_record
LCOV
assert_rejected "an incomplete later record" "$incomplete_record"

echo "coverage script tests passed"
