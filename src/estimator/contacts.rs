//! Possible surface contacts: terminal points that no known cause other than a planet explains.
//!
//! A contact is additional evidence only. It never creates a planet; the trajectory fit does.
//! The thresholds below are heuristics under NewtonWars defaults (player radius 4, trail limit
//! 2000 samples), not derived or calibrated values.
use crate::protocol::Point;
use crate::simulation::WorldParameters;

/// Distance within which a known player would explain a terminal point (radius 4 plus slack).
const PLAYER_HIT_DISTANCE: f64 = 4.1;
/// Samples at which the server stops a trail without any collision.
const TRAIL_LIMIT: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Contact {
    pub point: Point,
    /// How far inside the surface the terminal substep may lie: the projectile is stopped at the
    /// first substep inside the disk, so penetration is at most about one substep of travel.
    pub allowance: f64,
}

/// Classify a finished path's terminal point. `players` must hold every tracked pre-finish
/// position: an empty list cannot rule out a player hit, so it yields no contact.
pub(super) fn classify(
    points: &[Point],
    players: &[Point],
    world: WorldParameters,
) -> Option<Contact> {
    let point = points.last()?;
    let (x, y) = (f64::from(point.x), f64::from(point.y));
    if players.is_empty()
        || points.len() >= TRAIL_LIMIT
        || !points
            .iter()
            .all(|point| point.x.is_finite() && point.y.is_finite())
        || x <= -world.margin + 0.1
        || x >= world.battlefield_width + world.margin - 0.1
        || y <= -world.margin + 0.1
        || y >= world.battlefield_height + world.margin - 0.1
        || players.iter().any(|player| {
            (x - f64::from(player.x)).hypot(y - f64::from(player.y)) <= PLAYER_HIT_DISTANCE
        })
    {
        return None;
    }
    let step = match points {
        [.., before, last_regular, _] => (f64::from(last_regular.x) - f64::from(before.x))
            .hypot(f64::from(last_regular.y) - f64::from(before.y)),
        _ => 0.0,
    };
    Some(Contact {
        point: point.clone(),
        allowance: (2.0 * step / world.substeps_per_tick as f64).max(0.5),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(count: usize) -> Vec<Point> {
        (0..count)
            .map(|i| Point {
                x: 500.0 + i as f32,
                y: 500.0,
            })
            .collect()
    }

    #[test]
    fn terminal_point_is_a_contact_unless_a_known_cause_explains_it() {
        let world = WorldParameters::default();
        let points = line(10);
        let far = [Point { x: 0.0, y: 0.0 }];
        assert!(classify(&points, &far, world).is_some());
        assert!(
            classify(&points, &[points.last().unwrap().clone()], world).is_none(),
            "known player at the terminal point"
        );
        assert!(classify(&points, &[], world).is_none(), "players unknown");
        assert!(classify(&line(TRAIL_LIMIT), &far, world).is_none());
        let mut margin = points.clone();
        margin.last_mut().unwrap().x = -(world.margin as f32) - 1.0;
        assert!(classify(&margin, &far, world).is_none());
    }
}
