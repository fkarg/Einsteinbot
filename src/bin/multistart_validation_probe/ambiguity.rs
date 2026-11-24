//! Offline, uncalibrated ambiguity diagnostics. Never used by live targeting.
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::simulation::{ShotObservation, WorldParameters, simulate_sampled_positions};

/// A transparent fit screen, not a posterior probability or confidence interval.
/// The tolerance is relative to best validation RMSE, with an absolute noise floor.
pub fn qualified(scores: &[f64], relative: f64, floor: f64) -> Vec<usize> {
    let Some(best) = scores
        .iter()
        .copied()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .min_by(f64::total_cmp)
    else {
        return vec![];
    };
    let limit = best + floor.max(relative * best);
    let mut indices: Vec<_> = scores
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite() && **v >= 0.0 && **v <= limit)
        .map(|(i, _)| i)
        .collect();
    indices.sort_by(|&a, &b| scores[a].total_cmp(&scores[b]).then(a.cmp(&b)));
    indices
}

/// Select distinct qualified fields; return both all qualifying indices and
/// the capped set. No truth or future observation is accepted by this API.
pub fn select(
    candidates: &[(&str, PlanetMap, f64)],
    observed: &[ShotObservation],
    relative: f64,
    floor: f64,
    cap: usize,
) -> (Vec<usize>, Vec<usize>) {
    let scores: Vec<_> = candidates.iter().map(|c| c.2).collect();
    let all = qualified(&scores, relative, floor);
    let mut pool = all.clone();
    let mut selected = Vec::new();
    if cap > 0 && !pool.is_empty() {
        selected.push(pool.remove(0));
    }
    while selected.len() < cap && !pool.is_empty() {
        let best = pool
            .iter()
            .enumerate()
            .filter_map(|(rank, &i)| {
                let mut minimum = f64::INFINITY;
                for &j in &selected {
                    minimum = minimum.min(super::retention::distance(
                        &candidates[i].1,
                        &candidates[j].1,
                        observed,
                    )?);
                }
                Some((rank, minimum))
            })
            .filter(|(_, d)| *d > 1e-9)
            .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
        let Some((rank, _)) = best else {
            break;
        };
        selected.push(pool.remove(rank));
    }
    (all, selected)
}

/// Maximum pairwise field disagreement at a location; absent for <2 models or
/// an invalid field. Zero disagreement does not imply the field is known.
pub fn field_spread(maps: &[&PlanetMap], position: Vec2) -> Option<f64> {
    if maps.len() < 2 {
        return None;
    }
    let values: Vec<_> = maps
        .iter()
        .map(|m| m.acceleration(position).ok())
        .collect::<Option<_>>()?;
    let mut maximum: f64 = 0.0;
    for (i, a) in values.iter().enumerate() {
        for b in &values[i + 1..] {
            let d = (a.x - b.x).hypot(a.y - b.y);
            if !d.is_finite() {
                return None;
            }
            maximum = maximum.max(d);
        }
    }
    Some(maximum)
}

#[derive(Debug)]
pub struct Prediction {
    pub members: usize,
    pub failures: usize,
    pub nominal_rmse: Option<f64>,
    pub member_rmse: Vec<Option<f64>>,
    pub mean_member_rmse: Option<f64>,
    pub mean_rmse: Option<f64>,
    pub max_spread: Option<f64>,
}

