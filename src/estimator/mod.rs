//! Planet-map estimator (v2). The default belief is a flat world (no planets); a planet is
//! admitted only when trajectory evidence supports it.
//!
//! Estimation compares the model's gravity field with the field implied by the observed paths
//! (`window`: windowed velocity-change comparison with the engine's exact discrete kernel); no
//! dynamics are simulated in the fit. The likelihood (`noise`) is the exact measurement floor
//! plus a model-error process for the field of planets not found yet, whose two scalars are
//! estimated from prequential residuals and frozen within each operation. `fit` jointly fits K
//! planets `(x, y, radius)` to the rows of every path.
//!
//! The live map is maintained incrementally (`search`): each new path first gets a local refit of
//! the planets near it, and that map is published at once; bounded slices of birth work then
//! continue the planet search on a thinned row set, with state that survives across slices and
//! new paths. The published belief carries one map and no alternatives. The map the search keeps
//! is refit on all rows for publication, which also gives the covariance. A background audit
//! (`audit`) rebuilds the map from scratch (`rebuild`) and replaces the maintained map only when
//! its evidence on all rows is higher.
//!
//! Surface contacts (`contacts`) are extra evidence only: a robust surface likelihood in the
//! joint fit (a contact lies on the surface of the planet it hit within its penetration
//! allowance, or comes from a planet the map does not hold) that never creates a planet. Every
//! map, the flat one included, carries one term per contact, so maps are compared on the same
//! contacts. Oracle planets are never an input.
mod audit;
mod contacts;
mod eval;
mod fit;
mod lm;
mod noise;
mod rebuild;
mod residual;
mod rows;
mod search;
mod window;
mod work;

pub use eval::{position_rmse, shot_observations, shot_position_rmse};
pub use residual::{GATE_ACCELERATION, ResidualCorrection, ResidualGate, ResidualModel};
pub use work::{CeilingViolation, Part, PartCeilings, PartReport, UpdateReport};

use crate::map::PlanetMap;
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use audit::{Audit, Stage};
use fit::{CONTACT_MATCH, Evidence, Fit, planet_at};
use noise::{ModelError, Noise, TickMask};
use search::{Search, Slice};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use window::PathData;
use work::{Stats, cores};

/// Default cap on the number of planets in a map when the count is not known: the top of the
/// count prior's plausible range (24 + 2 sd). Raised from 24 once overlapping planets were
/// excluded (P21): with them excluded, caps 24, 32 and 64 gave the same maps on 24-planet replays,
/// and 32 finds 26-planet worlds without `--planets` (`docs/estimator-experiments.md`).
pub const DEFAULT_MAX_PLANETS: usize = 32;
/// Default evaluations of one search slice.
pub const DEFAULT_SLICE_EVALUATIONS: u64 = 300;
/// Default wall-clock ceiling of one search slice.
pub const DEFAULT_SLICE_CEILING: Duration = Duration::from_millis(250);
/// Default search slices owed after each new path.
pub const DEFAULT_SLICES_PER_PATH: usize = 6;
/// Evaluations of the full-row refit of the published map.
const POLISH_EVALUATIONS: u64 = 600;
/// Once this many paths have arrived since the last completed full-row refit, the next refit goes
/// before the search slices and is not pre-empted, so a steady stream of paths cannot starve the
/// covariance and the count-limit warning (about a second at 9 paths per second). It delays at
/// most one path by one refit at that interval.
const POLISH_STARVED_PATHS: usize = 8;
/// Count-limit warning (P23): a polished map at the planet cap whose residual correction still
/// reaches this acceleration somewhere is evidence of a planet the cap leaves out. Measured: at
/// most 0.015 (the third largest of five) on correct counts at the cap, at least 0.094 with two
/// or more planets left out (`docs/estimator-experiments.md`).
pub const COUNT_LIMIT_ACCELERATION: f64 = 0.03;
/// The warning holds when `COUNT_LIMIT_HITS` of the last `COUNT_LIMIT_WINDOW` polished maps at
/// the cap exceed `COUNT_LIMIT_ACCELERATION`.
pub const COUNT_LIMIT_WINDOW: usize = 5;
pub const COUNT_LIMIT_HITS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CovarianceUnavailableReason {
    /// No covariance was computed (the belief did not come from the estimator).
    NotComputed,
    /// The published map has no planets: uncertainty about planets not yet found is not modelled.
    NoPlanets,
}

