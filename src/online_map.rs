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
}

impl OnlineEstimator {
    /// Ingest an ordered group of events, then fit once if it contains new trajectories.
    pub fn observe_batch(
        &mut self,
        events: &[ServerEvent],
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        let mut prior_observations = self.observations.clone();
        let mut prior_map = self
            .belief
            .as_ref()
            .map(|belief| belief.map.clone())
            .unwrap_or_else(|| PlanetMap { planets: vec![] });
        let mut new_observations = Vec::new();
        let mut changed = false;
        for event in events {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            match event {
                ServerEvent::Planets { .. } => {
                    self.reset();
                    prior_observations.clear();
                    prior_map = PlanetMap { planets: vec![] };
                    new_observations.clear();
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
            self.observations.push(shot.clone());
            new_observations.push(shot);
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
        if !self.update_field_hypotheses_prequential(
            &prior_map,
            &belief.map,
            &prior_observations,
            &new_observations,
            cancelled,
        ) {
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

    /// Build source alternatives only from earlier paths, then score them on the newly
    /// arrived whole trajectories against the frozen pre-update physical map. This prevents
    /// the physical refit of a validation path from leaking its answer into source scoring.
    fn update_field_hypotheses_prequential(
        &mut self,
        baseline: &PlanetMap,
        post_fit_map: &PlanetMap,
        training: &[ShotObservation],
        validation: &[ShotObservation],
        cancelled: &AtomicBool,
    ) -> bool {
        use crate::field_hypothesis::{propose_joint_residual_field, validate_residual_field};
        if validation.is_empty() || training.is_empty() {
            return !cancelled.load(Ordering::Relaxed);
        }

        let active_validation = self.field_hypothesis.as_ref().map(|hypothesis| {
            validate_residual_field(
                hypothesis,
                training,
                validation,
                baseline,
                WorldParameters::default(),
                cancelled,
            )
        });
        if cancelled.load(Ordering::Relaxed) {
            return false;
        }
        let active_postfit = match (&self.field_hypothesis, &active_validation) {
            (Some(hypothesis), Some(Ok(Some(_)))) => Some(validate_residual_field(
                hypothesis,
                training,
                validation,
                post_fit_map,
                WorldParameters::default(),
                cancelled,
            )),
            _ => None,
        };
        if cancelled.load(Ordering::Relaxed) {
            return false;
        }
        let active_batch_improvement = active_validation
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(Option::as_ref)
            .map(|score| relative_improvement(score.baseline_rmse, score.candidate_rmse));
        if active_validation
            .as_ref()
            .is_some_and(|result| result.is_err())
            || active_validation
                .as_ref()
                .and_then(|result| result.as_ref().ok().and_then(Option::as_ref))
                .is_some_and(|score| score.regressed)
            || active_postfit
                .as_ref()
                .is_some_and(|result| result.is_err())
            || active_postfit
                .as_ref()
                .and_then(|result| result.as_ref().ok().and_then(Option::as_ref))
                .is_some_and(|score| {
                    score.regressed || score.candidate_rmse >= score.baseline_rmse * 0.8
                })
        {
            self.field_hypothesis = None;
        } else if let (Some(hypothesis), Some(Ok(Some(score)))) =
            (self.field_hypothesis.as_mut(), active_validation)
        {
            for shot in validation {
                if !hypothesis
                    .support_paths
                    .iter()
                    .any(|path| *path == shot.points)
                {
                    hypothesis.support_paths.push(shot.points.clone());
                }
            }
            let prior_paths = hypothesis.validation_paths as f64;
            let new_paths = score.paths as f64;
            let total_paths = prior_paths + new_paths;
            hypothesis.baseline_continuation_rmse = (hypothesis.baseline_continuation_rmse
                * prior_paths
                + score.baseline_rmse * new_paths)
                / total_paths;
            hypothesis.continuation_rmse = (hypothesis.continuation_rmse * prior_paths
                + score.candidate_rmse * new_paths)
                / total_paths;
            hypothesis.validation_paths += score.paths;
            hypothesis.relative_improvement = relative_improvement(
                hypothesis.baseline_continuation_rmse,
                hypothesis.continuation_rmse,
            );
        }
        if self
            .field_hypothesis
            .as_ref()
            .is_some_and(|hypothesis| hypothesis.relative_improvement <= 0.0)
        {
            self.field_hypothesis = None;
        }

        let Some(candidate) = propose_joint_residual_field(
            training,
            validation,
            baseline,
            WorldParameters::default(),
            cancelled,
        ) else {
            return !cancelled.load(Ordering::Relaxed);
        };
        if cancelled.load(Ordering::Relaxed) {
            return false;
        }
        // The physical refit includes the newly validated paths. A source that has
        // become redundant under that refit must not be published alongside it.
        let Some(post_fit_score) = validate_residual_field(
            &candidate,
            training,
            validation,
            post_fit_map,
            WorldParameters::default(),
            cancelled,
        )
        .ok()
        .flatten() else {
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            return true;
        };
        if post_fit_score.regressed || post_fit_score.improved_paths == 0 {
            return !cancelled.load(Ordering::Relaxed);
        }
        let replace = self.field_hypothesis.is_none()
            || active_batch_improvement
                .is_some_and(|active| candidate.relative_improvement > active + 0.1);
        if replace {
            self.field_hypothesis = Some(candidate);
        }
        true
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

fn relative_improvement(baseline: f64, candidate: f64) -> f64 {
    if baseline > 0.0 {
        (1.0 - candidate / baseline).clamp(0.0, 1.0)
    } else {
        0.0
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
    fn joint_field_hypothesis_uses_prior_paths_and_is_rechecked_on_new_paths() {
        let truth = PlanetMap {
            planets: vec![
                Planet {
                    x: 600.0,
                    y: 500.0,
                    radius: 30.0,
                    mass: 2700.0,
                },
                Planet {
                    x: 1000.0,
                    y: 350.0,
                    radius: 35.0,
                    mass: 4287.5,
                },
            ],
        };
        let known = PlanetMap {
            planets: vec![truth.planets[0].clone()],
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
        let cancel = AtomicBool::new(false);
        let training = [observe(&truth, 400.0), observe(&truth, 350.0)];
        let validation = [observe(&truth, 250.0)];
        let mut estimator = OnlineEstimator::default();
        assert!(estimator.update_field_hypotheses_prequential(
            &known,
            &known,
            &training,
            &validation,
            &cancel
        ));
        let hypothesis = estimator
            .field_hypothesis
            .as_ref()
            .expect("independent path supports source");
        assert_eq!(hypothesis.training_paths, 2);
        assert_eq!(hypothesis.validation_paths, 1);

        assert!(estimator.update_field_hypotheses_prequential(
            &known,
            &truth,
            &training,
            &validation,
            &cancel
        ));
        assert!(
            estimator.field_hypothesis.is_none(),
            "a source absorbed by the physical refit must not be counted twice"
        );

        let contradictory = [observe(&known, 200.0)];
        assert!(estimator.update_field_hypotheses_prequential(
            &known,
            &known,
            &training,
            &contradictory,
            &cancel
        ));
        assert!(
            estimator.field_hypothesis.is_none(),
            "a new path that rejects the source retires it"
        );
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
        for y in [400.0, 350.0] {
            estimator.observe(&make_event(y), &cancel).unwrap();
        }
        let first = estimator.observe(&make_event(300.0), &cancel).unwrap();
        assert!(first.field_hypothesis.is_some());
        // Model a previously accepted but inaccurate field; a later independent flight
        // supplies evidence to reject it and also supplies a new candidate.
        let wrong = estimator.field_hypothesis.as_mut().unwrap();
        wrong.x = 250.0;
        wrong.y = 350.0;
        wrong.mass = 1_000_000.0;
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: 1,
                x: 200.0,
                y: 250.0,
            },
            &cancel,
        );
        let replacement = estimator
            .observe(&make_event(250.0), &cancel)
            .unwrap()
            .field_hypothesis
            .expect("later path can propose a better field");
        assert!(
            (replacement.x - 600.0).hypot(replacement.y - 500.0) < 50.0,
            "replacement=({}, {}, mass={})",
            replacement.x,
            replacement.y,
            replacement.mass
        );
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
        let mut estimator = OnlineEstimator::default();
        let cancel = AtomicBool::new(false);
        let mut belief = None;
        for y in [400.0, 350.0, 300.0] {
            estimator.observe(
                &ServerEvent::PlayerPosition {
                    player_id: 1,
                    x: 200.0,
                    y,
                },
                &cancel,
            );
            let launch = ShotObservation {
                player_id: 1,
                angle_degrees: 0.0,
                velocity: 8.0,
                points: vec![Point { x: 200.0, y }],
            };
            let path = simulate_shot(&truth, &launch, WorldParameters::default(), 2000).unwrap();
            belief = estimator.observe(
                &ServerEvent::ShotFin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                    points: path.points,
                },
                &cancel,
            );
        }
        let belief = belief.unwrap();
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
        assert!(estimator.field_hypothesis.is_none());
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
