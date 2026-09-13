#!/usr/bin/env bash
# Check that the optional SQL Server fixture fails clearly when unwired.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
output_dir="$(mktemp -d "${TMPDIR:-/tmp}/mux-live-sqlserver-test.XXXXXX")"
trap 'rm -rf -- "$output_dir"' EXIT

set +e
MUX_TEST_SQLSERVER_URL='' "$script_dir/run-live-sqlserver.sh" \
    >"$output_dir/stdout" 2>"$output_dir/stderr"
status=$?
set -e

if [[ "$status" -ne 2 ]]; then
    echo "SQL Server live gate returned $status without a service URL" >&2
    cat "$output_dir/stderr" >&2
    exit 1
fi

grep -Fq 'MUX_TEST_SQLSERVER_URL' "$output_dir/stderr"
if grep -Eq 'sqlserver://|mssql://' "$output_dir/stderr"; then
    echo "SQL Server live gate printed a database URL" >&2
    exit 1
fi

echo "SQL Server live gate test passed"
