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
