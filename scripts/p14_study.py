#!/usr/bin/env python3 -I
"""P14 tuning study: Optuna (ask-and-tell, TPE with constraints, WilcoxonPruner racing over worlds)
over estimator search constants and targeting constants, scored by `devtool eval`.

Per world the score is the configuration's true hits normalized by the default configuration's on
the same world, (hits + 1) / (hits_default + 1); the study minimizes the negative mean. Synthetic
worlds score generated decisions on the configuration's own estimator beliefs (estimator and
targeting knobs both act); training captures replay the recorded decisions with the recorded
beliefs (targeting knobs only). Constraints (feasible when <= 0): CPU of the estimator replays at
most 1.5 times the default's on the same worlds; spurious planets at most 1.1 times the default's
plus 2. The default configuration is trial 0 and stays in every comparison.

  study:    p14_study.py study --devtool BIN --db DB --out DIR [--budget-cpu-h 20] [--trials 200]
  held-out: p14_study.py holdout --devtool BIN --db DB --out DIR --config best.json

Held-out worlds are evaluated once, at the end, for the default and the chosen configuration.
"""
import argparse, concurrent.futures as cf, hashlib, json, os, random, resource, subprocess, sys
import threading

TRAIN_SYNTH = [f"synth:{s}:24" for s in range(1001, 1061)]
TRAIN_CAPTURES = [f"capture:{c}:0" for c in (29, 30, 31, 32, 33, 35, 37, 39, 41, 42, 53, 55, 56, 57)]
HOLDOUT_SYNTH = [f"synth:{s}:24" for s in range(9001, 9021)]
# The newest live maps (one map per capture, recorded 2026-11-27 on main; no duplicates by
# `devtool games`).
HOLDOUT_CAPTURES = [f"capture:{c}:0" for c in range(59, 72)]

DEFAULTS = {
    "estimator.slices_per_path": 6, "estimator.slice_evaluations": 300,
    "search.seed_significance": 4.0, "search.max_seeds": 8, "search.max_candidates": 24,
    "search.joint_refits": 3, "search.death_trials": 2, "search.row_cap": 1000,
    "search.local_radius": 100.0, "search.local_evaluations": 300,
    "targeting.miss_distance_ticks": 5.0, "targeting.approach_min_energy": 15.0,
    "targeting.doom_threshold": 0.15,
}


def suggest(trial):
    return {
        "estimator.slices_per_path": trial.suggest_int("estimator.slices_per_path", 2, 12),
        "estimator.slice_evaluations": trial.suggest_int("estimator.slice_evaluations", 150, 600, log=True),
        "search.seed_significance": trial.suggest_float("search.seed_significance", 2.5, 6.0),
        "search.max_seeds": trial.suggest_int("search.max_seeds", 4, 12),
        "search.max_candidates": trial.suggest_int("search.max_candidates", 12, 40),
        "search.joint_refits": trial.suggest_int("search.joint_refits", 2, 5),
        "search.death_trials": trial.suggest_int("search.death_trials", 1, 4),
        "search.row_cap": trial.suggest_int("search.row_cap", 600, 2000, log=True),
        "search.local_radius": trial.suggest_float("search.local_radius", 60.0, 160.0),
        "search.local_evaluations": trial.suggest_int("search.local_evaluations", 150, 600, log=True),
        "targeting.miss_distance_ticks": trial.suggest_float("targeting.miss_distance_ticks", 1.0, 15.0),
        "targeting.approach_min_energy": trial.suggest_float("targeting.approach_min_energy", 5.0, 30.0),
        "targeting.doom_threshold": trial.suggest_float("targeting.doom_threshold", 0.05, 0.5),
    }


def config_json(params):
    cfg = {"estimator": {"search": {}}, "targeting": {}}
    for key, value in params.items():
        group, name = key.split(".")
        if group == "search":
            cfg["estimator"]["search"][name] = value
        else:
            cfg[group][name] = value
    return cfg


