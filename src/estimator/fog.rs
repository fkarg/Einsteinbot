//! Fog of war (P26): where planets the map does not hold yet could still be, and how hard they
//! could pull.
//!
//! The unseen planets are `N - K` planets placed like the generator's (uniform centres) but only
//! where the paths so far would still have missed them. Per possible centre `u` (the 10-unit
//! raster of `count::Coverage`: inside the battlefield by 20, not overlapping a found planet,
//! farther than 20 from every observed tick), the chance that a planet centred there would still
//! be missing from the map is `P_miss(d - 30)`, a logistic in the gap between the nearest observed
//! tick `d` and a planet's surface (mean radius 30), centre 38 and scale 8 units. It was fitted on
//! recorded maps (the fraction of true planets missing from the bot's map by that gap, capture 58,
//! similar on 60/67/69; truth only scored it) and so includes the estimator's own search lag, not
//! only where paths went. One unseen planet's centre is distributed as `pi(u) = P_miss(u) / sum`.
//!
//! The number of unseen planets comes from the count posterior (`count::CountPosterior`) with the
//! map's miss fraction `f = sum P_miss / centres`: with a known count it is `N - K` exactly, with
//! a prior it shrinks as `f` falls, so when the count is certain and every planet is found the fog
//! is zero everywhere, flown or not. The unseen planets are independent draws from `pi`, so
//!
//! - `risk_at(p)` = P(some unseen planet's centre within `RISK_RADIUS` of `p`)
//!   `= 1 - E[(1 - pi(disk))^(N - K)]` (the posterior's generating function), and
//! - `pull_at(p)` = the root mean square of their acceleration at `p` (Campbell's second moment):
//!   `sqrt(E[N - K] sum_u pi(u) E[m^2; r < |u - p|] / |u - p|^4)` with the radius uniform on
//!   20-40 and mass `r^3 / 10` (the generator's), over centres within `PULL_RADIUS` (the far rest
//!   adds a few percent).
//!
//! The location entropy is `E[N - K] H(pi)` bits (relative to the 10-unit raster); a path's
//! information is its drop, split into the part from a planet found (`E[N - K]` falling) and the
//! part from space ruled out (`pi` narrowing). The parameter covariance of the found planets is a
//! separate layer and not part of the fog.
use super::count::{COUNT_MAX, CountPosterior, CountPrior, Coverage};
use crate::map::Vec2;
use crate::protocol::Planet;
use crate::simulation::{ShotObservation, WorldParameters};

/// Radius of the risk disk (world units).
pub const RISK_RADIUS: f64 = 100.0;
/// Unseen centres farther than this from a point are left out of its pull.
pub const PULL_RADIUS: f64 = 400.0;
/// A possible centre at least this far inside the battlefield, from a tick and from a found
/// planet's surface (the smallest planet radius).
const CLEARANCE: f64 = 20.0;
/// Detectability: `P_miss(gap) = 1 / (1 + exp(-(gap - DETECT_GAP) / DETECT_SCALE))` with
/// `gap = d - DETECT_RADIUS`.
pub const DETECT_RADIUS: f64 = 30.0;
pub const DETECT_GAP: f64 = 38.0;
pub const DETECT_SCALE: f64 = 8.0;
/// The count posterior uses `f^MISS_EXPONENT` as the miss probability of a planet. With `f`
/// itself (uniformly placed planets) it kept 2-5 unseen planets on completed maps at 70-130 paths:
/// paths are not independent of where planets are (they bend toward them and end on them), so the
/// space no path has visited holds fewer planets than its share. 1.5 was the best of 1, 1.5 and 2
/// (and of P25's reach fractions at 100, 150 and 200 units) on the design maps 58/60/67/69: mean
/// bias of the unseen count -0.06, 90% interval coverage 0.98 (`fog_calibration`).
pub const MISS_EXPONENT: f64 = 1.5;
/// Raster spacing of the possible centres (`count::Coverage`).
const SPACING: f64 = 10.0;

/// Probability that a planet centred `distance` from the nearest observed tick is still missing
/// from the map.
pub fn miss_probability(distance: f64) -> f64 {
    if distance <= CLEARANCE {
        return 0.0;
    }
    let gap = distance - DETECT_RADIUS;
    1.0 / (1.0 + (-(gap - DETECT_GAP) / DETECT_SCALE).exp())
}

