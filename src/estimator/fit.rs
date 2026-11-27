//! Joint fit of K planets `(x, y, radius)` to the windowed field comparison of every path
//! (`window`), and its approximate evidence.
//!
//! Mass is tied to the radius (`MASS_PER_RADIUS_CUBED`, as the local NewtonWars generator does).
//! The residual vector is: the whitened window residuals of every path, one interior-sample
//! penalty per (path, planet) (an observed regular tick inside a disk is impossible), one gated
//! surface-contact term per contact, and one overlap penalty per pair of planets (free-free and
//! free-fixed; the generator never lets two planets overlap or even touch). Jacobians are analytic. The fit is a continuous
//! Levenberg-Marquardt refinement in logistic coordinates; no grid is searched.
//!
//! Evidence is `-chi2/2 - Occam(Laplace) - K ln 20`, under the noise the shots carry
//! (`noise.rs`; the same for every map compared, so the log-determinant is left out). The
//! weights built from it are heuristic rankings, not calibrated probabilities.
use super::Shot;
use super::lm::{self, Folder, Reduced, Triangle};
use super::window::PathData;
use super::work::{Part, Stats, par_map};
use crate::protocol::{Planet, Point};
use crate::simulation::WorldParameters;
use nalgebra::{DMatrix, DVector, SymmetricEigen};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Scale of the interior-sample penalty residual, in world units.
const PENALTY_SIGMA: f64 = 0.5;
/// Hard rule for maps and birth seeds: two planets whose disks overlap by at least this much
/// (`distance <= r_i + r_j - OVERLAP_TOLERANCE`) are rejected. The local generator redraws a
/// planet until `distance > r_i + r_j` for every earlier one (`initPlanets` in NewtonWars
/// `simulation.c`), so any overlap is estimation error; positions and radii of found planets are
/// typically within a unit or two (one-path position sd about 5), so 5 units leaves room for that
/// error while excluding concentric or deeply nested splits. The fit's overlap penalty starts at
/// touching (the generator's rule itself), so fits are pushed apart well before this bound.
const OVERLAP_TOLERANCE: f64 = 5.0;
/// Slack of a contact's signed surface distance beyond its penetration allowance (f32 positions,
/// substep geometry), as a Gaussian sd in world units.
const CONTACT_SIGMA: f64 = 0.25;
/// Density of a contact that comes from a source the map does not hold (a planet not found yet,
/// anywhere in the arena) relative to one on a found planet's surface. Order of magnitude only:
/// the surface band of a planet (circumference about 190 times a band of about 0.5) against the
/// arena (about 1.6e6 square units), times the odds that the hit planet is not in the map
/// (assumed about 1). A contact that no surface explains costs `2 ln(1 + 1 / CONTACT_BACKGROUND)`
/// (about 23) whatever the map, so it neither drags nor repels planets.
const CONTACT_BACKGROUND: f64 = 1e-5;
/// Contacts farther than this from the surface of every planet are reported as unmatched (a
/// coarse "no found planet near it" count for the belief, not part of the likelihood).
pub(super) const CONTACT_MATCH: f64 = 15.0;
/// Radius range of the local generator (an assumption about the server, not protocol fact).
const RADIUS_RANGE: (f64, f64) = (20.0, 40.0);
/// Mass is tied to the radius cubed, as the local generator does: `mass = 0.1 * radius^3`.
const MASS_PER_RADIUS_CUBED: f64 = 0.1;
/// Planets closer than this to a bound (in the units of the bound) are saturated: no covariance.
const BOUND_MARGIN: f64 = 0.5;
/// The correlation matrix of the sensitivities must have its smallest eigenvalue above this, or
/// the parameters are treated as under-identified.
const MIN_CORRELATION_EIGENVALUE: f64 = 1e-4;
const LM_PATIENCE: usize = 40;
/// An evaluation with fewer planet-times-node terms than this is not split over threads.
const PARALLEL_MIN_PLANET_NODES: usize = 40_000;
/// More threads than this per evaluation do not help (measured: 8 and 16 threads are alike).
const MAX_EVALUATION_THREADS: usize = 8;
/// A Jacobian is reduced in about this many groups of consecutive paths (the parallel work units).
const REDUCTION_GROUPS: usize = 16;
/// Prior odds against each additional planet. Not a count prior: a Poisson(24) count prior with
/// the `K!` label factor adds `+ln 24` per planet, which makes a planet the data do not constrain
/// (Occam term near 0) free, and the flat-world test grows five such planets in a corner
/// (P13, `docs/estimator-experiments.md`); a planet must be born from trajectory evidence.
const PLANET_PRIOR_ODDS: f64 = 20.0;

/// Diagnosis switch: fits leave the contact rows out (measures the contact term's effect).
#[cfg(test)]
pub(super) static CONTACTS_OFF: AtomicBool = AtomicBool::new(false);

pub(super) fn planet_at(theta: [f64; 3]) -> Planet {
    Planet {
        x: theta[0],
        y: theta[1],
        radius: theta[2],
        mass: MASS_PER_RADIUS_CUBED * theta[2].powi(3),
    }
}

pub(super) fn theta_of(planet: &Planet) -> [f64; 3] {
    [planet.x, planet.y, planet.radius]
}

/// Covariance of `[x, y, radius]` per planet expressed for `[x, y, mass]` (first-order, from the
/// mass tie).
pub(super) fn covariance_xym(covariance: &DMatrix<f64>, planets: &[Planet]) -> DMatrix<f64> {
    let mut transform = DMatrix::identity(covariance.nrows(), covariance.ncols());
    for (i, planet) in planets.iter().enumerate() {
        transform[(3 * i + 2, 3 * i + 2)] = 3.0 * MASS_PER_RADIUS_CUBED * planet.radius.powi(2);
    }
    &transform * covariance * transform.transpose()
}

