//! Path coverage of the battlefield, for the too-high planet-count signal (P25).
//!
//! A planet no path passes near leaves no pull the estimator could find, so with the count fixed
//! at `N` and `K < N` planets found, the other `N - K` must hide where no path has been. The
//! hidden region is the set of possible planet centres farther than a reach from every observed
//! tick and not overlapping a found planet; its fraction `f` of the battlefield shrinks as paths
//! cover the map. `Coverage` keeps the distance from every centre of a 10-unit raster to the
//! nearest observed tick, extended path by path like `noise::TickMask`.
use super::Shot;
use super::noise::nearest_free;
use crate::protocol::Planet;
use crate::simulation::WorldParameters;

/// Raster spacing (world units).
const SPACING: f64 = 10.0;
/// Smallest planet radius (NewtonWars draws 20-40): a planet centre lies at least this far inside
/// the battlefield and farther than `r_i + MIN_RADIUS` from a found planet `i`.
const MIN_RADIUS: f64 = 20.0;
/// Ticks this far outside the battlefield still count (the largest reach that is queried).
const PAD: f64 = 400.0;

/// Distances from candidate planet centres to the nearest observed tick of a map epoch.
#[derive(Clone, Debug)]
pub(super) struct Coverage {
    nx: usize,
    ny: usize,
    /// Raster nodes holding a tick (the raster spans the battlefield plus `PAD`).
    occupied: Vec<bool>,
    /// Paths stamped (the first ones of the slice given to `cover`).
    paths: usize,
    /// Per node, the distance to the nearest occupied node (`None`: stale after a `cover`).
    distance: Option<Vec<f64>>,
    /// Indices of the nodes that are candidate centres (inside the battlefield by `MIN_RADIUS`).
    centres: Vec<usize>,
}

impl Coverage {
    pub fn new(world: WorldParameters) -> Self {
        let count = |extent: f64| ((extent + 2.0 * PAD) / SPACING).floor() as usize + 1;
        let (nx, ny) = (
            count(world.battlefield_width),
            count(world.battlefield_height),
        );
        let mut centres = Vec::new();
        for j in 0..ny {
            for i in 0..nx {
                let [x, y] = node(i, j);
                if (MIN_RADIUS..=world.battlefield_width - MIN_RADIUS).contains(&x)
                    && (MIN_RADIUS..=world.battlefield_height - MIN_RADIUS).contains(&y)
                {
                    centres.push(j * nx + i);
                }
            }
        }
        Self {
            nx,
            ny,
            occupied: vec![false; nx * ny],
            paths: 0,
            distance: None,
            centres,
        }
    }

    /// Make the coverage that of the paths `shots` (stamp the ticks of the paths not stamped yet;
    /// a coverage of more paths than `shots` is rebuilt).
    pub fn cover(&mut self, shots: &[Shot]) {
        if self.paths > shots.len() {
            *self = Self::new(WorldParameters::default());
        }
        for path in shots[self.paths..]
            .iter()
            .filter_map(|shot| shot.path.as_ref())
        {
            for t in &path.ticks {
                let i = ((t[0] + PAD) / SPACING).round();
                let j = ((t[1] + PAD) / SPACING).round();
                if (0.0..self.nx as f64).contains(&i) && (0.0..self.ny as f64).contains(&j) {
                    self.occupied[j as usize * self.nx + i as usize] = true;
                    self.distance = None;
                }
            }
        }
        self.paths = shots.len();
    }

    fn distances(&mut self) -> &[f64] {
        let (nx, ny) = (self.nx, self.ny);
        let occupied = &self.occupied;
        self.distance.get_or_insert_with(|| {
            nearest_free(occupied, nx, ny)
                .iter()
                .enumerate()
                .map(|(k, &n)| {
                    if n == u32::MAX {
                        f64::INFINITY
                    } else {
                        let (a, b) = (node(k % nx, k / nx), node(n as usize % nx, n as usize / nx));
                        (a[0] - b[0]).hypot(a[1] - b[1])
                    }
                })
                .collect()
        })
    }

