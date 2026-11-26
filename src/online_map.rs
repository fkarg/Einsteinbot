//! Authoritative live physical planet map, seeded by contacts and refined by trajectories.
use crate::contacts::ContactMap;
use crate::map::PlanetMap;
use crate::planet_fit::{
    ContactConstraint, PlanetFitProfile, fit_planet_map, fit_planet_map_profiled,
};
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub struct OnlineStageProfile {
    pub prepare: Duration,
    pub planet_fit: Duration,
    pub fit_contact_seeding: Duration,
    pub fit_optimizer: Duration,
    pub fit_final_residual: Duration,
    pub fit_final_jacobian_and_covariance: Duration,
    pub fit_path_validation: Duration,
    pub fit_planets: usize,
    pub fit_observations: usize,
    pub fit_trajectory_points: usize,
    pub fit_contact_constraints: usize,
    pub fit_parameters: usize,
    pub fit_optimizer_evaluations: usize,
    pub fit_patience: Option<usize>,
    pub fit_optimizer_converged: Option<bool>,
    pub fit_candidate_accepted: Option<bool>,
}

#[derive(Clone, Default)]
pub struct OnlineEstimator {
    contacts: ContactMap,
    players: BTreeMap<u32, Point>,
    observations: Vec<ShotObservation>,
    belief: Option<MapBelief>,
}

impl OnlineEstimator {
    /// Ingest an ordered group of events, then fit once if it contains new trajectories.
    pub fn observe_batch(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        self.observe_batch_inner(events, cancelled, None, None)
    }

    /// Ingest one event with a caller-selected fit effort and return stage timings.
    /// This is intended for offline profiling and controlled fit-budget experiments.
    pub fn observe_profiled(
        &mut self,
        event: &ServerEvent,
        cancelled: &AtomicBool,
        fit_patience: usize,
    ) -> (Option<MapBelief>, OnlineStageProfile) {
        let mut stages = OnlineStageProfile::default();
        let belief = self.observe_batch_inner(
            std::slice::from_ref(event),
            cancelled,
            Some(&mut stages),
            Some(fit_patience),
        );
        (belief, stages)
    }

