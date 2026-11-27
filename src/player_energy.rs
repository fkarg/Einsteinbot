//! Every player's energy, inferred from the broadcast events.
//!
//! The server reports only our own energy (`OwnEnergy`). The rules (NewtonWars
//! `simulation.c`): every player gains the same energy per step up to a limit (default 200), a
//! shot costs its velocity, a spawn (join, respawn after a kill, new map) sets 20, and a killer
//! gains half of the victim's energy (`playerHit`; a self-kill gains nothing). Shots and kills
//! are broadcast (`ShotBegin`, the killing shot's `ShotFin` ending at the victim, then the
//! victim's new `PlayerPosition`), so a player's energy is known from their first observed
//! spawn on. Players first seen without a spawn (already present when we joined) stay unknown,
//! never guessed. The regeneration rate is measured from our own energy.

use crate::protocol::Point;
use std::collections::BTreeMap;

pub const SPAWN_ENERGY: f64 = 20.0;
/// Upstream default `limit`; a server may configure another.
pub const ENERGY_LIMIT: f64 = 200.0;
/// Energy per second until measured (local servers: about 2.5 at 30.5 steps per second).
pub const DEFAULT_ENERGY_RATE: f64 = 2.5;
/// A shot ending this close to a player's old position killed them (as `map_change`).
const KILL_RADIUS: f32 = 12.0;

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EnergyTracker {
    /// Energy and the local time it was known at, per player with a known energy.
    known: BTreeMap<u32, (f64, i64)>,
    /// Energy per second measured from our own energy reports.
    rate_per_s: Option<f64>,
    /// Our latest energy report, and whether anything but regeneration changed it since (a
    /// shot, a respawn, a kill).
    last_own: Option<(f64, i64, bool)>,
    /// Our player, from our energy reports.
    own_id: Option<u32>,
    /// The latest finished shot (shooter, end point), the candidate killer of the next
    /// reposition.
    last_shot_end: Option<(u32, Point)>,
    /// A map boundary was observed: every player seen for the first time since spawned (at
    /// the boundary or by joining) with `SPAWN_ENERGY`.
    seen_boundary: bool,
}

impl EnergyTracker {
    pub fn rate_per_s(&self) -> f64 {
        self.rate_per_s.unwrap_or(DEFAULT_ENERGY_RATE)
    }

    /// The energy of `player` at `now_ms`, or `None` when it is not known.
    pub fn energy(&self, player: u32, now_ms: i64) -> Option<f64> {
        let (energy, at_ms) = self.known.get(&player)?;
        let regen = self.rate_per_s() * (now_ms - at_ms).max(0) as f64 / 1000.0;
        Some((energy + regen).min(ENERGY_LIMIT.max(*energy)))
    }

    /// A new map: every listed player respawns with `SPAWN_ENERGY`.
    pub fn new_map(&mut self, players: impl IntoIterator<Item = u32>, now_ms: i64) {
        self.known = players
            .into_iter()
            .map(|id| (id, (SPAWN_ENERGY, now_ms)))
            .collect();
        self.last_shot_end = None;
        self.seen_boundary = true;
        if let Some(own) = &mut self.last_own {
            own.2 = true;
        }
    }

    /// A shot began. The server fires only with at least the velocity in energy, which corrects an
    /// underestimate. Debris of a kill begins as the victim's shots just before the respawn
    /// resets them.
    pub fn shot_begin(&mut self, player: u32, velocity: f64, now_ms: i64) {
        if let Some(energy) = self.energy(player, now_ms) {
            self.known
                .insert(player, (energy.max(velocity) - velocity, now_ms));
        }
        self.disturb_own(player);
    }

    /// The next energy report does not measure the rate if `player` is us.
    fn disturb_own(&mut self, player: u32) {
        if Some(player) == self.own_id
            && let Some(last) = &mut self.last_own
        {
            last.2 = true;
        }
    }

    pub fn shot_finished(&mut self, player: u32, end: Option<&Point>) {
        self.last_shot_end = end.map(|end| (player, end.clone()));
    }

