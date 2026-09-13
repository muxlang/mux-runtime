#!/usr/bin/env bash
# Run the provider fixtures and fail when CI forgot a required database URL.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

missing=()
for variable in MUX_TEST_POSTGRES_URL MUX_TEST_MYSQL_URL; do
    if [[ -z "${!variable:-}" ]]; then
        missing+=("$variable")
    fi
done

if (( ${#missing[@]} > 0 )); then
    printf 'live SQL fixture requires: %s\n' "${missing[*]}" >&2
    exit 2
fi

cd "$repo_root"
cargo test --locked --all-features --test sql_drivers_unit -- --test-threads=1

if [[ -n "${MUX_TEST_SQLSERVER_URL:-}" ]]; then
    printf 'SQL Server live fixture was enabled through MUX_TEST_SQLSERVER_URL\n'
else
    printf 'SQL Server live fixture skipped: MUX_TEST_SQLSERVER_URL not set\n'
fi
