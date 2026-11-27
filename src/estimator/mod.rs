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
//! Surface contacts (`contacts`) are extra evidence only: a gated surface constraint on the
//! joint fit that never creates a planet. Contact terms enter only planet-bearing models; the
//! flat model has none. Oracle planets are never an input.
mod audit;
mod contacts;
mod eval;
mod fit;
mod noise;
mod rebuild;
mod rows;
mod search;
mod window;
mod work;

pub use eval::{position_rmse, shot_observations, shot_position_rmse};
pub use work::{CeilingViolation, Part, PartCeilings, PartReport, UpdateReport};

use crate::map::PlanetMap;
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use audit::{Audit, Stage};
use fit::{CONTACT_FULL, Evidence, Fit, planet_at};
use noise::{ModelError, Noise};
use search::{Search, Slice};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use window::PathData;
use work::{Stats, cores};

/// Default cap on the number of planets in a map.
pub const DEFAULT_MAX_PLANETS: usize = 24;
/// Default evaluations of one search slice.
pub const DEFAULT_SLICE_EVALUATIONS: u64 = 300;
/// Default wall-clock ceiling of one search slice.
pub const DEFAULT_SLICE_CEILING: Duration = Duration::from_millis(250);
/// Default search slices owed after each new path.
pub const DEFAULT_SLICES_PER_PATH: usize = 6;
/// Evaluations of the full-row refit of the published map.
const POLISH_EVALUATIONS: u64 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CovarianceUnavailableReason {
    /// No covariance was computed (the belief did not come from the estimator).
    NotComputed,
    /// The published map has no planets: uncertainty about planets not yet found is not modelled.
    NoPlanets,
    /// The data leave the planets' parameters (nearly) undetermined, or a planet sits at a bound.
    UnderIdentified,
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
    /// Maximum number of planets in a map.
    pub max_planets: usize,
    /// Evaluations of one search slice (checked between batches of refits, so a slice may exceed
    /// it a little). Deterministic: the same paths and limits give the same belief whatever the
    /// load, as long as `slice_ceiling` does not cut a slice and no audit is adopted.
    pub slice_evaluations: u64,
    /// Wall-clock ceiling of a slice (`None`: no ceiling); the best map so far is kept when it is hit.
    pub slice_ceiling: Option<Duration>,
    /// Slices owed after each new path.
    pub slices_per_path: usize,
    /// Threads of the live search; the audit uses half of them.
    pub threads: usize,
    /// Run the background audit. Off by default, because an adopted audit depends on timing; the
    /// live map turns it on.
    pub audit_enabled: bool,
    /// Wall-clock ceilings per part of the update; exceeded ones are listed in `last_report`.
    pub ceilings: PartCeilings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_planets: DEFAULT_MAX_PLANETS,
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
    fn slice<'a>(
        &'a self,
        cancelled: &'a AtomicBool,
        stats: &'a Stats,
        noise: &'a Noise,
    ) -> Slice<'a> {
        Slice {
            noise,
            evaluations: self.slice_evaluations,
            deadline: Instant::now() + self.slice_ceiling.unwrap_or(Duration::from_secs(24 * 3600)),
            threads: self.threads,
            max_planets: self.max_planets,
            cancelled,
            stats,
        }
    }
}

