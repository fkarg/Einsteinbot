//! Round timing: how long the current map's playing phase has left.
//!
//! NewtonWars plays a round for a fixed number of server steps (`roundtime`), then shows the
//! end card (no shot moves, in-flight shots never land) and starts a new map with every
//! player's energy reset to 20. The length of the playing phase is learned from the latest map
//! observed from its boundary to its end (user rule: no estimate after joining mid-map): its
//! last finished shot marks the end of play. Rounds measured here vary by a few seconds with
//! the server's step pace, so the estimate is a heuristic, not a schedule.

/// A shorter "round" is not a full one (e.g. repeated manual reinitialization); it is ignored.
pub const MIN_ROUND_MS: i64 = 10_000;

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RoundClock {
    /// Start of the current map when it began at an observed boundary (`None`: joined mid-map
    /// or no clock).
    map_started_ms: Option<i64>,
    /// Latest finished shot of the current map.
    last_finish_ms: Option<i64>,
    /// Playing-phase length of the latest fully observed map: its start to its last finished
    /// shot.
    play_ms: Option<i64>,
}

impl RoundClock {
    /// A map boundary at `now_ms`: the ending map, if observed from its start, measures the
    /// round length. Calling it twice for one boundary keeps the measurement.
    pub fn new_map(&mut self, now_ms: Option<i64>) {
        if let (Some(start), Some(last)) = (self.map_started_ms, self.last_finish_ms) {
            if last - start >= MIN_ROUND_MS {
                self.play_ms = Some(last - start);
            }
        }
        self.map_started_ms = now_ms;
        self.last_finish_ms = None;
    }

    pub fn shot_finished(&mut self, now_ms: i64) {
        self.last_finish_ms = Some(now_ms);
    }

    /// Estimated milliseconds of play left at `now_ms` (negative once the estimated end has
    /// passed); `None` until a full map has been observed and the current one began at a
    /// boundary.
    pub fn time_left_ms(&self, now_ms: i64) -> Option<i64> {
        Some(self.map_started_ms? + self.play_ms? - now_ms)
    }

    /// Restore a round length measured elsewhere (offline replays of one-map captures).
    pub fn set_play_ms(&mut self, play_ms: Option<i64>) {
        self.play_ms = play_ms;
    }

    pub fn play_ms(&self) -> Option<i64> {
        self.play_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_length_is_known_only_after_a_full_observed_map() {
        let mut clock = RoundClock::default();
        // Joined mid-map: the first map's length is unknown.
        clock.shot_finished(5_000);
        clock.new_map(Some(20_000));
        assert_eq!(clock.time_left_ms(21_000), None);

        // The second map is observed from its boundary to its last finished shot.
        clock.shot_finished(60_000);
        clock.shot_finished(69_000);
        clock.new_map(Some(80_000));
        clock.new_map(Some(80_000)); // The same boundary reported twice.
        assert_eq!(clock.play_ms(), Some(49_000));
        assert_eq!(clock.time_left_ms(120_000), Some(9_000));

        // A one-second "map" (manual reinitialization) does not replace the measurement.
        clock.shot_finished(80_500);
        clock.new_map(Some(81_000));
        assert_eq!(clock.play_ms(), Some(49_000));

        // Without a boundary clock the current map's start is unknown.
        clock.new_map(None);
        assert_eq!(clock.time_left_ms(90_000), None);
    }
}