/// Whether the data determine a planet of a published map (its covariance is the posterior under
/// the Gaussian prior, so it always exists; this says how far to trust it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Identifiability {
    /// Every posterior sd of `[x, y, radius]` is at most half the prior sd.
    Identified,
    /// The prior carries much of the precision: the sds are honest but wide.
    PriorDominated,
    /// The planet sits at a bound of the radius range or the battlefield, where the Gaussian
    /// approximation does not hold (the sds may be too small or too large).
    AtBound,
}

/// Why the planet search has work left in the published belief.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLimit {
    /// The map has the maximum number of planets, so growing it is not attempted (a supported
    /// extra planet may or may not exist).
    PlanetCap,
    /// A search slice was cut by its wall-clock ceiling.
    TimeBudget,
    /// The search has work left: a slice reached its evaluation limit, or the slices owed after
    /// the latest path have not all run.
    WorkBudget,
}

/// Whether the planet cap may be below the map's true count (P23). A warning only: the cap is
/// never raised automatically.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CountLimit {
    /// The cap: the known count (`--planets`), else `max_planets`.
    pub cap: usize,
    /// The published map holds `cap` planets.
    pub at_cap: bool,
    /// Paths of the epoch when the rule (`count_limited`) first held; latched for the map epoch.
    /// `None`: it has not held (no evidence of more planets than the cap, which is not evidence
    /// that there are none).
    pub detected_at_path: Option<usize>,
    /// Largest residual-correction magnitude of each of the latest polished maps at the cap,
    /// oldest first (at most `COUNT_LIMIT_WINDOW`); `None` where no correction was applied (its
    /// gate: the model error or the correction below `GATE_ACCELERATION`), which counts as below
    /// the threshold.
    pub recent_magnitudes: Vec<Option<f64>>,
}

impl CountLimit {
    pub fn limited(&self) -> bool {
        self.detected_at_path.is_some()
    }
}

/// The count-limit rule on the latest magnitudes of polished maps at the cap.
fn count_limited(recent: &[Option<f64>]) -> bool {
    recent
        .iter()
        .rev()
        .take(COUNT_LIMIT_WINDOW)
        .filter(|m| m.is_some_and(|m| m > COUNT_LIMIT_ACCELERATION))
        .count()
        >= COUNT_LIMIT_HITS
}

/// A completed trajectory, its prepared windowed comparison (`None` when it is too short for a
/// window) and its possible terminal contact, classified with the players known when it finished.
#[derive(Clone, Debug)]
struct Shot {
    obs: ShotObservation,
    contact: Option<contacts::Contact>,
    path: Option<PathData>,
}

impl Shot {
    /// The noise of the path is the measurement floor alone (`noise::apply` adds a model error).
    fn new(
        obs: ShotObservation,
        contact: Option<contacts::Contact>,
        world: WorldParameters,
    ) -> Self {
        let path = PathData::new(&obs.points, obs.angle_degrees, obs.velocity, world);
        Self { obs, contact, path }
    }
}

/// Limits and switches of an estimator.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Maximum number of planets in a map when the count is not known (`planets` is `None`).
    pub max_planets: usize,
    /// The known total planet count of the map (the server's `planets` setting, which it does not
    /// send). `Some(n)` fixes the count: `n - K` planets are unseen and the cap is `n`. `None`: the
    /// unseen count comes from a broad prior on the total (`noise::unseen`), capped by
    /// `max_planets`.
    pub planets: Option<usize>,
    /// Evaluations of one search slice (checked between batches of refits, so a slice may exceed
    /// it a little). Deterministic: the same paths and limits give the same belief whatever the
    /// load, as long as `slice_ceiling` does not cut a slice and no audit is adopted. The polish
    /// of the published map may be cut by the ceiling, but it only shapes what is published: the
    /// noise and the search use the maintained map.
    pub slice_evaluations: u64,
    /// Wall-clock ceiling of a slice (`None`: no ceiling); the best map so far is kept when it is hit.
    pub slice_ceiling: Option<Duration>,
    /// Slices owed after each new path.
    pub slices_per_path: usize,
    /// Threads of the live search; the audit uses half of them.
    pub threads: usize,
    /// Run the background audit. Off by default, because an adopted audit depends on timing; the
    /// live map leaves it off too (P13: no measured benefit at 2.2 times the CPU).
    pub audit_enabled: bool,
    /// Wall-clock ceilings per part of the update; exceeded ones are listed in `last_report`.
    pub ceilings: PartCeilings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_planets: DEFAULT_MAX_PLANETS,
            planets: None,
            slice_evaluations: DEFAULT_SLICE_EVALUATIONS,
            slice_ceiling: Some(DEFAULT_SLICE_CEILING),
            slices_per_path: DEFAULT_SLICES_PER_PATH,
            threads: cores(),
            audit_enabled: false,
            ceilings: PartCeilings::default(),
        }
    }
}