    fn observe_batch_inner(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
        mut stages: Option<&mut OnlineStageProfile>,
        fit_patience: Option<usize>,
    ) -> Option<MapBelief> {
        let prepare_started = stages.as_ref().map(|_| Instant::now());
        let mut changed = false;
        for event in events {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            match event {
                ServerEvent::Planets { .. } => {
                    self.reset();
                    changed = false;
                    continue;
                }
                ServerEvent::PlayerPosition { player_id, x, y } => {
                    self.players.insert(*player_id, Point { x: *x, y: *y });
                    continue;
                }
                ServerEvent::PlayerLeave { player_id } => {
                    self.players.remove(player_id);
                    continue;
                }
                ServerEvent::ShotFin { .. } => {}
                _ => continue,
            }
            let Ok(shot) = ShotObservation::try_from(event) else {
                continue;
            };
            if self.observations.contains(&shot) {
                continue;
            }
            let players = self.players.values().cloned().collect::<Vec<_>>();
            self.contacts
                .observe(&shot.points, &players, WorldParameters::default());
            self.observations.push(shot);
            changed = true;
        }
        if !changed {
            return None;
        }
        let active = self
            .contacts
            .hypotheses()
            .iter()
            .filter(|h| !h.contradicted && !h.association_ambiguous)
            .collect::<Vec<_>>();
        let map = PlanetMap {
            planets: active.iter().map(|h| h.planet.clone()).collect(),
        };
        let contact_constraints = active
            .iter()
            .enumerate()
            .flat_map(|(planet_index, h)| {
                h.contacts.iter().map(move |point| ContactConstraint {
                    planet_index,
                    point: point.clone(),
                })
            })
            .chain(unassigned_surface_constraints(
                &map.planets,
                &self
                    .contacts
                    .hypotheses()
                    .iter()
                    .flat_map(|hypothesis| hypothesis.contacts.iter().cloned())
                    .collect::<Vec<_>>(),
                self.contacts.contact_points(),
            ))
            .collect::<Vec<_>>();
        let mut belief = MapBelief::new(map, None);
        if let (Some(profile), Some(started)) = (stages.as_deref_mut(), prepare_started) {
            profile.prepare += started.elapsed();
        }
        if !belief.map.planets.is_empty() {
            belief.covariance_unavailable_reason =
                Some(crate::estimator::CovarianceUnavailableReason::FitFailed);
            let fit_started = stages.as_ref().map(|_| Instant::now());
            let fit_result = if let Some(profile) = stages.as_deref_mut() {
                let mut fit_profile = PlanetFitProfile::default();
                let result = fit_planet_map_profiled(
                    &belief.map,
                    &self.observations,
                    &contact_constraints,
                    WorldParameters::default(),
                    cancelled,
                    &mut fit_profile,
                    fit_patience.unwrap_or(100),
                );
                profile.fit_planets = fit_profile.planets;
                profile.fit_observations = fit_profile.observations;
                profile.fit_trajectory_points = fit_profile.trajectory_points;
                profile.fit_contact_constraints = fit_profile.contact_constraints;
                profile.fit_parameters = fit_profile.parameters;
                profile.fit_optimizer_evaluations = fit_profile.optimizer_evaluations;
                profile.fit_patience = fit_patience;
                profile.fit_optimizer_converged = fit_profile.optimizer_converged;
                profile.fit_candidate_accepted = Some(result.is_ok());
                profile.fit_contact_seeding += fit_profile.contact_seeding;
                profile.fit_optimizer += fit_profile.optimizer;
                profile.fit_final_residual += fit_profile.final_residual;
                profile.fit_final_jacobian_and_covariance +=
                    fit_profile.final_jacobian_and_covariance;
                profile.fit_path_validation += fit_profile.path_validation;
                result
            } else {
                fit_planet_map(
                    &belief.map,
                    &self.observations,
                    &contact_constraints,
                    WorldParameters::default(),
                    cancelled,
                )
            };
            if let (Some(profile), Some(started)) = (stages.as_deref_mut(), fit_started) {
                profile.planet_fit += started.elapsed();
            }
            match fit_result {
                Ok(fit) => {
                    self.contacts.refine_active_planets(&fit.map.planets);
                    let refined_map = PlanetMap {
                        planets: self
                            .contacts
                            .hypotheses()
                            .iter()
                            .filter(|h| !h.contradicted && !h.association_ambiguous)
                            .map(|h| h.planet.clone())
                            .collect(),
                    };
                    // Refinement is expected to change floating-point geometry.
                    // Only promoted/removed hypotheses change the fitted topology.
                    let same_topology = refined_map.planets.len() == fit.map.planets.len();
                    belief.map = refined_map;
                    belief.fit_rank = Some(fit.rank);
                    belief.fit_parameter_count = Some(fit.map.planets.len() * 3);
                    belief.fit_independent_path_count = Some(fit.independent_paths);
                    belief.field_evidence = if same_topology {
                        fit.evidence_resolution.zip(fit.parameter_to_physical).map(
                            |(resolution, parameter_to_physical)| {
                                std::sync::Arc::new(crate::strategy::FieldEvidence {
                                    resolution,
                                    parameter_to_physical,
                                })
                            },
                        )
                    } else {
                        None
                    };
                    let fit_converged = same_topology
                        && fit.converged
                        && fit.rank == fit.map.planets.len() * 3
                        && fit.independent_paths > fit.rank
                        && fit.covariance_unavailable_reason.is_none()
                        && fit.covariance.is_some();
                    let (covariance, approximate_covariance) = if fit_converged {
                        (fit.covariance, None)
                    } else if same_topology {
                        (None, fit.approximate_covariance.or(fit.covariance))
                    } else {
                        (None, None)
                    };
                    belief.parameter_covariance = covariance;
                    belief.approximate_parameter_covariance = approximate_covariance;
                    belief.covariance_unavailable_reason = if !same_topology {
                        Some(crate::estimator::CovarianceUnavailableReason::TopologyChanged)
                    } else if !fit.converged {
                        Some(crate::estimator::CovarianceUnavailableReason::FitDidNotConverge)
                    } else {
                        fit.covariance_unavailable_reason
                    };
                    belief.fit_converged = Some(fit_converged);
                    belief.fit_approximate = Some(!fit_converged);
                    belief.optimizer_converged = Some(fit.converged);
                    belief.model_position_rmse = same_topology.then_some(fit.position_rmse);
                    eprintln!(
                        "LivePlanetFit planets={} observations={} position_rmse={:.4} contact_rmse={:.4} rank={} covariance={} approximate_covariance={} fit_converged={} optimizer_converged={}",
                        belief.map.planets.len(),
                        self.observations.len(),
                        fit.position_rmse,
                        fit.contact_rmse,
                        fit.rank,
                        belief.parameter_covariance.is_some(),
                        belief.approximate_parameter_covariance.is_some(),
                        fit_converged,
                        fit.converged
                    );
                }
                Err(error) => {
                    belief.fit_converged = Some(false);
                    belief.fit_approximate = Some(true);
                    // A failed fit may stop before or after the optimizer; no raw
                    // optimizer termination was returned by the fitting API.
                    belief.optimizer_converged = None;
                    eprintln!("LivePlanetFitError {error}; retaining contact-seeded partial map")
                }
            }
        } else {
            belief.covariance_unavailable_reason =
                Some(crate::estimator::CovarianceUnavailableReason::NoPlanetParameters);
            // No fitted model exists to call converged or approximate.
            belief.fit_converged = None;
            belief.fit_approximate = None;
            belief.optimizer_converged = None;
        }
        if cancelled.load(Ordering::SeqCst) {
            return None;
        }
        belief.planet_contact_counts = self
            .contacts
            .hypotheses()
            .iter()
            .filter(|h| !h.contradicted && !h.association_ambiguous)
            .map(|h| h.contacts.len())
            .collect();
        belief.uncertain_associations = self
            .contacts
            .hypotheses()
            .iter()
            .filter(|h| h.association_ambiguous)
            .count();
        belief.contact_points = self.contacts.contact_points().to_vec();
        if cancelled.load(Ordering::SeqCst) {
            return None;
        }
        self.belief = Some(belief.clone());
        Some(belief)
    }

