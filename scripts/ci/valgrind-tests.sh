#!/usr/bin/env bash
# Run every workspace test binary under Valgrind, failing on definite/indirect
# leaks or memory errors.
#
# OUTPUT FORMAT IS LOAD-BEARING. pr-comment.yml parses this log to build the PR
# report's pie chart: it counts lines matching '^>>> valgrind ' as the total and
# '^::error::Valgrind reported' as the failures. Changing either marker silently
# changes that chart, so keep them in step with emit_valgrind_pie there.
#
# Writes valgrind.log alongside stdout, which is the artifact that workflow
# uploads.
set -euo pipefail

exec > >(tee valgrind.log) 2>&1

# The commas below are part of the flag values, not array separators.
# shellcheck disable=SC2054
flags=(
  --quiet
  --error-exitcode=99
  --leak-check=full
  --errors-for-leak-kinds=definite,indirect
  --show-leak-kinds=definite,indirect
)

json="$(cargo test --no-run --all-features --workspace --message-format=json)"
mapfile -t bins < <(printf '%s\n' "$json" \
  | jq -r 'select(.executable != null and .profile.test == true) | .executable')

if [[ "${#bins[@]}" -eq 0 ]]; then
  echo "No test binaries were produced" >&2
  exit 1
fi

fail=0
for bin in "${bins[@]}"; do
  echo ">>> valgrind $bin"
  if ! valgrind "${flags[@]}" "$bin" --test-threads=1; then
    echo "::error::Valgrind reported definite/indirect leaks or memory errors in $bin"
    fail=1
  fi
done

exit "$fail"
