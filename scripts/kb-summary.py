#!/usr/bin/env python3
"""Summarise scripts/kb-measure.sh results: medians and means per group and arm.

Usage: kb-summary.py results.csv

`fresh` = input + output + cache-write tokens (what a run newly pays for).
`cache_rd` = cache-read tokens (replayed context). Medians are the headline;
means are shown because a single runaway session is itself a finding.
"""
import csv
import statistics as st
import sys
from collections import defaultdict

COLS = ["requests", "tool_calls", "fresh_tokens", "cache_read", "peak_context",
        "gated_cards", "fallback_cards"]
HEAD = ["req", "tools", "fresh", "cache_rd", "peak_ctx", "gated", "fallb"]


def table(rows, keys, title):
    groups = defaultdict(lambda: defaultdict(list))
    for r in rows:
        groups[tuple(r[k] for k in keys)][r["arm"]].append(r)
    print(f"== {title} ==")
    header = keys + ["arm", "runs"] + [f"med_{h}" for h in HEAD[:5]] + ["mean_fresh", "gated", "fallb"]
    print("  ".join(f"{h:>10}" for h in header))
    for key in sorted(groups):
        for arm in ("on", "off"):
            rs = groups[key].get(arm, [])
            if not rs:
                continue
            med = [st.median(int(float(r[c])) for r in rs) for c in COLS[:5]]
            mean_fresh = st.mean(int(float(r["fresh_tokens"])) for r in rs)
            gated = st.mean(int(r["gated_cards"]) for r in rs)
            fb = st.mean(int(r["fallback_cards"]) for r in rs)
            cells = list(key) + [arm, str(len(rs))] + [f"{m:.0f}" for m in med] + [
                f"{mean_fresh:.0f}", f"{gated:.1f}", f"{fb:.1f}"]
            print("  ".join(f"{c:>10}" for c in cells))
        on = groups[key].get("on", [])
        off = groups[key].get("off", [])
        if on and off:
            mo = st.median(int(float(r["fresh_tokens"])) for r in on)
            mf = st.median(int(float(r["fresh_tokens"])) for r in off)
            lower = sum(1 for r in on if int(float(r["fresh_tokens"])) < mf)
            print(f"{'':>10}  median fresh change on vs off: {100 * (mo - mf) / mf:+.1f}%"
                  f"   on-runs below the off median: {lower}/{len(on)}")
    print()


def main():
    rows = list(csv.DictReader(open(sys.argv[1])))
    if not rows:
        sys.exit("no rows")
    table(rows, ["group"], "per group")
    table(rows, ["prompt_id", "group"], "per prompt")
    print("fresh = input + output + cache-write tokens; cache_rd = cache reads (not in fresh).")
    print("Runs are few: read a per-prompt difference as a direction, not a measurement.")


if __name__ == "__main__":
    main()
