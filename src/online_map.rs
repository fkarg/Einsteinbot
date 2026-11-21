//! Authoritative live physical planet map, seeded by contacts and refined by trajectories.
use crate::contacts::ContactMap;
use crate::map::PlanetMap;
use crate::planet_fit::{ContactConstraint, fit_planet_map};
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct OnlineEstimator {
    contacts: ContactMap,
    players: BTreeMap<u32, Point>,
    observations: Vec<ShotObservation>,
    belief: Option<MapBelief>,
    field_hypothesis: Option<crate::field_hypothesis::FieldHypothesis>,
    pending_field_hypotheses: Vec<crate::field_hypothesis::FieldHypothesis>,
    attempted_hypothesis_shots: usize,
}

impl OnlineEstimator {
    /// Ingest an ordered group of events, then fit once if it contains new trajectories.
    pub fn observe_batch(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        let mut changed = false;
        for event in events {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            match event {
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
            .collect::<Vec<_>>();
        let mut belief = MapBelief::new(map, None);
        if !belief.map.planets.is_empty() {
            match fit_planet_map(
                &belief.map,
                &self.observations,
                &contact_constraints,
                WorldParameters::default(),
                cancelled,
            ) {
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
                    let same_topology = refined_map == fit.map;
                    belief.map = refined_map;
                    belief.parameter_covariance = if fit.converged && same_topology {
                        fit.covariance
                    } else {
                        None
                    };
                    belief.fit_converged = same_topology.then_some(fit.converged);
                    belief.model_position_rmse = same_topology.then_some(fit.position_rmse);
                    eprintln!(
                        "LivePlanetFit planets={} observations={} position_rmse={:.4} contact_rmse={:.4} rank={} covariance={} converged={}",
                        belief.map.planets.len(),
                        self.observations.len(),
                        fit.position_rmse,
                        fit.contact_rmse,
                        fit.rank,
                        belief.parameter_covariance.is_some(),
                        fit.converged
                    );
                }
                Err(error) => {
                    eprintln!("LivePlanetFitError {error}; retaining contact-seeded partial map")
                }
            }
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
        belief.local_field =
            crate::local_field::LocalField::from_observations(&belief.map, &self.observations);
        if !self.update_field_hypotheses(&belief.map, cancelled) {
            return None;
        }
        if let Some(hypothesis) = &self.field_hypothesis {
            let mut gravity_sources = belief.map.clone();
            gravity_sources.planets.push(crate::protocol::Planet {
                x: hypothesis.x,
                y: hypothesis.y,
                radius: 0.0,
                mass: hypothesis.mass,
            });
            belief.hypothesis_local_field =
                Some(crate::local_field::LocalField::from_observations(
                    &gravity_sources,
                    &self.observations,
                ));
            belief.field_hypothesis = Some(hypothesis.clone());
        }
        if cancelled.load(Ordering::SeqCst) {
            return None;
        }
        self.belief = Some(belief.clone());
        Some(belief)
    }

    /// Keep uncontradicted proposals until a separate path corroborates them.
    fn update_field_hypotheses(&mut self, baseline: &PlanetMap, cancelled: &AtomicBool) -> bool {
        use crate::field_hypothesis::{FieldHypothesis, propose_field, propose_residual_field};
        #[derive(PartialEq)]
        enum Assessment {
            Accepted,
            Pending,
            Rejected,
        }
        let assess = |hypothesis: &FieldHypothesis| -> Option<Assessment> {
            let mut proposed = baseline.clone();
            proposed.planets.push(crate::protocol::Planet {
                x: hypothesis.x,
                y: hypothesis.y,
                radius: 0.0,
                mass: hypothesis.mass,
            });
            let mut corroborated = false;
            for shot in &self.observations {
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                if shot.points.len() < 3 {
                    continue;
                }
                let is_support = shot
                    .points
                    .len()
                    .checked_sub(1)
                    .is_some_and(|n| shot.points[..n] == hypothesis.support);
                let base = crate::estimator::shot_position_rmse(
                    baseline,
                    shot,
                    WorldParameters::default(),
                );
                let candidate = crate::estimator::shot_position_rmse(
                    &proposed,
                    shot,
                    WorldParameters::default(),
                );
                match (base, candidate) {
                    (Ok(Some(base)), Ok(Some(candidate)))
                        if base.is_finite() && candidate.is_finite() =>
                    {
                        if candidate > base * 1.2 + 0.01 {
                            return Some(Assessment::Rejected);
                        }
                        if !is_support && base > 0.01 && candidate < base * 0.8 {
                            corroborated = true;
                        }
                    }
                    // Insufficient samples convey no evidence. A real simulation failure
                    // is not evidence of safety and must reject this proposal.
                    (Ok(None), Ok(None)) => continue,
                    _ => return Some(Assessment::Rejected),
                }
            }
            Some(if baseline.planets.is_empty() || corroborated {
                Assessment::Accepted
            } else {
                Assessment::Pending
            })
        };

        if let Some(hypothesis) = &self.field_hypothesis {
            match assess(hypothesis) {
                None => return false,
                Some(Assessment::Rejected) => self.field_hypothesis = None,
                Some(Assessment::Pending) => self
                    .pending_field_hypotheses
                    .push(self.field_hypothesis.take().unwrap()),
                Some(Assessment::Accepted) => {}
            }
        }
        let mut index = 0;
        while index < self.pending_field_hypotheses.len() {
            match assess(&self.pending_field_hypotheses[index]) {
                None => return false,
                Some(Assessment::Rejected) => {
                    self.pending_field_hypotheses.remove(index);
                }
                Some(Assessment::Accepted) if self.field_hypothesis.is_none() => {
                    self.field_hypothesis = Some(self.pending_field_hypotheses.remove(index));
                }
                _ => index += 1,
            }
        }
        while self.field_hypothesis.is_none()
            && self.attempted_hypothesis_shots < self.observations.len()
        {
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            let index = self.attempted_hypothesis_shots;
            let candidate = if baseline.planets.is_empty() {
                propose_field(
                    &self.observations[index],
                    WorldParameters::default(),
                    cancelled,
                )
            } else {
                propose_residual_field(
                    &self.observations[index],
                    baseline,
                    WorldParameters::default(),
                    cancelled,
                )
            };
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            self.attempted_hypothesis_shots += 1;
            if let Some(candidate) = candidate {
                match assess(&candidate) {
                    None => return false,
                    Some(Assessment::Accepted) => self.field_hypothesis = Some(candidate),
                    Some(Assessment::Pending) => self.pending_field_hypotheses.push(candidate),
                    Some(Assessment::Rejected) => {}
                }
            }
        }
        !cancelled.load(Ordering::Relaxed)
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
    fn residual_hypothesis_waits_for_independent_corroboration() {
        let known = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let truth = PlanetMap {
            planets: vec![
                known.planets[0].clone(),
                Planet {
                    x: 1000.0,
                    y: 350.0,
                    radius: 35.0,
                    mass: 4287.5,
                },
            ],
        };
        let observe = |y| {
            let mut shot = ShotObservation {
                player_id: 1,
                angle_degrees: 0.0,
                velocity: 8.0,
                points: vec![Point { x: 200.0, y }],
            };
            shot.points = simulate_shot(&truth, &shot, WorldParameters::default(), 2000)
                .unwrap()
                .points;
            shot
        };
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        estimator.observations.push(observe(400.0));
        estimator.update_field_hypotheses(&known, &cancelled);
        assert!(
            estimator.field_hypothesis.is_none(),
            "uncorroborated source must not be published"
        );
        assert_eq!(
            estimator.pending_field_hypotheses.len(),
            1,
            "retain the viable candidate"
        );
        let first_support = estimator.pending_field_hypotheses[0].support.clone();
        estimator.observations.push(observe(350.0));
        estimator.update_field_hypotheses(&known, &cancelled);
        let selected = estimator
            .field_hypothesis
            .as_ref()
            .expect("later path corroborates original candidate");
        assert_eq!(selected.support, first_support);
    }

    #[test]
    fn contradictory_path_discards_pending_hypothesis() {
        let known = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let truth = PlanetMap {
            planets: vec![
                known.planets[0].clone(),
                Planet {
                    x: 1000.0,
                    y: 350.0,
                    radius: 35.0,
                    mass: 4287.5,
                },
            ],
        };
        let observe = |map: &PlanetMap, y| {
            let mut shot = ShotObservation {
                player_id: 1,
                angle_degrees: 0.0,
                velocity: 8.0,
                points: vec![Point { x: 200.0, y }],
            };
            shot.points = simulate_shot(map, &shot, WorldParameters::default(), 2000)
                .unwrap()
                .points;
            shot
        };
        let mut estimator = OnlineEstimator::default();
        let cancelled = AtomicBool::new(false);
        estimator.observations.push(observe(&truth, 400.0));
        estimator.update_field_hypotheses(&known, &cancelled);
        assert_eq!(estimator.pending_field_hypotheses.len(), 1);
        estimator.observations.push(observe(&known, 350.0));
        estimator.update_field_hypotheses(&known, &cancelled);
        assert!(estimator.field_hypothesis.is_none());
        assert!(estimator.pending_field_hypotheses.is_empty());
    }

    #[test]
    fn physical_map_rejects_a_redundant_hypothesis_on_its_support_path() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        shot.points = simulate_shot(&truth, &shot, WorldParameters::default(), 2000)
            .unwrap()
            .points;
        let cancel = AtomicBool::new(false);
        let hypothesis =
            crate::field_hypothesis::propose_field(&shot, WorldParameters::default(), &cancel)
                .unwrap();
        let mut estimator = OnlineEstimator {
            observations: vec![shot],
            field_hypothesis: Some(hypothesis),
            attempted_hypothesis_shots: 1,
            ..Default::default()
        };
        assert!(estimator.update_field_hypotheses(&truth, &cancel));
        assert!(estimator.field_hypothesis.is_none());
        assert!(estimator.pending_field_hypotheses.is_empty());
    }

    #[test]
    fn contradicted_hypothesis_can_be_replaced_from_a_later_path() {
        // A rejected candidate must not cause every later update to retry the same seed.
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let make_event = |y: f32| {
            let launch = ShotObservation {
                player_id: 1,
                angle_degrees: 0.0,
                velocity: 8.0,
                points: vec![Point { x: 200.0, y }],
            };
            let path = simulate_shot(&truth, &launch, WorldParameters::default(), 2000).unwrap();
            assert_eq!(
                path.termination,
                crate::simulation::Termination::BattlefieldMargin
            );
            ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.0,
                velocity: 8.0,
                points: path.points,
            }
        };
        let cancel = AtomicBool::new(false);
        let mut estimator = OnlineEstimator::default();
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 200.0,
                y: 400.0,
            },
            &cancel,
        );
        let first = estimator.observe(&make_event(400.0), &cancel).unwrap();
        assert!(first.field_hypothesis.is_some());
        // Model a previously accepted but inaccurate field; a later independent flight
        // supplies evidence to reject it and also supplies a new candidate.
        let wrong = estimator.field_hypothesis.as_mut().unwrap();
        wrong.x = 250.0;
        wrong.y = 350.0;
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 200.0,
                y: 300.0,
            },
            &cancel,
        );
        let replacement = estimator
            .observe(&make_event(300.0), &cancel)
            .unwrap()
            .field_hypothesis
            .expect("later path can propose a better field");
        assert!((replacement.x - 600.0).hypot(replacement.y - 500.0) < 20.0);
    }

    #[test]
    fn short_path_does_not_discard_previously_learned_field_hypothesis() {
        // A terminal path without two timed positions cannot contradict a learned field.
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let launch = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let path = simulate_shot(&truth, &launch, WorldParameters::default(), 2000).unwrap();
        let mut estimator = OnlineEstimator::default();
        let cancel = AtomicBool::new(false);
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 200.0,
                y: 400.0,
            },
            &cancel,
        );
        let belief = estimator
            .observe(
                &ServerEvent::ShotFin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                    points: path.points,
                },
                &cancel,
            )
            .unwrap();
        assert!(
            belief.field_hypothesis.is_some(),
            "flyby must create hypothesis"
        );
        let short = ServerEvent::ShotFin {
            player_id: 1,
            angle: 90.0,
            velocity: 2.0,
            points: vec![Point { x: 200.0, y: 400.0 }, Point { x: 200.0, y: 399.0 }],
        };
        let updated = estimator.observe(&short, &cancel).unwrap();
        assert!(
            updated.field_hypothesis.is_some(),
            "unscorable path is not contrary evidence"
        );
    }

    #[test]
    fn flyby_supplies_local_gravity_for_nearby_prediction_without_planet_contact() {
        use crate::simulation::{
            simulate_sampled_positions, simulate_sampled_positions_with_local_field,
        };
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let training = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let trajectory =
            simulate_shot(&truth, &training, WorldParameters::default(), 2000).unwrap();
        assert_eq!(
            trajectory.termination,
            crate::simulation::Termination::BattlefieldMargin
        );
        let mut estimator = OnlineEstimator::default();
        let cancel = AtomicBool::new(false);
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 200.0,
                y: 400.0,
            },
            &cancel,
        );
        let belief = estimator
            .observe(
                &ServerEvent::ShotFin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                    points: trajectory.points,
                },
                &cancel,
            )
            .unwrap();
        assert!(belief.map.planets.is_empty());
        let heldout = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 410.0 }],
        };
        let observed =
            simulate_sampled_positions(&truth, &heldout, WorldParameters::default(), 70).unwrap();
        let flat =
            simulate_sampled_positions(&belief.map, &heldout, WorldParameters::default(), 70)
                .unwrap();
        let learned = simulate_sampled_positions_with_local_field(
            &belief.map,
            &belief.local_field,
            &heldout,
            WorldParameters::default(),
            70,
        )
        .unwrap();
        let error = |prediction: &[crate::map::Vec2]| {
            prediction
                .iter()
                .zip(&observed)
                .map(|(a, b)| (a.x - b.x).powi(2) + (a.y - b.y).powi(2))
                .sum::<f64>()
        };
        assert!(
            error(&learned) < error(&flat) * 0.25,
            "learned={} flat={}",
            error(&learned),
            error(&flat)
        );
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
