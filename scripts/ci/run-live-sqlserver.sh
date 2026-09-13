#!/usr/bin/env bash
# Run the optional SQL Server driver fixture when a hosted TDS service exists.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

if [[ -z "${MUX_TEST_SQLSERVER_URL:-}" ]]; then
    printf 'SQL Server live fixture requires MUX_TEST_SQLSERVER_URL\n' >&2
    exit 2
fi

cd "$repo_root"
cargo test --locked --all-features --test sql_drivers_unit sqlserver_live_driver -- --test-threads=1
