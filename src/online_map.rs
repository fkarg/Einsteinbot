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
