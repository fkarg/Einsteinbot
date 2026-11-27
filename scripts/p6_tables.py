#!/usr/bin/env python3 -I
"""Tables for docs/estimator-experiments.md "P6 results" from the output of the diagnose_incremental
and diagnose_rebuild harnesses (src/estimator/diagnose/incremental.rs).

usage: p6_tables.py {cost,quality,audit,support} OUTPUT_FILE... [--worlds PREFIX] [--cfg SUBSTR]
"""
import os
import statistics as st
import sys
from collections import defaultdict


def parse(files, world_prefix="", cfg_sub=""):
    rows = defaultdict(list)
    for f in files:
        for line in open(f, errors="replace"):
            # The first line of a test's output follows "test name ... " on the same line.
            for marker in ("UPD ", "QUAL ", "AUDIT ", "PLANET "):
                at = line.find(marker)
                if at > 0 and "world=" in line[at:]:
                    line = line[at:]
                    break
            tag, _, rest = line.partition(" ")
            if tag not in ("UPD", "QUAL", "AUDIT", "PLANET"):
                continue
            d = {}
            for tok in rest.split():
                if "=" in tok:
                    k, v = tok.split("=", 1)
                    d[k] = v
            # cfg may contain no spaces but may contain '=' and ':' ; re-split on first '='
            if "cfg" in d and not d["world"].startswith(world_prefix):
                continue
            if cfg_sub and cfg_sub not in d.get("cfg", ""):
                continue
            rows[tag].append(d)
    return rows


def num(d, k):
    try:
        return float(d[k])
    except (KeyError, ValueError):
        return float("nan")


def bucket(n):
    for hi in (4, 8, 16, 32, 60):
        if n <= hi:
            return hi
    return 60


def cost(rows):
    g = defaultdict(list)
    kmin = int(os.environ.get("KMIN", "0"))
    for d in rows["UPD"]:
        if int(d["k"]) < kmin:
            continue
        g[(d["cfg"], d["threads"], bucket(int(d["n"])))].append(d)
    print("cfg | threads | paths<= | updates | median ms | p90 ms | max ms | median evals | median equiv | median K")
    for (cfg, th, b), v in sorted(g.items()):
        ms = sorted(num(x, "wall_ms") for x in v)
        p90 = ms[min(len(ms) - 1, int(0.9 * len(ms)))]
        print(f"{cfg} | {th} | {b} | {len(v)} | {st.median(ms):.0f} | {p90:.0f} | {ms[-1]:.0f} | "
              f"{st.median(num(x, 'evals') for x in v):.0f} | {st.median(num(x, 'equiv') for x in v):.0f} | "
              f"{st.median(num(x, 'k') for x in v):.0f}")


def latency(files):
    print("run | threads | updates paths<=16: median / p90 / max ms | updates paths>16: median / p90 / max ms, median evals, median full-row equiv, median K")
    for f in files:
        rows = parse([f])["UPD"]
        def summary(sel):
            ms = sorted(num(x, "wall_ms") for x in sel)
            if not ms:
                return "-"
            p90 = ms[min(len(ms) - 1, int(0.9 * len(ms)))]
            return f"{st.median(ms):.0f} / {p90:.0f} / {ms[-1]:.0f}"
        early = [x for x in rows if int(x["n"]) <= 16]
        late = [x for x in rows if int(x["n"]) > 16]
        ev = f"{st.median(num(x, 'evals') for x in late):.0f}, {st.median(num(x, 'equiv') for x in late):.0f}, {st.median(num(x, 'k') for x in late):.0f}" if late else "-"
        print(f"{os.path.basename(f)} | {rows[0]['threads']} | {len(early)}: {summary(early)} | {len(late)}: {summary(late)}, {ev}")


def quality(rows):
    g = defaultdict(list)
    for d in rows["QUAL"]:
        g[(d["cfg"], int(d["n"]))].append(d)
    print("cfg | n | worlds | recall2 | recall15 | spurious | K | ratio<=1.2 | median ratio | field expl/flat | unexpl/flat | whole/flat")
    for (cfg, n), v in sorted(g.items(), key=lambda x: (x[0][0], x[0][1])):
        kt = [num(x, "kt") for x in v]
        r2 = sum(num(x, "rec2") for x in v) / sum(kt)
        r15 = sum(num(x, "rec15") for x in v) / sum(kt)
        sp = st.mean(num(x, "spur") for x in v)
        k = st.mean(num(x, "k") for x in v)
        ok = sum(num(x, "ratio") <= 1.2 for x in v)
        mr = st.median(num(x, "ratio") for x in v)
        f = lambda a, b: st.mean(num(x, a) / num(x, b) for x in v)
        print(f"{cfg} | {n} | {len(v)} | {r2:.0%} | {r15:.0%} | {sp:.1f} | {k:.1f} | {ok}/{len(v)} | {mr:.2e} | "
              f"{f('fe','fe0'):.2e} | {f('fu','fu0'):.2e} | {f('fw','fw0'):.2e}")


