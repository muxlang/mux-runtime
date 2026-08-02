#!/usr/bin/env python3
"""A/B benchmark regression gate comparing the PR head's `cargo bench`
results against a cached baseline, following roc-lang/roc's threshold rule
rather than a blunt ratio:

- Dual threshold: only trips if BOTH the relative change exceeds
  --pct-threshold AND the absolute change exceeds --abs-ns-threshold, so a
  large relative swing on an already-tiny benchmark cannot fire alone.
- Confirmation re-run: a trip re-runs `cargo bench` for that one benchmark
  ID and fails only if the second run also exceeds both thresholds.

Uses criterion's own baseline comparison (`--baseline main`), not
hyperfine: hot_paths.rs benchmarks run in the ns-to-low-us range, well
below hyperfine's process-spawn floor, and criterion is already the tool
this repo has tuned for that granularity. The workflow runs `cargo bench
--bench hot_paths -- --baseline main` before invoking this script; this
script only reads target/criterion/*/*/{new,main}/estimates.json and
applies the thresholds.

No byte-identical-output escape hatch here (deferred - see plan.md step 5
and the PR that added this script): there is no separate "compiled output"
to diff for a Rust microbenchmark the way there is for the compiler side
of this gate, and a disassembly-diff analog would need a hand-maintained
benchmark-to-FFI-symbol map for comparatively low payoff at this
granularity.

--pct-threshold and --abs-ns-threshold are empirically chosen, not a
literal port of Roc's 4%/5ms - and PCT_THRESHOLD_DEFAULT specifically was
revised upward after this gate's first real CI run: GitHub-hosted shared
runners are noisy enough, at nanosecond scale, that 4% + one confirmation
re-run produced 4 false positives out of 30 benchmarks in a single run
with ZERO source changes (up to +10%, surviving the confirmation re-run
too - so the noise was correlated enough within that run for both samples
to agree). mux-runtime's own README already documents that shared runners
are too noisy for a naive wall-clock threshold; this is that same problem
showing up here. mux-compiler's side of this gate did not need the same
adjustment - its benchmarks run whole compiled programs (hundreds of ms),
where the same absolute jitter is a much smaller fraction of the signal.
15% comfortably clears the observed noise with margin while still catching
anything resembling a real regression (a deliberately-injected regression
used to validate this gate before merging showed changes of 400-900%).
Revisit downward once step 10 (Blacksmith) gives this job more consistent
hardware to run on. See PCT_THRESHOLD_DEFAULT / ABS_NS_THRESHOLD_DEFAULT
below for the full reasoning.

Not a CI-only tool: run locally after `cargo bench -- --save-baseline
main` and a second `cargo bench -- --baseline main` to reproduce a
failure exactly.
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

# Raised from Roc's 4% after this gate's first real GitHub Actions run
# produced false positives at up to 10% - see the module docstring above.
PCT_THRESHOLD_DEFAULT = 15.0
# Measured range across every hot_paths benchmark on this repo's main:
# ~0.9ns (primitive/int_add) to ~32us (map/build_free). 2ns keeps the
# absolute half of the dual threshold meaningful at both ends: at the fast
# end (sub-50ns benchmarks) it requires roughly a 10-20%+ relative move
# before firing, which is appropriately conservative given criterion's own
# confidence intervals are typically under 1% wide there; at the slow end
# (hundreds of ns and up) the 15% relative threshold is already well above
# 2ns, so it is the binding constraint, same as intended. Left unchanged by
# the PCT_THRESHOLD_DEFAULT revision - the observed false positives were
# all well above this floor already, so the absolute half was not the gap.
ABS_NS_THRESHOLD_DEFAULT = 2.0


def confined(candidate: Path, repo_root: Path, what: str) -> Path:
    # Resolve and require the path stay within the repo, so untrusted CLI
    # arguments cannot read or write outside the project tree. Same pattern
    # as scripts/bench-summary.py.
    resolved = candidate.resolve()
    if resolved != repo_root and repo_root not in resolved.parents:
        print(f"error: {what} {resolved} is outside {repo_root}", file=sys.stderr)
        raise SystemExit(2)
    return resolved


def discover_benchmarks(criterion_dir: Path) -> list[str]:
    # Each benchmark is a <group>/<name> pair with its own new/ and main/
    # subdirectories; report/ is criterion's HTML output, not a benchmark.
    ids = []
    for new_dir in sorted(criterion_dir.glob("*/*/new")):
        bench_dir = new_dir.parent
        if bench_dir.name == "report" or bench_dir.parent.name == "report":
            continue
        if not (bench_dir / "main").is_dir():
            continue
        ids.append(f"{bench_dir.parent.name}/{bench_dir.name}")
    return ids


def median_ns(bench_dir: Path, baseline_name: str) -> float:
    estimates = json.loads((bench_dir / baseline_name / "estimates.json").read_text(encoding="utf-8"))
    return estimates["median"]["point_estimate"]


def evaluate(bench_dir: Path, pct_threshold: float, abs_ns_threshold: float) -> tuple[bool, float, float, float, float]:
    baseline_ns = median_ns(bench_dir, "main")
    new_ns = median_ns(bench_dir, "new")
    pct_change = (new_ns - baseline_ns) / baseline_ns * 100
    abs_delta_ns = abs(new_ns - baseline_ns)
    tripped = pct_change > pct_threshold and abs_delta_ns > abs_ns_threshold
    return tripped, pct_change, abs_delta_ns, baseline_ns, new_ns


def rerun_benchmark(bench_id: str) -> None:
    # Criterion filters by substring/regex against the full "group/name"
    # ID, so this re-executes only the one tripped benchmark, not the
    # whole ~30-function suite.
    result = subprocess.run(
        ["cargo", "bench", "--bench", "hot_paths", "--", "--baseline", "main", f"^{bench_id}$"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"error: confirmation re-run failed for {bench_id}:\n{result.stderr}", file=sys.stderr)
        raise SystemExit(1)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("criterion_dir", nargs="?", type=Path)
    parser.add_argument("--pct-threshold", type=float, default=PCT_THRESHOLD_DEFAULT)
    parser.add_argument("--abs-ns-threshold", type=float, default=ABS_NS_THRESHOLD_DEFAULT)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent.parent
    criterion_dir = confined(
        args.criterion_dir or (repo_root / "target" / "criterion"), repo_root, "criterion dir"
    )
    if not criterion_dir.is_dir():
        print(f"error: {criterion_dir} not found; run `cargo bench -- --baseline main` first", file=sys.stderr)
        return 1

    bench_ids = discover_benchmarks(criterion_dir)
    if not bench_ids:
        print(f"error: no benchmarks with both new/ and main/ estimates found under {criterion_dir}", file=sys.stderr)
        return 1

    rows = []
    failed = False
    for bench_id in bench_ids:
        bench_dir = criterion_dir / bench_id
        tripped, pct_change, abs_delta_ns, baseline_ns, new_ns = evaluate(
            bench_dir, args.pct_threshold, args.abs_ns_threshold
        )

        verdict = "ok"
        if tripped:
            rerun_benchmark(bench_id)
            confirm_tripped, _, _, _, _ = evaluate(bench_dir, args.pct_threshold, args.abs_ns_threshold)
            if confirm_tripped:
                verdict = "REGRESSION"
                failed = True
            else:
                verdict = "ok (confirmation run did not reproduce)"

        rows.append(
            {
                "bench": bench_id,
                "baseline_ns": baseline_ns,
                "new_ns": new_ns,
                "pct_change": pct_change,
                "abs_delta_ns": abs_delta_ns,
                "verdict": verdict,
            }
        )

    header = f"{'benchmark':<28} {'baseline (ns)':>14} {'new (ns)':>12} {'change':>8} {'abs (ns)':>10}  verdict"
    print(header)
    print("-" * len(header))
    for r in rows:
        print(
            f"{r['bench']:<28} {r['baseline_ns']:>14.2f} {r['new_ns']:>12.2f} "
            f"{r['pct_change']:>+7.1f}% {r['abs_delta_ns']:>10.2f}  {r['verdict']}"
        )

    if failed:
        print("\n::error::One or more benchmarks regressed beyond the dual threshold, confirmed on re-run.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
