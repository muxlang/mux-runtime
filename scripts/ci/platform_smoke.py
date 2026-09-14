#!/usr/bin/env python3
"""Run the native-host runtime smoke commands without a shell."""

from __future__ import annotations

import argparse
import os
import shutil
import signal
import subprocess
import sys
from collections.abc import Sequence


Command = Sequence[str]


def process_group_exists(process_group_id: int) -> bool:
    """Return whether a POSIX process group still has members."""
    try:
        os.killpg(process_group_id, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def command_plan(cargo: str) -> tuple[tuple[str, ...], ...]:
    """Return the checks that every native-host runner must execute."""
    return (
        (cargo, "check", "--locked", "--all-features", "--workspace", "--all-targets"),
        (cargo, "test", "--locked", "--all-features", "--workspace"),
    )


def terminate_process_tree(process: subprocess.Popen[bytes]) -> None:
    """Stop a timed-out command and descendants on the current host."""
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return

    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return

    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass

    if process_group_exists(process.pid):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run_command(command: Command, timeout_seconds: int) -> int:
    """Run one command with a process-tree timeout and inherited output."""
    kwargs: dict[str, object] = {}
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True

    print("running:", " ".join(command), flush=True)
    process = subprocess.Popen(command, shell=False, **kwargs)
    try:
        return process.wait(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        print(f"command timed out after {timeout_seconds}s", file=sys.stderr, flush=True)
        terminate_process_tree(process)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        return 124


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the runtime checks used by the native-host smoke matrix."
    )
    parser.add_argument(
        "--timeout-seconds",
        type=int,
        default=int(os.environ.get("MUX_PLATFORM_SMOKE_TIMEOUT_SECS", "2700")),
        help="maximum time for each cargo command (default: 2700)",
    )
    args = parser.parse_args(argv)
    if args.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be positive")

    cargo = shutil.which("cargo")
    if cargo is None:
        parser.error("cargo executable not found on PATH")
    args.cargo = cargo
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    for command in command_plan(args.cargo):
        status = run_command(command, args.timeout_seconds)
        if status != 0:
            print(f"command exited with status {status}", file=sys.stderr)
            return status
    print("platform smoke checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