def audit(rows):
    g = defaultdict(list)
    for d in rows["AUDIT"]:
        g[(d["cfg"], bucket(int(d["n"])))].append(d)
    print("cfg | paths<= | audits | any map change (moved>3 or added or removed) | mean moved | mean added | mean removed | rebuild better (rel. evidence) | "
          "rebuild rec2 > inc | rebuild rec2 < inc | rebuild worse on adoption (rec2 lower or spurious higher) | ratio_rb<ratio_inc")
    for (cfg, b), v in sorted(g.items()):
        chg = sum(num(x, "moved") + num(x, "added") + num(x, "removed") > 0 for x in v)
        better = [x for x in v if x["better"] == "true"]
        worse = sum(num(x, "rec2_rb") < num(x, "rec2_inc") or num(x, "spur_rb") > num(x, "spur_inc") for x in better)
        print(f"{cfg} | {b} | {len(v)} | {chg} | {st.mean(num(x,'moved') for x in v):.1f} | "
              f"{st.mean(num(x,'added') for x in v):.1f} | {st.mean(num(x,'removed') for x in v):.1f} | {len(better)} | "
              f"{sum(num(x,'rec2_rb')>num(x,'rec2_inc') for x in v)} | {sum(num(x,'rec2_rb')<num(x,'rec2_inc') for x in v)} | "
              f"{worse} | {sum(num(x,'ratio_rb')<num(x,'ratio_inc') for x in v)}")


def support(rows, key="d05", later=False):
    kt = {}
    for d in rows["QUAL"]:
        kt[(d["cfg"], d["world"], int(d["n"]))] = num(d, "kt")
    g = defaultdict(list)
    for d in rows["PLANET"]:
        g[(d["cfg"], bucket(int(d["n"])))].append(d)
    thr = [1, 10, 30, 100, 1e3, 1e4, 1e5, 1e6, 3e6, 1e7, 3e7, 1e8, 1e9]
    print(f"statistic {key} (relative-likelihood chi2 increase when the planet alone is removed)")
    for (cfg, b), v in sorted(g.items()):
        good = lambda x: (num(x, "dlater") < 5.0) if later else (num(x, "dtrue") < 2.0)
        spur = lambda x: (num(x, "dlater") >= 15.0) if later else (num(x, "dtrue") >= 15.0)
        c = [num(x, key) for x in v if good(x)]
        s = [num(x, key) for x in v if spur(x)]
        n_true = sum(val for (cf, w, n), val in kt.items() if cf == cfg and bucket(n) == b)
        print(f"-- {cfg}, paths<= {b}: {len(c)} correct, {len(s)} spurious, {len(v)-len(c)-len(s)} in between; true planets {n_true:.0f}")
        if c:
            print(f"   median {key}: correct {st.median(c):.3g}, spurious {st.median(s) if s else float('nan'):.3g}")
        if c and s:
            q = lambda v, f: sorted(v)[min(len(v) - 1, int(f * len(v)))]
            auc = sum((a > b) + 0.5 * (a == b) for a in c for b in s) / (len(c) * len(s))
            print(f"   quantiles 10/50/90%: correct {q(c,.1):.2g}/{q(c,.5):.2g}/{q(c,.9):.2g}; spurious {q(s,.1):.2g}/{q(s,.5):.2g}/{q(s,.9):.2g}; AUC (correct ranks above spurious) {auc:.2f}")
        print("   threshold | correct kept | spurious dropped | precision of kept | share of true planets hidden")
        for t in thr:
            ck = sum(x >= t for x in c)
            sd = sum(x < t for x in s)
            kept = ck + sum(x >= t for x in s)
            prec = ck / kept if kept else float("nan")
            hidden = (1 - ck / n_true) if (n_true and not later) else float("nan")
            print(f"   {t:g} | {ck}/{len(c)} | {sd}/{len(s)} | {prec:.2f} | {hidden:.0%}")


if __name__ == "__main__":
    args = sys.argv[1:]
    mode = args.pop(0)
    wp = cfg = ""
    if "--worlds" in args:
        i = args.index("--worlds")
        wp = args[i + 1]
        del args[i:i + 2]
    if "--cfg" in args:
        i = args.index("--cfg")
        cfg = args[i + 1]
        del args[i:i + 2]
    if mode == "latency":
        latency(args)
        sys.exit(0)
    rows = parse(args, wp, cfg)
    if mode == "support":
        support(rows, "d05", later="live" in wp)
        support(rows, "d02", later="live" in wp)
        support(rows, "dloc", later="live" in wp)
    else:
        {"cost": cost, "quality": quality, "audit": audit}[mode](rows)
