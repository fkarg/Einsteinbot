//! Work counters, per-part timings and wall-clock ceilings of one estimator update (a path's local
//! refit and the search slices that follow it).
//!
//! Two independent kinds of limit exist. The *work limit* (`Settings::slice_evaluations` per
//! slice) counts residual and Jacobian evaluations and is deterministic: the same data and limit
//! give the same belief whatever the machine load. *Wall-clock* limits (`Settings::slice_ceiling`,
//! which cuts a slice and keeps the best map so far, and the per-part `PartCeilings`) depend on
//! the machine. A ceiling that is exceeded is never silent: it is recorded in the `UpdateReport`
//! (and `UpdateReport::assert_within_ceilings` panics naming the part).
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A part of the update with its own timing. Leaf parts are timed per call (summed over threads,
/// so a parallel part can exceed wall time) and their ceiling applies to the longest single call;
/// phase parts are timed on the coordinating thread and their ceiling applies to the update total.
/// `LmSolve` contains the evaluations of its runs, and `Evidence` contains a Jacobian evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// Preparing a path's windowed comparison (per shot; leaf).
    WindowBuild,
    /// Classifying a path's terminal contact (per shot; leaf).
    Contacts,
    /// One residual evaluation (leaf).
    ResidualEval,
    /// One residual and Jacobian evaluation (leaf).
    JacobianEval,
    /// One Levenberg-Marquardt refinement, all its iterations (leaf).
    LmSolve,
    /// One Laplace evidence evaluation (leaf; contains a Jacobian and the covariance).
    Evidence,
    /// The covariance and conditioning part of an evidence evaluation (leaf).
    Covariance,
    /// The model-error estimate and noise of a new path (phase).
    Noise,
    /// Local refit of the planets near a new path (phase).
    WarmRefit,
    /// Finding birth seeds from the unexplained windows (phase).
    Seeding,
    /// Fitting each birth seed alone against the parent's field (phase).
    BirthAlone,
    /// Joint refit of the best seeds with every planet (phase).
    BirthJoint,
    /// Death trials and refits (phase).
    Deaths,
    /// Full-row refit and covariance of the published map (phase).
    Polish,
    /// Diagnostics of a published belief (position RMSE, contact bookkeeping; phase).
    Diagnostics,
}

const PARTS: [Part; 15] = [
    Part::WindowBuild,
    Part::Contacts,
    Part::ResidualEval,
    Part::JacobianEval,
    Part::LmSolve,
    Part::Evidence,
    Part::Covariance,
    Part::Noise,
    Part::WarmRefit,
    Part::Seeding,
    Part::BirthAlone,
    Part::BirthJoint,
    Part::Deaths,
    Part::Polish,
    Part::Diagnostics,
];

impl Part {
    pub const ALL: [Part; 15] = PARTS;

    fn is_leaf(self) -> bool {
        !matches!(
            self,
            Part::Noise
                | Part::WarmRefit
                | Part::Seeding
                | Part::BirthAlone
                | Part::BirthJoint
                | Part::Deaths
                | Part::Polish
                | Part::Diagnostics
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Part::WindowBuild => "window_build",
            Part::Contacts => "contacts",
            Part::ResidualEval => "residual_eval",
            Part::JacobianEval => "jacobian_eval",
            Part::LmSolve => "lm_solve",
            Part::Evidence => "evidence",
            Part::Covariance => "covariance",
            Part::Noise => "noise",
            Part::WarmRefit => "warm_refit",
            Part::Seeding => "seeding",
            Part::BirthAlone => "birth_alone",
            Part::BirthJoint => "birth_joint",
            Part::Deaths => "deaths",
            Part::Polish => "polish",
            Part::Diagnostics => "diagnostics",
        }
    }

    pub fn from_name(name: &str) -> Option<Part> {
        PARTS.into_iter().find(|part| part.name() == name)
    }
}

/// Wall-clock ceilings per part; an absent part has none. Meant for tests and the scorecard.
#[derive(Clone, Debug, Default)]
pub struct PartCeilings(pub BTreeMap<Part, Duration>);

#[derive(Default)]
struct PartStat {
    calls: AtomicU64,
    nanos: AtomicU64,
    max_nanos: AtomicU64,
}

/// Thread-safe accumulators of one update.
#[derive(Default)]
pub(super) struct Stats {
    parts: [PartStat; PARTS.len()],
}