impl Settings {
    /// The planet cap: the known count, else `max_planets`.
    pub fn cap(&self) -> usize {
        self.planets.unwrap_or(self.max_planets)
    }

    fn slice<'a>(
        &'a self,
        cancelled: &'a AtomicBool,
        preempt: &'a AtomicBool,
        stats: &'a Stats,
        noise: &'a Noise,
    ) -> Slice<'a> {
        Slice {
            noise,
            evaluations: self.slice_evaluations,
            deadline: Instant::now() + self.slice_ceiling.unwrap_or(Duration::from_secs(24 * 3600)),
            threads: self.threads,
            max_planets: self.cap(),
            cancelled,
            preempt,
            stats,
        }
    }
}

pub struct Estimator {
    players: BTreeMap<u32, Point>,
    /// Every path of the map epoch. Their noise is whatever the last polish set; every user sets
    /// the noise it needs (`noise::apply`, `with_noise`).
    shots: Vec<Shot>,
    /// The ticks of the first paths of `shots` stamped on the candidate-centre raster (extended
    /// path by path; every noise of the epoch starts from it).
    mask: TickMask,
    /// Prequential residuals of the latest paths (at most `noise::WINDOW`).
    series: Vec<noise::Series>,
    /// The model-error scalars of the current fits: estimated from `series` without the latest
    /// path's.
    model: ModelError,
    search: Search,
    /// Paths already given to the search (the rest were ingested when an update was cancelled).
    searched: usize,
    audit: Option<Audit>,
    belief: Option<MapBelief>,
    /// Search slices still owed for the latest path.
    slices_left: usize,
    /// The published map has not been polished since the latest search change.
    polish_due: bool,
    /// Paths given to the search since the last completed polish.
    paths_since_polish: usize,
    /// Why the search has work left, if it has.
    limit: Option<SearchLimit>,
    pub settings: Settings,
    stats: Stats,
    /// Work and timing of the most recent update (a path's local refit and the slices since).
    last_report: Option<UpdateReport>,
    /// Window-build and contact-classification times of shots ingested since the last update.
    pending_timings: Vec<(Part, Duration)>,
    /// `rho0` of the latest path's noise.
    last_rho0: Option<f64>,
    /// Residual-correction magnitudes of the latest polished maps at the cap (`CountLimit`).
    cap_magnitudes: Vec<Option<f64>>,
    /// Paths when the count-limit rule first held in this epoch.
    count_limited_at: Option<usize>,
}

/// What the estimator holds about its map epoch, for status output (cheap: no fit).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EstimatorSummary {
    /// Distinct paths ingested and paths given to the search.
    pub paths: usize,
    pub searched: usize,
    /// Planets of the maintained map.
    pub planets: usize,
    /// Whether `step` has work left (slices owed, a polish, an audit to adopt).
    pub search_pending: bool,
    pub search_limit: Option<SearchLimit>,
    /// Model-error scalars and the known total planet count (`None`: the prior).
    pub model: ResidualModel,
    /// Expected unseen planets `N_u` for the maintained map, and the expected total `K + N_u`
    /// (the known count when given; with the prior it is the prior's mean given `N >= K`, not a
    /// fitted count).
    pub unseen: f64,
    pub expected_total: f64,
    /// The most probable total count: the known one, else the prior's mode given `N >= K`
    /// (`max(K, 24)`). The data say no more about the count than that `K` were found.
    pub likely_total: usize,
    /// Near cutoff of the latest path's noise (`None` before the first path).
    pub rho0: Option<f64>,
    /// The count-limit warning as of the latest publication.
    pub count_limit: CountLimit,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            players: BTreeMap::new(),
            shots: Vec::new(),
            mask: TickMask::new(WorldParameters::default()),
            series: Vec::new(),
            model: ModelError::PRIOR,
            search: Search::new(),
            searched: 0,
            audit: None,
            belief: None,
            slices_left: 0,
            polish_due: false,
            paths_since_polish: 0,
            limit: None,
            settings: Settings::default(),
            stats: Stats::default(),
            last_report: None,
            pending_timings: Vec::new(),
            last_rho0: None,
            cap_magnitudes: Vec::new(),
            count_limited_at: None,
        }
    }
}

