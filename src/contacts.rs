//! Contact-derived planet seeds and supporting observations for joint physical-map fitting.
//! Classification assumes NewtonWars defaults: player radius 4 and maximum trail 2000.
use crate::map::Vec2;
use crate::protocol::{Planet, Point};
use crate::simulation::{WorldParameters, reconstruct_acceleration};

#[derive(Clone, Debug)]
pub struct ContactHypothesis {
    pub planet: Planet,
    pub residual_rmse: f64,
    pub contact: Point,
    pub contacts: Vec<Point>,
    /// Local seeding does not claim covariance; the joint fit supplies it.
    pub parameter_covariance: Option<nalgebra::DMatrix<f64>>,
    pub association_ambiguous: bool,
    /// Later regular path segments passed through the proposed interior.
    pub contradicted: bool,
    acceleration_samples: Vec<Vec<(Vec2, Vec2)>>,
}

#[derive(Clone, Debug, Default)]
pub struct ContactMap {
    hypotheses: Vec<ContactHypothesis>,
    contacts: Vec<Point>,
}

impl ContactMap {
    pub fn hypotheses(&self) -> &[ContactHypothesis] {
        &self.hypotheses
    }
    /// Classified contact points remain useful even when local geometry cannot be fitted.
    pub fn contact_points(&self) -> &[Point] {
        &self.contacts
    }
    /// Apply joint-fit geometry in the same uncontradicted, unambiguous order used to seed it.
    pub fn refine_active_planets(&mut self, planets: &[Planet]) {
        assert_eq!(
            self.hypotheses
                .iter()
                .filter(|hypothesis| !hypothesis.contradicted && !hypothesis.association_ambiguous)
                .count(),
            planets.len()
        );
        assert!(
            planets.iter().enumerate().all(|(index, planet)| {
                planets[..index]
                    .iter()
                    .all(|other| !disks_overlap(planet, other))
            }),
            "joint-fit planets must not overlap"
        );
        for (hypothesis, planet) in self
            .hypotheses
            .iter_mut()
            .filter(|hypothesis| !hypothesis.contradicted && !hypothesis.association_ambiguous)
            .zip(planets)
        {
            hypothesis.planet = planet.clone();
        }
        self.promote_unblocked();
    }