    /// Distance from `p` to the nearest observed tick (raster resolution).
    #[cfg(test)]
    pub fn tick_distance(&mut self, p: [f64; 2]) -> f64 {
        let (nx, ny) = (self.nx, self.ny);
        let i = ((p[0] + PAD) / SPACING).round().clamp(0.0, (nx - 1) as f64) as usize;
        let j = ((p[1] + PAD) / SPACING).round().clamp(0.0, (ny - 1) as f64) as usize;
        self.distances()[j * nx + i]
    }

    /// Fraction of the candidate centres that are farther than `reach` from every observed tick
    /// and do not overlap a planet of `planets`.
    pub fn hidden_fraction(&mut self, planets: &[Planet], reach: f64) -> f64 {
        let nx = self.nx;
        let centres = std::mem::take(&mut self.centres);
        let distance = self.distances();
        let hidden = centres
            .iter()
            .filter(|&&k| {
                let [x, y] = node(k % nx, k / nx);
                distance[k] > reach
                    && planets
                        .iter()
                        .all(|p| (p.x - x).hypot(p.y - y) > p.radius + MIN_RADIUS)
            })
            .count();
        let fraction = hidden as f64 / centres.len() as f64;
        self.centres = centres;
        fraction
    }
}

/// World position of raster node `(i, j)`.
fn node(i: usize, j: usize) -> [f64; 2] {
    [i as f64 * SPACING - PAD, j as f64 * SPACING - PAD]
}

/// Count the dynamic mode starts from (the server default).
pub const LEARN_START: usize = 24;
/// Too-high signal (P25). With the count fixed at `N` and `K < N` planets found, the `N - K`
/// others must lie in the hidden region (fraction `f` at `TOO_HIGH_REACH`). For `N` planets
/// placed uniformly, the likelihood of exactly `K` found and the rest hidden is
/// `C(N, K) (1 - f)^K f^(N - K)`, so `N - 1` is more likely than `N` when
/// `N f / (N - K) < TOO_HIGH_RATIO` (1). The rule also needs the map to leave no planet-sized
/// pull (the residual correction below `TOO_HIGH_ACCELERATION` on the last `TOO_HIGH_WINDOW`
/// polished maps; a gated correction counts as none) and `K` unchanged for `TOO_HIGH_PLATEAU`
/// polished maps. Measured on replays (`docs/estimator-experiments.md`, P25): the ratio was at
/// most 0.37 when it fired with the count too high, and at least 3.3 on correct counts with the
/// other conditions met.
pub const TOO_HIGH_REACH: f64 = 200.0;
pub const TOO_HIGH_RATIO: f64 = 1.0;
pub const TOO_HIGH_ACCELERATION: f64 = 0.01;
pub const TOO_HIGH_WINDOW: usize = 5;
pub const TOO_HIGH_PLATEAU: usize = 8;

/// The planet count of the dynamic mode (`Settings::learn_planets`): fixed at `planets` within a
/// map, raised by one when the count-limit warning (P23) holds and lowered by one on the too-high
/// signal; carried across the maps of a session.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LearnedCount {
    /// The count in use.
    pub planets: usize,
    pub start: usize,
    /// The count is never lowered below this: the highest count a raise reached (a raise is
    /// evidence of more planets than the count before it), so it cannot oscillate.
    pub floor: usize,
    /// Every change, oldest first.
    pub changes: Vec<CountChange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CountChange {
    pub from: usize,
    pub to: usize,
    /// Map epoch of the session (0: the first the estimator saw) and paths of that map.
    pub map: usize,
    pub at_path: usize,
}

