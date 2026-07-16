#!/usr/bin/env python3
"""Aggregate criterion hot-path benchmark medians into a compact JSON summary.

`cargo bench --bench hot_paths` measures each benchmark individually and writes a
median to `target/criterion/<group>/<bench>/new/estimates.json`. This script
aggregates those per-benchmark medians into one median per group and writes a
small `{"phases": [{"name", "median_ns", "n"}]}` JSON.

It is consumed by the PR-comment workflow, which renders the summary as charts.
It is a reporting tool only - not a CI gate.

    python3 scripts/bench-summary.py [CRITERION_DIR] -o OUTPUT.json

CRITERION_DIR defaults to `<repo>/target/criterion`.
"""

import argparse
import json
import statistics
import sys
from pathlib import Path

# Hot-path benchmark groups defined in benches/hot_paths.rs, in a stable order.
# Any other group found in the criterion output is appended after these.
PREFERRED_ORDER = [
    "refcount",
    "primitive",
    "list",
    "map",
    "set",
    "string",
    "wrappers",
    "json",
]


def collect(criterion_dir: Path) -> dict[str, list[float]]:
    """Map each benchmark group to the list of per-benchmark median times (ns)."""
    groups: dict[str, list[float]] = {}
    for estimates in criterion_dir.glob("*/*/new/estimates.json"):
        group = estimates.parts[-4]
        if group == "report":
            continue
        try:
            data = json.loads(estimates.read_text(encoding="utf-8"))
            median = float(data["median"]["point_estimate"])
        except (OSError, ValueError, KeyError):
            continue
        groups.setdefault(group, []).append(median)
    return groups


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("criterion_dir", nargs="?", type=Path)
    parser.add_argument("-o", "--output", type=Path, required=True)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent

    def confined(candidate: Path, what: str) -> Path:
        # Resolve and require the path stay within the repo, so untrusted CLI
        # arguments cannot read or write outside the project tree.
        resolved = candidate.resolve()
        if resolved != repo_root and repo_root not in resolved.parents:
            print(f"error: {what} {resolved} is outside {repo_root}", file=sys.stderr)
            raise SystemExit(2)
        return resolved

    criterion_dir = confined(
        args.criterion_dir or (repo_root / "target" / "criterion"), "criterion dir"
    )
    if not criterion_dir.is_dir():
        print(f"error: {criterion_dir} not found; run `cargo bench` first", file=sys.stderr)
        return 1

    groups = collect(criterion_dir)
    if not groups:
        print(f"error: no estimates found under {criterion_dir}", file=sys.stderr)
        return 1

    order = [g for g in PREFERRED_ORDER if g in groups]
    order += sorted(g for g in groups if g not in PREFERRED_ORDER)

    summary = {
        "phases": [
            {"name": name, "median_ns": statistics.median(groups[name]), "n": len(groups[name])}
            for name in order
        ]
    }

    output = confined(args.output, "output")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(f"wrote {output}")
    for phase in summary["phases"]:
        print(f"  {phase['name']:<10} n={phase['n']:<3} median={phase['median_ns']:.0f} ns")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
