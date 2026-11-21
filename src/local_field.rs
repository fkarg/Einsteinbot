//! Local residual gravity reconstructed from observed trajectories.
//!
//! Samples estimate acceleration by differencing f32 tick positions. They are correlated
//! tick-scale averages, not independent instantaneous sensors or calibrated uncertainty.
//! The physical field is subtracted at construction; rebuild whenever that map changes.
use crate::map::{PlanetMap, Vec2};
use crate::simulation::{ShotObservation, reconstruct_acceleration};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

const SUPPORT: f64 = 40.0;
const AGGREGATE_CELL: f64 = 8.0;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Sample {
    x: f64,
    y: f64,
    ax: f64,
    ay: f64,
}

/// A snapshot of residual observations near completed trajectories, not a planet map.
/// Serialized samples permit reconstructing the input used by targeting.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LocalField {
    samples: Vec<Sample>,
    #[serde(skip)]
    grid: BTreeMap<(i32, i32), Vec<usize>>,
}

impl<'de> Deserialize<'de> for LocalField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedField {
            samples: Vec<Sample>,
        }

        let SerializedField { samples } = SerializedField::deserialize(deserializer)?;
        let mut field = Self {
            samples,
            grid: BTreeMap::new(),
        };
        for (index, sample) in field.samples.iter().enumerate() {
            field
                .grid
                .entry(cell(Vec2::new(sample.x, sample.y), SUPPORT))
                .or_default()
                .push(index);
        }
        Ok(field)
    }
}

fn cell(position: Vec2, width: f64) -> (i32, i32) {
    (
        (position.x / width).floor() as i32,
        (position.y / width).floor() as i32,
    )
}

impl LocalField {
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Build residuals against exactly this physical map. Each 8-unit cell receives
    /// equal weight regardless of how many trajectory samples visited it.
    pub fn from_observations(map: &PlanetMap, observations: &[ShotObservation]) -> Self {
        let mut aggregate: BTreeMap<(i32, i32), (f64, f64, f64, f64, usize)> = BTreeMap::new();
        for observation in observations {
            for (position, observed) in reconstruct_acceleration(&observation.points) {
                let Ok(physical) = map.acceleration(position) else {
                    continue;
                };
                let ax = observed.x - physical.x;
                let ay = observed.y - physical.y;
                if ![position.x, position.y, ax, ay]
                    .iter()
                    .all(|v| v.is_finite())
                {
                    continue;
                }
                let entry = aggregate.entry(cell(position, AGGREGATE_CELL)).or_default();
                entry.0 += position.x;
                entry.1 += position.y;
                entry.2 += ax;
                entry.3 += ay;
                entry.4 += 1;
            }
        }
        let mut result = Self::default();
        for (_, (x, y, ax, ay, count)) in aggregate {
            let count = count as f64;
            let sample = Sample {
                x: x / count,
                y: y / count,
                ax: ax / count,
                ay: ay / count,
            };
            let key = cell(Vec2::new(sample.x, sample.y), SUPPORT);
            result
                .grid
                .entry(key)
                .or_default()
                .push(result.samples.len());
            result.samples.push(sample);
        }
        result
    }

    /// Estimated residual acceleration within 40 units of observations; `None` means
    /// no local evidence. A smooth taper prevents a field jump at the support boundary.
    /// This is a heuristic estimate, not a confidence or collision estimate.
    pub fn residual(&self, position: Vec2) -> Option<Vec2> {
        if self.samples.is_empty() {
            return None;
        }
        let (x, y) = cell(position, SUPPORT);
        let mut weighted_x = 0.0;
        let mut weighted_y = 0.0;
        let mut weights = 0.0;
        let mut nearest = f64::INFINITY;
        for gx in x.saturating_sub(1)..=x.saturating_add(1) {
            for gy in y.saturating_sub(1)..=y.saturating_add(1) {
                let Some(indices) = self.grid.get(&(gx, gy)) else {
                    continue;
                };
                for &index in indices {
                    let sample = &self.samples[index];
                    let distance_squared =
                        (sample.x - position.x).powi(2) + (sample.y - position.y).powi(2);
                    if distance_squared >= SUPPORT * SUPPORT {
                        continue;
                    }
                    let base = (-distance_squared / 800.0).exp();
                    let weight = base * (1.0 - distance_squared / (SUPPORT * SUPPORT)).powi(2);
                    nearest = nearest.min(distance_squared);
                    weighted_x += weight * sample.ax;
                    weighted_y += weight * sample.ay;
                    weights += weight;
                }
            }
        }
        (weights > 0.0).then(|| {
            let taper = (1.0 - nearest / (SUPPORT * SUPPORT)).powi(2);
            Vec2::new(taper * weighted_x / weights, taper * weighted_y / weights)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Planet, Point};
    use crate::simulation::{WorldParameters, simulate_shot};

    fn trajectory(map: &PlanetMap, y: f32) -> ShotObservation {
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
    }

    #[test]
    fn flat_observations_do_not_invent_gravity_and_unsupported_is_explicit() {
        let flat = PlanetMap { planets: vec![] };
        let field = LocalField::from_observations(&flat, &[trajectory(&flat, 400.0)]);
        let residual = field.residual(Vec2::new(400.0, 400.0)).unwrap();
        assert!(residual.norm_squared().sqrt() < 1e-6);
        assert!(field.residual(Vec2::new(400.0, 200.0)).is_none());
    }

    #[test]
    fn observed_flyby_supplies_nearby_gravity_without_inventing_a_planet() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let flat = PlanetMap { planets: vec![] };
        let observation = trajectory(&truth, 400.0);
        let (position, acceleration) = reconstruct_acceleration(&observation.points)[20];
        let field = LocalField::from_observations(&flat, &[observation]);
        let estimate = field.residual(position).unwrap();
        let error = Vec2::new(estimate.x - acceleration.x, estimate.y - acceleration.y)
            .norm_squared()
            .sqrt();
        assert!(
            error < acceleration.norm_squared().sqrt() * 0.2,
            "error={error}"
        );
    }

    #[test]
    fn extremely_distant_finite_query_is_unsupported() {
        let flat = PlanetMap { planets: vec![] };
        let field = LocalField::from_observations(&flat, &[trajectory(&flat, 400.0)]);
        assert!(field.residual(Vec2::new(f64::MAX, -f64::MAX)).is_none());
    }

    #[test]
    fn correctly_modelled_planet_is_not_counted_twice() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let observation = trajectory(&truth, 400.0);
        let position = reconstruct_acceleration(&observation.points)[20].0;
        let field = LocalField::from_observations(&truth, &[observation]);
        let residual = field.residual(position).unwrap();
        assert!(
            residual.norm_squared().sqrt() < 0.001,
            "residual={residual:?}"
        );
    }

    #[test]
    fn serialized_local_field_rebuilds_its_spatial_index() {
        let flat = PlanetMap { planets: vec![] };
        let field = LocalField::from_observations(&flat, &[trajectory(&flat, 400.0)]);
        let point = Vec2::new(400.0, 400.0);
        let expected = field.residual(point).unwrap();
        let restored: LocalField =
            serde_json::from_value(serde_json::to_value(&field).expect("serialize field"))
                .expect("deserialize field");

        assert_eq!(restored.residual(point), Some(expected));
    }
}