class Evaluator:
    def __init__(self, args):
        self.args = args
        os.makedirs(f"{args.out}/configs", exist_ok=True)
        os.makedirs(f"{args.out}/metrics", exist_ok=True)
        self.lock = threading.Lock()

    def config_path(self, params):
        text = json.dumps(config_json(params), sort_keys=True)
        path = f"{self.args.out}/configs/{hashlib.sha1(text.encode()).hexdigest()[:12]}.json"
        if not os.path.exists(path):
            with open(path, "w") as f:
                f.write(text)
        return path

    def eval(self, params, world, mode, belief, paths):
        cfg = self.config_path(params)
        tag = hashlib.sha1(f"{cfg}{world}{mode}{belief}{paths}".encode()).hexdigest()[:16]
        out = f"{self.args.out}/metrics/{tag}.json"
        cmd = [self.args.devtool, "eval", "--world", world, "--config", cfg, "--db", self.args.db,
               "--paths", str(paths), "--mode", mode, "--belief", belief, "--situations", "8",
               "--checkpoints", "16,32", "--threads", "1", "--cache", f"{self.args.out}/cache",
               "--out", out]
        subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL)
        with open(out) as f:
            return json.load(f)


def train_eval(ev, params, world):
    if world.startswith("synth"):
        return ev.eval(params, world, "both", "refit", 60)
    return ev.eval(params, world, "targeting", "recorded", 100000)


def facts(m):
    """(true hits, estimator cpu s or None, spurious at the last checkpoint or None)."""
    est = m.get("estimator")
    return (m["targeting"]["true_hits"],
            est["work"]["cpu_s"] if est else None,
            est["checkpoints"][-1]["spurious"] if est else None)


def cpu_h():
    r = resource.getrusage(resource.RUSAGE_CHILDREN)
    return (r.ru_utime + r.ru_stime) / 3600