/// Simulate each gravity-only map separately, at fixed timed horizons without
/// collision handling; all members must succeed for ensemble summaries.
pub fn predict(maps: &[&PlanetMap], shot: &ShotObservation, world: WorldParameters) -> Prediction {
    let count = shot.points.len().saturating_sub(1);
    let mut result = Prediction {
        members: maps.len(),
        failures: 0,
        nominal_rmse: None,
        member_rmse: Vec::new(),
        mean_member_rmse: None,
        mean_rmse: None,
        max_spread: None,
    };
    if maps.is_empty() || count < 2 {
        return result;
    }
    let predicted: Vec<_> = maps
        .iter()
        .map(|m| {
            simulate_sampled_positions(m, shot, world, count)
                .ok()
                .filter(|p| {
                    p.len() == count && p.iter().all(|v| v.x.is_finite() && v.y.is_finite())
                })
        })
        .collect();
    result.failures = predicted.iter().filter(|p| p.is_none()).count();
    let rmse = |positions: &[Vec2]| {
        let mse: f64 = positions
            .iter()
            .zip(&shot.points)
            .skip(1)
            .map(|(p, o)| (p.x - f64::from(o.x)).powi(2) + (p.y - f64::from(o.y)).powi(2))
            .sum();
        let v = (mse / (2 * (count - 1)) as f64).sqrt();
        v.is_finite().then_some(v)
    };
    result.member_rmse = predicted
        .iter()
        .map(|p| p.as_ref().and_then(|p| rmse(p)))
        .collect();
    result.nominal_rmse = result.member_rmse[0];
    if result.failures != 0 || result.member_rmse.iter().any(Option::is_none) {
        return result;
    }
    result.mean_member_rmse =
        Some(result.member_rmse.iter().flatten().sum::<f64>() / maps.len() as f64);
    let predicted: Vec<_> = predicted.into_iter().flatten().collect();
    let mut mean = vec![Vec2::default(); count];
    let mut max_spread: f64 = 0.0;
    for t in 0..count {
        for trajectory in &predicted {
            mean[t].x += trajectory[t].x / maps.len() as f64;
            mean[t].y += trajectory[t].y / maps.len() as f64;
        }
        for (i, a) in predicted.iter().enumerate() {
            for b in &predicted[i + 1..] {
                max_spread = max_spread.max((a[t].x - b[t].x).hypot(a[t].y - b[t].y));
            }
        }
    }
    result.mean_rmse = rmse(&mean);
    result.max_spread = (maps.len() >= 2 && max_spread.is_finite()).then_some(max_spread);
    result
}