pub struct Estimator {
    players: BTreeMap<u32, Point>,
    /// Every path of the map epoch, with the measurement floor as their noise.
    shots: Vec<Shot>,
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
    /// Why the search has work left, if it has.
    limit: Option<SearchLimit>,
    pub settings: Settings,
    stats: Stats,
    /// Work and timing of the most recent update (a path's local refit and the slices since).
    last_report: Option<UpdateReport>,
    /// Window-build and contact-classification times of shots ingested since the last update.
    pending_timings: Vec<(Part, Duration)>,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            players: BTreeMap::new(),
            shots: Vec::new(),
            series: Vec::new(),
            model: ModelError::PRIOR,
            search: Search::new(),
            searched: 0,
            audit: None,
            belief: None,
            slices_left: 0,
            polish_due: false,
            limit: None,
            settings: Settings::default(),
            stats: Stats::default(),
            last_report: None,
            pending_timings: Vec::new(),
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
            // The model-error estimate from the earlier paths' prequential residuals: a path is
            // fitted (its local refit and the slices after it) under noise that has not seen it,
            // so its own unexplained signature cannot widen the noise it is fitted with. Then its
            // residuals against the published map join the window for the next path.
            let started = Instant::now();
            let published = self.published_planets();
            let mut noise = Noise::new(self.model, &published, &self.shots[..=self.searched]);
            let current = self.shots[self.searched]
                .path
                .as_ref()
                .map(|path| noise::Series::new(path, &published, &noise));
            self.model = noise::estimate(&self.series, current.as_ref());
            noise.model = self.model;
            if let Some(series) = current {
                self.series.push(series);
                if self.series.len() > noise::WINDOW {
                    self.series.remove(0);
                }
            }
            self.stats.add(Part::Noise, started.elapsed());
            let slice = self.settings.slice(cancelled, &self.stats, &noise);
            self.search.add_path(&self.shots[self.searched], &slice);
            self.searched += 1;
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
                self.settings.max_planets,
                self.audit_threads(),
            ));
        }
        let theta = self.search.theta.clone();
        Some(self.publish(&theta, None))
    }

    /// Sequential replay: one event.
    pub fn observe(&mut self, event: &ServerEvent, cancelled: &AtomicBool) -> Option<MapBelief> {
        self.observe_batch(std::slice::from_ref(event), cancelled)
    }

    /// The most recently published belief.
    pub fn belief(&self) -> Option<&MapBelief> {
        self.belief.as_ref()
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
        if self.audit.as_ref().is_some_and(Audit::finished) {
            if let Some(belief) = self.collect_audit() {
                return Some(belief);
            }
        }
        if !self.search.idle() && self.slices_left > 0 {
            self.slices_left -= 1;
            let noise = self.noise();
            let slice = self.settings.slice(cancelled, &self.stats, &noise);
            let before = self.stats.evaluations();
            let changed = self.search.step(&slice);
            let spent = self.stats.evaluations() - before;
            self.limit = if self.search.idle() {
                None
            } else if spent >= self.settings.slice_evaluations {
                Some(SearchLimit::WorkBudget)
            } else {
                Some(SearchLimit::TimeBudget)
            };
            self.polish_due |= changed;
            if changed {
                let theta = self.search.theta.clone();
                return Some(self.publish(&theta, None));
            }
            return None;
        }
        if self.polish_due {
            self.polish_due = false;
            let started = Instant::now();
            let shots = noise::with_noise(&self.shots, &self.noise());
            let full = Fit::new(&shots, WorldParameters::default(), cancelled, true, vec![])
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
                // The covariance needs a Jacobian of all rows; a refit that used up its time
                // leaves it out (`not_computed`) rather than run over the ceiling.
                let evidence = deadline
                    .is_none_or(|deadline| Instant::now() < deadline)
                    .then(|| full.evidence(&theta))
                    .flatten();
                (theta, evidence)
            };
            self.stats.add(Part::Polish, started.elapsed());
            return Some(self.publish(&theta, evidence));
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
        Some(self.publish(&theta, None))
    }

    /// Planets of the published map (none before the first publication).
    fn published_planets(&self) -> Vec<crate::protocol::Planet> {
        self.belief
            .as_ref()
            .map(|b| b.map.planets.clone())
            .unwrap_or_default()
    }

    /// The noise of an operation starting now: the current model-error estimate, shaped by the
    /// published map and every path.
    fn noise(&self) -> Noise {
        Noise::new(self.model, &self.published_planets(), &self.shots)
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

    /// The belief of the map `theta`; `evidence` is its exact evidence when that was computed.
    fn publish(&mut self, theta: &[[f64; 3]], evidence: Option<Evidence>) -> MapBelief {
        let started = Instant::now();
        let world = WorldParameters::default();
        let planets: Vec<_> = theta.iter().map(|th| planet_at(*th)).collect();
        let mut belief = MapBelief::new(PlanetMap { planets }, None);
        belief.covariance_unavailable_reason = match (theta.is_empty(), evidence) {
            (true, _) => Some(CovarianceUnavailableReason::NoPlanets),
            (false, None) => Some(CovarianceUnavailableReason::NotComputed),
            (
                false,
                Some(Evidence {
                    covariance: None, ..
                }),
            ) => Some(CovarianceUnavailableReason::UnderIdentified),
            (
                false,
                Some(Evidence {
                    covariance: Some(covariance),
                    ..
                }),
            ) => {
                belief.parameter_covariance =
                    Some(fit::covariance_xym(&covariance, &belief.map.planets));
                None
            }
        };
        belief.search_limit = if theta.len() >= self.settings.max_planets {
            Some(SearchLimit::PlanetCap)
        } else {
            self.limit
        };
        let observations = self
            .shots
            .iter()
            .map(|shot| shot.obs.clone())
            .collect::<Vec<_>>();
        belief.model_position_rmse = position_rmse(&belief.map, &observations, world).ok();
        for contact in self.shots.iter().filter_map(|shot| shot.contact.as_ref()) {
            if !belief.contact_points.contains(&contact.point) {
                belief.contact_points.push(contact.point.clone());
            }
        }
        // Contacts beyond the full-strength range of every planet surface are not fully
        // explained by the map (a model mismatch, or planets this slice does not model).
        belief.unmatched_contacts = belief
            .contact_points
            .iter()
            .filter(|point| {
                !belief.map.planets.iter().any(|planet| {
                    ((f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y)
                        - planet.radius)
                        .abs()
                        <= CONTACT_FULL
                })
            })
            .count();
        self.stats.add(Part::Diagnostics, started.elapsed());
        self.last_report = Some(self.stats.report(&self.settings.ceilings));
        self.belief = Some(belief.clone());
        belief
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod measurements;

#[cfg(test)]
mod diagnose;
