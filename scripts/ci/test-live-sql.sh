#!/usr/bin/env bash
# Check that the live SQL gate rejects an unwired service matrix.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
output_dir="$(mktemp -d "${TMPDIR:-/tmp}/mux-live-sql-test.XXXXXX")"
trap 'rm -rf -- "$output_dir"' EXIT

set +e
MUX_TEST_POSTGRES_URL='' MUX_TEST_MYSQL_URL='' MUX_TEST_SQLSERVER_URL='' \
    "$script_dir/run-live-sql.sh" >"$output_dir/stdout" 2>"$output_dir/stderr"
status=$?
set -e

if [[ "$status" -ne 2 ]]; then
    echo "live SQL gate returned $status without service URLs" >&2
    cat "$output_dir/stderr" >&2
    exit 1
fi

grep -Fq 'MUX_TEST_POSTGRES_URL' "$output_dir/stderr"
grep -Fq 'MUX_TEST_MYSQL_URL' "$output_dir/stderr"
if grep -Fq 'MUX_TEST_SQLSERVER_URL' "$output_dir/stderr"; then
    echo "live SQL gate treated optional SQL Server URL as required" >&2
    exit 1
fi
if grep -Eq 'postgres://|mysql://' "$output_dir/stderr"; then
    echo "live SQL gate printed a database URL" >&2
    exit 1
fi

echo "live SQL gate test passed"
