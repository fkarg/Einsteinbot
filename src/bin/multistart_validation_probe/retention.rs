//! Truth-free pruning rules for the offline candidate experiment.
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::simulation::ShotObservation;

#[derive(Clone, Copy, Debug)]
pub enum Rule {
    Ranked,
    Count,
    Field,
}

/// Index 0 is the pinned no-source model. `slots` counts additional models.
pub fn retain(
    candidates: &[(&str, PlanetMap, f64)],
    observed: &[ShotObservation],
    slots: usize,
    rule: Rule,
) -> Vec<usize> {
    if candidates.is_empty() {
        return vec![];
    }
    let mut ranked: Vec<_> = (1..candidates.len())
        .filter(|&i| candidates[i].2.is_finite())
        .collect();
    ranked.sort_by(|&a, &b| candidates[a].2.total_cmp(&candidates[b].2).then(a.cmp(&b)));
    let mut keep = vec![0];
    if slots == 0 || ranked.is_empty() {
        return keep;
    }
    keep.push(ranked.remove(0));
    while keep.len() <= slots && !ranked.is_empty() {
        let choice = match rule {
            Rule::Ranked => 0,
            Rule::Count => ranked
                .iter()
                .position(|&i| {
                    !keep[1..]
                        .iter()
                        .any(|&j| candidates[i].1.planets.len() == candidates[j].1.planets.len())
                })
                .unwrap_or(0),
            Rule::Field => ranked
                .iter()
                .enumerate()
                .filter_map(|(rank, &i)| {
                    let distances: Vec<_> = keep[1..]
                        .iter()
                        .map(|&j| distance(&candidates[i].1, &candidates[j].1, observed))
                        .collect();
                    if distances.iter().any(Option::is_none) {
                        return None;
                    }
                    Some((
                        rank,
                        distances
                            .into_iter()
                            .flatten()
                            .fold(f64::INFINITY, f64::min),
                    ))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                .map(|x| x.0)
                .unwrap_or(0),
        };
        keep.push(ranked.remove(choice));
    }
    keep
}

pub(super) fn distance(a: &PlanetMap, b: &PlanetMap, paths: &[ShotObservation]) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0;
    for shot in paths {
        for point in shot
            .points
            .iter()
            .take(shot.points.len().saturating_sub(1))
            .step_by(10)
        {
            let p = Vec2::new(point.x.into(), point.y.into());
            let (x, y) = (a.acceleration(p).ok()?, b.acceleration(p).ok()?);
            let d = (x.x - y.x).hypot(x.y - y.y);
            if !d.is_finite() {
                return None;
            }
            sum += d;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use einsteinbot::protocol::{Planet, Point};
    fn map(x: f64, mass: f64, count: usize) -> PlanetMap {
        PlanetMap {
            planets: (0..count)
                .map(|_| Planet {
                    x,
                    y: 100.0,
                    radius: 20.0,
                    mass,
                })
                .collect(),
        }
    }
    #[test]
    fn pinned_baseline_and_alternative_rules_use_only_input_scores_and_fields() {
        let c = vec![
            ("flat", map(0.0, 0.0, 0), 10.0),
            ("a", map(100.0, 1000.0, 1), 1.0),
            ("duplicate", map(100.0, 1000.0, 1), 2.0),
            ("different", map(300.0, 1000.0, 2), 3.0),
        ];
        let paths = vec![ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 1.0,
            points: vec![Point { x: 200.0, y: 200.0 }, Point { x: 201.0, y: 200.0 }],
        }];
        assert_eq!(retain(&c, &paths, 0, Rule::Ranked), vec![0]);
        assert_eq!(retain(&c, &paths, 2, Rule::Ranked), vec![0, 1, 2]);
        assert_eq!(retain(&c, &paths, 2, Rule::Count), vec![0, 1, 3]);
        assert_eq!(retain(&c, &paths, 2, Rule::Field), vec![0, 1, 3]);
        assert_eq!(retain(&c, &paths, 9, Rule::Field).len(), 4);
    }

    #[test]
    fn field_diversity_distinguishes_equal_counts_and_falls_back_without_samples() {
        let c = vec![
            ("flat", map(0.0, 0.0, 0), 0.0),
            ("best", map(100.0, 1000.0, 1), 1.0),
            ("duplicate", map(100.0, 1000.0, 1), 2.0),
            ("other", map(300.0, 1000.0, 1), 3.0),
            ("invalid", map(400.0, 1000.0, 1), f64::NAN),
        ];
        let path = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 1.0,
            points: vec![Point { x: 200.0, y: 200.0 }, Point { x: 201.0, y: 200.0 }],
        };
        assert_eq!(retain(&c, &[path], 2, Rule::Field), vec![0, 1, 3]);
        assert_eq!(retain(&c, &[], 2, Rule::Field), vec![0, 1, 2]);
        assert_eq!(retain(&c, &[], 9, Rule::Ranked), vec![0, 1, 2, 3]);
    }
}
