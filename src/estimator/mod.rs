//! Planet-map estimator (v2). The default belief is a flat world (no planets); a planet is
//! admitted only when trajectory evidence supports it.
//!
//! Estimation compares the model's gravity field with the field implied by the observed paths
//! (`window`: windowed velocity-change comparison with the engine's exact discrete kernel and the
//! f32 rounding covariance); no dynamics are simulated in the fit. `fit` jointly fits K planets
//! to every path, and `search` grows and shrinks K on the approximate evidence (Laplace/Occam
//! with a prior of 20:1 against each extra planet, so K=0 is favoured). Distinct supported modes
//! are kept (at most `MAX_MODES`) with normalized evidence weights. **These weights are heuristic
//! rankings, not calibrated probabilities.** The published `map` is the maximum-weight mode.
//!
//! Surface contacts (`contacts`) are extra evidence only: a gated surface constraint on the
//! joint fit that never creates a planet. Contact terms enter only planet-bearing models; the
//! flat model has none. A hit can therefore lower a planet's evidence relative to flat but never
//! raise it. Oracle planets are never an input.
mod contacts;
mod eval;
mod fit;
mod search;
mod window;

pub use eval::{position_rmse, shot_observations, shot_position_rmse};

use crate::map::PlanetMap;
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::{MapBelief, WeightedMap};
use fit::CONTACT_FULL;
use search::{Limits, Mode, same_map};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use window::PathData;

/// At most this many modes (including the flat one) are retained.
const MAX_MODES: usize = 4;
/// Modes whose log evidence trails the best by more than this are dropped (relative weight
/// below about 1e-3).
const RETAIN_NATS: f64 = 7.0;
/// Position-noise levels (world units, on top of f32 rounding) the fit goes through, coarse to
/// fine; the last is the scored likelihood. PROVISIONAL, NOT MEASURED: the measured residual
/// scale against the disclosed truth (`measure_residual_scale_on_live_captures`) is f32 rounding
/// only, i.e. 0, but with 0 (or an annealed 0.3 -> 0.03 -> 0) the multi-planet scorecard cases are
/// far worse than with a single 0.1 level (docs/estimator-evaluation.md). Needs a decision.
const NOISE_LEVELS: [f64; 1] = [0.1];
/// Default cap on the number of planets in a map.
pub const DEFAULT_MAX_PLANETS: usize = 24;
/// Default wall-clock budget per update for growing and shrinking K.
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_secs(5);

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

/// Why the search for more (or fewer) planets stopped before it ran out of proposals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLimit {
    /// The best map already has the maximum number of planets.
    PlanetCap,
    /// The per-update time budget ran out.
    TimeBudget,
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
    fn new(
        obs: ShotObservation,
        contact: Option<contacts::Contact>,
        world: WorldParameters,
    ) -> Self {
        let path = PathData::new(
            &obs.points,
            obs.angle_degrees,
            obs.velocity,
            world,
            &NOISE_LEVELS,
        );
        Self { obs, contact, path }
    }
}

#[derive(Clone)]
pub struct Estimator {
    players: BTreeMap<u32, Point>,
    shots: Vec<Shot>,
    /// Planets of the previous update's retained modes; warm starts for the next update.
    warm_starts: Vec<Vec<crate::protocol::Planet>>,
    belief: Option<MapBelief>,
    /// Shots were added since the last published belief: the next event retries the fit even if it
    /// brings nothing new (a cancelled or failed fit must not stall the estimator).
    stale: bool,
    /// Maximum number of planets in a map.
    pub max_planets: usize,
    /// Wall-clock budget per update for growing and shrinking K; the best belief found so far is
    /// published when it runs out.
    pub time_budget: Duration,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            players: BTreeMap::new(),
            shots: Vec::new(),
            warm_starts: Vec::new(),
            belief: None,
            stale: false,
            max_planets: DEFAULT_MAX_PLANETS,
            time_budget: DEFAULT_TIME_BUDGET,
        }
    }
}

impl Estimator {
    /// Ingest an ordered group of events, then fit once if there are unpublished trajectories.
    /// Returns `None` when nothing changed or the fit was cancelled or failed; the fit is then
    /// retried on the next event.
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
                ServerEvent::Planets { .. } => self.reset(),
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
                    let contact = contacts::classify(&obs.points, &players, world);
                    self.shots.push(Shot::new(obs, contact, world));
                    self.stale = true;
                }
                _ => {}
            }
        }
        if !self.stale {
            return None;
        }
        let Some(belief) = self.infer(world, cancelled) else {
            if !cancelled.load(Ordering::SeqCst) {
                eprintln!("estimator: the fit failed; it will be retried on the next event");
            }
            return None;
        };
        self.stale = false;
        self.belief = Some(belief.clone());
        Some(belief)
    }

    /// Sequential replay fits after each event.
    pub fn observe(&mut self, event: &ServerEvent, cancelled: &AtomicBool) -> Option<MapBelief> {
        self.observe_batch(std::slice::from_ref(event), cancelled)
    }

    pub fn belief(&self) -> Option<&MapBelief> {
        self.belief.as_ref()
    }

    /// Forget the map epoch's evidence. Player positions survive: the server announces a
    /// player's new position just before that player's `Planets` event.
    pub fn reset(&mut self) {
        *self = Self {
            players: std::mem::take(&mut self.players),
            max_planets: self.max_planets,
            time_budget: self.time_budget,
            ..Self::default()
        };
    }

    /// Search K, rank the modes and build the belief. `None` when cancelled or when the flat model
    /// cannot be evaluated.
    fn infer(&mut self, world: WorldParameters, cancelled: &AtomicBool) -> Option<MapBelief> {
        let limits = Limits {
            max_planets: self.max_planets,
            deadline: Instant::now() + self.time_budget,
        };
        let (mut modes, limit) =
            search::search(&self.shots, world, cancelled, &self.warm_starts, &limits)?;
        // One ranking pass: best first, coinciding modes merged, weak ones dropped.
        modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
        let best = modes[0].log_evidence;
        let mut kept: Vec<Mode> = Vec::new();
        for mode in modes {
            let duplicate = kept
                .iter()
                .any(|other| same_map(&other.planets, &mode.planets));
            if !duplicate && mode.log_evidence >= best - RETAIN_NATS && kept.len() < MAX_MODES {
                kept.push(mode);
            }
        }
        let total: f64 = kept
            .iter()
            .map(|mode| (mode.log_evidence - best).exp())
            .sum();
        let weight = |mode: &Mode| (mode.log_evidence - best).exp() / total;
        let map_of = |mode: &Mode| PlanetMap {
            planets: mode.planets.clone(),
        };

        let top = &kept[0];
        let mut belief = MapBelief::new(map_of(top), None);
        belief.map_weight = Some(weight(top));
        belief.alternatives = kept[1..]
            .iter()
            .map(|mode| WeightedMap {
                map: map_of(mode),
                weight: weight(mode),
            })
            .collect();
        belief.covariance_unavailable_reason = match (top.planets.is_empty(), &top.covariance) {
            (true, _) => Some(CovarianceUnavailableReason::NoPlanets),
            (false, None) => Some(CovarianceUnavailableReason::UnderIdentified),
            (false, Some(covariance)) => {
                belief.parameter_covariance = Some(fit::covariance_xym(covariance, &top.planets));
                None
            }
        };
        belief.search_limit = limit;

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
        self.warm_starts = kept.into_iter().map(|mode| mode.planets).collect();
        Some(belief)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod measurements;
