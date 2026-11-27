#!/usr/bin/env python3 -I
"""Tables for docs/estimator-experiments.md "P8 results" from the QUAL lines of the diagnose_baseline
and diagnose_incremental harnesses (src/estimator/diagnose/baseline.rs, incremental.rs).

usage: p8_tables.py {rows,summary,csv} OUTPUT_FILE... [--min-paths N] [--max-paths N]

Methods (cfg): flat, prod (default Estimator, per-update evaluation limit), rebuild (E9 cascade +
E9c guarded polish), and optionally B* (a cfg starting with "lv=": raw) and its guarded exact
refit (cfg ending in "+x"). Only the row with the largest n per (world, cfg) is used.
Metric directions: recall higher, spurious/K-error/ratio/field error lower. Continuous metrics
within 1% count as equal.
"""
import csv
import math
import sys
from collections import defaultdict

METHODS = [("flat", "flat"), ("prod", "prod"), ("rebuild", "cascade"), ("bstar", "B*"), ("bstarx", "B*+x")]


def method_of(cfg):
    if cfg in ("flat", "prod", "rebuild"):
        return cfg
    if cfg.startswith("lv="):
        return "bstarx" if cfg.endswith("+x") else "bstar"
    return None


def load(files):
    worlds = defaultdict(dict)
    for f in files:
        for line in open(f, errors="replace"):
            at = line.find("QUAL ")
            if at < 0:
                continue
            d = {}
            for tok in line[at + 5 :].split():
                if "=" in tok:
                    k, v = tok.split("=", 1)
                    d[k] = v
            m = method_of(d.get("cfg", ""))
            if m is None or not d.get("world", "").startswith("live_"):
                continue
            n = int(d["n"])
            old = worlds[d["world"]].get(m)
            if old is None or n >= int(old["n"]):
                worlds[d["world"]][m] = d
    return worlds


def f(d, k):
    try:
        return float(d[k])
    except (KeyError, ValueError):
        return float("nan")


def metrics(d):
    """Metric name -> value for one method row (field errors relative to the flat map)."""
    def rel(a, b):
        return f(d, a) / f(d, b) if f(d, b) > 0 else float("nan")
    return {
        "rec2": f(d, "rec2"), "rec15": f(d, "rec15"), "spur": f(d, "spur"), "k": f(d, "k"),
        "ratio": f(d, "ratio"), "fe": rel("fe", "fe0"), "fu": rel("fu", "fu0"), "fw": rel("fw", "fw0"),
    }


# metric -> (label, higher is better)
METRICS = [
    ("rec2", "recall(2)", True), ("rec15", "recall(15)", True), ("spur", "spurious", False),
    ("ratio", "chi2 ratio", False), ("fe", "field explored/flat", False),
    ("fu", "field unexplored/flat", False), ("fw", "field whole/flat", False),
]
DISCRETE = {"rec2", "rec15", "spur"}


def compare(metric, a, b, higher):
    """'better', 'equal' or 'worse' for value a against b; None if undefined."""
    if math.isnan(a) or math.isnan(b):
        return None
    if metric in DISCRETE:
        if a == b:
            return "equal"
    elif abs(a - b) <= 0.01 * max(abs(a), abs(b)):
        return "equal"
    return "better" if (a > b) == higher else "worse"


def fmt(v, k):
    if math.isnan(v):
        return "n/a"
    if k in ("rec2", "rec15", "spur", "k"):
        return f"{v:.0f}"
    return f"{v:.2e}" if (abs(v) >= 1000 or abs(v) < 0.01) else f"{v:.3g}"