    fn promote_unblocked(&mut self) {
        // A corrected active disk can free a previously conflicting candidate.
        // Promote one at a time so promoted disks also constrain later candidates.
        for index in 0..self.hypotheses.len() {
            if self.hypotheses[index].contradicted || !self.hypotheses[index].association_ambiguous
            {
                continue;
            }
            let overlaps_active = self
                .hypotheses
                .iter()
                .enumerate()
                .any(|(other_index, other)| {
                    other_index != index
                        && !other.contradicted
                        && !other.association_ambiguous
                        && disks_overlap(&self.hypotheses[index].planet, &other.planet)
                });
            if !overlaps_active {
                self.hypotheses[index].association_ambiguous = false;
            }
        }
    }
    pub fn reset(&mut self) {
        self.hypotheses.clear();
        self.contacts.clear();
    }
    /// Apply sampled-position free-space evidence, then infer a probable contact under
    /// the documented default server limits. `players` must contain all tracked
    /// pre-finish positions; an empty list cannot rule out player termination.
    pub fn observe(&mut self, points: &[Point], players: &[Point], world: WorldParameters) {
        // Only recorded sample positions are guaranteed to lie on the actual curved
        // trajectory. A straight chord between two samples can cut across a planet
        // even when the simulated path bends safely around it.
        let regular = &points[..points.len().saturating_sub(1)];
        for hypothesis in &mut self.hypotheses {
            let radius = (hypothesis.planet.radius - 5.0).max(0.0);
            if regular.iter().any(|point| {
                (f64::from(point.x) - hypothesis.planet.x).powi(2)
                    + (f64::from(point.y) - hypothesis.planet.y).powi(2)
                    < radius * radius
            }) {
                hypothesis.contradicted = true;
            }
        }
        self.promote_unblocked();
        let Some(contact) = points.last() else {
            return;
        };
        let x = f64::from(contact.x);
        let y = f64::from(contact.y);
        if players.is_empty()
            || points.len() >= 2000
            || !points
                .iter()
                .all(|point| point.x.is_finite() && point.y.is_finite())
            || x <= -world.margin + 0.1
            || x >= world.battlefield_width + world.margin - 0.1
            || y <= -world.margin + 0.1
            || y >= world.battlefield_height + world.margin - 0.1
            || players
                .iter()
                .any(|player| (x - f64::from(player.x)).hypot(y - f64::from(player.y)) <= 4.1)
        {
            return;
        }
        if !self.contacts.contains(contact) {
            self.contacts.push(contact.clone());
        }
        let samples = reconstruct_acceleration(points);
        let samples = &samples[samples.len().saturating_sub(5)..];
        if samples.len() < 3 {
            return;
        }
        let mean = samples
            .iter()
            .fold(Vec2::default(), |sum, (_, acceleration)| {
                Vec2::new(
                    sum.x + acceleration.x / samples.len() as f64,
                    sum.y + acceleration.y / samples.len() as f64,
                )
            });
        let variation: f64 = samples
            .iter()
            .map(|(_, acceleration)| {
                (acceleration.x - mean.x).powi(2) + (acceleration.y - mean.y).powi(2)
            })
            .sum();
        // A constant fitted background makes a single/constant acceleration
        // sequence compatible with arbitrary circles, so do not invent geometry.
        if variation <= 1e-6 {
            return;
        }
        if self.hypotheses.iter().any(|hypothesis| {
            hypothesis
                .contacts
                .iter()
                .zip(&hypothesis.acceleration_samples)
                .any(|(old_contact, old_samples)| old_contact == contact && old_samples == samples)
        }) {
            return;
        }
        let Some(hypothesis) = fit_contact(contact, samples, world) else {
            return;
        };
        self.register(hypothesis, world);
    }