impl Estimator {
    /// Ingest an ordered group of events. When there are new trajectories, the planets near each
    /// get a local refit and the resulting map is published (returned). The rest of the search is
    /// `step`. `None` when nothing changed or the update was cancelled.
    pub fn observe_batch(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        let world = WorldParameters::default();
        for event in events {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            match event {
                ServerEvent::Planets { .. } => {
                    self.reset();
                }
                ServerEvent::PlayerPosition { player_id, x, y } => {
                    self.players.insert(*player_id, Point { x: *x, y: *y });
                }
                ServerEvent::PlayerLeave { player_id } => {
                    self.players.remove(player_id);
                }
                ServerEvent::ShotFin { .. } => {
                    let Ok(obs) = ShotObservation::try_from(event) else {
                        continue;
                    };
                    // Non-finite values would make every residual (and the evidence) NaN.
                    if !obs.angle_degrees.is_finite()
                        || !obs.velocity.is_finite()
                        || !obs
                            .points
                            .iter()
                            .all(|point| point.x.is_finite() && point.y.is_finite())
                        || self.shots.iter().any(|shot| shot.obs == obs)
                    {
                        continue;
                    }
                    let players = self.players.values().cloned().collect::<Vec<_>>();
                    let started = Instant::now();
                    let contact = contacts::classify(&obs.points, &players, world);
                    self.pending_timings
                        .push((Part::Contacts, started.elapsed()));
                    let started = Instant::now();
                    self.shots.push(Shot::new(obs, contact, world));
                    self.pending_timings
                        .push((Part::WindowBuild, started.elapsed()));
                }
                _ => {}
            }
        }
        if self.searched == self.shots.len() {
            return None;
        }
        let before = self.searched;
        self.stats = Stats::default();
        for (part, elapsed) in self.pending_timings.drain(..) {
            self.stats.add(part, elapsed);
        }
        while self.searched < self.shots.len() {
            // The new path's prequential residuals against the maintained map before its local
            // refit. The background scalars come from the earlier paths' residuals only, so a path
            // is fitted (its local refit and the slices after it) under a background that has not
            // seen it; its residuals join the window for the next path.
            let started = Instant::now();
            let maintained = self.maintained_planets();
            self.mask.cover(&self.shots[..=self.searched]);
            let mut noise =
                Noise::with_mask(self.model, &maintained, &self.mask, self.settings.planets);
            let current = self.shots[self.searched]
                .path
                .as_ref()
                .map(|path| noise::Series::new(path, &maintained, &noise));
            self.model = noise::estimate(&self.series, current.as_ref());
            noise.model = self.model;
            self.last_rho0 = Some(noise.rho0);
            if let Some(series) = current {
                self.series.push(series);
                if self.series.len() > noise::WINDOW {
                    self.series.remove(0);
                }
            }
            self.stats.add(Part::Noise, started.elapsed());
            // The path's own local refit is not pre-empted (nothing new can be waiting for it).
            let never = AtomicBool::new(false);
            let slice = self.settings.slice(cancelled, &never, &self.stats, &noise);
            self.search.add_path(&self.shots[self.searched], &slice);
            self.searched += 1;
            self.paths_since_polish += 1;
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
        }
        self.slices_left = self.settings.slices_per_path;
        self.polish_due = true;
        self.limit = (!self.search.idle()).then_some(SearchLimit::WorkBudget);
        if self.audit.is_none()
            && self.settings.audit_enabled
            && (before + 1..=self.searched).any(audit::due)
        {
            self.audit = Some(Audit::rebuild(
                self.shots.clone(),
                self.noise(),
                self.settings.cap(),
                self.audit_threads(),
            ));
        }
        let theta = self.search.theta.clone();
        Some(self.publish(&theta, None, false))
    }

    /// Sequential replay: one event.
    pub fn observe(&mut self, event: &ServerEvent, cancelled: &AtomicBool) -> Option<MapBelief> {
        self.observe_batch(std::slice::from_ref(event), cancelled)
    }