def rows(worlds):
    names = {m: n for m, n in METHODS}
    print("| capture | paths | method | K | rec2 | rec15 | spur | chi2 ratio | field expl/flat | unexpl/flat | whole/flat | evals | wall s |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for w in sorted(worlds, key=lambda s: int(s.split("_")[1])):
        for m, _ in METHODS:
            d = worlds[w].get(m)
            if d is None:
                continue
            x = metrics(d)
            kt = d.get("kt", "?")
            print(f"| {w.split('_')[1]} | {d['n']} | {names[m]} | {fmt(x['k'],'k')} | {fmt(x['rec2'],'rec2')}/{kt} | {fmt(x['rec15'],'rec15')}/{kt} | {fmt(x['spur'],'spur')} | {fmt(x['ratio'],'ratio')} | {fmt(x['fe'],'fe')} | {fmt(x['fu'],'fu')} | {fmt(x['fw'],'fw')} | {d.get('evals','')} | {d.get('wall','')} |")


def summary(worlds):
    pairs = [("prod", "flat"), ("rebuild", "flat"), ("rebuild", "prod"), ("bstarx", "flat"), ("bstarx", "rebuild")]
    label = dict(METHODS)
    print("Counts over captures (better / equal / worse of the first method against the second; n = captures where both are defined).\n")
    print("| comparison | " + " | ".join(l for _, l, _ in METRICS) + " |")
    print("|---|" + "---|" * len(METRICS))
    notbetter = defaultdict(list)
    for a, b in pairs:
        cells = []
        for key, _, higher in METRICS:
            c = defaultdict(int)
            for w, ms in worlds.items():
                if a in ms and b in ms:
                    r = compare(key, metrics(ms[a])[key], metrics(ms[b])[key], higher)
                    if r:
                        c[r] += 1
                        if r == "worse":
                            notbetter[(a, b, key)].append(w.split("_")[1])
            n = sum(c.values())
            cells.append(f"{c['better']} / {c['equal']} / {c['worse']} (n={n})" if n else "n/a")
        print(f"| {label[a]} vs {label[b]} | " + " | ".join(cells) + " |")
    print("\nCaptures where the first method is worse on at least one of recall(2), recall(15), chi2 ratio, field explored, unexplored, whole (spurious planets excluded: flat has none by construction):\n")
    core = [m for m in METRICS if m[0] != "spur"]
    for a, b in pairs:
        bad, both = [], 0
        for w, ms in worlds.items():
            if a in ms and b in ms:
                both += 1
                if any(compare(k, metrics(ms[a])[k], metrics(ms[b])[k], h) == "worse" for k, _, h in core):
                    bad.append(w.split("_")[1])
        print(f"- {label[a]} vs {label[b]}: worse somewhere in {len(bad)} of {both} captures ({', '.join(sorted(bad, key=int))})")
    print("\nCaptures where the first method is worse, by metric:\n")
    for (a, b, key), caps in sorted(notbetter.items()):
        print(f"- {label[a]} vs {label[b]}, {key}: {', '.join(sorted(caps, key=int))}")


def write_csv(worlds):
    out = csv.writer(sys.stdout)
    out.writerow(["capture", "method", "paths", "kt", "k", "rec2", "rec15", "spur", "ratio", "fe", "fu", "fw", "fe0", "fu0", "fw0", "evals", "wall"])
    for w in sorted(worlds, key=lambda s: int(s.split("_")[1])):
        for m, _ in METHODS:
            d = worlds[w].get(m)
            if d:
                out.writerow([w.split("_")[1], m, d["n"], d.get("kt"), d["k"], d["rec2"], d["rec15"], d["spur"], d["ratio"], d["fe"], d["fu"], d["fw"], d["fe0"], d["fu0"], d["fw0"], d.get("evals", ""), d.get("wall", "")])


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] not in ("rows", "summary", "csv"):
        sys.exit(__doc__)
    args = sys.argv[2:]
    lo, hi = 0, 10**9
    for flag in ("--min-paths", "--max-paths"):
        if flag in args:
            i = args.index(flag)
            v = int(args[i + 1])
            lo, hi = (v, hi) if flag == "--min-paths" else (lo, v)
            del args[i : i + 2]
    w = {k: v for k, v in load(args).items() if lo <= int(next(iter(v.values()))["n"]) <= hi}
    {"rows": rows, "summary": summary, "csv": write_csv}[sys.argv[1]](w)