/// Whether planets `a` and `b` (`[x, y, radius]`) overlap by at least `OVERLAP_TOLERANCE`.
pub(super) fn overlapping(a: &[f64; 3], b: &[f64; 3]) -> bool {
    (a[0] - b[0]).hypot(a[1] - b[1]) <= a[2] + b[2] - OVERLAP_TOLERANCE
}

/// Whether any two planets of `theta` overlap by at least `OVERLAP_TOLERANCE`.
pub(super) fn any_overlap(theta: &[[f64; 3]]) -> bool {
    theta
        .iter()
        .enumerate()
        .any(|(i, a)| theta[i + 1..].iter().any(|b| overlapping(a, b)))
}

fn surface_error(theta: &[f64; 3], point: &Point) -> f64 {
    (f64::from(point.x) - theta[0]).hypot(f64::from(point.y) - theta[1]) - theta[2]
}

/// Complementary error function (Numerical Recipes `erfcc`, relative error below 1.2e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.26551223
        + t * (1.00002368
            + t * (0.37409196
                + t * (0.09678418
                    + t * (-0.18628806
                        + t * (0.27886807
                            + t * (-1.13520398
                                + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277))))))));
    let value = t * poly.exp();
    if x >= 0.0 { value } else { 2.0 - value }
}

/// Signed residual of a terminal contact whose signed distance outside the nearest planet surface
/// is `signed`, from its log-likelihood. The projectile stops at the first substep inside the
/// disk, so on the surface of the planet it hit the distance is uniform in `[-allowance, 0]`, plus
/// a Gaussian slack `CONTACT_SIGMA` (one-sided: penetration, never a gap). The contact may also
/// come from a planet the map does not hold (`CONTACT_BACKGROUND`). The residual is
/// `sqrt(-2 ln)` of that mixture relative to its best value: zero on the allowance, growing with
/// the distance from it, and flat at `unexplained_contact()` once the background dominates (a
/// proper robust likelihood: monotone, bounded, no pull from far contacts).
pub(super) fn contact_residual(signed: f64, allowance: f64) -> f64 {
    let normal_cdf = |z: f64| 0.5 * erfc(-z / std::f64::consts::SQRT_2);
    let on_surface =
        |s: f64| normal_cdf((s + allowance) / CONTACT_SIGMA) - normal_cdf(s / CONTACT_SIGMA);
    let relative = (on_surface(signed) / on_surface(-allowance / 2.0)).clamp(0.0, 1.0);
    let cost = 2.0 * ((1.0 / CONTACT_BACKGROUND).ln_1p() - (relative / CONTACT_BACKGROUND).ln_1p());
    cost.max(0.0).sqrt().copysign(signed + allowance / 2.0)
}

/// The residual of a contact that no planet of the map explains (also every contact of the flat
/// map, so that maps with and without planets are compared on the same contacts).
pub(super) fn unexplained_contact() -> f64 {
    (2.0 * (1.0 / CONTACT_BACKGROUND).ln_1p()).sqrt()
}

/// Acceleration at one observed tick that the model leaves unexplained.
pub(super) struct Unexplained {
    pub position: [f64; 2],
    pub acceleration: [f64; 2],
    /// Whitened residual (innovation) magnitude (about 1.25 on average, 1.4 rms, for a correct
    /// model and noise).
    pub significance: f64,
}

pub(super) struct Evidence {
    pub log_evidence: f64,
    /// `[x, y, radius]` covariance of all planets, `None` when the data leave the parameters
    /// under-identified or a planet sits at a bound.
    pub covariance: Option<DMatrix<f64>>,
}

/// Limits of one `refine`: a deterministic evaluation count and an optional wall-clock instant
/// (in addition to the `Fit`'s own deadline).
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub evaluations: u64,
    pub deadline: Option<Instant>,
    /// Threads one evaluation may use (1: none are spawned).
    pub threads: usize,
}

impl Budget {
    #[cfg(test)]
    pub const UNLIMITED: Budget = Budget {
        evaluations: u64::MAX,
        deadline: None,
        threads: 1,
    };
}

pub(super) struct Fit<'a> {
    shots: &'a [Shot],
    world: WorldParameters,
    cancelled: &'a AtomicBool,
    /// `refine` stops improving (keeping its best point so far) at this instant, or once this
    /// flag is set (pre-emption: new work is waiting).
    deadline: Option<Instant>,
    preempt: Option<&'a AtomicBool>,
    use_contacts: bool,
    /// Planets held fixed; the fitted planets add to their field. Evidence needs this empty.
    fixed: Vec<Planet>,
    /// Field of the fixed planets at each shot's nodes (empty arrays without a path or planets).
    base: Vec<[Vec<f64>; 2]>,
    stats: Option<&'a Stats>,
}

impl<'a> Fit<'a> {
    pub fn new(
        shots: &'a [Shot],
        world: WorldParameters,
        cancelled: &'a AtomicBool,
        use_contacts: bool,
        fixed: Vec<Planet>,
    ) -> Self {
        let base = shots
            .iter()
            .map(|shot| {
                let mut field = [Vec::new(), Vec::new()];
                if let (Some(path), false) = (&shot.path, fixed.is_empty()) {
                    field = [vec![0.0; path.row_count()], vec![0.0; path.row_count()]];
                    for planet in &fixed {
                        path.add_planet(planet.x, planet.y, planet.mass, &mut field);
                    }
                }
                field
            })
            .collect();
        Self {
            shots,
            world,
            cancelled,
            deadline: None,
            preempt: None,
            use_contacts,
            fixed,
            base,
            stats: None,
        }
    }

    pub fn with_stats(mut self, stats: &'a Stats) -> Self {
        self.stats = Some(stats);
        self
    }

    #[cfg(test)]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// `refine` stops (keeping its best point) at `deadline` or once `preempt` is set.
    pub fn with_cut(mut self, deadline: Instant, preempt: &'a AtomicBool) -> Self {
        self.deadline = Some(deadline);
        self.preempt = Some(preempt);
        self
    }

