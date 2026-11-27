#!/usr/bin/env python3 -I
"""Paired comparison of the default and tuned configurations on the P14 held-out set
(`p14_study.py holdout` output, holdout.jsonl): per world/game, two-sided Wilcoxon signed-rank.
Usage: p14_holdout_report.py holdout.jsonl"""
import collections, json, sys
from scipy.stats import wilcoxon

rows = [json.loads(line) for line in open(sys.argv[1])]
by = collections.defaultdict(dict)
for r in rows:
    by[(r["world"], r["mode"], r["belief"], r["paths"])][r["config"]] = r["metrics"]


def metric(m, path):
    try:
        for key in path:
            m = m[key]
        return m
    except (KeyError, IndexError, TypeError):
        return None


SETS = {
    "synthetic (own beliefs, 60 paths)": lambda k: k[0].startswith("synth"),
    "captures 59-71, recorded beliefs": lambda k: k[0].startswith("capture") and k[2] == "recorded",
    "captures 59-71, refit beliefs": lambda k: k[0].startswith("capture") and k[2] == "refit",
}
METRICS = {
    "true_hits": ("targeting", "true_hits"),
    "wasted": ("targeting", "wasted"),
    "hits_per_100_energy": ("targeting", "hits_per_100_energy"),
    "recall2@last": ("estimator", "checkpoints", -1, "recall2"),
    "spurious@last": ("estimator", "checkpoints", -1, "spurious"),
    "field_explored_rmse@last": ("estimator", "checkpoints", -1, "field_explored_rmse"),
    "field_unexplored_rmse@last": ("estimator", "checkpoints", -1, "field_unexplored_rmse"),
    "chi2_ratio@last": ("estimator", "checkpoints", -1, "chi2_ratio"),
    "estimator_cpu_s": ("estimator", "work", "cpu_s"),
    "slowest_update_ms": ("estimator", "work", "slowest_update_ms"),
}
for name, keep in SETS.items():
    keys = sorted(k for k in by if keep(k) and {"default", "tuned"} <= by[k].keys())
    print(f"\n## {name}: {len(keys)} worlds")
    print("| metric | default (sum or median) | tuned | tuned better / worse / equal | Wilcoxon p |")
    print("|---|---|---|---|---|")
    for mname, path in METRICS.items():
        pairs = [(metric(by[k]["default"], path), metric(by[k]["tuned"], path)) for k in keys]
        pairs = [(a, b) for a, b in pairs if a is not None and b is not None]
        if not pairs:
            continue
        higher_better = mname in ("true_hits", "hits_per_100_energy", "recall2@last")
        better = sum((b > a) if higher_better else (b < a) for a, b in pairs)
        worse = sum((b < a) if higher_better else (b > a) for a, b in pairs)
        diffs = [b - a for a, b in pairs]
        p = wilcoxon(diffs).pvalue if any(diffs) else 1.0
        summed = mname in ("true_hits", "wasted", "spurious@last", "recall2@last", "estimator_cpu_s")
        agg = (lambda v: f"{sum(v):.4g}") if summed else (lambda v: f"{sorted(v)[len(v) // 2]:.4g}")
        print(f"| {mname} | {agg([a for a, _ in pairs])} | {agg([b for _, b in pairs])} | "
              f"{better} / {worse} / {len(pairs) - better - worse} | {p:.3g} |")
