//! Planet-map estimator (v2). The default belief is a flat world (no planets); a planet is
//! admitted only when trajectory evidence supports it.
//!
//! Slice P1 considers K in {0, 1} planets. Each K is fitted separately: K=0 is the flat model,
//! K=1 is one planet fitted by multi-start least squares (`fit`). The two are compared with an
//! approximate evidence (Laplace/Occam factor on noise-floor-normalized trajectory residuals)
//! and a prior that favours K=0 by 20:1. Distinct supported modes are kept (at most
//! `MAX_MODES`) with normalized evidence weights. **These weights are heuristic rankings, not
//! calibrated probabilities.** The published `map` is the maximum-weight mode.
//!
//! Surface contacts (`contacts`) are extra evidence: they start some fits and add a surface
//! constraint to the K=1 fit, but never create a planet. Oracle planets are never an input.
//!
//! Contact terms (and the interior-sample terms that forbid a regular sample inside the disk)
//! enter only the planet's chi-square; the flat model has no such terms. A hit can therefore
//! lower a planet's evidence relative to flat but never raise it: a contact never helps admit a
//! planet, only trajectory curvature does.
mod contacts;
mod eval;
mod fit;

pub use eval::{position_rmse, shot_observations, shot_position_rmse};

use crate::map::PlanetMap;
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::{MapBelief, WeightedMap};
use fit::{CONTACT_FULL, Mode, Problem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// At most this many modes (including the flat one) are retained.
const MAX_MODES: usize = 4;
/// Modes whose log evidence trails the best by more than this are dropped (relative weight
/// below about 1e-3).
const RETAIN_NATS: f64 = 7.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CovarianceUnavailableReason {
    /// No covariance was computed (the belief did not come from the estimator).
    NotComputed,
    /// The published map has no planets: uncertainty about planets not yet found is not modelled.
    NoPlanets,
    /// The data leave the planet's parameters (nearly) undetermined, or the mode sits at a bound.
    UnderIdentified,
}

/// A completed trajectory plus its possible terminal contact, classified with the players known
/// when it finished.
#[derive(Clone, Debug)]
struct Shot {
    obs: ShotObservation,
    contact: Option<contacts::Contact>,
}

#[derive(Clone, Default)]
pub struct Estimator {
    players: BTreeMap<u32, Point>,
    shots: Vec<Shot>,
    /// `[x, y, radius]` of the previous fit's planet modes; warm starts for the next fit.
    warm_starts: Vec<[f64; 3]>,
    belief: Option<MapBelief>,
    /// Shots were added since the last published belief: the next event retries the fit even if it
    /// brings nothing new (a cancelled or failed fit must not stall the estimator).
    stale: bool,
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
                    self.shots.push(Shot { obs, contact });
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
                eprintln!(
                    "estimator: simulation failed; the fit will be retried on the next event"
                );
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
        let players = std::mem::take(&mut self.players);
        *self = Self::default();
        self.players = players;
    }

    /// Fit K=0 and K=1, rank the modes and build the belief. `None` when cancelled or when a
    /// path cannot be simulated.
    fn infer(&mut self, world: WorldParameters, cancelled: &AtomicBool) -> Option<MapBelief> {
        let problem = Problem {
            shots: &self.shots,
            world,
            cancelled,
            use_contacts: true,
        };
        let mut modes = vec![Mode {
            planet: None,
            log_evidence: -0.5 * problem.flat_chi2()?,
            covariance: None,
        }];
        modes.extend(fit::planet_modes(&problem, &self.warm_starts)?);
        // One ranking pass: best first, coinciding modes merged, weak ones dropped.
        modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
        let best = modes[0].log_evidence;
        let mut kept: Vec<Mode> = Vec::new();
        for mode in modes {
            let duplicate = kept
                .iter()
                .any(|other| match (&other.planet, &mode.planet) {
                    (Some(a), Some(b)) => fit::same_mode(fit::theta_of(a), fit::theta_of(b)),
                    _ => false,
                });
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
            planets: mode.planet.clone().into_iter().collect(),
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
        belief.covariance_unavailable_reason = match (&top.planet, &top.covariance) {
            (None, _) => Some(CovarianceUnavailableReason::NoPlanets),
            (Some(_), None) => Some(CovarianceUnavailableReason::UnderIdentified),
            (Some(planet), Some(covariance)) => {
                belief.parameter_covariance = Some(fit::covariance_xym(covariance, planet.radius));
                None
            }
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
        self.warm_starts = kept
            .iter()
            .filter_map(|mode| mode.planet.as_ref().map(fit::theta_of))
            .collect();
        Some(belief)
    }
}

#[cfg(test)]
mod tests;
