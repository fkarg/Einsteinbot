//! Offline truth-scoring of a frozen belief on a common spatial grid.
//! Truth is an input only to this scorer and never to an estimator.
use crate::coverage::{CELL_SIZE, path_cells};
use crate::local_field::LocalField;
use crate::map::{PlanetMap, Vec2};
use crate::protocol::Planet;
use crate::simulation::{ShotObservation, WorldParameters, field_acceleration};
use crate::strategy::MapBelief;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldModel {
    Physical,
    NoSource,
    Validated,
    ValidatedSourceOnly,
    Provisional,
    ProvisionalSourceOnly,
}

#[derive(Clone, Debug, Serialize)]
pub struct FieldStats {
    pub samples: usize,
    pub invalid: usize,
    pub finite_samples: usize,
    /// Diagnostic conditional on finite predictions; never replaces the complete score.
    pub finite_only_vector_rmse: Option<f64>,
    /// Number of sampled locations with local residual support, if this model uses one.
    pub residual_supported: Option<usize>,
    pub vector_rmse: Option<f64>,
    pub median_error: Option<f64>,
    pub p90_error: Option<f64>,
    pub max_error: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct FieldScore {
    pub model: FieldModel,
    pub explored_cells: usize,
    pub unexplored_cells: usize,
    pub explored: Option<FieldStats>,
    pub unexplored: Option<FieldStats>,
    pub whole: Option<FieldStats>,
    pub excluded_truth_interior: usize,
}

fn stats(errors: Vec<(Option<f64>, Option<bool>)>) -> Option<FieldStats> {
    if errors.is_empty() {
        return None;
    }
    let samples = errors.len();
    let invalid = errors.iter().filter(|(error, _)| error.is_none()).count();
    let residual_supported = errors.first().and_then(|(_, support)| {
        support.map(|_| errors.iter().filter(|(_, s)| *s == Some(true)).count())
    });
    let mut values = errors
        .into_iter()
        .filter_map(|(error, _)| error)
        .collect::<Vec<_>>();
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
        residual_supported,
        vector_rmse: metrics.map(|m| m.0),
        median_error: metrics.map(|m| m.1),
        p90_error: metrics.map(|m| m.2),
        max_error: metrics.map(|m| m.3),
    })
}

/// Score identical battlefield grid centers for all models, splitting by cells visited
/// by completed, timed trajectory segments. An unavailable model returns None.
pub fn score_field(
    truth: &PlanetMap,
    belief: &MapBelief,
    observations: &[ShotObservation],
    world: WorldParameters,
    model: FieldModel,
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
    let provisional = match model {
        FieldModel::Provisional | FieldModel::ProvisionalSourceOnly => {
            Some(belief.provisional_field_hypothesis.as_ref()?)
        }
        _ => None,
    };
    let provisional_residual = provisional
        .filter(|_| matches!(model, FieldModel::Provisional))
        .map(|p| {
            let mut sources = belief.map.clone();
            sources.planets.push(Planet {
                x: p.source.x,
                y: p.source.y,
                mass: p.source.mass,
                radius: 0.0,
            });
            LocalField::from_observations(&sources, observations)
        });
    let (residual, hypothesis) = match model {
        FieldModel::Physical => (None, None),
        FieldModel::NoSource => (Some(&belief.local_field), None),
        FieldModel::Validated => (
            Some(belief.hypothesis_local_field.as_ref()?),
            Some(belief.field_hypothesis.as_ref()?),
        ),
        FieldModel::ValidatedSourceOnly => (None, Some(belief.field_hypothesis.as_ref()?)),
        FieldModel::Provisional => (
            provisional_residual.as_ref(),
            provisional.map(|p| &p.source),
        ),
        FieldModel::ProvisionalSourceOnly => (None, provisional.map(|p| &p.source)),
    };
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
                field_acceleration(&belief.map, residual, hypothesis, position)
                    .ok()
                    .and_then(|estimate| {
                        let error = (actual.x - estimate.x).hypot(actual.y - estimate.y);
                        error.is_finite().then_some(error)
                    })
            });
            let support = residual.map(|field| field.residual(position).is_some());
            if explored_cells.contains(&(x, y)) {
                explored.push((error, support));
            } else {
                unexplored.push((error, support));
            }
        }
    }
    let explored_cells = explored.len();
    let unexplored_cells = unexplored.len();
    let whole = explored.iter().chain(&unexplored).copied().collect();
    Some(FieldScore {
        model,
        explored_cells,
        unexplored_cells,
        explored: stats(explored),
        unexplored: stats(unexplored),
        whole: stats(whole),
        excluded_truth_interior: inside,
    })
}