/// Scoring-only equal-weight mean field on the common truth-masked grid.
/// A failure in any sampled model invalidates the corresponding complete score.
pub fn mean_field_scores(
    maps: &[&PlanetMap],
    truth: &PlanetMap,
    observed: &[ShotObservation],
    world: WorldParameters,
) -> [Option<f64>; 3] {
    use einsteinbot::coverage::{CELL_SIZE, path_cells};
    use std::collections::BTreeSet;
    if maps.is_empty() {
        return [None; 3];
    }
    let mut cells = BTreeSet::new();
    for shot in observed {
        cells.extend(path_cells(
            &shot.points[..shot.points.len().saturating_sub(1)],
        ));
    }
    let mut errors: [Vec<Option<f64>>; 2] = [vec![], vec![]];
    for x in 0..(world.battlefield_width / CELL_SIZE).ceil() as i32 {
        for y in 0..(world.battlefield_height / CELL_SIZE).ceil() as i32 {
            let pos = Vec2::new(
                (f64::from(x) + 0.5) * CELL_SIZE,
                (f64::from(y) + 0.5) * CELL_SIZE,
            );
            if pos.x >= world.battlefield_width
                || pos.y >= world.battlefield_height
                || truth
                    .planets
                    .iter()
                    .any(|p| (p.x - pos.x).hypot(p.y - pos.y) <= p.radius)
            {
                continue;
            }
            let actual = truth.acceleration(pos).ok();
            let estimated: Option<Vec<_>> = maps.iter().map(|m| m.acceleration(pos).ok()).collect();
            let error = actual.zip(estimated).and_then(|(a, values)| {
                let (mut x, mut y) = (0.0, 0.0);
                for v in values {
                    x += v.x / maps.len() as f64;
                    y += v.y / maps.len() as f64;
                }
                let e = (x - a.x).hypot(y - a.y);
                e.is_finite().then_some(e)
            });
            errors[usize::from(!cells.contains(&(x, y)))].push(error);
        }
    }
    let score = |values: &[Option<f64>]| -> Option<f64> {
        if values.is_empty() || values.iter().any(Option::is_none) {
            return None;
        }
        let maximum = values.iter().flatten().copied().fold(0.0_f64, f64::max);
        if maximum == 0.0 {
            return Some(0.0);
        }
        Some(
            maximum
                * (values
                    .iter()
                    .flatten()
                    .map(|v| (v / maximum).powi(2))
                    .sum::<f64>()
                    / values.len() as f64)
                    .sqrt(),
        )
    };
    let both: Vec<_> = errors.iter().flat_map(|v| v.iter().copied()).collect();
    [score(&errors[0]), score(&errors[1]), score(&both)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use einsteinbot::protocol::{Planet, Point};
    #[test]
    fn qualification_never_pins_bad_baseline_and_keeps_ties() {
        assert_eq!(
            qualified(&[200.0, 100.0, 120.0, 126.0, f64::NAN], 0.25, 0.01),
            vec![1, 2]
        );
        assert_eq!(qualified(&[0.0, 0.01, 0.011], 0.25, 0.01), vec![0, 1]);
        assert!(qualified(&[f64::NAN], 0.25, 0.01).is_empty());
    }
    #[test]
    fn selection_keeps_qualified_distinct_fields_with_a_cap() {
        let flat = PlanetMap { planets: vec![] };
        let source = PlanetMap {
            planets: vec![Planet {
                x: 100.0,
                y: 100.0,
                radius: 20.0,
                mass: 1000.0,
            }],
        };
        let c = vec![
            ("best", flat.clone(), 1.0),
            ("same", flat, 1.1),
            ("source", source, 1.2),
        ];
        let path = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 1.0,
            points: vec![Point { x: 200.0, y: 200.0 }, Point { x: 201.0, y: 200.0 }],
        };
        let (all, kept) = select(&c, &[path], 0.25, 0.01, 2);
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(kept, vec![0, 2]);
    }
    #[test]
    fn disagreement_is_unavailable_for_one_model_and_detects_distinct_fields() {
        let flat = PlanetMap { planets: vec![] };
        let source = PlanetMap {
            planets: vec![Planet {
                x: 100.0,
                y: 100.0,
                radius: 20.0,
                mass: 1000.0,
            }],
        };
        assert_eq!(field_spread(&[&flat], Vec2::new(200.0, 200.0)), None);
        assert!(field_spread(&[&flat, &source], Vec2::new(200.0, 200.0)).unwrap() > 0.0);
        assert_eq!(
            field_spread(&[&flat, &source], Vec2::new(100.0, 100.0)),
            None
        );
    }
    #[test]
    fn mean_field_score_matches_single_model_and_zero_truth() {
        let world = WorldParameters::default();
        let flat = PlanetMap { planets: vec![] };
        let source = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 20.0,
                mass: 1000.0,
            }],
        };
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 1.0,
            points: vec![
                Point { x: 100.0, y: 100.0 },
                Point { x: 200.0, y: 100.0 },
                Point { x: 201.0, y: 100.0 },
            ],
        };
        let scores = mean_field_scores(&[&flat, &source], &flat, &[shot], world);
        let full = mean_field_scores(&[&source], &flat, &[], world);
        assert!(scores[0].is_some());
        assert!((scores[2].unwrap() * 2.0 - full[2].unwrap()).abs() < 1e-12);
        assert_eq!(
            mean_field_scores(&[&flat], &flat, &[], world),
            [None, Some(0.0), Some(0.0)]
        );
    }
    #[test]
    fn gravity_free_prediction_uses_launch_as_sample_zero() {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![
                Point { x: 200.0, y: 400.0 },
                Point { x: 208.0, y: 400.0 },
                Point { x: 216.0, y: 400.0 },
                Point { x: 224.0, y: 400.0 },
                Point { x: 232.0, y: 400.0 },
            ],
        };
        let flat = PlanetMap { planets: vec![] };
        let prediction = predict(&[&flat], &shot, WorldParameters::default());
        assert_eq!(prediction.failures, 0);
        assert!(prediction.nominal_rmse.unwrap() < 1e-9);
    }
    #[test]
    fn gravity_only_forecast_does_not_create_collision_surfaces() {
        let world = WorldParameters::default();
        let field = PlanetMap {
            planets: vec![Planet {
                x: 250.0,
                y: 410.0,
                radius: 50.0,
                mass: 1.0,
            }],
        };
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let points = simulate_sampled_positions(&field, &shot, world, 20).unwrap();
        assert!(
            points
                .iter()
                .any(|p| (p.x - 250.0).hypot(p.y - 410.0) < 50.0)
        );
        shot.points = points
            .iter()
            .map(|p| Point {
                x: p.x as f32,
                y: p.y as f32,
            })
            .collect();
        shot.points.push(shot.points.last().unwrap().clone());
        let result = predict(&[&field], &shot, world);
        assert_eq!(result.failures, 0);
        assert!(result.nominal_rmse.unwrap() < 1e-4);
    }
    #[test]
    fn predicted_paths_show_disagreement_and_exact_flat_nominal() {
        let world = WorldParameters::default();
        let flat = PlanetMap { planets: vec![] };
        let source = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 20.0,
                mass: 1000.0,
            }],
        };
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let points = simulate_sampled_positions(&flat, &shot, world, 20).unwrap();
        shot.points = points
            .iter()
            .map(|p| Point {
                x: p.x as f32,
                y: p.y as f32,
            })
            .collect();
        shot.points.push(shot.points.last().unwrap().clone());
        let p = predict(&[&flat, &source], &shot, world);
        assert_eq!(p.failures, 0);
        assert!(p.nominal_rmse.unwrap() < 1e-4);
        assert!(p.mean_rmse.unwrap() > 0.0);
        assert!(p.max_spread.unwrap() > 0.0);
        assert_eq!(predict(&[&flat], &shot, world).max_spread, None);
    }
}