/// What the learner sees of one polished map.
#[derive(Clone, Copy, Debug)]
pub(super) struct Observation {
    /// Paths of the map epoch.
    pub paths: usize,
    /// Planets of the map.
    pub found: usize,
    /// The count-limit warning holds.
    pub limited: bool,
    /// Largest residual-correction magnitude (`None`: gated, no correction).
    pub magnitude: Option<f64>,
    /// Hidden fraction at `TOO_HIGH_REACH` (`Coverage::hidden_fraction`).
    pub hidden: f64,
}

/// `LearnedCount` with the state of the current map's too-high signal.
#[derive(Clone, Debug)]
pub(super) struct Learner {
    pub learned: LearnedCount,
    map: usize,
    pub coverage: Coverage,
    /// Magnitudes of the latest polished maps (at most `TOO_HIGH_WINDOW`).
    recent: Vec<Option<f64>>,
    /// `found` of the latest polished map and how many polished maps it has held since.
    last_found: Option<usize>,
    unchanged: usize,
}

impl Learner {
    pub fn new(start: usize) -> Self {
        Self {
            learned: LearnedCount {
                planets: start,
                start,
                floor: 1,
                changes: Vec::new(),
            },
            map: 0,
            coverage: Coverage::new(WorldParameters::default()),
            recent: Vec::new(),
            last_found: None,
            unchanged: 0,
        }
    }

    /// The learner of the next map: the learned count, a fresh signal.
    pub fn next_map(&self) -> Self {
        let mut next = Self::new(self.learned.start);
        next.learned = self.learned.clone();
        next.map = self.map + 1;
        next
    }

    /// One polished map; the new count when it changes. After a change the signals start over.
    pub fn observe(&mut self, o: Observation) -> Option<usize> {
        self.unchanged = if self.last_found == Some(o.found) {
            self.unchanged + 1
        } else {
            0
        };
        self.last_found = Some(o.found);
        self.recent.push(o.magnitude);
        if self.recent.len() > TOO_HIGH_WINDOW {
            self.recent.remove(0);
        }
        let n = self.learned.planets;
        let to = if o.limited {
            n + 1
        } else if n > self.learned.floor.max(1)
            && self.unchanged >= TOO_HIGH_PLATEAU
            && too_high(n, o.found, o.hidden, &self.recent)
        {
            n - 1
        } else {
            return None;
        };
        if to > n {
            self.learned.floor = self.learned.floor.max(to);
        }
        self.learned.changes.push(CountChange {
            from: n,
            to,
            map: self.map,
            at_path: o.paths,
        });
        self.learned.planets = to;
        self.recent.clear();
        self.last_found = None;
        self.unchanged = 0;
        Some(to)
    }
}

/// The too-high rule on one polished map with `found` of `planets` planets, `hidden` the hidden
/// fraction at `TOO_HIGH_REACH` and `recent` the latest correction magnitudes.
pub(super) fn too_high(planets: usize, found: usize, hidden: f64, recent: &[Option<f64>]) -> bool {
    found < planets
        && recent.len() >= TOO_HIGH_WINDOW
        && recent
            .iter()
            .all(|m| m.is_none_or(|m| m < TOO_HIGH_ACCELERATION))
        && (planets as f64 * hidden / (planets - found) as f64) < TOO_HIGH_RATIO
}

impl super::Estimator {
    /// Start the dynamic count when `learn_planets` is on and no count is given (`--planets`
    /// wins: it stays exactly as given).
    pub(super) fn start_learning(&mut self) {
        if self.settings.learn_planets && self.settings.planets.is_none() {
            let learner = Learner::new(LEARN_START);
            self.settings.planets = Some(learner.learned.planets);
            self.learner = Some(learner);
        }
    }

    /// Continue the dynamic count of `previous`, the estimator of the map before this one.
    pub fn carry_count(&mut self, previous: &super::Estimator) {
        if let (Some(_), Some(learner)) = (&self.learner, &previous.learner) {
            let next = learner.next_map();
            self.settings.planets = Some(next.learned.planets);
            self.learner = Some(next);
        }
    }