    /// The most recently published belief.
    pub fn belief(&self) -> Option<&MapBelief> {
        self.belief.as_ref()
    }

    /// An estimator with `settings`.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// The state of the map epoch (`EstimatorSummary`).
    pub fn summary(&self) -> EstimatorSummary {
        let planets = self.search.theta.len();
        let unseen = noise::unseen(planets, self.settings.planets);
        EstimatorSummary {
            paths: self.shots.len(),
            searched: self.searched,
            planets,
            search_pending: self.search_pending() || self.searched < self.shots.len(),
            search_limit: self.belief.as_ref().and_then(|b| b.search_limit),
            model: ResidualModel::new(self.model, self.settings.planets),
            unseen,
            expected_total: planets as f64 + unseen,
            likely_total: noise::likely_total(planets, self.settings.planets),
            rho0: self.last_rho0,
            count_limit: self.count_limit(planets),
        }
    }

    /// The completed paths of the map epoch.
    pub fn observations(&self) -> Vec<ShotObservation> {
        self.shots.iter().map(|shot| shot.obs.clone()).collect()
    }

    /// Work counts and per-part timings of the most recent update.
    pub fn last_report(&self) -> Option<&UpdateReport> {
        self.last_report.as_ref()
    }

    /// Whether an audit is running, so that `step` will have a map to adopt at some point.
    pub fn audit_running(&self) -> bool {
        self.audit.is_some()
    }

    /// Whether `step` has work: a finished audit to adopt, search slices owed, or a map to polish.
    pub fn search_pending(&self) -> bool {
        self.audit.as_ref().is_some_and(Audit::finished)
            || (!self.search.idle() && self.slices_left > 0)
            || self.polish_due
    }

    /// One unit of background work: adopt a finished audit, else one search slice, else the exact
    /// refit of the published map. Returns the belief when the published map changed.
    pub fn step(&mut self, cancelled: &AtomicBool) -> Option<MapBelief> {
        self.step_until(cancelled, &AtomicBool::new(false))
    }

    /// `step` that stops early once `preempt` is set (new work is waiting, for the live map):
    /// a cut slice keeps its pending state for the next one, and a cut polish is dropped (still
    /// due). Neither publishes: the waiting path's update publishes the maintained map. Only a
    /// wall clock sets `preempt`, so replays without it stay deterministic.
    pub fn step_until(
        &mut self,
        cancelled: &AtomicBool,
        preempt: &AtomicBool,
    ) -> Option<MapBelief> {
        if preempt.load(Ordering::Relaxed) {
            return None;
        }
        if self.audit.as_ref().is_some_and(Audit::finished) {
            if let Some(belief) = self.collect_audit() {
                return Some(belief);
            }
        }
        let starved = self.polish_due && self.paths_since_polish >= POLISH_STARVED_PATHS;
        if !starved && !self.search.idle() && self.slices_left > 0 {
            self.slices_left -= 1;
            let noise = self.noise();
            let slice = self.settings.slice(cancelled, preempt, &self.stats, &noise);
            let before = self.stats.evaluations();
            let changed = self.search.step(&slice);
            let spent = self.stats.evaluations() - before;
            let preempted = preempt.load(Ordering::Relaxed);
            self.limit = if self.search.idle() {
                None
            } else if spent >= self.settings.slice_evaluations || preempted {
                Some(SearchLimit::WorkBudget)
            } else {
                Some(SearchLimit::TimeBudget)
            };
            self.polish_due |= changed;
            if preempted {
                // Not a spent slice: the next one continues the same work.
                self.slices_left += 1;
                return None;
            }
            if changed {
                let theta = self.search.theta.clone();
                return Some(self.publish(&theta, None, false));
            }
            return None;
        }
        if self.polish_due {
            self.polish_due = false;
            let started = Instant::now();
            // In place: every other user of `self.shots` sets its own noise.
            let noise = self.noise();
            noise::apply(&mut self.shots, &noise);
            let far = started + Duration::from_secs(24 * 3600);
            let never = AtomicBool::new(false);
            let preempt = if starved { &never } else { preempt };
            let full = Fit::new(
                &self.shots,
                WorldParameters::default(),
                cancelled,
                true,
                vec![],
            )
            .with_cut(far, preempt)
            .with_stats(&self.stats);
            let theta = self.search.theta.clone();
            let (theta, evidence) = if theta.is_empty() {
                (theta, None)
            } else {
                let deadline = self.settings.slice_ceiling.map(|ceiling| started + ceiling);
                let budget = fit::Budget {
                    evaluations: POLISH_EVALUATIONS,
                    deadline,
                    threads: self.settings.threads,
                };
                let theta = full.refine(&theta, &budget).map_or(theta, |(t, _)| t);
                if full.preempted() {
                    self.polish_due = true;
                    self.stats.add(Part::Polish, started.elapsed());
                    return None;
                }
                // The covariance needs a Jacobian of all rows; a refit that used up its time
                // leaves it out (`not_computed`) rather than run over the ceiling.
                let evidence = deadline
                    .is_none_or(|deadline| Instant::now() < deadline)
                    .then(|| full.evidence(&theta, self.settings.threads))
                    .flatten();
                (theta, evidence)
            };
            self.stats.add(Part::Polish, started.elapsed());
            self.paths_since_polish = 0;
            return Some(self.publish(&theta, evidence, true));
        }
        None
    }