    /// Sequential replay fits after each event, preserving the historical decision boundary.
    pub fn observe(&mut self, event: &ServerEvent, cancelled: &AtomicBool) -> Option<MapBelief> {
        self.observe_batch(std::slice::from_ref(event), cancelled)
    }
    pub fn belief(&self) -> Option<&MapBelief> {
        self.belief.as_ref()
    }
    pub fn reset(&mut self) {
        let players = std::mem::take(&mut self.players);
        *self = Self::default();
        self.players = players;
    }
}

/// Reuse terminal points that were classified as likely planet contacts even when
/// their short terminal acceleration trace could not seed a separate contact fit.
/// A point only constrains a planet when it lies within 8 units of exactly one
/// estimated surface; this prevents a bare endpoint from inventing geometry or
/// binding to either member of a crowded pair. The tolerance is a heuristic.
fn unassigned_surface_constraints<'a>(
    active: &'a [crate::protocol::Planet],
    owned_points: &'a [Point],
    contact_points: &'a [Point],
) -> impl Iterator<Item = ContactConstraint> + 'a {
    const MAX_SURFACE_ERROR: f64 = 8.0;
    contact_points.iter().filter_map(move |point| {
        if owned_points.contains(point) {
            return None;
        }
        let mut nearest: Option<(usize, f64)> = None;
        let mut second_error = f64::INFINITY;
        for (planet_index, planet) in active.iter().enumerate() {
            let error = ((f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y)
                - planet.radius)
                .abs();
            if error < nearest.map_or(f64::INFINITY, |(_, best)| best) {
                second_error = nearest.map_or(f64::INFINITY, |(_, best)| best);
                nearest = Some((planet_index, error));
            } else {
                second_error = second_error.min(error);
            }
        }
        let (planet_index, error) = nearest?;
        (error <= MAX_SURFACE_ERROR && second_error > MAX_SURFACE_ERROR).then(|| {
            ContactConstraint {
                planet_index,
                point: point.clone(),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::shot_position_rmse;
    use crate::protocol::Planet;
    use crate::simulation::simulate_shot;

    fn contact(x: f64) -> ServerEvent {
        let map = PlanetMap {
            planets: vec![Planet {
                x,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 3.0,
            points: vec![Point {
                x: (x - 90.0) as f32,
                y: 500.0,
            }],
        };
        let path = simulate_shot(&map, &shot, WorldParameters::default(), 180).unwrap();
        ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 3.0,
            points: path.points,
        }
    }

    #[test]
    fn bare_terminal_point_constrains_one_nearby_estimated_surface() {
        let planets = vec![
            Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            },
            Planet {
                x: 700.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            },
        ];
        let point = Point { x: 670.0, y: 500.0 };
        let constraints =
            unassigned_surface_constraints(&planets, &[], std::slice::from_ref(&point))
                .collect::<Vec<_>>();
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].planet_index, 1);
        assert_eq!(constraints[0].point, point);
    }

    #[test]
    fn terminal_point_is_not_reassigned_when_owned_ambiguous_or_far_from_surface() {
        let planets = vec![
            Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            },
            Planet {
                x: 545.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            },
        ];
        let ambiguous = Point { x: 522.5, y: 500.0 };
        let distant = Point { x: 500.0, y: 461.0 };
        let owned = Point { x: 500.0, y: 470.0 };
        let candidates = [ambiguous.clone(), distant.clone(), owned.clone()];
        let constraints =
            unassigned_surface_constraints(&planets, std::slice::from_ref(&owned), &candidates)
                .collect::<Vec<_>>();
        assert!(constraints.is_empty());
    }

    #[test]
    fn a_contact_becomes_gravity_evidence_without_oracle_and_duplicate_reset_are_safe() {
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 410.0,
                y: 500.0,
            },
            &cancelled,
        );
        let event = contact(500.0);
        let belief = estimator
            .observe(&event, &cancelled)
            .expect("contact seeded physical map");
        assert_eq!(belief.map.planets.len(), 1);
        assert_eq!(belief.fit_approximate, Some(true));
        assert_eq!(belief.fit_converged, Some(false));
        assert!(belief.optimizer_converged.is_some());
        assert!(belief.field_evidence.is_some());
        assert!(belief.approximate_parameter_covariance.is_some());
        assert!(belief.parameter_covariance.is_none());
        let planet = &belief.map.planets[0];
        assert_eq!(planet.mass, planet.radius.powi(3) / 10.0);
        let shot = ShotObservation::try_from(&event).unwrap();
        let learned = shot_position_rmse(&belief.map, &shot, WorldParameters::default())
            .unwrap()
            .unwrap();
        let flat = shot_position_rmse(
            &PlanetMap { planets: vec![] },
            &shot,
            WorldParameters::default(),
        )
        .unwrap()
        .unwrap();
        assert!(learned < flat / 10.0, "learned={learned} flat={flat}");
        assert!(estimator.observe(&event, &cancelled).is_none());
        assert_eq!(estimator.observations.len(), 1);
        estimator.reset();
        assert!(estimator.belief().is_none());
        assert!(estimator.observations.is_empty());
    }

    #[test]
    fn reset_preserves_previously_received_player_positions_for_contact_classification() {
        // Upstream sends our respawn position before our Planets epoch notification.
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        let event = contact(500.0);
        let ServerEvent::ShotFin { points, .. } = &event else {
            unreachable!()
        };
        let endpoint = points.last().unwrap();
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 2,
                x: endpoint.x,
                y: endpoint.y,
            },
            &cancelled,
        );
        estimator.reset();
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 100.0,
                y: 100.0,
            },
            &cancelled,
        );
        let belief = estimator.observe(&event, &cancelled).unwrap();
        assert!(
            belief.map.planets.is_empty(),
            "known player termination must not seed a planet after reset"
        );
        assert!(belief.contact_points.is_empty());
    }

    #[test]
    fn planet_reset_inside_batch_discards_old_map_training_paths() {
        let mut estimator = OnlineEstimator::default();
        let cancel = AtomicBool::new(false);
        estimator.observe(&contact(600.0), &cancel);
        assert!(!estimator.observations.is_empty());

        let empty = PlanetMap { planets: vec![] };
        let launch = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let path = simulate_shot(&empty, &launch, WorldParameters::default(), 30).unwrap();
        estimator.observe_batch(
            &[
                ServerEvent::Planets { planets: vec![] },
                ServerEvent::ShotFin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                    points: path.points,
                },
            ],
            &cancel,
        );

        assert_eq!(estimator.observations.len(), 1);
        assert!(estimator.contacts.hypotheses().is_empty());
    }

    #[test]
    fn batched_contacts_use_player_positions_at_each_shot() {
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        let first = contact(500.0);
        let second = contact(800.0);
        let ServerEvent::ShotFin {
            points: first_points,
            ..
        } = &first
        else {
            unreachable!()
        };
        let ServerEvent::ShotFin {
            points: second_points,
            ..
        } = &second
        else {
            unreachable!()
        };
        let target = first_points.last().unwrap();
        let events = vec![
            ServerEvent::PlayerPosition {
                player_id: 1,
                x: 100.0,
                y: 100.0,
            },
            ServerEvent::PlayerPosition {
                player_id: 2,
                x: target.x,
                y: target.y,
            },
            first.clone(),
            ServerEvent::PlayerPosition {
                player_id: 2,
                x: 1500.0,
                y: 1000.0,
            },
            second.clone(),
        ];
        let belief = estimator.observe_batch(&events, &cancelled).unwrap();
        assert_eq!(estimator.observations.len(), 2);
        assert_eq!(
            belief.contact_points,
            vec![second_points.last().unwrap().clone()]
        );
        assert_eq!(belief.map.planets.len(), 1);
    }

    #[test]
    fn physical_planet_registry_can_grow_beyond_four_sources() {
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        for x in [200.0, 500.0, 800.0, 1100.0, 1400.0] {
            estimator.observe(
                &ServerEvent::PlayerPosition {
                    player_id: 1,
                    x: (x - 90.0) as f32,
                    y: 500.0,
                },
                &cancelled,
            );
            estimator.observe(&contact(x), &cancelled);
        }
        assert_eq!(estimator.belief().unwrap().map.planets.len(), 5);
    }
}