/// What the fog of a belief was built with, recorded with the belief (`fog_model`) so that a
/// recorded decision rebuilds the same fog from its planets and the paths it had fitted.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FogModel {
    /// Prior on the total count (`sd` 0: known).
    pub prior: CountPrior,
    /// The count lies in `lower..=upper` (the count-limit warning raises `lower`, the too-high
    /// signal lowers `upper`; dynamic count only).
    pub lower: usize,
    pub upper: usize,
}

impl FogModel {
    /// Without `--planets`: the 24 +- 4 prior.
    pub const DEFAULT: Self = Self {
        prior: CountPrior::DEFAULT,
        lower: 1,
        upper: COUNT_MAX,
    };

    pub fn known(count: usize) -> Self {
        Self {
            prior: CountPrior::known(count),
            lower: 1,
            upper: COUNT_MAX,
        }
    }
}

/// The fog of one map: the count posterior and the location distribution of an unseen planet.
#[derive(Clone, Debug)]
pub struct Fog {
    pub model: FogModel,
    pub posterior: CountPosterior,
    /// `f`: average miss probability over the possible centres.
    pub miss_fraction: f64,
    /// `H(pi)` in bits.
    pub location_bits: f64,
    /// `pi` on the rectangle of possible centres, row-major from `origin`.
    origin: [f64; 2],
    columns: usize,
    rows: usize,
    probability: Vec<f64>,
}

/// Cheap description of a fog for status and capture diagnostics (`map_belief.fog`).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FogSummary {
    pub expected_unseen: f64,
    pub entropy_bits: f64,
    pub miss_fraction: f64,
    /// Mean, sd and mode of the total count's posterior.
    pub count_mean: f64,
    pub count_sd: f64,
    pub count_mode: usize,
}

impl Fog {
    /// The fog of the map `planets` over `coverage` (already covering the map's paths).
    pub(super) fn build(planets: &[Planet], coverage: &mut Coverage, model: FogModel) -> Self {
        let (centres, columns) = coverage.centre_distances();
        let origin = centres.first().map_or([0.0, 0.0], |c| c.0);
        // Centres overlapping a found planet, stamped disk by disk on the centre rectangle.
        let mut free = vec![true; centres.len()];
        let rows = if columns == 0 {
            0
        } else {
            centres.len() / columns
        };
        for p in planets {
            let reach = p.radius + CLEARANCE;
            let span = |c: f64, o: f64, n: usize| {
                let lo = ((c - reach - o) / SPACING).ceil().max(0.0) as usize;
                let hi = ((c + reach - o) / SPACING).floor().min(n as f64 - 1.0);
                (lo, if hi < 0.0 { None } else { Some(hi as usize) })
            };
            let ((i0, i1), (j0, j1)) = (span(p.x, origin[0], columns), span(p.y, origin[1], rows));
            let (Some(i1), Some(j1)) = (i1, j1) else {
                continue;
            };
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let k = j * columns + i;
                    let u = centres[k].0;
                    if (p.x - u[0]).hypot(p.y - u[1]) <= reach {
                        free[k] = false;
                    }
                }
            }
        }
        let mut weight: Vec<f64> = centres
            .iter()
            .zip(&free)
            .map(|(&(_, d), &free)| if free { miss_probability(d) } else { 0.0 })
            .collect();
        let total: f64 = weight.iter().sum();
        let miss_fraction = if centres.is_empty() {
            0.0
        } else {
            total / centres.len() as f64
        };
        let posterior = CountPosterior::new(
            model.prior,
            planets.len(),
            miss_fraction.powf(MISS_EXPONENT),
            model.lower,
            model.upper,
        );
        if total <= 0.0 {
            // Nowhere a planet could still hide: if the count still needs one (a count-limit
            // floor), it could be at any free centre.
            weight = free.iter().map(|&f| if f { 1.0 } else { 0.0 }).collect();
        }
        let sum: f64 = weight.iter().sum();
        let probability: Vec<f64> = weight
            .iter()
            .map(|w| if sum > 0.0 { w / sum } else { 0.0 })
            .collect();
        let location_bits = probability
            .iter()
            .filter(|p| **p > 0.0)
            .map(|p| -p * p.log2())
            .sum();
        Self {
            model,
            posterior,
            miss_fraction,
            location_bits,
            origin,
            columns,
            rows: if columns == 0 {
                0
            } else {
                probability.len() / columns
            },
            probability,
        }
    }

    pub fn expected_unseen(&self) -> f64 {
        self.posterior.expected_unseen()
    }

    /// Remaining location entropy of the unseen planets (bits).
    pub fn entropy_bits(&self) -> f64 {
        self.expected_unseen() * self.location_bits
    }

    pub fn summary(&self) -> FogSummary {
        FogSummary {
            expected_unseen: self.expected_unseen(),
            entropy_bits: self.entropy_bits(),
            miss_fraction: self.miss_fraction,
            count_mean: self.posterior.mean(),
            count_sd: self.posterior.sd(),
            count_mode: self.posterior.mode(),
        }
    }

    /// Visit the raster nodes within `radius` of `p` with their probability and squared distance.
    fn around(&self, p: Vec2, radius: f64, mut visit: impl FnMut(f64, f64)) {
        if self.columns == 0 {
            return;
        }
        let range = |c: f64, origin: f64, count: usize| {
            let lo = ((c - radius - origin) / SPACING).ceil().max(0.0) as usize;
            let hi = ((c + radius - origin) / SPACING).floor();
            if hi < 0.0 {
                return (1, 0);
            }
            (lo, (hi as usize).min(count - 1))
        };
        let (i0, i1) = range(p.x, self.origin[0], self.columns);
        let (j0, j1) = range(p.y, self.origin[1], self.rows);
        let r2 = radius * radius;
        for j in j0..=j1.min(self.rows.saturating_sub(1)) {
            let dy = self.origin[1] + j as f64 * SPACING - p.y;
            for i in i0..=i1 {
                let q = self.probability[j * self.columns + i];
                if q == 0.0 {
                    continue;
                }
                let dx = self.origin[0] + i as f64 * SPACING - p.x;
                let d2 = dx * dx + dy * dy;
                if d2 <= r2 {
                    visit(q, d2);
                }
            }
        }
    }

    /// Probability that an unseen planet's centre lies within `RISK_RADIUS` of `p`.
    pub fn risk_at(&self, p: Vec2) -> f64 {
        let mut disk = 0.0;
        self.around(p, RISK_RADIUS, |q, _| disk += q);
        (1.0 - self.posterior.generating((1.0 - disk).max(0.0))).clamp(0.0, 1.0)
    }

    /// Root mean square acceleration of the unseen planets at `p` (acceleration units).
    pub fn pull_at(&self, p: Vec2) -> f64 {
        let mut second = 0.0;
        self.around(p, PULL_RADIUS, |q, d2| {
            let m2 = mass_square_below(d2.sqrt());
            if m2 > 0.0 {
                second += q * m2 / (d2 * d2);
            }
        });
        (self.expected_unseen() * second).sqrt()
    }
}

