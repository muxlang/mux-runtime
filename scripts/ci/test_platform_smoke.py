#!/usr/bin/env python3
"""Unit tests for the platform smoke command builder."""

from __future__ import annotations

import contextlib
import io
import importlib.util
import pathlib
import sys
import tempfile
import time
import unittest


SCRIPT = pathlib.Path(__file__).with_name("platform_smoke.py")
SPEC = importlib.util.spec_from_file_location("platform_smoke", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"could not load {SCRIPT}")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PlatformSmokeTests(unittest.TestCase):
    def test_command_plan_has_compile_and_test_gates(self) -> None:
        self.assertEqual(
            MODULE.command_plan("cargo"),
            (
                (
                    "cargo",
                    "check",
                    "--locked",
                    "--all-features",
                    "--workspace",
                    "--all-targets",
                ),
                ("cargo", "test", "--locked", "--all-features", "--workspace"),
            ),
        )

    def test_timeout_must_be_positive(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                MODULE.parse_args(["--timeout-seconds", "0"])

    def test_timeout_returns_shell_independent_status(self) -> None:
        status = MODULE.run_command(
            (sys.executable, "-c", "import time; time.sleep(10)"), 1
        )
        self.assertEqual(status, 124)

    @unittest.skipUnless(sys.platform != "win32", "POSIX process groups only")
    def test_timeout_kills_child_after_parent_exits_on_term(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            marker = pathlib.Path(directory) / "child-survived"
            child_code = (
                "import pathlib, signal, time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"time.sleep(2); pathlib.Path({str(marker)!r}).touch()"
            )
            parent_code = (
                "import subprocess, sys, time; "
                f"subprocess.Popen([sys.executable, '-c', {child_code!r}]); "
                "time.sleep(10)"
            )

            status = MODULE.run_command((sys.executable, "-c", parent_code), 1)
            self.assertEqual(status, 124)

            deadline = time.monotonic() + 3
            while time.monotonic() < deadline and not marker.exists():
                time.sleep(0.05)
            self.assertFalse(marker.exists(), "timed-out child survived cleanup")


if __name__ == "__main__":
    unittest.main()
