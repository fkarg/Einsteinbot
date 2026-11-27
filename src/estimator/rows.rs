//! Row thinning for the search stages: the fit stays cheap and its cost independent of the number
//! of paths by keeping a bounded set of the most informative rows (measured in
//! `docs/estimator-experiments.md`, E9 and P6).
use super::Shot;

/// Rows are kept at most `PER_CELL` per `CELL`-unit cell of the battlefield.
const CELL: f64 = 14.0;
const PER_CELL: usize = 2;

/// The rows of `shots` to keep: at most `PER_CELL` per cell and `cap` in all, strongest observed
/// acceleration first. Each path keeps its own noise model (and contact); a path left without rows
/// has none. The row noise of a thinned path drops the correlation between neighbouring rows, so
/// the result is an approximate objective for searching only.
pub(super) fn thin_rows(shots: &[Shot], cap: usize) -> Vec<Shot> {
    // (shot, row, strength, cell)
    let mut candidates: Vec<(usize, usize, f64, (i64, i64))> = Vec::new();
    for (s, shot) in shots.iter().enumerate() {
        let Some(path) = &shot.path else { continue };
        for r in 0..path.row_count() {
            let c = path.center(r);
            candidates.push((
                s,
                r,
                path.strength(r),
                ((c[0] / CELL).floor() as i64, (c[1] / CELL).floor() as i64),
            ));
        }
    }
    candidates.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut count: std::collections::HashMap<(i64, i64), usize> = Default::default();
    let mut kept: Vec<Vec<usize>> = vec![Vec::new(); shots.len()];
    let mut total = 0;
    for (s, r, _, cell) in candidates {
        if total >= cap {
            break;
        }
        let n = count.entry(cell).or_default();
        if *n < PER_CELL {
            *n += 1;
            kept[s].push(r);
            total += 1;
        }
    }
    shots
        .iter()
        .zip(kept)
        .map(|(shot, mut keep)| {
            keep.sort_unstable();
            Shot {
                obs: shot.obs.clone(),
                contact: shot.contact.clone(),
                path: shot.path.as_ref().and_then(|p| p.thinned(&keep)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Point;
    use crate::simulation::{ShotObservation, WorldParameters};

    /// The bounded row set respects the row cap and the per-cell limit.
    #[test]
    fn thinning_bounds_rows_and_cells() {
        let points: Vec<Point> = (0..80)
            .map(|i| Point {
                x: 100.0 + 0.3 * (i * i) as f32 / 10.0,
                y: 200.0 + 0.5 * i as f32,
            })
            .collect();
        let obs = ShotObservation {
            player_id: 1,
            angle_degrees: 10.0,
            velocity: 4.0,
            points,
        };
        let shots = vec![Shot::new(obs, None, WorldParameters::default(), 0.005)];
        let thin = thin_rows(&shots, 30);
        let path = thin[0].path.as_ref().unwrap();
        assert!(path.row_count() <= 30 && path.row_count() > 0);
        let mut cells = std::collections::HashMap::new();
        for r in 0..path.row_count() {
            let c = path.center(r);
            *cells
                .entry(((c[0] / CELL).floor() as i64, (c[1] / CELL).floor() as i64))
                .or_insert(0) += 1;
        }
        assert!(cells.values().all(|&n| n <= PER_CELL));
    }
}
