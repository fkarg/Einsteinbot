#!/usr/bin/env python3 -I
"""Tables for docs/estimator-experiments.md "P7 results" from the output of diagnose_resumable
(src/estimator/diagnose/resumable.rs). Quality tables use scripts/p6_tables.py (same QUAL lines).

usage: p7_tables.py {latency,timeline} OUTPUT_FILE... [--cfg SUBSTR]
  latency   per (world, cfg): updates, median / p90 / max wall_ms of an update (local refit plus
            the slice, or the whole blocking update), over all updates and over updates after path 8
  timeline  per (world, cfg, period): time to the first correct planet (rec2 >= 1), to 10 and 50
            percent of the final recall(2), recall(2) at fixed times, final, worst publish lag
"""
import statistics as st
import sys
from collections import defaultdict


def parse(files, tags, cfg_sub=""):
    rows = defaultdict(list)
    for f in files:
        for line in open(f, errors="replace"):
            for marker in tags:
                at = line.find(marker + " ")
                if at >= 0 and "world=" in line[at:]:
                    line = line[at:]
                    break
            tag, _, rest = line.partition(" ")
            if tag not in tags:
                continue
            d = dict(tok.split("=", 1) for tok in rest.split() if "=" in tok)
            if cfg_sub and cfg_sub not in d.get("cfg", ""):
                continue
            rows[tag].append(d)
    return rows


def num(d, k):
    try:
        return float(d[k])
    except (KeyError, ValueError):
        return float("nan")


def pct(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


def latency(rows):
    g = defaultdict(list)
    for d in rows["UPD"]:
        g[(d["world"], d["cfg"])].append(d)
    print("world | cfg | threads | updates | median / p90 / max ms | updates after path 8: median / p90 / max ms | median K late")
    for (w, c), v in sorted(g.items()):
        ms = [num(x, "wall_ms") for x in v]
        late = [num(x, "wall_ms") for x in v if int(x["n"]) > 8]
        kl = [num(x, "k") for x in v if int(x["n"]) > 8]
        s = lambda m: f"{st.median(m):.0f} / {pct(m, .9):.0f} / {max(m):.0f}" if m else "-"
        print(f"{w} | {c} | {v[0]['threads']} | {len(v)} | {s(ms)} | {s(late)} | {st.median(kl) if kl else float('nan'):.0f}")


def timeline(rows):
    g = defaultdict(list)
    for d in rows["TL"]:
        g[(d["world"], d["cfg"], d["period_ms"])].append(d)
    lag = {(d["world"], d["cfg"], d["period_ms"]): d["worst_publish_lag_ms"] for d in rows["TLEND"]}
    marks = [5, 10, 20, 30, 45, 60]
    print("world | cfg | period ms | first correct planet s | 10% / 50% of final s | " +
          " | ".join(f"rec2 at {m}s" for m in marks) + " | final rec2 (spur) at last event | worst publish lag ms")
    for key, v in sorted(g.items()):
        v.sort(key=lambda x: num(x, "t_ms"))
        final = num(v[-1], "rec2")
        first = next((num(x, "t_ms") / 1e3 for x in v if num(x, "rec2") >= 1), float("nan"))
        def reach(share):
            return next((num(x, "t_ms") / 1e3 for x in v if num(x, "rec2") >= share * final and final > 0), float("nan"))
        def at(m):
            before = [x for x in v if num(x, "t_ms") <= m * 1e3]
            return f"{num(before[-1], 'rec2'):.0f}" if before else "0"
        print(f"{key[0]} | {key[1]} | {key[2]} | {first:.1f} | {reach(.1):.1f} / {reach(.5):.1f} | " +
              " | ".join(at(m) for m in marks) + f" | {final:.0f} ({num(v[-1], 'spur'):.0f}) | {lag.get(key, '?')}")


if __name__ == "__main__":
    args = sys.argv[1:]
    mode = args.pop(0)
    cfg = ""
    if "--cfg" in args:
        i = args.index("--cfg")
        cfg = args[i + 1]
        del args[i:i + 2]
    if mode == "latency":
        latency(parse(args, ("UPD",), cfg))
    else:
        timeline(parse(args, ("TL", "TLEND"), cfg))