    /// `player` moved from `old` (`None`: first sighting). A move is a respawn; a shot that
    /// just ended at the old position was the kill, and its shooter gains half the energy.
    /// A first sighting is a spawn too once a boundary was seen; before that (players present
    /// when we joined) the energy is unknown.
    pub fn repositioned(&mut self, player: u32, old: Option<&Point>, now_ms: i64) {
        let shot_end = self.last_shot_end.take();
        let Some(old) = old else {
            if self.seen_boundary {
                self.known.insert(player, (SPAWN_ENERGY, now_ms));
            }
            return;
        };
        let victim_energy = self.energy(player, now_ms);
        self.known.insert(player, (SPAWN_ENERGY, now_ms));
        self.disturb_own(player);
        let killer = shot_end.and_then(|(shooter, end)| {
            ((end.x - old.x).hypot(end.y - old.y) <= KILL_RADIUS).then_some(shooter)
        });
        if let Some(killer) = killer.filter(|killer| *killer != player) {
            self.disturb_own(killer);
            match (victim_energy, self.energy(killer, now_ms)) {
                (Some(victim), Some(energy)) => {
                    self.known.insert(killer, (energy + victim / 2.0, now_ms));
                }
                _ => {
                    self.known.remove(&killer);
                }
            }
        }
    }

    pub fn player_left(&mut self, player: u32) {
        self.known.remove(&player);
    }

    /// Our energy as reported by the server: exact, and a rate sample when nothing but
    /// regeneration happened since the previous report.
    pub fn own_energy(&mut self, player: u32, energy: f64, now_ms: i64) {
        if let Some((previous, at_ms, disturbed)) = self.last_own
            && !disturbed
            && now_ms - at_ms >= 500
            && energy < ENERGY_LIMIT
            && energy >= previous
        {
            let sample = (energy - previous) / ((now_ms - at_ms) as f64 / 1000.0);
            self.rate_per_s = Some(match self.rate_per_s {
                Some(rate) => 0.8 * rate + 0.2 * sample,
                None => sample,
            });
        }
        self.last_own = Some((energy, now_ms, false));
        self.own_id = Some(player);
        self.known.insert(player, (energy, now_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    #[test]
    fn energy_follows_spawns_shots_kills_and_regeneration() {
        let mut tracker = EnergyTracker::default();
        // Players already present when we joined are unknown until they spawn.
        tracker.repositioned(2, None, 0);
        assert_eq!(tracker.energy(2, 0), None);

        // Our reports measure the rate: 5 energy in 2 s.
        tracker.own_energy(1, 10.0, 0);
        tracker.own_energy(1, 15.0, 2_000);
        assert!((tracker.rate_per_s() - 2.5).abs() < 1e-12);

        // A new map: everybody has 20, then regenerates.
        tracker.new_map([1, 2, 3], 10_000);
        assert_eq!(tracker.energy(3, 12_000), Some(25.0));

        // A shot proves its shooter had at least the velocity in energy.
        tracker.shot_begin(1, 30.0, 10_000);
        assert_eq!(tracker.energy(1, 10_000), Some(0.0));

        // Player 3 shoots with velocity 15, then player 2 kills player 3 (the shot ends at
        // player 3, who respawns): 2 gains half of 3's energy, 3 restarts at 20.
        tracker.shot_begin(3, 15.0, 12_000);
        assert_eq!(tracker.energy(3, 12_000), Some(10.0));
        tracker.shot_finished(2, Some(&at(503.0, 100.0)));
        tracker.repositioned(3, Some(&at(500.0, 100.0)), 12_000);
        assert_eq!(tracker.energy(3, 12_000), Some(20.0));
        assert_eq!(tracker.energy(2, 12_000), Some(30.0));

        // A respawn with no shot ending there (e.g. a self-kill elsewhere) transfers nothing.
        tracker.shot_finished(2, Some(&at(900.0, 900.0)));
        tracker.repositioned(3, Some(&at(10.0, 10.0)), 12_000);
        assert_eq!(tracker.energy(2, 12_000), Some(30.0));

        // After a boundary a newcomer (a join) spawns with 20.
        tracker.repositioned(5, None, 12_000);
        assert_eq!(tracker.energy(5, 12_000), Some(20.0));

        // Energy is capped at the server limit.
        assert_eq!(tracker.energy(2, 1_000_000), Some(ENERGY_LIMIT));
        tracker.player_left(2);
        assert_eq!(tracker.energy(2, 12_000), None);
    }
}
