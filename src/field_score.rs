//! Offline truth-scoring of a frozen belief on a common spatial grid.
//! Truth is an input only to this scorer and never to an estimator.
use crate::coverage::{CELL_SIZE, path_cells};
use crate::map::{PlanetMap, Vec2};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize)]
pub struct FieldStats {
    pub samples: usize,
    pub invalid: usize,
    pub finite_samples: usize,
    /// Diagnostic conditional on finite predictions; never replaces the complete score.
    pub finite_only_vector_rmse: Option<f64>,
    pub vector_rmse: Option<f64>,
    pub median_error: Option<f64>,
    pub p90_error: Option<f64>,
    pub max_error: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct FieldScore {
    pub explored_cells: usize,
    pub unexplored_cells: usize,
    pub explored: Option<FieldStats>,
    pub unexplored: Option<FieldStats>,
    pub whole: Option<FieldStats>,
    pub excluded_truth_interior: usize,
}

fn stats(errors: Vec<Option<f64>>) -> Option<FieldStats> {
    if errors.is_empty() {
        return None;
    }
    let samples = errors.len();
    let invalid = errors.iter().filter(|error| error.is_none()).count();
    let mut values = errors.into_iter().flatten().collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let finite_samples = values.len();
    // Scale before squaring so a finite but large field error cannot overflow
    // and silently serialize as an unavailable score.
    let finite_only_vector_rmse = values.last().map(|max| {
        if *max == 0.0 {
            0.0
        } else {
            max * (values.iter().map(|v| (v / max).powi(2)).sum::<f64>() / finite_samples as f64)
                .sqrt()
        }
    });
    let metrics = (invalid == 0).then(|| {
        let rmse = finite_only_vector_rmse.expect("nonempty valid samples");
        let middle = samples / 2;
        let median = if samples % 2 == 0 {
            values[middle - 1] + (values[middle] - values[middle - 1]) / 2.0
        } else {
            values[middle]
        };
        let p90 = values[((samples * 9).div_ceil(10) - 1).min(samples - 1)];
        (rmse, median, p90, values[samples - 1])
    });
    Some(FieldStats {
        samples,
        invalid,
        finite_samples,
        finite_only_vector_rmse,
        vector_rmse: metrics.map(|m| m.0),
        median_error: metrics.map(|m| m.1),
        p90_error: metrics.map(|m| m.2),
        max_error: metrics.map(|m| m.3),
    })
}

/// Score the belief's physical-planet field on battlefield grid centers, splitting by cells visited
/// by completed, timed trajectory segments.
pub fn score_field(
    truth: &PlanetMap,
    belief: &MapBelief,
    observations: &[ShotObservation],
    world: WorldParameters,
) -> Option<FieldScore> {
    if !world.battlefield_width.is_finite()
        || !world.battlefield_height.is_finite()
        || world.battlefield_width <= 0.0
        || world.battlefield_height <= 0.0
    {
        return None;
    }
    let mut explored_cells = BTreeSet::new();
    for shot in observations {
        let timed = &shot.points[..shot.points.len().saturating_sub(1)];
        explored_cells.extend(path_cells(timed));
    }
    let mut inside = 0;
    let mut explored = Vec::new();
    let mut unexplored = Vec::new();
    let nx = (world.battlefield_width / CELL_SIZE).ceil() as i32;
    let ny = (world.battlefield_height / CELL_SIZE).ceil() as i32;
    for x in 0..nx {
        for y in 0..ny {
            let position = Vec2::new(
                (f64::from(x) + 0.5) * CELL_SIZE,
                (f64::from(y) + 0.5) * CELL_SIZE,
            );
            if position.x >= world.battlefield_width || position.y >= world.battlefield_height {
                continue;
            }
            if truth
                .planets
                .iter()
                .any(|p| (p.x - position.x).hypot(p.y - position.y) <= p.radius)
            {
                inside += 1;
                continue;
            }
            let error = truth.acceleration(position).ok().and_then(|actual| {
                belief.map.acceleration(position).ok().and_then(|estimate| {
                    let error = (actual.x - estimate.x).hypot(actual.y - estimate.y);
                    error.is_finite().then_some(error)
                })
            });
            if explored_cells.contains(&(x, y)) {
                explored.push(error);
            } else {
                unexplored.push(error);
            }
        }
    }
    let explored_cells = explored.len();
    let unexplored_cells = unexplored.len();
    let whole = explored.iter().chain(&unexplored).copied().collect();
    Some(FieldScore {
        explored_cells,
        unexplored_cells,
        explored: stats(explored),
        unexplored: stats(unexplored),
        whole: stats(whole),
        excluded_truth_interior: inside,
    })
}
