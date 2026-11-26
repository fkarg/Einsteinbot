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
mod contacts;
mod eval;
mod fit;

pub use eval::{position_rmse, shot_observations, shot_position_rmse};

use crate::map::PlanetMap;
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::{MapBelief, WeightedMap};
use fit::{CONTACT_GATE, Problem};
use nalgebra::DMatrix;
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
    NotComputed,
    /// The published map has no planets: uncertainty about planets not yet found is not modelled.
    NoPlanets,
    /// The data leave the planet's parameters (nearly) undetermined.
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
}

struct Component {
    map: PlanetMap,
    log_evidence: f64,
    covariance: Option<nalgebra::Matrix3<f64>>,
    /// `[x, y, radius]`; `None` for the flat model.
    theta: Option<[f64; 3]>,
}

impl Estimator {
    /// Ingest an ordered group of events, then fit once if it contains new trajectories.
    /// Returns `None` when nothing changed or the fit was cancelled.
    pub fn observe_batch(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        let world = WorldParameters::default();
        let mut changed = false;
        for event in events {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            match event {
                ServerEvent::Planets { .. } => {
                    self.reset();
                    changed = false;
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
                    if self.shots.iter().any(|shot| shot.obs == obs) {
                        continue;
                    }
                    let players = self.players.values().cloned().collect::<Vec<_>>();
                    let contact = contacts::classify(&obs.points, &players, world);
                    self.shots.push(Shot { obs, contact });
                    changed = true;
                }
                _ => {}
            }
        }
        if !changed {
            return None;
        }
        let belief = self.infer(world, cancelled)?;
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

    fn infer(&mut self, world: WorldParameters, cancelled: &AtomicBool) -> Option<MapBelief> {
        let problem = Problem {
            shots: &self.shots,
            world,
            cancelled,
            use_contacts: true,
        };
        let flat = Component {
            map: PlanetMap { planets: vec![] },
            log_evidence: -0.5 * problem.flat_chi2()?,
            covariance: None,
            theta: None,
        };
        let mut components = vec![flat];
        components.extend(
            fit::planet_modes(&problem, &self.warm_starts)?
                .into_iter()
                .map(|mode| Component {
                    theta: Some([mode.planet.x, mode.planet.y, mode.planet.radius]),
                    map: PlanetMap {
                        planets: vec![mode.planet],
                    },
                    log_evidence: mode.log_evidence,
                    covariance: mode.covariance,
                }),
        );
        components.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
        let best = components[0].log_evidence;
        components.retain(|component| component.log_evidence >= best - RETAIN_NATS);
        components.truncate(MAX_MODES);
        let total: f64 = components
            .iter()
            .map(|component| (component.log_evidence - best).exp())
            .sum();
        let weight = |component: &Component| (component.log_evidence - best).exp() / total;

        let top = &components[0];
        let mut belief = MapBelief::new(top.map.clone(), None);
        belief.map_weight = Some(weight(top));
        belief.alternatives = components[1..]
            .iter()
            .map(|component| WeightedMap {
                map: component.map.clone(),
                weight: weight(component),
            })
            .collect();
        belief.covariance_unavailable_reason = match (&top.theta, &top.covariance) {
            (None, _) => Some(CovarianceUnavailableReason::NoPlanets),
            (Some(_), None) => Some(CovarianceUnavailableReason::UnderIdentified),
            (Some(theta), Some(covariance)) => {
                // [x, y, radius] -> [x, y, mass], mass = radius^3 / 10.
                let mut transform = DMatrix::identity(3, 3);
                transform[(2, 2)] = 0.3 * theta[2] * theta[2];
                let covariance = DMatrix::from_fn(3, 3, |i, j| covariance[(i, j)]);
                belief.parameter_covariance = Some(&transform * covariance * transform.transpose());
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
        belief.unmatched_contacts = belief
            .contact_points
            .iter()
            .filter(|point| {
                !belief.map.planets.iter().any(|planet| {
                    ((f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y)
                        - planet.radius)
                        .abs()
                        <= CONTACT_GATE
                })
            })
            .count();
        self.warm_starts = components.iter().filter_map(|c| c.theta).collect();
        Some(belief)
    }
}

#[cfg(test)]
mod tests;