    /// Whether `preempt` (see `with_cut`) is set.
    pub fn preempted(&self) -> bool {
        self.preempt.is_some_and(|p| p.load(Ordering::Relaxed))
    }

    /// Predicted second difference of every row of one path (per axis): the fixed planets'
    /// plus `theta`'s kernel-weighted field.
    pub(super) fn field(&self, index: usize, path: &PathData, theta: &[[f64; 3]]) -> [Vec<f64>; 2] {
        let mut field = if self.base[index][0].is_empty() {
            [vec![0.0; path.row_count()], vec![0.0; path.row_count()]]
        } else {
            self.base[index].clone()
        };
        for th in theta {
            let planet = planet_at(*th);
            path.add_planet(planet.x, planet.y, planet.mass, &mut field);
        }
        field
    }

    fn data_rows(&self) -> usize {
        self.shots
            .iter()
            .filter_map(|shot| shot.path.as_ref())
            .map(|path| 2 * path.row_count())
            .sum()
    }

    /// Residual vector (and its Jacobian with respect to `theta`) of the free planets.
    pub(super) fn evaluate(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
        threads: usize,
    ) -> Option<(DVector<f64>, Option<DMatrix<f64>>)> {
        let started = Instant::now();
        let out = self.compute(theta, want_jacobian, threads);
        if let Some(stats) = self.stats {
            let part = if want_jacobian {
                Part::JacobianEval
            } else {
                Part::ResidualEval
            };
            stats.add(part, started.elapsed());
        }
        out
    }

