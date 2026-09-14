# CI review fixes

## Causes and fixes

- Curl prints protocol support on its `Features:` line. The workflow and the
  acceptance runner now search that line for `HTTP2` and `HTTP3` instead of
  inspecting only line 1.
- The acceptance runner rewrote curl arguments by array position. Those
  indexes did not identify stable option slots, so curl could receive an
  option name where it expected a value. The runner now creates the complete
  argument array with the temporary body path in place. Its write-out format
  also ends with a newline, so Bash `read` does not treat a valid final record
  as an error under `set -e`.
- POSIX timeout cleanup waited for the command leader only. If the leader
  exited after `SIGTERM`, an ignoring child could keep the process group alive.
  Cleanup now checks the group after the grace period and sends `SIGKILL` to
  any remaining members.

## Changed paths

- `.github/workflows/http-acceptance.yml`
- `scripts/ci/run-http-acceptance.sh`
- `scripts/ci/test-http-acceptance.sh`
- `scripts/ci/platform_smoke.py`
- `scripts/ci/test_platform_smoke.py`

The assigned `.github/workflows/platform-smoke.yml` was reviewed and needed no
edit for this fix.

## Verification

- `bash -n scripts/ci/run-http-acceptance.sh scripts/ci/test-http-acceptance.sh`
- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts/ci/test_platform_smoke.py`
  passed 4 tests, including the bounded descendant cleanup regression.
- `PYTHONDONTWRITEBYTECODE=1 scripts/ci/test-http-acceptance.sh` passed. The
  fake curl test covered all three protocol flags, response checks, argument
  pairing, and bearer headers.
- No Python bytecode or `target/` directory was created by these checks.

## Remaining issues

- Coupled runtime/compiler/example pull requests use the `paired-compiler:<branch>`
  and `paired-examples:<branch>` labels so downstream smoke tests exercise the
  matching revisions instead of unrelated repository defaults.
- Hosted workflow execution remains a CI responsibility; local helper tests,
  runtime tests, and Clippy pass.
- Pre-existing dirty and untracked changes outside the assigned CI paths were
  left untouched.