def study(args):
    import optuna
    ev = Evaluator(args)
    worlds = TRAIN_SYNTH + TRAIN_CAPTURES
    pool = cf.ThreadPoolExecutor(args.workers)
    print(f"optuna {optuna.__version__}; baseline on {len(worlds)} worlds", flush=True)
    base = dict(zip(worlds, pool.map(lambda w: facts(train_eval(ev, DEFAULTS, w)), worlds)))
    print(f"baseline done, {cpu_h():.2f} CPU-h", flush=True)

    def constraints(trial):
        return trial.user_attrs.get("constraints", [1.0, 1.0])

    sampler = optuna.samplers.TPESampler(seed=14, multivariate=True, constant_liar=True,
                                         constraints_func=constraints, n_startup_trials=16)
    pruner = optuna.pruners.WilcoxonPruner(p_threshold=0.1, n_startup_steps=8)
    storage = f"sqlite:///{args.out}/study.db"
    st = optuna.create_study(study_name="p14", storage=storage, sampler=sampler, pruner=pruner,
                             direction="minimize", load_if_exists=True)
    st.enqueue_trial({k: v for k, v in DEFAULTS.items()})
    stop = threading.Event()
    log = open(f"{args.out}/trials.jsonl", "a")

    def run_trial():
        while not stop.is_set():
            with ev.lock:
                if len(st.trials) >= args.trials or cpu_h() > args.budget_cpu_h:
                    stop.set()
                    return
                trial = st.ask()
            params = suggest(trial)
            order = list(range(len(worlds)))
            random.Random(trial.number).shuffle(order)
            scores, cpu, cpu0, spur, spur0, pruned = [], 0.0, 0.0, 0, 0, False
            batch = args.batch
            with cf.ThreadPoolExecutor(batch) as local:
                for start in range(0, len(order), batch):
                    ids = order[start:start + batch]
                    results = list(local.map(lambda i: facts(train_eval(ev, params, worlds[i])), ids))
                    for i, (hits, c, s) in zip(ids, results):
                        bh, bc, bs = base[worlds[i]]
                        score = (hits + 1) / (bh + 1)
                        scores.append(score)
                        trial.report(-score, i)
                        if c is not None:
                            cpu, cpu0, spur, spur0 = cpu + c, cpu0 + bc, spur + s, spur0 + bs
                    if trial.should_prune():
                        pruned = True
                        break
            cons = [cpu / max(cpu0, 1e-9) - 1.5, spur - (1.1 * spur0 + 2)]
            trial.set_user_attr("constraints", cons)
            trial.set_user_attr("worlds", len(scores))
            trial.set_user_attr("pruned_early", pruned)
            value = -sum(scores) / len(scores)
            st.tell(trial, value)
            row = {"trial": trial.number, "value": value, "worlds": len(scores), "pruned": pruned,
                   "constraints": cons, "params": params, "cpu_h_total": cpu_h()}
            with ev.lock:
                log.write(json.dumps(row) + "\n")
                log.flush()
            print(json.dumps(row), flush=True)

    threads = [threading.Thread(target=run_trial) for _ in range(args.parallel)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    write_best(st, args)


def write_best(st, args):
    """The best complete (not pruned) feasible trial as best.json."""
    feasible = [t for t in st.trials if t.state.name == "COMPLETE"
                and t.value is not None and not t.user_attrs.get("pruned_early")
                and all(c <= 0 for c in t.user_attrs.get("constraints", [1]))]
    best = min(feasible, key=lambda t: t.value)
    params = {k: best.params[k] for k in DEFAULTS}
    with open(f"{args.out}/best.json", "w") as f:
        json.dump(config_json(params), f, indent=1)
    print(json.dumps({"best_trial": best.number, "value": best.value, "params": params,
                      "trials": len(st.trials), "cpu_h": cpu_h()}), flush=True)


def best(args):
    """Pick the best trial of a stopped study (`study` stops at its budget; this ends it early)."""
    import optuna
    st = optuna.load_study(study_name="p14", storage=f"sqlite:///{args.out}/study.db")
    write_best(st, args)


def holdout(args):
    ev = Evaluator(args)
    with open(args.config) as f:
        cfg = json.load(f)
    tuned = dict(DEFAULTS)
    for group, values in cfg.items():
        for name, value in values.items():
            if name == "search":
                tuned.update({f"search.{k}": v for k, v in value.items()})
            else:
                tuned[f"{group}.{name}"] = value
    jobs = []
    for w in HOLDOUT_SYNTH:
        jobs.append((w, "both", "refit", 60))
    for w in HOLDOUT_CAPTURES:
        jobs.append((w, "targeting", "recorded", 100000))
        jobs.append((w, "both", "refit", args.capture_refit_paths))
    pool = cf.ThreadPoolExecutor(args.workers)
    futures = {(name, j): pool.submit(ev.eval, p, *j)
               for name, p in (("default", DEFAULTS), ("tuned", tuned)) for j in jobs}
    out = open(f"{args.out}/holdout.jsonl", "w")
    for (name, j), fut in futures.items():
        m = fut.result()
        out.write(json.dumps({"config": name, "world": j[0], "mode": j[1], "belief": j[2],
                              "paths": j[3], "metrics": m}) + "\n")
    print(f"held-out done, {cpu_h():.2f} CPU-h", flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("command", choices=["study", "best", "holdout"])
    p.add_argument("--devtool", required=True)
    p.add_argument("--db", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--config")
    p.add_argument("--budget-cpu-h", type=float, default=20.0)
    p.add_argument("--trials", type=int, default=200)
    p.add_argument("--parallel", type=int, default=4)
    p.add_argument("--batch", type=int, default=7)
    p.add_argument("--workers", type=int, default=28)
    p.add_argument("--capture-refit-paths", type=int, default=200)
    args = p.parse_args()
    {"study": study, "best": best, "holdout": holdout}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