    /// Run `step` until nothing is pending; the latest published belief if any was.
    pub fn settle(&mut self, cancelled: &AtomicBool) -> Option<MapBelief> {
        let mut latest = None;
        while self.search_pending() && !cancelled.load(Ordering::SeqCst) {
            latest = self.step(cancelled).or(latest);
        }
        latest
    }

    /// Wait for the running audit to end and adopt its map if it is better. Returns the belief
    /// when it was adopted. For replays, where time does not pass between paths.
    pub fn finish_audit(&mut self) -> Option<MapBelief> {
        while self.audit.is_some() {
            if let Some(belief) = self.collect_audit() {
                return Some(belief);
            }
        }
        None
    }

    fn audit_threads(&self) -> usize {
        (self.settings.threads / 2).max(1)
    }

    /// Take the result of the audit thread (waiting if it still runs). A rebuilt map goes on to
    /// its comparison on the paths known now; a map that won the comparison replaces the
    /// maintained one (it was refit on the paths known when the comparison started).
    fn collect_audit(&mut self) -> Option<MapBelief> {
        let audit = self.audit.take()?;
        let stage = audit.stage;
        let found = audit.join()?;
        if stage == Stage::Rebuild {
            self.audit = Some(Audit::compare(
                self.shots.clone(),
                self.noise(),
                self.search.theta.clone(),
                found,
                self.audit_threads(),
            ));
            return None;
        }
        self.search.adopt(found);
        self.slices_left = self.settings.slices_per_path;
        self.polish_due = true;
        self.limit = Some(SearchLimit::WorkBudget);
        let theta = self.search.theta.clone();
        Some(self.publish(&theta, None, false))
    }

    /// Planets of the maintained map (the search's, not the polished published one, so that a
    /// polish cut by its wall ceiling cannot change any later noise or search).
    fn maintained_planets(&self) -> Vec<crate::protocol::Planet> {
        self.search.theta.iter().map(|t| planet_at(*t)).collect()
    }

    /// The noise of an operation starting now: the current model-error estimate, shaped by the
    /// maintained map and every path.
    fn noise(&mut self) -> Noise {
        self.mask.cover(&self.shots);
        Noise::with_mask(
            self.model,
            &self.maintained_planets(),
            &self.mask,
            self.settings.planets,
        )
    }

    /// Forget the map epoch's evidence. Player positions survive: the server announces a
    /// player's new position just before that player's `Planets` event.
    pub fn reset(&mut self) {
        *self = Self {
            players: std::mem::take(&mut self.players),
            settings: self.settings.clone(),
            ..Self::default()
        };
    }

    /// The count-limit state for a map of `planets` planets.
    fn count_limit(&self, planets: usize) -> CountLimit {
        CountLimit {
            cap: self.settings.cap(),
            at_cap: planets >= self.settings.cap(),
            detected_at_path: self.count_limited_at,
            recent_magnitudes: self.cap_magnitudes.clone(),
        }
    }