/// `E[m^2; r < d]` for a planet radius `r` uniform on 20-40 and mass `r^3 / 10`: a planet whose
/// surface would cover the point contributes nothing.
fn mass_square_below(d: f64) -> f64 {
    let hi = d.clamp(20.0, 40.0);
    (hi.powi(7) - 20f64.powi(7)) / (7.0 * 100.0 * 20.0)
}

impl super::Estimator {
    /// The fog of the map `planets` after the paths given to the search (`fog_coverage` is
    /// extended path by path).
    pub(super) fn fog(&mut self, planets: &[Planet]) -> Fog {
        if self.fog_coverage.covered() > self.searched {
            self.fog_coverage = Coverage::new(WorldParameters::default());
        }
        for shot in &self.shots[self.fog_coverage.covered()..self.searched] {
            self.fog_coverage.add_points(&shot.obs.points);
        }
        let model = self.fog_model();
        Fog::build(planets, &mut self.fog_coverage, model)
    }
}

/// The fog of the map `planets` after the paths `observations` of its epoch (the ones the map
/// had fitted), for recorded beliefs (`fog_model`) and replays.
pub fn fog(planets: &[Planet], observations: &[ShotObservation], model: FogModel) -> Fog {
    let mut coverage = Coverage::new(WorldParameters::default());
    coverage.cover_observations(observations);
    Fog::build(planets, &mut coverage, model)
}

/// What one path (and the map published after it) taught: the drop in the unseen planets'
/// location entropy, split into a planet found and space ruled out. Negative when the map lost a
/// planet.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct PathInformation {
    pub bits: f64,
    /// From the expected unseen count falling while the map gained a planet.
    pub found_bits: f64,
    /// The rest: the location distribution narrowing (and the count falling as space is ruled
    /// out without a new planet).
    pub ruled_out_bits: f64,
    /// After the path.
    pub entropy_bits: f64,
    pub expected_unseen: f64,
    pub planets: usize,
}