    fn register(&mut self, mut hypothesis: ContactHypothesis, world: WorldParameters) {
        let contact = &hypothesis.contacts[0];
        let samples = &hypothesis.acceleration_samples[0];
        let compatible = self
            .hypotheses
            .iter()
            .enumerate()
            .filter(|(_, old)| {
                !old.contradicted
                    && !old.association_ambiguous
                    && compatible_planets(old, &hypothesis)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if compatible.len() == 1 {
            let index = compatible[0];
            let existing = &self.hypotheses[index];
            let mut contacts = existing.contacts.clone();
            let mut paths = existing.acceleration_samples.clone();
            if let Some(index) = contacts.iter().position(|old| old == contact) {
                paths[index] = samples.to_vec();
            } else {
                contacts.push(contact.clone());
                paths.push(samples.to_vec());
            }
            if let Some(refined) = fit_contacts(&contacts, &paths, world) {
                let overlaps_other =
                    self.hypotheses
                        .iter()
                        .enumerate()
                        .any(|(other_index, other)| {
                            other_index != index
                                && !other.contradicted
                                && !other.association_ambiguous
                                && disks_overlap(&refined.planet, &other.planet)
                        });
                if !overlaps_other {
                    self.hypotheses[index] = refined;
                    return;
                }
            }
        }
        // Multiple plausible objects cannot be resolved by this local observation.
        // Preserve a separate, explicitly ambiguous hypothesis for the joint fitter.
        hypothesis.association_ambiguous = compatible.len() > 1
            || self.hypotheses.iter().any(|old| {
                !old.contradicted
                    && !old.association_ambiguous
                    && disks_overlap(&old.planet, &hypothesis.planet)
            });
        self.hypotheses.push(hypothesis);
    }
}

fn disks_overlap(left: &Planet, right: &Planet) -> bool {
    (left.x - right.x).hypot(left.y - right.y) < left.radius + right.radius
}

fn compatible_planets(old: &ContactHypothesis, new: &ContactHypothesis) -> bool {
    let center_distance = (old.planet.x - new.planet.x).hypot(old.planet.y - new.planet.y);
    center_distance < old.planet.radius.min(new.planet.radius) * 0.5
        && new.contacts.iter().all(|contact| {
            ((f64::from(contact.x) - old.planet.x).hypot(f64::from(contact.y) - old.planet.y)
                - old.planet.radius)
                .abs()
                <= 10.0
        })
        && old.contacts.iter().all(|contact| {
            ((f64::from(contact.x) - new.planet.x).hypot(f64::from(contact.y) - new.planet.y)
                - new.planet.radius)
                .abs()
                <= 10.0
        })
}

fn fit_contact(
    contact: &Point,
    samples: &[(Vec2, Vec2)],
    world: WorldParameters,
) -> Option<ContactHypothesis> {
    fit_contacts(&[contact.clone()], &[samples.to_vec()], world)
}

fn fit_contacts(
    contacts: &[Point],
    paths: &[Vec<(Vec2, Vec2)>],
    world: WorldParameters,
) -> Option<ContactHypothesis> {
    let contact = &contacts[0];
    // Each approach has its own unknown constant background. Normalize its
    // acceleration variation, and express radial contact mismatch in 1-unit
    // residuals; the resulting score is dimensionless, not calibrated uncertainty.
    let scales = paths
        .iter()
        .map(|samples| {
            let mean = samples
                .iter()
                .fold(Vec2::default(), |sum, (_, acceleration)| {
                    Vec2::new(
                        sum.x + acceleration.x / samples.len() as f64,
                        sum.y + acceleration.y / samples.len() as f64,
                    )
                });
            (samples
                .iter()
                .map(|(_, acceleration)| {
                    (acceleration.x - mean.x).powi(2) + (acceleration.y - mean.y).powi(2)
                })
                .sum::<f64>()
                / (2 * samples.len()) as f64)
                .sqrt()
                .max(1e-4)
        })
        .collect::<Vec<_>>();
    let mut best: Option<ContactHypothesis> = None;
    for radius_step in 40..=80 {
        let radius = f64::from(radius_step) * 0.5;
        let mass = radius.powi(3) / 10.0;
        for angle in (0..360).step_by(3) {
            let angle = f64::from(angle).to_radians();
            let planet = Planet {
                x: f64::from(contact.x) + radius * angle.cos(),
                y: f64::from(contact.y) + radius * angle.sin(),
                radius,
                mass,
            };
            if planet.x < radius
                || planet.x > world.battlefield_width - radius
                || planet.y < radius
                || planet.y > world.battlefield_height - radius
            {
                continue;
            }
            let mut squared_error = 0.0;
            let mut residual_count = 0;
            let mut valid = true;
            for (samples, scale) in paths.iter().zip(&scales) {
                let mut residuals = Vec::with_capacity(samples.len());
                for (position, acceleration) in samples {
                    let dx = planet.x - position.x;
                    let dy = planet.y - position.y;
                    let distance_squared = dx * dx + dy * dy;
                    if distance_squared <= radius * radius {
                        valid = false;
                        break;
                    }
                    let factor = mass / distance_squared.powf(1.5);
                    residuals.push(Vec2::new(
                        factor * dx - acceleration.x,
                        factor * dy - acceleration.y,
                    ));
                }
                if !valid {
                    break;
                }
                let mean = residuals.iter().fold(Vec2::default(), |sum, residual| {
                    Vec2::new(
                        sum.x + residual.x / samples.len() as f64,
                        sum.y + residual.y / samples.len() as f64,
                    )
                });
                squared_error += residuals
                    .iter()
                    .map(|residual| {
                        ((residual.x - mean.x).powi(2) + (residual.y - mean.y).powi(2))
                            / scale.powi(2)
                    })
                    .sum::<f64>();
                residual_count += 2 * samples.len();
            }
            if !valid {
                continue;
            }
            for support in contacts.iter().skip(1) {
                let radial_error = (f64::from(support.x) - planet.x)
                    .hypot(f64::from(support.y) - planet.y)
                    - radius;
                squared_error += radial_error.powi(2);
                residual_count += 1;
            }
            let residual_rmse = (squared_error / residual_count as f64).sqrt();
            if best
                .as_ref()
                .is_none_or(|old| residual_rmse < old.residual_rmse)
            {
                best = Some(ContactHypothesis {
                    planet,
                    contact: contact.clone(),
                    contacts: contacts.to_vec(),
                    parameter_covariance: None,
                    association_ambiguous: false,
                    residual_rmse,
                    contradicted: false,
                    acceleration_samples: paths.to_vec(),
                });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::PlanetMap;
    use crate::simulation::{ShotObservation, Termination, simulate_shot};

    fn contact_path() -> Vec<Point> {
        let map = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let trajectory = simulate_shot(
            &map,
            &ShotObservation {
                player_id: 1,
                angle_degrees: 0.0,
                velocity: 4.0,
                points: vec![Point { x: 350.0, y: 500.0 }],
            },
            WorldParameters::default(),
            500,
        )
        .unwrap();
        assert_eq!(trajectory.termination, Termination::PlanetCollision(0));
        trajectory.points
    }
    fn players() -> Vec<Point> {
        vec![Point { x: 350.0, y: 500.0 }]
    }

    #[test]
    fn contact_estimates_an_isolated_planet_without_oracle_input() {
        let mut map = ContactMap::default();
        let points = contact_path();
        map.observe(&points, &players(), WorldParameters::default());
        assert_eq!(map.hypotheses().len(), 1);
        let planet = &map.hypotheses()[0].planet;
        assert!((planet.radius - 30.0).abs() < 4.0, "{planet:?}");
        assert!(
            (planet.x - 500.0).hypot(planet.y - 500.0) < 5.0,
            "{planet:?}"
        );
        map.observe(&points, &players(), WorldParameters::default());
        assert_eq!(map.hypotheses().len(), 1);
    }

    #[test]
    fn contacts_from_two_approach_directions_associate_with_one_planet() {
        let mut contacts = ContactMap::default();
        let first = contact_path();
        contacts.observe(&first, &players(), WorldParameters::default());
        let initial = &contacts.hypotheses()[0].planet;
        let initial_error = (initial.x - 500.0).hypot(initial.y - 500.0);
        let map = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let second = simulate_shot(
            &map,
            &ShotObservation {
                player_id: 2,
                angle_degrees: 90.0,
                velocity: 4.0,
                points: vec![Point { x: 500.0, y: 650.0 }],
            },
            WorldParameters::default(),
            500,
        )
        .unwrap()
        .points;
        contacts.observe(
            &second,
            &[Point { x: 500.0, y: 650.0 }],
            WorldParameters::default(),
        );
        assert_eq!(contacts.hypotheses().len(), 1);
        assert_eq!(contacts.hypotheses()[0].contacts.len(), 2);
        let refined = &contacts.hypotheses()[0].planet;
        assert!(
            (refined.x - 500.0).hypot(refined.y - 500.0) < initial_error,
            "initial={initial_error}, refined={refined:?}"
        );
        let planet = &contacts.hypotheses()[0].planet;
        assert!((planet.radius - 30.0).abs() < 2.0, "{planet:?}");
        assert!(
            (planet.x - 500.0).hypot(planet.y - 500.0) < 2.0,
            "{planet:?}"
        );
    }

    #[test]
    fn distinct_nearby_planets_do_not_associate() {
        let mut contacts = ContactMap::default();
        let map = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.0,
                    y: 500.0,
                    radius: 30.0,
                    mass: 2700.0,
                },
                Planet {
                    x: 570.0,
                    y: 500.0,
                    radius: 30.0,
                    mass: 2700.0,
                },
            ],
        };
        for (launch, angle) in [(350.0, 0.0), (720.0, 180.0)] {
            let start = Point {
                x: launch,
                y: 500.0,
            };
            let path = simulate_shot(
                &map,
                &ShotObservation {
                    player_id: 1,
                    angle_degrees: angle,
                    velocity: 4.0,
                    points: vec![start.clone()],
                },
                WorldParameters::default(),
                500,
            )
            .unwrap()
            .points;
            contacts.observe(&path, &[start], WorldParameters::default());
        }
        assert_eq!(contacts.hypotheses().len(), 2);
        assert!(
            contacts
                .hypotheses()
                .iter()
                .all(|hypothesis| { hypothesis.contacts.len() == 1 && !hypothesis.contradicted })
        );
        // Distinct true bodies must not merge. Their approximate disks can
        // overlap; that ambiguity must not become two overlapping gravity sources.
        let active = contacts
            .hypotheses()
            .iter()
            .filter(|hypothesis| !hypothesis.association_ambiguous)
            .collect::<Vec<_>>();
        assert!(!active.is_empty());
        assert!(active.iter().enumerate().all(|(index, hypothesis)| {
            active[..index]
                .iter()
                .all(|other| !disks_overlap(&hypothesis.planet, &other.planet))
        }));
    }

    fn seed(x: f64, y: f64, radius: f64) -> ContactHypothesis {
        let contact = Point {
            x: (x - radius) as f32,
            y: y as f32,
        };
        ContactHypothesis {
            planet: Planet {
                x,
                y,
                radius,
                mass: radius.powi(3) / 10.0,
            },
            residual_rmse: 0.0,
            contact: contact.clone(),
            contacts: vec![contact],
            parameter_covariance: None,
            association_ambiguous: false,
            contradicted: false,
            acceleration_samples: vec![vec![(
                Vec2::new(x - radius - 10.0, y),
                Vec2::new(1.0, 0.0),
            )]],
        }
    }

    #[test]
    fn drifted_overlapping_seeds_never_become_multiple_active_planets() {
        // Capture21 produced these three overlapping estimates for one region.
        let mut map = ContactMap::default();
        for (x, y, radius) in [
            (944.7, 716.5, 20.0),
            (940.9, 718.9, 29.5),
            (928.3, 707.6, 28.5),
        ] {
            map.register(seed(x, y, radius), WorldParameters::default());
        }
        assert_eq!(
            map.hypotheses()
                .iter()
                .filter(|hypothesis| !hypothesis.contradicted && !hypothesis.association_ambiguous)
                .count(),
            1
        );
        assert!(
            map.hypotheses()
                .iter()
                .any(|hypothesis| hypothesis.association_ambiguous)
        );
    }

    #[test]
    fn joint_geometry_updates_reject_overlaps_without_changing_the_registry() {
        let mut map = ContactMap::default();
        map.register(seed(500.0, 500.0, 20.0), WorldParameters::default());
        map.register(seed(600.0, 500.0, 20.0), WorldParameters::default());
        let mut overlapping = map
            .hypotheses()
            .iter()
            .map(|hypothesis| hypothesis.planet.clone())
            .collect::<Vec<_>>();
        overlapping[1].x = 510.0;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            map.refine_active_planets(&overlapping)
        }));
        assert!(outcome.is_err());
        assert_eq!(map.hypotheses()[1].planet.x, 600.0);
    }

    #[test]
    fn improved_geometry_can_resolve_a_previous_overlap_ambiguity() {
        let mut map = ContactMap::default();
        map.register(seed(500.0, 500.0, 20.0), WorldParameters::default());
        map.register(seed(530.0, 500.0, 20.0), WorldParameters::default());
        assert!(map.hypotheses()[1].association_ambiguous);
        let mut revised = map.hypotheses()[0].planet.clone();
        revised.x = 480.0;
        map.refine_active_planets(&[revised]);
        assert!(
            map.hypotheses()
                .iter()
                .all(|hypothesis| !hypothesis.association_ambiguous)
        );
        assert!(!disks_overlap(
            &map.hypotheses()[0].planet,
            &map.hypotheses()[1].planet
        ));
    }

    #[test]
    fn free_space_can_unblock_an_ambiguous_planet_without_a_joint_fit() {
        let mut map = ContactMap::default();
        map.register(seed(500.0, 500.0, 20.0), WorldParameters::default());
        map.register(seed(530.0, 500.0, 20.0), WorldParameters::default());
        assert!(map.hypotheses()[1].association_ambiguous);
        let points = vec![
            Point { x: 480.0, y: 500.0 },
            Point { x: 500.0, y: 500.0 },
            Point { x: 501.0, y: 500.0 },
        ];
        map.observe(&points, &[points[2].clone()], WorldParameters::default());
        assert!(map.hypotheses()[0].contradicted);
        assert!(!map.hypotheses()[1].contradicted);
        assert!(!map.hypotheses()[1].association_ambiguous);
    }

    #[test]
    fn local_fit_never_seeds_a_circle_outside_the_battlefield() {
        let path = contact_path();
        let samples = reconstruct_acceleration(&path);
        let samples = &samples[samples.len() - 5..];
        let mut outside = path.last().unwrap().clone();
        outside.x = -1.0;
        assert!(fit_contact(&outside, samples, WorldParameters::default()).is_none());
    }

    #[test]
    fn player_margin_sample_limit_and_unknown_players_create_no_planet() {
        let points = contact_path();
        let world = WorldParameters::default();
        let mut map = ContactMap::default();
        map.observe(&points, &[points.last().unwrap().clone()], world);
        let mut margin = points.clone();
        margin.last_mut().unwrap().x = -(world.margin as f32) - 1.0;
        map.observe(&margin, &players(), world);
        let mut capped = points.clone();
        capped.resize(2000, points.last().unwrap().clone());
        map.observe(&capped, &players(), world);
        map.observe(&points, &[], world);
        assert!(map.hypotheses().is_empty());
        assert!(map.contact_points().is_empty());
    }

    #[test]
    fn constant_acceleration_retains_contact_evidence_without_inventing_a_circle() {
        let mut map = ContactMap::default();
        let points = (0..10)
            .map(|i| Point {
                x: 500.0 + i as f32,
                y: 500.0,
            })
            .collect::<Vec<_>>();
        map.observe(&points, &players(), WorldParameters::default());
        assert!(map.hypotheses().is_empty());
        assert_eq!(map.contact_points(), &[points.last().unwrap().clone()]);
        map.observe(&points, &players(), WorldParameters::default());
        assert_eq!(map.contact_points().len(), 1);
        map.reset();
        assert!(map.contact_points().is_empty());
    }

    #[test]
    fn later_free_space_contradicts_geometry_and_epoch_reset_forgets_contacts() {
        let mut map = ContactMap::default();
        map.observe(&contact_path(), &players(), WorldParameters::default());
        assert_eq!(map.hypotheses().len(), 1);
        let planet = map.hypotheses()[0].planet.clone();
        let through = vec![
            Point {
                x: (planet.x - 50.0) as f32,
                y: planet.y as f32,
            },
            Point {
                x: planet.x as f32,
                y: planet.y as f32,
            },
            Point {
                x: (planet.x + 50.0) as f32,
                y: planet.y as f32,
            },
            Point {
                x: (planet.x + 51.0) as f32,
                y: planet.y as f32,
            },
        ];
        map.observe(&through, &players(), WorldParameters::default());
        assert!(map.hypotheses()[0].contradicted);
        assert!(!map.hypotheses()[0].acceleration_samples.is_empty());
        map.reset();
        assert!(map.hypotheses().is_empty());
    }

    #[test]
    fn curved_flyby_does_not_contradict_a_planet_based_on_its_chord() {
        let mut map = ContactMap::default();
        map.observe(&contact_path(), &players(), WorldParameters::default());
        let planet = map.hypotheses()[0].planet.clone();
        let radius = planet.radius * 1.1;
        let arc = [(-radius, 0.0), (0.0, radius), (radius, 0.0)].map(|(dx, dy)| Point {
            x: (planet.x + dx) as f32,
            y: (planet.y + dy) as f32,
        });
        assert!(arc.iter().all(|point| {
            (f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y)
                > planet.radius - 5.0
        }));
        map.observe(&arc, &players(), WorldParameters::default());
        assert!(!map.hypotheses()[0].contradicted);
    }
}