impl Stats {
    pub fn add(&self, part: Part, elapsed: Duration) {
        let stat = &self.parts[part as usize];
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        stat.calls.fetch_add(1, Ordering::Relaxed);
        stat.nanos.fetch_add(nanos, Ordering::Relaxed);
        stat.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// Add everything `other` has counted.
    pub fn absorb(&self, other: &Stats) {
        for (mine, theirs) in self.parts.iter().zip(&other.parts) {
            mine.calls
                .fetch_add(theirs.calls.load(Ordering::Relaxed), Ordering::Relaxed);
            mine.nanos
                .fetch_add(theirs.nanos.load(Ordering::Relaxed), Ordering::Relaxed);
            mine.max_nanos
                .fetch_max(theirs.max_nanos.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    pub fn time<R>(&self, part: Part, f: impl FnOnce() -> R) -> R {
        let started = Instant::now();
        let out = f();
        self.add(part, started.elapsed());
        out
    }

    /// Wall (phase parts) or summed (leaf parts) time spent in `part` so far.
    pub fn total(&self, part: Part) -> Duration {
        Duration::from_nanos(self.parts[part as usize].nanos.load(Ordering::Relaxed))
    }

    /// Residual and Jacobian evaluations so far: the deterministic work count.
    pub fn evaluations(&self) -> u64 {
        [Part::ResidualEval, Part::JacobianEval]
            .iter()
            .map(|part| self.parts[*part as usize].calls.load(Ordering::Relaxed))
            .sum()
    }

    pub fn report(&self, ceilings: &PartCeilings) -> UpdateReport {
        let mut parts = Vec::new();
        let mut violations = Vec::new();
        for part in PARTS {
            let stat = &self.parts[part as usize];
            let calls = stat.calls.load(Ordering::Relaxed);
            let total = Duration::from_nanos(stat.nanos.load(Ordering::Relaxed));
            let longest = Duration::from_nanos(stat.max_nanos.load(Ordering::Relaxed));
            if let Some(ceiling) = ceilings.0.get(&part) {
                let observed = if part.is_leaf() { longest } else { total };
                if observed > *ceiling {
                    violations.push(CeilingViolation {
                        part,
                        observed_ms: observed.as_secs_f64() * 1e3,
                        ceiling_ms: ceiling.as_secs_f64() * 1e3,
                    });
                }
            }
            if calls > 0 {
                parts.push(PartReport {
                    part,
                    calls,
                    total_ms: total.as_secs_f64() * 1e3,
                    longest_ms: longest.as_secs_f64() * 1e3,
                });
            }
        }
        UpdateReport {
            evaluations: self.evaluations(),
            parts,
            violations,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PartReport {
    pub part: Part,
    pub calls: u64,
    /// Leaf parts: summed over threads (CPU time); phase parts: wall time.
    pub total_ms: f64,
    pub longest_ms: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct CeilingViolation {
    pub part: Part,
    pub observed_ms: f64,
    pub ceiling_ms: f64,
}

/// Work and timing of the most recent update.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct UpdateReport {
    /// Residual plus Jacobian evaluations (the quantity `max_evaluations` limits).
    pub evaluations: u64,
    pub parts: Vec<PartReport>,
    pub violations: Vec<CeilingViolation>,
}

impl UpdateReport {
    pub fn part(&self, part: Part) -> Option<&PartReport> {
        self.parts.iter().find(|report| report.part == part)
    }

    /// Panics, naming each part, if any ceiling was exceeded.
    pub fn assert_within_ceilings(&self) {
        assert!(
            self.violations.is_empty(),
            "wall-clock ceiling exceeded: {}",
            self.violations
                .iter()
                .map(|v| format!(
                    "{} took {:.1} ms (ceiling {:.1} ms)",
                    v.part.name(),
                    v.observed_ms,
                    v.ceiling_ms
                ))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

/// Test-only thread limit (0: none), so the diagnosis harness can measure at 1, 4 and 8 threads.
#[cfg(test)]
pub(super) static TEST_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Logical cores available to the process.
pub(super) fn cores() -> usize {
    #[cfg(test)]
    if TEST_THREADS.load(Ordering::Relaxed) > 0 {
        return TEST_THREADS.load(Ordering::Relaxed);
    }
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// `f` over `items` on up to `workers` threads (none are spawned for one), results in item order.
pub(super) fn par_map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let workers = workers.min(items.len());
    if workers <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break local;
                        };
                        local.push((index, f(item)));
                    }
                })
            })
            .collect::<Vec<_>>();
        // A panicking worker is a bug, not "nothing found": propagate it.
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}