    /// The rows of one path: its whitened window residuals, axis by axis, then one interior-sample
    /// penalty per planet; with their Jacobian with respect to `theta` (column-major) if wanted.
    fn path_block(
        &self,
        index: usize,
        theta: &[[f64; 3]],
        want_jacobian: bool,
    ) -> (Vec<f64>, Option<Vec<f64>>) {
        let path = self.shots[index].path.as_ref().expect("a path");
        let (r, k) = (path.row_count(), theta.len());
        let rows = 2 * r + k;
        let field = self.field(index, path, theta);
        let mut res = vec![0.0; rows];
        for axis in 0..2 {
            path.residual(axis, &field[axis], &mut res[axis * r..(axis + 1) * r]);
        }
        let mut jacobian = want_jacobian.then(|| vec![0.0; rows * 3 * k]);
        if let Some(block) = jacobian.as_mut() {
            let mut scratch = vec![0.0; r];
            let mut derivative: [Vec<f64>; 6] = std::array::from_fn(|_| vec![0.0; r]);
            for (j, th) in theta.iter().enumerate() {
                let mass = MASS_PER_RADIUS_CUBED * th[2].powi(3);
                let dmass = 3.0 * MASS_PER_RADIUS_CUBED * th[2].powi(2);
                derivative.iter_mut().for_each(|d| d.fill(0.0));
                path.add_planet_derivatives(*th, mass, dmass, &mut derivative);
                for param in 0..3 {
                    let column = &mut block[(3 * j + param) * rows..][..rows];
                    for axis in 0..2 {
                        scratch.copy_from_slice(&derivative[param * 2 + axis]);
                        path.whiten(axis, &mut scratch);
                        for (out, v) in column[axis * r..(axis + 1) * r].iter_mut().zip(&scratch) {
                            *out = -v;
                        }
                    }
                }
            }
        }
        for (j, th) in theta.iter().enumerate() {
            // Closest point of the path (the chords between ticks: at high speed a planet can sit
            // between two ticks).
            let (_, px, py) = path
                .ticks
                .windows(2)
                .map(|w| {
                    let (a, b) = (w[0], w[1]);
                    let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
                    let t = (((th[0] - a[0]) * ex + (th[1] - a[1]) * ey)
                        / (ex * ex + ey * ey).max(1e-18))
                    .clamp(0.0, 1.0);
                    let (px, py) = (a[0] + t * ex, a[1] + t * ey);
                    ((th[0] - px).powi(2) + (th[1] - py).powi(2), px, py)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .expect("paths have ticks");
            let distance = (th[0] - px).hypot(th[1] - py);
            let row = 2 * r + j;
            let value = (th[2] - distance).max(0.0) / PENALTY_SIGMA;
            res[row] = value;
            if let (Some(block), true) = (jacobian.as_mut(), value > 0.0) {
                block[3 * j * rows + row] = -(th[0] - px) / distance.max(1e-9) / PENALTY_SIGMA;
                block[(3 * j + 1) * rows + row] =
                    -(th[1] - py) / distance.max(1e-9) / PENALTY_SIGMA;
                block[(3 * j + 2) * rows + row] = 1.0 / PENALTY_SIGMA;
            }
        }
        (res, jacobian)
    }

    /// One surface-likelihood term per contact (none with contacts off; the flat map's are all
    /// `unexplained_contact()`), with their Jacobian (column-major) if wanted.
    fn contact_rows(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
    ) -> (Vec<f64>, Option<Vec<f64>>) {
        #[cfg(test)]
        let use_contacts = self.use_contacts && !CONTACTS_OFF.load(Ordering::Relaxed);
        #[cfg(not(test))]
        let use_contacts = self.use_contacts;
        let contacts: Vec<_> = if use_contacts {
            self.shots
                .iter()
                .filter_map(|shot| shot.contact.as_ref())
                .collect()
        } else {
            Vec::new()
        };
        let rows = contacts.len();
        let mut res = vec![0.0; rows];
        let mut jacobian = want_jacobian.then(|| vec![0.0; rows * 3 * theta.len()]);
        if theta.is_empty() {
            res.fill(unexplained_contact());
            return (res, jacobian);
        }
        for (row, contact) in contacts.into_iter().enumerate() {
            // Each contact is explained by at most the planet whose surface it is closest to
            // (the mixture's dominant term; planets never overlap), so none is double counted.
            let (j, signed) = theta
                .iter()
                .enumerate()
                .map(|(j, th)| (j, surface_error(th, &contact.point)))
                .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .expect("planets");
            res[row] = contact_residual(signed, contact.allowance);
            if let Some(block) = jacobian.as_mut() {
                let h = 1e-4;
                let slope = (contact_residual(signed + h, contact.allowance)
                    - contact_residual(signed - h, contact.allowance))
                    / (2.0 * h);
                let th = &theta[j];
                let distance = (signed + th[2]).max(1e-9);
                block[3 * j * rows + row] = slope * (th[0] - f64::from(contact.point.x)) / distance;
                block[(3 * j + 1) * rows + row] =
                    slope * (th[1] - f64::from(contact.point.y)) / distance;
                block[(3 * j + 2) * rows + row] = -slope;
            }
        }
        (res, jacobian)
    }

    /// One overlap penalty per pair of free planets and per (free, fixed) pair: the depth by which
    /// the disks overlap (`max(0, r_i + r_j - distance)`) over `PENALTY_SIGMA`, with its Jacobian
    /// with respect to the free planets (column-major) if wanted.
    fn overlap_rows(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
    ) -> (Vec<f64>, Option<Vec<f64>>) {
        let k = theta.len();
        let fixed: Vec<[f64; 3]> = self.fixed.iter().map(theta_of).collect();
        let pairs: Vec<(usize, Option<usize>, [f64; 3])> = (0..k)
            .flat_map(|i| {
                let free = (i + 1..k).map(move |j| (i, Some(j), theta[j]));
                free.chain(fixed.iter().map(move |f| (i, None, *f)))
            })
            .collect();
        let rows = pairs.len();
        let mut res = vec![0.0; rows];
        let mut jacobian = want_jacobian.then(|| vec![0.0; rows * 3 * k]);
        for (row, &(i, j, other)) in pairs.iter().enumerate() {
            let a = &theta[i];
            let (dx, dy) = (a[0] - other[0], a[1] - other[1]);
            let distance = dx.hypot(dy);
            let depth = a[2] + other[2] - distance;
            if depth <= 0.0 {
                continue;
            }
            res[row] = depth / PENALTY_SIGMA;
            if let Some(block) = jacobian.as_mut() {
                let (ux, uy) = (dx / distance.max(1e-9), dy / distance.max(1e-9));
                let mut set = |planet: usize, sign: f64| {
                    block[3 * planet * rows + row] = -sign * ux / PENALTY_SIGMA;
                    block[(3 * planet + 1) * rows + row] = -sign * uy / PENALTY_SIGMA;
                    block[(3 * planet + 2) * rows + row] = 1.0 / PENALTY_SIGMA;
                };
                set(i, 1.0);
                if let Some(j) = j {
                    set(j, -1.0);
                }
            }
        }
        (res, jacobian)
    }

    /// The rows not tied to a path, contacts then overlaps, as one column-major block.
    fn global_rows(&self, theta: &[[f64; 3]], want_jacobian: bool) -> (Vec<f64>, Option<Vec<f64>>) {
        let (mut res, contacts) = self.contact_rows(theta, want_jacobian);
        let (overlaps, overlap_block) = self.overlap_rows(theta, want_jacobian);
        let (a, b) = (res.len(), overlaps.len());
        res.extend(overlaps);
        let jacobian = contacts.zip(overlap_block).map(|(first, second)| {
            (0..3 * theta.len())
                .flat_map(|c| {
                    first[c * a..(c + 1) * a]
                        .iter()
                        .chain(&second[c * b..(c + 1) * b])
                        .copied()
                })
                .collect()
        });
        (res, jacobian)
    }

    /// Threads one evaluation uses: they only pay off when there is enough work to share
    /// (measured in `diagnose.rs`).
    fn evaluation_threads(&self, k: usize, threads: usize) -> usize {
        let planet_nodes: usize = k * self
            .shots
            .iter()
            .filter_map(|shot| shot.path.as_ref())
            .map(|path| path.nodes.len())
            .sum::<usize>();
        if planet_nodes >= PARALLEL_MIN_PLANET_NODES {
            threads.min(MAX_EVALUATION_THREADS)
        } else {
            1
        }
    }

    fn path_indices(&self) -> Vec<usize> {
        (0..self.shots.len())
            .filter(|&index| self.shots[index].path.is_some())
            .collect()
    }

    fn compute(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
        threads: usize,
    ) -> Option<(DVector<f64>, Option<DMatrix<f64>>)> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let k = theta.len();
        let threads = self.evaluation_threads(k, threads);
        let workers = if want_jacobian {
            threads
        } else {
            threads.min(k.max(1))
        };
        let mut blocks = par_map(&self.path_indices(), workers, |&index| {
            self.path_block(index, theta, want_jacobian)
        });
        blocks.push(self.global_rows(theta, want_jacobian));
        let total: usize = blocks.iter().map(|(res, _)| res.len()).sum();
        let mut res = Vec::with_capacity(total);
        let mut jacobian = want_jacobian.then(|| DMatrix::zeros(total, 3 * k));
        for (part, block) in blocks {
            if let (Some(jacobian), Some(block)) = (jacobian.as_mut(), block) {
                let rows = part.len();
                for c in 0..3 * k {
                    jacobian
                        .view_mut((res.len(), c), (rows, 1))
                        .copy_from_slice(&block[c * rows..(c + 1) * rows]);
                }
            }
            res.extend(part);
        }
        res.iter()
            .all(|v| v.is_finite())
            .then(|| (DVector::from_vec(res), jacobian))
    }

    /// The pivoted QR of the Jacobian with respect to the logistic coordinates `u` (of `theta`)
    /// and `Q^T` of the residuals, without forming the full Jacobian: consecutive paths are
    /// grouped by rows (from the data alone, so the result does not depend on `threads`), each
    /// group's blocks are folded into one triangle on its own thread, and the triangles and the
    /// contact and overlap rows are reduced last. `None` if cancelled or a residual is not finite.
    fn reduce(&self, theta: &[[f64; 3]], u: &DVector<f64>, threads: usize) -> Option<Reduced> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let (k, n) = (theta.len(), 3 * theta.len());
        let threads = self.evaluation_threads(k, threads);
        let paths = self.path_indices();
        let rows_of =
            |index: usize| 2 * self.shots[index].path.as_ref().expect("a path").row_count() + k;
        let total: usize = paths.iter().map(|&index| rows_of(index)).sum();
        let target = total.div_ceil(REDUCTION_GROUPS).max(1);
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut rows = target;
        for &index in &paths {
            if rows >= target {
                groups.push(Vec::new());
                rows = 0;
            }
            groups.last_mut().expect("pushed").push(index);
            rows += rows_of(index);
        }
        let triangles = par_map(&groups, threads, |group| {
            let mut folder = Folder::new(n);
            for &index in group {
                let (res, block) = self.path_block(index, theta, true);
                if !res.iter().all(|v| v.is_finite()) {
                    return None;
                }
                let mut block = block.expect("requested");
                self.chain(theta, u, &mut block, res.len());
                folder.add(&block, &res);
            }
            Some(folder.finish())
        });
        let triangles: Vec<Triangle> = triangles
            .into_iter()
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let (res, block) = self.global_rows(theta, true);
        if !res.iter().all(|v| v.is_finite()) {
            return None;
        }
        let mut block = block.expect("requested");
        self.chain(theta, u, &mut block, res.len());
        Some(Reduced::new(&triangles, &block, &res, n, total + res.len()))
    }