pub fn information(before: &Fog, after: &Fog) -> PathInformation {
    let (e0, e1) = (before.expected_unseen(), after.expected_unseen());
    let count_bits = (e0 - e1) * before.location_bits;
    let location_bits = e1 * (before.location_bits - after.location_bits);
    let found = after.posterior.found > before.posterior.found;
    let found_bits = if found { count_bits } else { 0.0 };
    PathInformation {
        bits: count_bits + location_bits,
        found_bits,
        ruled_out_bits: count_bits + location_bits - found_bits,
        entropy_bits: after.entropy_bits(),
        expected_unseen: e1,
        planets: after.posterior.found,
    }
}

/// The fog after each path of an epoch: `beliefs` are `(paths fitted, planets, model)` in
/// increasing paths; path `k` (1-based) uses the latest belief that had fitted at most `k` paths
/// (none: an empty map with the default model). Returns the fog before any path and one
/// `PathInformation` per path.
pub fn timeline(
    observations: &[ShotObservation],
    beliefs: &[(usize, Vec<Planet>, FogModel)],
) -> (FogSummary, Vec<PathInformation>) {
    let mut coverage = Coverage::new(WorldParameters::default());
    let belief_at = |k: usize| {
        beliefs.iter().take_while(|b| b.0 <= k).last().map_or(
            (&[][..], beliefs.first().map_or(FogModel::DEFAULT, |b| b.2)),
            |b| (b.1.as_slice(), b.2),
        )
    };
    let (planets, model) = belief_at(0);
    let mut before = Fog::build(planets, &mut coverage, model);
    let start = before.summary();
    let mut out = Vec::with_capacity(observations.len());
    for k in 1..=observations.len() {
        coverage.cover_observations(&observations[..k]);
        let (planets, model) = belief_at(k);
        let after = Fog::build(planets, &mut coverage, model);
        out.push(information(&before, &after));
        before = after;
    }
    (start, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Point;

    fn planet(x: f64, y: f64, radius: f64) -> Planet {
        Planet {
            x,
            y,
            radius,
            mass: radius.powi(3) / 10.0,
        }
    }

    /// A straight path at height `y` across the battlefield.
    fn line(y: f32) -> ShotObservation {
        ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 5.0,
            points: (0..400)
                .map(|i| Point {
                    x: i as f32 * 5.0,
                    y,
                })
                .collect(),
        }
    }

    fn mean_risk(fog: &Fog) -> f64 {
        let mut sum = 0.0;
        let mut n = 0.0;
        for x in (50..1850).step_by(100) {
            for y in (50..1050).step_by(100) {
                sum += fog.risk_at(Vec2::new(x as f64, y as f64));
                n += 1.0;
            }
        }
        sum / n
    }

    #[test]
    fn a_known_count_with_every_planet_found_has_no_fog_even_where_no_path_went() {
        let planets: Vec<Planet> = (0..3)
            .map(|i| planet(300.0 + 500.0 * i as f64, 500.0, 30.0))
            .collect();
        let fog = fog(&planets, &[line(100.0)], FogModel::known(3));
        assert_eq!(fog.expected_unseen(), 0.0);
        assert_eq!(fog.entropy_bits(), 0.0);
        let far = Vec2::new(1500.0, 900.0);
        assert_eq!(fog.risk_at(far), 0.0);
        assert_eq!(fog.pull_at(far), 0.0);
        // The same map with the 24 +- 4 prior keeps fog where a planet could hide.
        let open = super::fog(&planets, &[line(100.0)], FogModel::DEFAULT);
        assert!(open.expected_unseen() > 5.0);
        assert!(open.risk_at(far) > 0.2, "{}", open.risk_at(far));
        assert!(open.pull_at(far) > 0.0);
        // Not on the path itself: nothing can be centred within 20 of a tick.
        assert!(open.risk_at(Vec2::new(900.0, 100.0)) < open.risk_at(far));
    }

    #[test]
    fn fog_shrinks_with_planets_found_and_with_coverage() {
        let paths: Vec<ShotObservation> = (0..22).map(|i| line(20.0 + 50.0 * i as f32)).collect();
        let empty = fog(&[], &[], FogModel::DEFAULT);
        let some = fog(&[], &paths[..3], FogModel::DEFAULT);
        let all = fog(&[], &paths, FogModel::DEFAULT);
        assert!(mean_risk(&some) < mean_risk(&empty));
        assert!(mean_risk(&all) < mean_risk(&some));
        assert!(all.entropy_bits() < some.entropy_bits());
        assert!(some.entropy_bits() < empty.entropy_bits());
        // The fully covered battlefield leaves (almost) nothing missable: the posterior puts the
        // count at what was found.
        assert!(all.miss_fraction < 0.01, "{}", all.miss_fraction);
        let found: Vec<Planet> = (0..5)
            .map(|i| planet(200.0 + 300.0 * i as f64, 520.0, 25.0))
            .collect();
        let fewer = fog(&found, &paths[..3], FogModel::known(8));
        let more = fog(&found[..2], &paths[..3], FogModel::known(8));
        assert!(fewer.expected_unseen() < more.expected_unseen());
        assert!(mean_risk(&fewer) < mean_risk(&more));
    }

    #[test]
    fn information_splits_into_planet_found_and_space_ruled_out() {
        let paths = [line(300.0), line(700.0)];
        let found = [planet(900.0, 500.0, 30.0)];
        let beliefs = vec![
            (1, vec![], FogModel::DEFAULT),
            (2, found.to_vec(), FogModel::DEFAULT),
        ];
        let (start, steps) = timeline(&paths, &beliefs);
        assert_eq!(steps.len(), 2);
        // Path 1 rules out a corridor only; path 2 also finds a planet.
        assert_eq!(steps[0].found_bits, 0.0);
        assert!(steps[0].ruled_out_bits > 0.0);
        assert!(steps[1].found_bits > 5.0, "{:?}", steps[1]);
        let total: f64 = steps.iter().map(|s| s.bits).sum();
        assert!((start.entropy_bits - total - steps[1].entropy_bits).abs() < 1e-6);
        for s in &steps {
            assert!((s.bits - s.found_bits - s.ruled_out_bits).abs() < 1e-9);
        }
    }

    #[test]
    fn the_fog_model_round_trips_and_rebuilds_the_same_fog() {
        let model = FogModel {
            prior: CountPrior {
                centre: 25.0,
                sd: 1.5,
            },
            lower: 3,
            upper: 30,
        };
        let json = serde_json::to_value(model).unwrap();
        let back: FogModel = serde_json::from_value(json).unwrap();
        assert_eq!(back, model);
        let paths = [line(400.0)];
        let found = [planet(600.0, 600.0, 30.0)];
        let a = fog(&found, &paths, model);
        let b = fog(&found, &paths, back);
        let p = Vec2::new(1500.0, 900.0);
        assert_eq!(a.risk_at(p), b.risk_at(p));
        assert_eq!(a.pull_at(p), b.pull_at(p));
        assert_eq!(a.summary(), b.summary());
    }

    #[test]
    fn the_estimator_records_the_fog_model_and_paths_and_they_rebuild_its_fog() {
        use super::super::tests::{shot_in, world_of};
        use std::sync::atomic::AtomicBool;
        let truth = world_of(vec![planet(900.0, 500.0, 30.0)]);
        let mut estimator = super::super::Estimator::new(super::super::Settings {
            planets: Some(5),
            ..super::super::Settings::default()
        });
        let events = [
            shot_in(&truth, (100.0, 300.0), 0.0, 6.0),
            shot_in(&truth, (100.0, 800.0), 0.0, 6.0),
        ];
        let belief = estimator
            .observe_batch(&events, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(belief.paths_fitted, Some(2));
        assert_eq!(belief.fog_model, Some(FogModel::known(5)));
        let p = Vec2::new(1500.0, 900.0);
        let live = belief.unseen_risk_at(p).unwrap();
        // The recorded belief rebuilds the same fog from its planets and the fitted paths.
        let json = belief.diagnostic_json();
        let model: FogModel = serde_json::from_value(json["fog_model"].clone()).unwrap();
        let summary: FogSummary = serde_json::from_value(json["fog"].clone()).unwrap();
        let planets: Vec<Planet> = serde_json::from_value(json["planets"].clone()).unwrap();
        let observations = estimator.observations();
        let rebuilt = fog(&planets, &observations[..2], model);
        assert_eq!(rebuilt.risk_at(p), live);
        assert_eq!(rebuilt.summary(), summary);
        // A belief from elsewhere has no fog: unavailable, not zero.
        let plain = crate::strategy::MapBelief::new(world_of(vec![]), None);
        assert_eq!(plain.unseen_risk_at(p), None);
        assert!(plain.diagnostic_json()["fog_model"].is_null());
    }

    #[test]
    fn miss_probability_follows_the_fitted_detectability() {
        assert_eq!(miss_probability(15.0), 0.0);
        assert!(miss_probability(30.0 + 5.0) < 0.02);
        assert!((miss_probability(30.0 + 38.0) - 0.5).abs() < 1e-12);
        assert!(miss_probability(150.0) > 0.99);
    }
}

#[cfg(test)]
mod calibration;
