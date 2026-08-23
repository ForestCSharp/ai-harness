#!/usr/bin/env python3
"""Summarise a directory of run records against the pre-registered metrics.

    python3 bench/metrics.py bench/baselines/before
    python3 bench/metrics.py bench/baselines/before bench/baselines/after

With two directories it prints the comparison. The metrics are fixed in this
file rather than chosen at the call site, so a later run cannot quietly become a
different experiment by picking a kinder number.

The three that decide anything:

  lookup_ratio     (reads + searches + fetches) / iterations
                   PRIMARY. Exactly what a batch element changes: it is the
                   share of round-trips spent gathering rather than doing.

  wrote            whether the run made any edit at all
                   SECONDARY. The ripgrep instance burned its whole budget
                   without writing once, which is the failure being targeted.

  protocol_errors  total malformed replies
                   GUARD. A tenth reply element is a bet that round-trips saved
                   outweigh new ways to get the protocol wrong. If this rises,
                   the win is smaller than it looks and the result says so.

Note on the secondary metric: the plan called for *wall-clock* to first write.
That is not derivable — the run record carries total `wall_ms` and action counts
but no per-action timestamps, and transcript entries are not timestamped either.
Rather than add timestamps to serve a benchmark, this reports whether a write
happened at all, which is the distinction that actually mattered in the run that
prompted this.
"""

from __future__ import annotations

import json
import statistics
import sys
from pathlib import Path
from typing import Any


def load(directory: Path) -> list[tuple[str, dict[str, Any]]]:
    records = []
    for path in sorted(directory.glob("*.json")):
        try:
            records.append((path.stem, json.loads(path.read_text())))
        except json.JSONDecodeError:
            print(f"  ! {path.name}: unreadable, skipped", file=sys.stderr)
    return records


def metrics(record: dict[str, Any]) -> dict[str, Any]:
    actions = record.get("actions", {})
    iterations = record.get("iterations", 0) or 0
    lookups = (
        actions.get("reads", 0)
        + actions.get("searches", 0)
        + actions.get("fetches", 0)
    )
    ledger = record.get("ledger", {})
    prompt_tokens = ledger.get("prompt_tokens", 0) or 0
    return {
        "exit": record.get("exit"),
        "iterations": iterations,
        "lookups": lookups,
        "lookup_ratio": lookups / iterations if iterations else 0.0,
        "wrote": actions.get("writes", 0) > 0,
        "writes": actions.get("writes", 0),
        "shells": actions.get("shells", 0),
        "protocol_errors": record.get("protocol_errors", {}).get("total", 0),
        "wall_s": round((record.get("wall_ms") or 0) / 1000, 1),
        "cache_hit_rate": (
            ledger.get("cached_tokens", 0) / prompt_tokens if prompt_tokens else 0.0
        ),
        "requests": ledger.get("requests", 0),
        "s_per_request": round(
            (ledger.get("waiting_ms") or 0) / 1000 / max(ledger.get("requests", 1), 1), 1
        ),
    }


def summarise(label: str, directory: Path) -> dict[str, Any] | None:
    records = load(directory)
    if not records:
        print(f"{label}: no records in {directory}")
        return None

    rows = [(name, metrics(rec)) for name, rec in records]
    print(f"\n=== {label}  ({len(rows)} run(s) from {directory}) ===")
    print(
        f"{'instance':<28} {'exit':<9} {'iters':>5} {'looks':>6} "
        f"{'ratio':>6} {'wrote':>6} {'perr':>5} {'wall_s':>7} {'s/req':>6}"
    )
    for name, m in rows:
        print(
            f"{name:<28} {str(m['exit']):<9} {m['iterations']:>5} {m['lookups']:>6} "
            f"{m['lookup_ratio']:>6.2f} {str(m['wrote']):>6} "
            f"{m['protocol_errors']:>5} {m['wall_s']:>7.0f} {m['s_per_request']:>6.1f}"
        )

    ratios = [m["lookup_ratio"] for _, m in rows]
    aggregate = {
        "runs": len(rows),
        "lookup_ratio_mean": statistics.mean(ratios),
        "lookup_ratio_min": min(ratios),
        "lookup_ratio_max": max(ratios),
        "wrote_count": sum(1 for _, m in rows if m["wrote"]),
        "protocol_errors_total": sum(m["protocol_errors"] for _, m in rows),
        "iterations_mean": statistics.mean(m["iterations"] for _, m in rows),
    }
    print(
        f"\n  PRIMARY  lookup_ratio    mean {aggregate['lookup_ratio_mean']:.2f}"
        f"  (min {aggregate['lookup_ratio_min']:.2f}, max {aggregate['lookup_ratio_max']:.2f})"
    )
    print(
        f"  SECONDARY wrote at all    {aggregate['wrote_count']}/{aggregate['runs']} runs"
    )
    print(f"  GUARD    protocol_errors {aggregate['protocol_errors_total']} total")

    # The gate this whole phase exists to answer.
    gate = aggregate["lookup_ratio_min"] >= 0.5
    print(
        f"\n  GATE (lookups >= 50% of iterations in every run): "
        f"{'PASS — batching is worth building' if gate else 'FAIL — the first run was an outlier'}"
    )
    return aggregate


def compare(before: dict[str, Any], after: dict[str, Any]) -> None:
    print("\n=== before → after ===")
    d_ratio = after["lookup_ratio_mean"] - before["lookup_ratio_mean"]
    d_iters = after["iterations_mean"] - before["iterations_mean"]
    d_perr = after["protocol_errors_total"] - before["protocol_errors_total"]
    print(
        f"  lookup_ratio    {before['lookup_ratio_mean']:.2f} → "
        f"{after['lookup_ratio_mean']:.2f}  ({d_ratio:+.2f})"
    )
    print(
        f"  iterations mean {before['iterations_mean']:.1f} → "
        f"{after['iterations_mean']:.1f}  ({d_iters:+.1f})"
    )
    print(f"  wrote at all    {before['wrote_count']} → {after['wrote_count']}")
    print(f"  protocol_errors {before['protocol_errors_total']} → "
          f"{after['protocol_errors_total']}  ({d_perr:+d})")
    if d_perr > 0:
        print(
            "\n  NOTE: protocol errors rose. Report that alongside the round-trip\n"
            "  win rather than under it — the guard exists precisely so a new\n"
            "  element cannot pay for itself in violations without being seen to."
        )


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    before = summarise("before" if len(args) > 1 else Path(args[0]).name, Path(args[0]))
    if len(args) > 1:
        after = summarise("after", Path(args[1]))
        if before and after:
            compare(before, after)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