    /// Feed a polished map of `planets` with correction `magnitude` to the dynamic count; on a
    /// change the count-limit warning starts over and the search resumes under the new count.
    pub(super) fn learn(&mut self, planets: &[Planet], magnitude: Option<f64>) {
        let Some(learner) = self.learner.as_mut() else {
            return;
        };
        learner.coverage.cover(&self.shots);
        let observation = Observation {
            paths: self.searched,
            found: planets.len(),
            limited: self.count_limited_at.is_some(),
            magnitude,
            hidden: learner.coverage.hidden_fraction(planets, TOO_HIGH_REACH),
        };
        let Some(count) = learner.observe(observation) else {
            return;
        };
        self.settings.planets = Some(count);
        self.cap_magnitudes.clear();
        self.count_limited_at = None;
        self.search.adopt(self.search.theta.clone());
        self.slices_left = self.settings.slices_per_path;
        self.polish_due = true;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{planet, shot_in, world_of};
    use super::super::{Estimator, Settings};
    use super::*;
    use crate::simulation::ShotObservation;

    fn observation(found: usize, limited: bool, magnitude: f64, hidden: f64) -> Observation {
        Observation {
            paths: 10,
            found,
            limited,
            magnitude: Some(magnitude),
            hidden,
        }
    }

    /// `count` polished maps that meet the too-high rule for 24 planets with 22 found.
    fn quiet(learner: &mut Learner, count: usize) -> Vec<usize> {
        (0..count)
            .filter_map(|_| learner.observe(observation(22, false, 1e-3, 0.01)))
            .collect()
    }

    #[test]
    fn too_high_needs_a_small_ratio_no_pull_and_a_full_window() {
        let calm = [Some(1e-3); TOO_HIGH_WINDOW];
        // 24 * 0.04 / 2 = 0.48 < 1: 23 is more likely than 24.
        assert!(too_high(24, 22, 0.04, &calm));
        // 24 * 0.1 / 2 = 1.2: not.
        assert!(!too_high(24, 22, 0.1, &calm));
        assert!(!too_high(24, 24, 0.0, &calm));
        assert!(!too_high(24, 22, 0.0, &calm[1..]));
        let mut pull = calm;
        pull[2] = Some(0.02);
        assert!(!too_high(24, 22, 0.0, &pull));
        // A gated correction (no pull worth building) counts as calm.
        let mut gated = calm;
        gated[0] = None;
        assert!(too_high(24, 22, 0.0, &gated));
    }

    #[test]
    fn the_count_drops_after_a_quiet_plateau_and_rises_on_the_count_limit() {
        let mut learner = Learner::new(24);
        // The first map starts the plateau; the window and the plateau must fill.
        assert_eq!(quiet(&mut learner, TOO_HIGH_PLATEAU), Vec::<usize>::new());
        assert_eq!(quiet(&mut learner, 1), vec![23]);
        assert_eq!(learner.learned.changes[0].from, 24);
        // A change starts the signals over: 23 with 22 found needs the plateau again.
        assert_eq!(quiet(&mut learner, TOO_HIGH_PLATEAU), Vec::<usize>::new());
        assert_eq!(learner.observe(observation(23, true, 0.5, 0.0)), Some(24));
        assert_eq!(learner.learned.floor, 24);
    }

    #[test]
    fn after_a_raise_the_count_never_drops_below_it() {
        let mut learner = Learner::new(24);
        assert_eq!(learner.observe(observation(24, true, 0.5, 0.5)), Some(25));
        assert_eq!(quiet(&mut learner, 100), Vec::<usize>::new());
        assert_eq!(learner.learned.planets, 25);
        // A raise still works above the floor.
        assert_eq!(learner.observe(observation(25, true, 0.5, 0.0)), Some(26));
        let changes: Vec<_> = learner.learned.changes.iter().map(|c| c.to).collect();
        assert_eq!(changes, vec![25, 26]);
    }

    #[test]
    fn the_learned_count_carries_to_the_next_map_with_a_fresh_signal() {
        let mut learner = Learner::new(24);
        quiet(&mut learner, TOO_HIGH_PLATEAU);
        let next = learner.next_map();
        assert_eq!(next.learned.planets, 24);
        assert_eq!(next.map, 1);
        assert!(next.recent.is_empty() && next.last_found.is_none());
        assert_eq!(
            learner.observe(observation(22, false, 1e-3, 0.01)),
            Some(23)
        );
        let next = learner.next_map();
        assert_eq!(next.learned.planets, 23);
        assert_eq!(next.learned.changes.len(), 1);
    }

    #[test]
    fn a_given_planet_count_wins_over_the_dynamic_mode() {
        let fixed = Estimator::new(Settings {
            planets: Some(20),
            learn_planets: true,
            ..Settings::default()
        });
        assert_eq!(fixed.settings.planets, Some(20));
        assert_eq!(fixed.summary().learned, None);
        let learning = Estimator::new(Settings {
            learn_planets: true,
            ..Settings::default()
        });
        assert_eq!(learning.settings.planets, Some(LEARN_START));
        assert_eq!(learning.summary().learned.unwrap().planets, LEARN_START);
        assert_eq!(Estimator::default().summary().learned, None);
    }

    #[test]
    fn a_change_resets_the_warning_and_resumes_the_search_and_carries_across_maps() {
        let settings = Settings {
            learn_planets: true,
            ..Settings::default()
        };
        let mut estimator = Estimator::new(settings.clone());
        estimator.count_limited_at = Some(30);
        estimator.cap_magnitudes = vec![Some(0.5); 3];
        estimator.learn(&[], Some(0.5));
        assert_eq!(estimator.settings.planets, Some(LEARN_START + 1));
        assert_eq!(estimator.count_limited_at, None);
        assert!(estimator.cap_magnitudes.is_empty());
        assert!(estimator.search_pending());
        // A `Planets` reset and a new estimator for the next map both keep the learned count.
        estimator.reset();
        assert_eq!(estimator.settings.planets, Some(LEARN_START + 1));
        let mut next = Estimator::new(settings);
        next.carry_count(&estimator);
        assert_eq!(next.settings.planets, Some(LEARN_START + 1));
        assert_eq!(next.summary().learned.unwrap().changes.len(), 1);
        // Without the dynamic mode nothing is carried.
        let mut plain = Estimator::default();
        plain.carry_count(&estimator);
        assert_eq!(plain.settings.planets, None);
    }

    #[test]
    fn coverage_hides_what_no_path_reaches_and_what_no_planet_occupies() {
        let world = WorldParameters::default();
        let mut coverage = Coverage::new(world);
        assert_eq!(coverage.hidden_fraction(&[], 200.0), 1.0);
        let found = [planet(1000.0, 200.0, 40.0)];
        let blocked = 1.0 - coverage.hidden_fraction(&found, 200.0);
        // A disk of radius 60 out of the battlefield's candidate centres.
        let disk = std::f64::consts::PI * 60.0 * 60.0
            / ((world.battlefield_width - 40.0) * (world.battlefield_height - 40.0));
        assert!((blocked - disk).abs() < 0.2 * disk, "{blocked} vs {disk}");
        // A horizontal path across the middle hides nothing within 200 of it.
        let event = shot_in(&world_of(vec![]), (0.0, 530.0), 0.0, 8.0);
        let obs = ShotObservation::try_from(&event).unwrap();
        let shots = vec![super::super::Shot::new(obs, None, world)];
        coverage.cover(&shots);
        assert!(coverage.tick_distance([900.0, 600.0]) <= 80.0);
        let reached = coverage.hidden_fraction(&[], 200.0);
        let band = 400.0 / (world.battlefield_height - 40.0);
        assert!(
            reached < 1.0 - 0.8 * band && reached > 1.0 - 1.2 * band,
            "{reached} vs {}",
            1.0 - band
        );
    }
}
