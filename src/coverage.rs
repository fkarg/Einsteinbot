//! Coarse spatial coverage, used only as a diversity heuristic, not a posterior.
use crate::protocol::Point;
use std::collections::BTreeSet;

pub const CELL_SIZE: f64 = 20.0;

/// Distinct cells crossed by path segments, excluding the shared launch cell.
/// Callers omit untimed terminal observations before measuring field coverage.
pub fn path_cells(points: &[Point]) -> BTreeSet<(i32, i32)> {
    let mut cells = BTreeSet::new();
    for segment in points.windows(2) {
        let x = f64::from(segment[0].x) / CELL_SIZE;
        let y = f64::from(segment[0].y) / CELL_SIZE;
        let end_x = f64::from(segment[1].x) / CELL_SIZE;
        let end_y = f64::from(segment[1].y) / CELL_SIZE;
        if ![x, y, end_x, end_y].iter().all(|value| value.is_finite()) {
            continue;
        }
        let mut cell = (x.floor() as i32, y.floor() as i32);
        let end = (end_x.floor() as i32, end_y.floor() as i32);
        let dx = end_x - x;
        let dy = end_y - y;
        let step_x = if dx > 0.0 { 1 } else { -1 };
        let step_y = if dy > 0.0 { 1 } else { -1 };
        let mut tx = if dx == 0.0 {
            f64::INFINITY
        } else {
            (f64::from(cell.0 + i32::from(dx > 0.0)) - x) / dx
        };
        let mut ty = if dy == 0.0 {
            f64::INFINITY
        } else {
            (f64::from(cell.1 + i32::from(dy > 0.0)) - y) / dy
        };
        cells.insert(cell);
        while cell != end {
            // A boundary endpoint belongs to its floor cell; do not step an
            // already-finished axis past it when both crossings have t=1.
            if cell.0 == end.0 {
                tx = f64::INFINITY;
            }
            if cell.1 == end.1 {
                ty = f64::INFINITY;
            }
            if tx < ty {
                cell.0 += step_x;
                tx += 1.0 / dx.abs();
            } else if ty < tx {
                cell.1 += step_y;
                ty += 1.0 / dy.abs();
            } else {
                cell.0 += step_x;
                cell.1 += step_y;
                tx += 1.0 / dx.abs();
                ty += 1.0 / dy.abs();
            }
            cells.insert(cell);
        }
    }
    if let Some(launch) = points.first() {
        cells.remove(&(
            (f64::from(launch.x) / CELL_SIZE).floor() as i32,
            (f64::from(launch.y) / CELL_SIZE).floor() as i32,
        ));
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_direction_grid_corner_terminates_in_the_endpoint_cell() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(path_cells(&[
                Point { x: 1.0, y: 1.0 },
                Point { x: 20.0, y: 0.0 },
            ]))
            .ok();
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1))
                .expect("grid traversal must terminate"),
            BTreeSet::from([(1, 0)])
        );
    }

    #[test]
    fn coverage_is_independent_of_sampling_density_and_direction() {
        let sparse = vec![Point { x: 1.0, y: 1.0 }, Point { x: 81.0, y: 1.0 }];
        let dense = (1..=81)
            .map(|x| Point {
                x: x as f32,
                y: 1.0,
            })
            .collect::<Vec<_>>();
        let expected = BTreeSet::from([(1, 0), (2, 0), (3, 0), (4, 0)]);
        assert_eq!(path_cells(&sparse), expected);
        assert_eq!(path_cells(&dense), expected);
        assert_eq!(
            path_cells(&[sparse[1].clone(), sparse[0].clone()]),
            BTreeSet::from([(0, 0), (1, 0), (2, 0), (3, 0)])
        );
        assert_eq!(
            path_cells(&[Point { x: 1.0, y: 1.0 }, Point { x: -41.0, y: -41.0 }]),
            BTreeSet::from([(-1, -1), (-2, -2), (-3, -3)])
        );
    }
}