    /// Chains a column-major Jacobian block (`rows` rows) with respect to `theta` to the logistic
    /// coordinates `u`: x and y depend on their own coordinate and, through the bounds, on the
    /// radius coordinate.
    fn chain(&self, theta: &[[f64; 3]], u: &DVector<f64>, block: &mut [f64], rows: usize) {
        let sigmoid = |v: f64| 1.0 / (1.0 + (-v).exp());
        for (j, th) in theta.iter().enumerate() {
            let (ux, uy, ur) = (u[3 * j], u[3 * j + 1], u[3 * j + 2]);
            let radius_slope =
                (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(ur) * (1.0 - sigmoid(ur));
            let dx_dux =
                (self.world.battlefield_width - 2.0 * th[2]) * sigmoid(ux) * (1.0 - sigmoid(ux));
            let dy_duy =
                (self.world.battlefield_height - 2.0 * th[2]) * sigmoid(uy) * (1.0 - sigmoid(uy));
            let dx_dur = (1.0 - 2.0 * sigmoid(ux)) * radius_slope;
            let dy_dur = (1.0 - 2.0 * sigmoid(uy)) * radius_slope;
            let (jx, rest) = block[3 * j * rows..].split_at_mut(rows);
            let (jy, rest) = rest.split_at_mut(rows);
            let jr = &mut rest[..rows];
            for i in 0..rows {
                let (x, y, r) = (jx[i], jy[i], jr[i]);
                jx[i] = x * dx_dux;
                jy[i] = y * dy_duy;
                jr[i] = x * dx_dur + y * dy_dur + r * radius_slope;
            }
        }
    }

    /// Whether no two planets of `theta`, nor one of `theta` and a fixed planet, overlap by
    /// `OVERLAP_TOLERANCE` or more. `refine` only accepts steps to such points.
    pub fn feasible(&self, theta: &[[f64; 3]]) -> bool {
        !any_overlap(theta)
            && !theta
                .iter()
                .any(|a| self.fixed.iter().any(|f| overlapping(a, &theta_of(f))))
    }

    /// Chi-square.
    pub fn chi2(&self, theta: &[[f64; 3]]) -> Option<f64> {
        self.evaluate(theta, false, 1)
            .map(|(residuals, _)| residuals.norm_squared())
    }

    /// Whether every planet lies inside the radius range and the battlefield, shrunk by `margin`.
    pub fn in_bounds(&self, theta: &[[f64; 3]], margin: f64) -> bool {
        theta.iter().all(|th| {
            let r = th[2];
            (RADIUS_RANGE.0 + margin..=RADIUS_RANGE.1 - margin).contains(&r)
                && th[0] >= r + margin
                && th[0] <= self.world.battlefield_width - r - margin
                && th[1] >= r + margin
                && th[1] <= self.world.battlefield_height - r - margin
        })
    }

    /// Logistic coordinates keep each centre in the battlefield and each radius in range.
    fn decode(&self, u: &DVector<f64>) -> Vec<[f64; 3]> {
        let sigmoid = |v: f64| 1.0 / (1.0 + (-v).exp());
        u.as_slice()
            .chunks(3)
            .map(|c| {
                let radius = RADIUS_RANGE.0 + (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(c[2]);
                [
                    radius + (self.world.battlefield_width - 2.0 * radius) * sigmoid(c[0]),
                    radius + (self.world.battlefield_height - 2.0 * radius) * sigmoid(c[1]),
                    radius,
                ]
            })
            .collect()
    }

    fn encode(&self, theta: &[[f64; 3]]) -> DVector<f64> {
        let logit = |v: f64| {
            let v = v.clamp(1e-6, 1.0 - 1e-6);
            (v / (1.0 - v)).ln()
        };
        DVector::from_iterator(
            3 * theta.len(),
            theta.iter().flat_map(|th| {
                let r = th[2];
                [
                    logit((th[0] - r) / (self.world.battlefield_width - 2.0 * r)),
                    logit((th[1] - r) / (self.world.battlefield_height - 2.0 * r)),
                    logit((r - RADIUS_RANGE.0) / (RADIUS_RANGE.1 - RADIUS_RANGE.0)),
                ]
            }),
        )
    }

    /// Local minimum from `start`, or the best point reached when the deadline passes (never worse
    /// than `start`: `Lm` remembers the lowest-norm point it evaluated, because the optimizer
    /// leaves its last, possibly worse, trial step as the parameters when it is interrupted).
    /// `None` if cancelled or the model cannot be evaluated at the result. Running out of patience
    /// is not a failure: the iterate is still a valid point and is scored by its evidence like any
    /// other.
    pub fn refine(&self, start: &[[f64; 3]], budget: &Budget) -> Option<(Vec<[f64; 3]>, f64)> {
        let started = Instant::now();
        let mut u = self.encode(start);
        let mut problem = Lm {
            fit: self,
            best: None,
            budget,
            used: 0,
        };
        lm::minimize(&mut problem, u.clone(), LM_PATIENCE);
        if let Some((_, best)) = problem.best {
            u = best;
        }
        let theta = self.decode(&u);
        let chi2 = self.chi2(&theta)?;
        if let Some(stats) = self.stats {
            stats.add(Part::LmSolve, started.elapsed());
        }
        Some((theta, chi2))
    }

    /// Laplace evidence and covariance at `theta` (all planets free, none fixed).
    pub fn evidence(&self, theta: &[[f64; 3]]) -> Option<Evidence> {
        let started = Instant::now();
        let out = self.laplace(theta);
        if let Some(stats) = self.stats {
            stats.add(Part::Evidence, started.elapsed());
        }
        out
    }

    fn laplace(&self, theta: &[[f64; 3]]) -> Option<Evidence> {
        debug_assert!(self.fixed.is_empty());
        let k = theta.len();
        let (residuals, jacobian) = self.evaluate(theta, true, 1)?;
        let chi2 = residuals.norm_squared();
        if k == 0 {
            return Some(Evidence {
                log_evidence: -0.5 * chi2,
                covariance: None,
            });
        }
        let jacobian = jacobian.expect("requested");
        let covariance_started = Instant::now();
        let information = jacobian.tr_mul(&jacobian);
        // Gaussian prior as wide as a uniform one over the allowed ranges.
        let prior_sd = |i: usize| {
            [
                self.world.battlefield_width,
                self.world.battlefield_height,
                RADIUS_RANGE.1 - RADIUS_RANGE.0,
            ][i % 3]
                / 12f64.sqrt()
        };
        let scaled = DMatrix::from_fn(3 * k, 3 * k, |i, j| {
            information[(i, j)] * prior_sd(i) * prior_sd(j) + if i == j { 1.0 } else { 0.0 }
        });
        let occam: f64 = scaled
            .cholesky()
            .map(|c| (0..3 * k).map(|i| c.l()[(i, i)].ln()).sum())?;
        let log_evidence = -0.5 * chi2 - occam - k as f64 * PLANET_PRIOR_ODDS.ln();
        // No covariance for a mode saturated at a bound (the Gaussian approximation does not hold
        // there) or with a badly conditioned (diagonal-normalized) information matrix.
        let scale = DMatrix::from_diagonal(&information.diagonal().map(|d| d.sqrt().recip()));
        let correlation = &scale * &information * &scale;
        let well_conditioned = correlation.iter().all(|v| v.is_finite())
            && SymmetricEigen::new(correlation).eigenvalues.min() > MIN_CORRELATION_EIGENVALUE;
        let covariance = (well_conditioned && self.in_bounds(theta, BOUND_MARGIN))
            .then(|| information.clone().try_inverse())
            .flatten()
            .map(|inverse| {
                // Inflate when the residuals exceed what the noise model expects.
                let dof = (self.data_rows() as f64 - 3.0 * k as f64).max(1.0);
                inverse * (chi2 / dof).max(1.0)
            });
        if let Some(stats) = self.stats {
            stats.add(Part::Covariance, covariance_started.elapsed());
        }
        log_evidence.is_finite().then_some(Evidence {
            log_evidence,
            covariance,
        })
    }

    /// Observed rows ordered by how far the model leaves them unexplained.
    pub fn unexplained(&self, theta: &[[f64; 3]]) -> Vec<Unexplained> {
        let mut out = Vec::new();
        for (index, shot) in self.shots.iter().enumerate() {
            let Some(path) = &shot.path else { continue };
            let r = path.row_count();
            let field = self.field(index, path, theta);
            let (mut raw, mut white) = ([vec![0.0; r], vec![0.0; r]], [vec![0.0; r], vec![0.0; r]]);
            for axis in 0..2 {
                path.raw_residual(axis, &field[axis], &mut raw[axis]);
                white[axis].copy_from_slice(&raw[axis]);
                path.whiten(axis, &mut white[axis]);
            }
            for i in 0..r {
                out.push(Unexplained {
                    position: path.center(i),
                    acceleration: [raw[0][i], raw[1][i]],
                    significance: white[0][i].hypot(white[1][i]),
                });
            }
        }
        out.sort_by(|a, b| b.significance.total_cmp(&a.significance));
        out
    }
}

struct Lm<'a> {
    fit: &'a Fit<'a>,
    /// Lowest residual norm evaluated so far and its parameters.
    best: Option<(f64, DVector<f64>)>,
    budget: &'a Budget,
    /// Evaluations of this refinement so far.
    used: u64,
}

impl Lm<'_> {
    /// Whether another evaluation is allowed; counts it when it is.
    fn spend(&mut self) -> bool {
        let out_of_time = self
            .fit
            .deadline
            .into_iter()
            .chain(self.budget.deadline)
            .any(|d| Instant::now() >= d);
        if out_of_time || self.fit.preempted() || self.used >= self.budget.evaluations {
            return false;
        }
        self.used += 1;
        true
    }
}