    /// The belief of the map `theta`; `evidence` is its exact evidence when that was computed.
    /// A `polished` map at the cap adds its residual-correction magnitude to the count-limit
    /// window (one sample per polish, so at most one per path between audit adoptions).
    fn publish(
        &mut self,
        theta: &[[f64; 3]],
        evidence: Option<Evidence>,
        polished: bool,
    ) -> MapBelief {
        let started = Instant::now();
        let planets: Vec<_> = theta.iter().map(|th| planet_at(*th)).collect();
        let mut belief = MapBelief::new(PlanetMap { planets }, None);
        belief.covariance_unavailable_reason = match (theta.is_empty(), evidence) {
            (true, _) => Some(CovarianceUnavailableReason::NoPlanets),
            (
                false,
                Some(Evidence {
                    covariance: Some(covariance),
                    identifiability,
                    ..
                }),
            ) => {
                belief.parameter_covariance =
                    Some(fit::covariance_xym(&covariance, &belief.map.planets));
                belief.planet_identifiability = identifiability;
                None
            }
            (false, _) => Some(CovarianceUnavailableReason::NotComputed),
        };
        belief.search_limit = if theta.len() >= self.settings.cap() {
            Some(SearchLimit::PlanetCap)
        } else {
            self.limit
        };
        for contact in self.shots.iter().filter_map(|shot| shot.contact.as_ref()) {
            if !belief.contact_points.contains(&contact.point) {
                belief.contact_points.push(contact.point.clone());
            }
        }
        // Contacts farther than `CONTACT_MATCH` from every planet surface: no found planet near
        // them (a planet not found yet, or a model mismatch).
        belief.unmatched_contacts = belief
            .contact_points
            .iter()
            .filter(|point| {
                !belief.map.planets.iter().any(|planet| {
                    ((f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y)
                        - planet.radius)
                        .abs()
                        <= CONTACT_MATCH
                })
            })
            .count();
        self.stats.add(Part::Diagnostics, started.elapsed());
        // The correction must match the published map (a birth explains part of the residual).
        let started = Instant::now();
        self.mask.cover(&self.shots);
        let noise = Noise::with_mask(
            self.model,
            &belief.map.planets,
            &self.mask,
            self.settings.planets,
        );
        belief.residual_model = Some(ResidualModel::new(self.model, self.settings.planets));
        match residual::build(
            &belief.map.planets,
            &self.shots,
            &noise,
            self.settings.threads,
        ) {
            Ok(correction) => {
                belief.residual_correction = Some(correction);
                belief.residual_correction_unavailable_reason = None;
            }
            Err(gate) => belief.residual_correction_unavailable_reason = Some(gate),
        }
        self.stats.add(Part::ResidualCorrection, started.elapsed());
        if polished && theta.len() >= self.settings.cap() {
            self.cap_magnitudes
                .push(belief.residual_correction.as_ref().map(|c| c.max_magnitude));
            if self.cap_magnitudes.len() > COUNT_LIMIT_WINDOW {
                self.cap_magnitudes.remove(0);
            }
            if self.count_limited_at.is_none() && count_limited(&self.cap_magnitudes) {
                self.count_limited_at = Some(self.searched);
            }
        }
        belief.count_limit = Some(self.count_limit(theta.len()));
        self.last_report = Some(self.stats.report(&self.settings.ceilings));
        self.belief = Some(belief.clone());
        belief
    }
}

/// The residual correction (`residual.rs`) of `map` from the paths `observations` of its epoch,
/// for replays and recorded decisions. `model` is the recorded model-error scalars (with the
/// planet count they were used with); without one they are estimated from the last
/// `noise::WINDOW` paths' residuals against `map` (not prequential like the live estimator's, so
/// the scale can come out lower), with the count unknown.
pub fn residual_correction(
    map: &PlanetMap,
    observations: &[ShotObservation],
    model: Option<ResidualModel>,
) -> (ResidualModel, Result<ResidualCorrection, ResidualGate>) {
    let world = WorldParameters::default();
    let shots: Vec<Shot> = observations
        .iter()
        .map(|obs| Shot::new(obs.clone(), None, world))
        .collect();
    let model = model.unwrap_or_else(|| {
        let noise = Noise::new(ModelError::PRIOR, &map.planets, &shots, None);
        let series: Vec<noise::Series> = shots
            .iter()
            .rev()
            .filter_map(|shot| shot.path.as_ref())
            .take(noise::WINDOW)
            .map(|path| noise::Series::new(path, &map.planets, &noise))
            .collect();
        ResidualModel::new(noise::estimate(&series, None), None)
    });
    let noise = Noise::new(model.into(), &map.planets, &shots, model.total_planets);
    (
        model,
        residual::build(&map.planets, &shots, &noise, cores()),
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod measurements;

#[cfg(test)]
mod diagnose;
