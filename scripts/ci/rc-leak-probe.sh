#!/usr/bin/env bash
# Verify that the rc-leak-check feature actually detects a leak.
#
# This is a check on the checker. rc-leak-check sits outside mux-runtime's
# `full` feature set, so the runtime the compiler links by default never carries
# the exit-time assertion - which means a broken assertion would look exactly
# like a clean program. The probe runs both halves: a control that must exit 0
# and a deliberate leak that must exit 101.
set -euo pipefail

cargo build --example rc_leak_check_probe --features rc-leak-check

./target/debug/examples/rc_leak_check_probe clean

set +e
./target/debug/examples/rc_leak_check_probe leak
code=$?
set -e

if [[ "$code" -ne 101 ]]; then
  echo "rc-leak-check probe should exit 101 on a leaked block, got ${code}" >&2
  printf '::error::rc-leak-check probe should exit 101 on a leaked block, got %s\n' "$code"
  exit 1
fi

echo "Leaked RC block correctly detected (exit 101)."