impl lm::Problem for Lm<'_> {
    fn residual_norm(&mut self, u: &DVector<f64>) -> Option<f64> {
        if !self.spend() {
            return None;
        }
        let theta = self.fit.decode(u);
        // An overlapping point counts as infinitely bad: the step is rejected and the trust
        // region shrinks, so iterates stay feasible (an infeasible start does not move).
        if !self.fit.feasible(&theta) {
            return Some(f64::INFINITY);
        }
        let (residuals, _) = self.fit.evaluate(&theta, false, self.budget.threads)?;
        let norm = residuals.norm();
        if self.best.as_ref().is_none_or(|(lowest, _)| norm < *lowest) {
            self.best = Some((norm, u.clone()));
        }
        Some(norm)
    }

    fn reduced(&mut self, u: &DVector<f64>) -> Option<Reduced> {
        if !self.spend() {
            return None;
        }
        let started = Instant::now();
        let out = self.fit.reduce(&self.fit.decode(u), u, self.budget.threads);
        if let Some(stats) = self.fit.stats {
            stats.add(Part::JacobianEval, started.elapsed());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{planet, shot_in, shot_with_contact, world_of};
    use super::*;
    use crate::simulation::WorldParameters;
    use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
    use nalgebra::{Dyn, U1, VecStorage};

    fn shots(planets: Vec<Planet>, starts: &[((f32, f32), f64)]) -> Vec<Shot> {
        let truth = world_of(planets);
        starts
            .iter()
            .map(|(start, angle)| {
                shot_with_contact(
                    &shot_in(&truth, *start, *angle, 6.0),
                    &[Point { x: 1.0, y: 1.0 }],
                )
            })
            .collect()
    }

    fn three_planets() -> Vec<Shot> {
        shots(
            vec![
                planet(500.0, 500.0, 30.0),
                planet(1200.0, 700.0, 28.0),
                planet(900.0, 250.0, 25.0),
            ],
            &[
                ((350.0, 500.0), 0.0),
                ((1050.0, 700.0), 0.0),
                ((900.0, 450.0), 90.0),
                ((300.0, 900.0), 20.0),
                ((1500.0, 300.0), 170.0),
            ],
        )
    }

    /// The dense Jacobian in the logistic coordinates and the residuals at `u`.
    fn dense(fit: &Fit, u: &DVector<f64>) -> (DVector<f64>, DMatrix<f64>) {
        let theta = fit.decode(u);
        let (residuals, jacobian) = fit.evaluate(&theta, true, 1).unwrap();
        let mut jacobian = jacobian.unwrap();
        let rows = jacobian.nrows();
        fit.chain(&theta, u, jacobian.as_mut_slice(), rows);
        (residuals, jacobian)
    }

    // The reduction is the QR of the full Jacobian: `R^T R = J^T J` and `R^T Q^T f = J^T f`.
    #[test]
    fn the_reduced_factor_is_the_qr_of_the_full_jacobian() {
        let cancelled = AtomicBool::new(false);
        let shots = three_planets();
        let fit = Fit::new(&shots, WorldParameters::default(), &cancelled, true, vec![]);
        let theta = [
            [505.0, 497.0, 30.5],
            [1195.0, 703.0, 27.0],
            [905.0, 255.0, 25.5],
        ];
        let u = fit.encode(&theta);
        let (residuals, jacobian) = dense(&fit, &u);
        let reduced = fit.reduce(&theta, &u, 1).unwrap();
        let (r, qtb) = reduced.unpivoted();
        let information = jacobian.tr_mul(&jacobian);
        let scale = information.amax();
        assert!((r.tr_mul(&r) - &information).amax() < 1e-10 * scale);
        let gradient = jacobian.tr_mul(&residuals);
        assert!((r.tr_mul(&qtb) - &gradient).amax() < 1e-10 * gradient.amax());
        for threads in [2, 8] {
            let other = fit.reduce(&theta, &u, threads).unwrap().unpivoted();
            assert!(other.0 == r && other.1 == qtb, "{threads} threads");
        }
    }

    /// Shots around one planet of radius 36 at (800, 500), the planet P18 split into three.
    fn one_large_planet() -> Vec<Shot> {
        shots(
            vec![planet(800.0, 500.0, 36.0)],
            &[
                ((650.0, 500.0), 0.0),
                ((800.0, 650.0), 270.0),
                ((950.0, 520.0), 180.0),
                ((800.0, 350.0), 90.0),
                ((600.0, 700.0), 330.0),
            ],
        )
    }

    // The overlap rows' Jacobian is their derivative (free-free and free-fixed pairs), and the
    // reduction of an overlapping map is still the QR of its full Jacobian.
    #[test]
    fn overlap_rows_have_the_jacobian_of_their_residuals() {
        let cancelled = AtomicBool::new(false);
        let shots = three_planets();
        let fixed = vec![planet(540.0, 500.0, 30.0)];
        let fit = Fit::new(&shots, WorldParameters::default(), &cancelled, true, fixed);
        let theta = [
            [505.0, 497.0, 30.5],
            [520.0, 470.0, 27.0],
            [905.0, 255.0, 25.5],
        ];
        let (residuals, jacobian) = fit.overlap_rows(&theta, true);
        let jacobian = jacobian.unwrap();
        let rows = residuals.len();
        assert_eq!(rows, 3 + 3);
        assert!(residuals.iter().filter(|v| **v > 0.0).count() >= 3);
        for c in 0..9 {
            let h = 1e-6;
            let shifted = |delta: f64| {
                let mut t = theta;
                t[c / 3][c % 3] += delta;
                fit.overlap_rows(&t, false).0
            };
            let (plus, minus) = (shifted(h), shifted(-h));
            for row in 0..rows {
                let numeric = (plus[row] - minus[row]) / (2.0 * h);
                let analytic = jacobian[c * rows + row];
                assert!(
                    (numeric - analytic).abs() < 1e-5,
                    "column {c} row {row}: {numeric} vs {analytic}"
                );
            }
        }
        let u = fit.encode(&theta);
        let (residuals, jacobian) = dense(&fit, &u);
        let (r, qtb) = fit.reduce(&theta, &u, 1).unwrap().unpivoted();
        let information = jacobian.tr_mul(&jacobian);
        assert!((r.tr_mul(&r) - &information).amax() < 1e-10 * information.amax());
        let gradient = jacobian.tr_mul(&residuals);
        assert!((r.tr_mul(&qtb) - &gradient).amax() < 1e-10 * gradient.amax());
    }

    // One true planet and two estimated ones beside it: the data pull both to the true centre
    // (P18's concentric split, masses adding up to the true one), the fit stops short of
    // overlapping. Without the step barrier (overlap penalty alone) both end at the centre.
    #[test]
    fn a_fit_cannot_converge_into_overlap() {
        let cancelled = AtomicBool::new(false);
        let shots = one_large_planet();
        let fit = Fit::new(&shots, WorldParameters::default(), &cancelled, true, vec![]);
        let start = [[770.0, 500.0, 28.0], [830.0, 505.0, 26.0]];
        assert!(fit.feasible(&start));
        let (theta, _) = fit.refine(&start, &Budget::UNLIMITED).unwrap();
        assert!(!any_overlap(&theta), "{theta:?}");
        // A fixed planet (too small for the field) holds a free one off the same way.
        let fixed = planet(800.0, 500.0, 25.0);
        let alone = Fit::new(
            &shots,
            WorldParameters::default(),
            &cancelled,
            false,
            vec![fixed.clone()],
        );
        let (theta, _) = alone
            .refine(&[[870.0, 500.0, 30.0]], &Budget::UNLIMITED)
            .unwrap();
        assert!(!overlapping(&theta[0], &theta_of(&fixed)), "{theta:?}");
    }

    // A planet whose disk crosses the battlefield edge (which the generator never places) is
    // fitted inside it: the centre stays within `[r, W - r] x [r, H - r]`.
    #[test]
    fn fitted_planets_stay_inside_the_battlefield() {
        let cancelled = AtomicBool::new(false);
        let shots = shots(
            vec![planet(10.0, 500.0, 30.0)],
            &[
                ((150.0, 500.0), 170.0),
                ((150.0, 650.0), 200.0),
                ((120.0, 300.0), 120.0),
            ],
        );
        let fit = Fit::new(
            &shots,
            WorldParameters::default(),
            &cancelled,
            false,
            vec![],
        );
        let (theta, _) = fit
            .refine(&[[60.0, 500.0, 30.0]], &Budget::UNLIMITED)
            .unwrap();
        assert!(fit.in_bounds(&theta, 0.0), "{theta:?}");
        assert!(
            theta[0][0] - theta[0][2] < 1.0,
            "pressed against the edge: {theta:?}"
        );
    }

    /// The crate's minimizer on the dense problem, as `refine` ran before.
    struct Dense<'a> {
        fit: &'a Fit<'a>,
        u: DVector<f64>,
    }

    impl LeastSquaresProblem<f64, Dyn, Dyn> for Dense<'_> {
        type ParameterStorage = VecStorage<f64, Dyn, U1>;
        type ResidualStorage = VecStorage<f64, Dyn, U1>;
        type JacobianStorage = VecStorage<f64, Dyn, Dyn>;
        fn set_params(&mut self, u: &DVector<f64>) {
            self.u.copy_from(u);
        }
        fn params(&self) -> DVector<f64> {
            self.u.clone()
        }
        fn residuals(&self) -> Option<DVector<f64>> {
            Some(dense(self.fit, &self.u).0)
        }
        fn jacobian(&self) -> Option<DMatrix<f64>> {
            Some(dense(self.fit, &self.u).1)
        }
    }

    // Unlimited refits from starts several units off reach the minimum the crate reaches.
    #[test]
    fn the_solver_reaches_the_minimum_of_the_crate() {
        let cancelled = AtomicBool::new(false);
        let two = shots(
            vec![planet(500.0, 500.0, 30.0), planet(1200.0, 700.0, 30.0)],
            &[
                ((350.0, 500.0), 0.0),
                ((1050.0, 700.0), 0.0),
                ((900.0, 600.0), 150.0),
                ((300.0, 900.0), 20.0),
            ],
        );
        let three = three_planets();
        let cases: [(&[Shot], Vec<[f64; 3]>); 3] = [
            (&two, vec![[520.0, 480.0, 35.0], [1180.0, 720.0, 26.0]]),
            (
                &three,
                vec![
                    [510.0, 490.0, 33.0],
                    [1190.0, 710.0, 25.0],
                    [915.0, 240.0, 22.0],
                ],
            ),
            (
                &three,
                vec![
                    [540.0, 470.0, 24.0],
                    [1230.0, 680.0, 34.0],
                    [880.0, 280.0, 30.0],
                ],
            ),
        ];
        for (shots, start) in cases {
            for contacts in [false, true] {
                let fit = Fit::new(
                    shots,
                    WorldParameters::default(),
                    &cancelled,
                    contacts,
                    vec![],
                );
                let (theta, chi2) = fit.refine(&start, &Budget::UNLIMITED).unwrap();
                let (crate_fit, _) = LevenbergMarquardt::new()
                    .with_patience(LM_PATIENCE)
                    .minimize(Dense {
                        fit: &fit,
                        u: fit.encode(&start),
                    });
                let reference = fit.decode(&crate_fit.u);
                let reference_chi2 = fit.chi2(&reference).unwrap();
                let moved = theta
                    .iter()
                    .zip(&reference)
                    .flat_map(|(a, b)| (0..3).map(move |i| (a[i] - b[i]).abs()))
                    .fold(0.0, f64::max);
                assert!(
                    (chi2 - reference_chi2).abs() <= 1e-6 * reference_chi2.max(1.0) && moved < 1e-3,
                    "chi2 {chi2} vs {reference_chi2}, max difference {moved}: {theta:?} vs {reference:?}"
                );
            }
        }
    }
}
