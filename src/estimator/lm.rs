//! Levenberg-Marquardt minimization on a QR factor reduced from a tall Jacobian.
//!
//! The trust-region loop and its subproblem are a port of the `levenberg-marquardt` crate 0.15
//! (MIT; itself a port of MINPACK's `lmder`/`lmpar`/`qrsolv`, Moré 1978) with the crate's default
//! tolerances. The difference is where the QR factor comes from: the crate takes the full
//! Jacobian and pivots a dense QR of it (single-threaded, and the cost of the fit on many rows).
//! Here the caller folds row blocks into small upper triangular factors (`fold`, unpivoted
//! Householder, in parallel and without keeping the full Jacobian) and `Reduced::new` runs the
//! pivoted QR on the stacked factors only. That gives the pivoted QR of the full Jacobian: if
//! `J = Q1 R1` then `J P = Q1 (R1 P)`, and greedy column pivoting sees the same column norms in
//! `R1 P` as in `J P`. No normal equations are formed (E8: they reach different minima).
use nalgebra::{DMatrix, DVector};

const TOLERANCE: f64 = 30.0 * f64::EPSILON;
const STEP_BOUND: f64 = 100.0;

/// The upper triangular factor `R` (`n x n`, column-major) and `Q^T b` of some rows `[A | b]`.
#[derive(Clone)]
pub(super) struct Triangle {
    pub r: Vec<f64>,
    pub qtb: Vec<f64>,
}

/// `a . b` with eight partial sums: a single running sum is a chain of dependent additions that
/// the compiler may not reorder, and this dot product is most of the cost of a fit.
fn dot(a: &[f64], b: &[f64]) -> f64 {
    let mut sums = [0.0; 8];
    let (chunks_a, chunks_b) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f64 = chunks_a
        .remainder()
        .iter()
        .zip(chunks_b.remainder())
        .map(|(x, y)| x * y)
        .sum();
    for (x, y) in chunks_a.zip(chunks_b) {
        for i in 0..8 {
            sums[i] += x[i] * y[i];
        }
    }
    sums.iter().sum::<f64>() + tail
}

/// Householder reflection of column `j` of the column-major `a` (`rows` rows, `n` columns) applied
/// to the columns after it and to `b`; the column becomes its `R` diagonal. Returns the diagonal
/// (0 for a zero column, which is left as it is).
fn reflect(a: &mut [f64], b: &mut [f64], rows: usize, n: usize, j: usize) -> f64 {
    let (head, tail) = a.split_at_mut((j + 1) * rows);
    let axis = &mut head[j * rows + j..];
    let mut norm = dot(axis, axis).sqrt();
    if norm == 0.0 {
        return 0.0;
    }
    if axis[0] < 0.0 {
        norm = -norm;
    }
    for v in axis.iter_mut() {
        *v /= norm;
    }
    axis[0] += 1.0;
    let axis = &*axis;
    let apply = |column: &mut [f64]| {
        let scale = -dot(column, axis) / axis[0];
        for (c, w) in column.iter_mut().zip(axis) {
            *c += scale * w;
        }
    };
    for k in 0..n - j - 1 {
        apply(&mut tail[k * rows + j..(k + 1) * rows]);
    }
    apply(&mut b[j..]);
    -norm
}

/// Rows folded into one triangle, a buffer of rows at a time (so the work stays in cache and the
/// overhead of refolding the triangle stays small). Rows whose Jacobian row is zero (most
/// interior-sample penalties) change neither `R` nor the leading `n` entries of `Q^T b` and are
/// left out.
pub(super) struct Folder {
    n: usize,
    triangle: Option<Triangle>,
    /// Buffered rows, column by column.
    columns: Vec<Vec<f64>>,
    rhs: Vec<f64>,
}

impl Folder {
    pub fn new(n: usize) -> Self {
        Self {
            n,
            triangle: None,
            columns: vec![Vec::new(); n],
            rhs: Vec::new(),
        }
    }

    /// Adds the rows of the column-major `block` with right-hand side `rhs`.
    pub fn add(&mut self, block: &[f64], rhs: &[f64]) {
        let rows = rhs.len();
        let mut keep = vec![false; rows];
        for column in block.chunks_exact(rows.max(1)) {
            for (keep, value) in keep.iter_mut().zip(column) {
                *keep |= *value != 0.0;
            }
        }
        let kept = |values: &[f64]| {
            values
                .iter()
                .zip(&keep)
                .filter(|(_, keep)| **keep)
                .map(|(value, _)| *value)
                .collect::<Vec<_>>()
        };
        for (c, column) in self.columns.iter_mut().enumerate() {
            column.extend(kept(&block[c * rows..(c + 1) * rows]));
        }
        self.rhs.extend(kept(rhs));
        if self.rhs.len() >= (8 * self.n).max(256) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.rhs.is_empty() {
            return;
        }
        let block: Vec<f64> = self.columns.concat();
        self.triangle = Some(fold(self.triangle.as_ref(), &block, &self.rhs, self.n));
        self.columns.iter_mut().for_each(Vec::clear);
        self.rhs.clear();
    }

    /// The triangle of every row added, `None` without any.
    pub fn finish(mut self) -> Option<Triangle> {
        self.flush();
        self.triangle
    }
}

/// The triangle of `acc` stacked over the `rows x n` column-major `block` with right-hand side
/// `rhs` (unpivoted Householder QR).
fn fold(acc: Option<&Triangle>, block: &[f64], rhs: &[f64], n: usize) -> Triangle {
    let rows = rhs.len();
    debug_assert_eq!(block.len(), rows * n);
    let top = if acc.is_some() { n } else { 0 };
    let total = top + rows;
    let mut a = vec![0.0; total * n];
    let mut b = vec![0.0; total];
    for c in 0..n {
        let column = &mut a[c * total..(c + 1) * total];
        if let Some(acc) = acc {
            column[..n].copy_from_slice(&acc.r[c * n..(c + 1) * n]);
        }
        column[top..].copy_from_slice(&block[c * rows..(c + 1) * rows]);
    }
    if let Some(acc) = acc {
        b[..n].copy_from_slice(&acc.qtb);
    }
    b[top..].copy_from_slice(rhs);
    let mut r = vec![0.0; n * n];
    for j in 0..total.min(n) {
        let diagonal = reflect(&mut a, &mut b, total, n, j);
        r[j * n + j] = diagonal;
    }
    for c in 0..n {
        for i in 0..c.min(total) {
            r[c * n + i] = a[c * total + i];
        }
    }
    b.resize(n, 0.0);
    Triangle { r, qtb: b }
}

/// The pivoted QR of a Jacobian with `n` columns and `rows` rows, from triangles of its row blocks
/// (in any order) and extra rows, with the trust-region subproblem on it.
pub(super) struct Reduced {
    /// `R` in pivoted column order; its strict lower triangle is workspace.
    r: DMatrix<f64>,
    permutation: Vec<usize>,
    qtb: DVector<f64>,
    /// Column norms of the Jacobian (original order).
    column_norms: DVector<f64>,
    /// Rank bound `min(rows, n)`.
    max_rank: usize,
    l_diag: DVector<f64>,
    work: DVector<f64>,
}

impl Reduced {
    /// `extra` is `rows x n` column-major with right-hand side `extra_rhs`; `rows` counts the
    /// Jacobian rows all of them stand for.
    pub fn new(
        triangles: &[Triangle],
        extra: &[f64],
        extra_rhs: &[f64],
        n: usize,
        rows: usize,
    ) -> Self {
        let m = triangles.len() * n + extra_rhs.len();
        let mut a = vec![0.0; m * n];
        let mut b = Vec::with_capacity(m);
        for c in 0..n {
            let column = &mut a[c * m..(c + 1) * m];
            for (t, triangle) in triangles.iter().enumerate() {
                column[t * n..(t + 1) * n].copy_from_slice(&triangle.r[c * n..(c + 1) * n]);
            }
            let start = triangles.len() * n;
            column[start..].copy_from_slice(&extra[c * extra_rhs.len()..(c + 1) * extra_rhs.len()]);
        }
        for triangle in triangles {
            b.extend_from_slice(&triangle.qtb);
        }
        b.extend_from_slice(extra_rhs);
        let norm = |column: &[f64]| column.iter().map(|v| v * v).sum::<f64>().sqrt();
        let column_norms = DVector::from_iterator(n, (0..n).map(|c| norm(&a[c * m..(c + 1) * m])));
        // Pivoted Householder QR (LAPACK xGEQPF with the norm downdate of LAWN 176, as the crate).
        let mut r_diag = column_norms.clone();
        let mut work = column_norms.clone();
        let mut permutation: Vec<usize> = (0..n).collect();
        let max_rank = rows.min(n);
        for j in 0..m.min(n) {
            let kmax = (j..n)
                .max_by(|&x, &y| r_diag[x].total_cmp(&r_diag[y]).then(y.cmp(&x)))
                .expect("j < n");
            if kmax != j {
                for i in 0..m {
                    a.swap(j * m + i, kmax * m + i);
                }
                permutation.swap(j, kmax);
                r_diag[kmax] = r_diag[j];
                work[kmax] = work[j];
            }
            if r_diag[j] == 0.0 {
                // Every remaining column is zero.
                continue;
            }
            r_diag[j] = reflect(&mut a, &mut b, m, n, j);
            if r_diag[j] == 0.0 {
                continue;
            }
            for k in j + 1..n {
                if r_diag[k] == 0.0 {
                    continue;
                }
                let head = a[k * m + j];
                r_diag[k] *= (1.0 - (head / r_diag[k]).powi(2)).max(0.0).sqrt();
                if 0.05 * (r_diag[k] / work[k]).powi(2) <= f64::EPSILON {
                    r_diag[k] = norm(&a[k * m + j + 1..(k + 1) * m]);
                    work[k] = r_diag[k];
                }
            }
        }
        let mut r = DMatrix::zeros(n, n);
        for c in 0..n {
            for i in 0..c.min(max_rank) {
                r[(i, c)] = a[c * m + i];
            }
            if c < max_rank && c < m {
                r[(c, c)] = r_diag[c];
            }
        }
        let mut qtb = DVector::zeros(n);
        for i in 0..max_rank.min(m) {
            qtb[i] = b[i];
        }
        Self {
            r,
            permutation,
            qtb,
            column_norms,
            max_rank,
            l_diag: DVector::zeros(n),
            work: DVector::zeros(n),
        }
    }

    /// `R` and `Q^T b` with the columns in their original order (`R` is then not triangular).
    #[cfg(test)]
    pub fn unpivoted(&self) -> (DMatrix<f64>, DVector<f64>) {
        let n = self.n();
        let mut r = DMatrix::zeros(n, n);
        for (j, &index) in self.permutation.iter().enumerate() {
            for i in 0..=j {
                r[(i, index)] = self.r[(i, j)];
            }
        }
        (r, self.qtb.clone())
    }

    fn n(&self) -> usize {
        self.qtb.len()
    }

    /// Largest `|J_j^T b| / (|J_j| |b|)` (the scaled gradient), `None` if not a number.
    fn max_a_t_b_scaled(&self, b_norm: f64) -> Option<f64> {
        let mut max = 0.0_f64;
        for j in 0..self.n() {
            let scale = self.column_norms[self.permutation[j]];
            if scale == 0.0 {
                continue;
            }
            let sum: f64 = (0..=j).map(|i| self.r[(i, j)] * self.qtb[i] / b_norm).sum();
            let temp = (sum / scale).abs();
            if temp.is_nan() {
                return None;
            }
            max = max.max(temp);
        }
        Some(max)
    }

    /// `|J x|`.
    fn a_x_norm(&self, x: &DVector<f64>) -> f64 {
        let mut out = DVector::<f64>::zeros(self.n());
        for (i, &index) in self.permutation.iter().enumerate() {
            for k in 0..=i {
                out[k] += x[index] * self.r[(k, i)];
            }
        }
        out.norm()
    }

    fn is_non_singular(&self) -> bool {
        self.max_rank == self.n() && (0..self.n()).all(|j| self.r[(j, j)] != 0.0)
    }

    /// The Gauss-Newton step (minimum norm on the leading nonsingular block of `R`).
    fn solve_with_zero_diagonal(&mut self) -> DVector<f64> {
        let n = self.n();
        let rank = (0..self.max_rank)
            .position(|i| self.r[(i, i)] == 0.0)
            .unwrap_or(self.max_rank);
        let mut w = self.qtb.clone();
        for v in w.iter_mut().skip(rank) {
            *v = 0.0;
        }
        for j in (0..rank).rev() {
            let sum: f64 = (j + 1..rank).map(|k| self.r[(j, k)] * w[k]).sum();
            w[j] = (w[j] - sum) / self.r[(j, j)];
        }
        let mut x = DVector::zeros(n);
        for j in 0..n {
            x[self.permutation[j]] = w[j];
        }
        x
    }

    /// Solves `R^T y = P^T rhs` (`lower` false) or `L y = P^T rhs` (`lower` true, after
    /// `solve_with_diagonal`); `y` stays in pivoted order.
    fn solve_factor(&self, rhs: &DVector<f64>, lower: bool) -> DVector<f64> {
        let n = self.n();
        let mut w = DVector::from_iterator(n, (0..n).map(|i| rhs[self.permutation[i]]));
        if lower {
            for j in 0..n {
                w[j] /= self.l_diag[j];
                let x = w[j];
                for i in j + 1..n {
                    w[i] -= x * self.r[(i, j)];
                }
            }
        } else {
            for j in 0..n {
                let sum: f64 = (0..j).map(|i| w[i] * self.r[(i, j)]).sum();
                w[j] = (w[j] - sum) / self.r[(j, j)];
            }
        }
        w
    }

    /// `R^T Q^T b` (pivoted order).
    fn r_t_qtb(&self) -> DVector<f64> {
        let n = self.n();
        DVector::from_iterator(
            n,
            (0..n).map(|i| (0..=i).map(|k| self.qtb[k] * self.r[(k, i)]).sum()),
        )
    }

    /// Minimizes `|J x - b|^2 + |diag x|^2`: Givens rotations eliminate `diag` into `L` (the
    /// lower triangle of `r` and `l_diag`; `R` is kept in the upper triangle).
    fn solve_with_diagonal(&mut self, diag: &DVector<f64>) -> DVector<f64> {
        let n = self.n();
        let mut rhs = self.qtb.clone();
        for j in 0..n {
            for i in j + 1..n {
                self.r[(i, j)] = self.r[(j, i)];
            }
            self.work[j] = self.r[(j, j)];
        }
        for j in 0..n {
            let entry = diag[self.permutation[j]];
            if entry != 0.0 {
                self.l_diag[j] = entry;
                for v in self.l_diag.iter_mut().skip(j + 1) {
                    *v = 0.0;
                }
                let mut qtbpj = 0.0;
                for k in j..n {
                    if self.l_diag[k] == 0.0 {
                        continue;
                    }
                    let r_kk = self.r[(k, k)];
                    let (sin, cos) = if r_kk.abs() < self.l_diag[k].abs() {
                        let cot = r_kk / self.l_diag[k];
                        let sin = 0.5 / (0.25 + 0.25 * cot * cot).sqrt();
                        (sin, sin * cot)
                    } else {
                        let tan = self.l_diag[k] / r_kk;
                        let cos = 0.5 / (0.25 + 0.25 * tan * tan).sqrt();
                        (cos * tan, cos)
                    };
                    self.r[(k, k)] = cos * r_kk + sin * self.l_diag[k];
                    let temp = cos * rhs[k] + sin * qtbpj;
                    qtbpj = -sin * rhs[k] + cos * qtbpj;
                    rhs[k] = temp;
                    for i in k + 1..n {
                        let r_ik = self.r[(i, k)];
                        let temp = cos * r_ik + sin * self.l_diag[i];
                        self.l_diag[i] = -sin * r_ik + cos * self.l_diag[i];
                        self.r[(i, k)] = temp;
                    }
                }
            }
            self.l_diag[j] = self.r[(j, j)];
            self.r[(j, j)] = self.work[j];
        }
        let rank = self.l_diag.iter().position(|v| *v == 0.0).unwrap_or(n);
        for v in rhs.iter_mut().skip(rank) {
            *v = 0.0;
        }
        for j in (0..rank).rev() {
            let dot: f64 = (j + 1..rank).map(|i| self.r[(i, j)] * rhs[i]).sum();
            rhs[j] = (rhs[j] - dot) / self.l_diag[j];
        }
        let mut x = DVector::zeros(n);
        for j in 0..n {
            x[self.permutation[j]] = rhs[j];
        }
        x
    }

    /// The step `p` with `|D p|` about `delta` (or the Gauss-Newton step inside it), its damping
    /// and `|D p|` (Moré's `lmpar`).
    fn step(
        &mut self,
        diag: &DVector<f64>,
        delta: f64,
        initial_lambda: f64,
    ) -> (DVector<f64>, f64, f64) {
        let n = self.n();
        let non_singular = self.is_non_singular();
        let mut p = self.solve_with_zero_diagonal();
        let mut diag_p = p.component_mul(diag);
        let mut diag_p_norm = diag_p.norm();
        let mut fp = diag_p_norm - delta;
        if fp <= 0.1 * delta {
            return (p, 0.0, diag_p_norm);
        }
        let newton = |this: &Self, diag_p: &DVector<f64>, diag_p_norm: f64, fp: f64, lower| {
            let direction = (diag_p / diag_p_norm).component_mul(diag);
            let norm = this.solve_factor(&direction, lower).norm();
            ((fp / delta) / norm) / norm
        };
        let mut lambda_lower = if non_singular {
            newton(self, &diag_p, diag_p_norm, fp, false)
        } else {
            0.0
        };
        let mut gradient = self.r_t_qtb();
        for j in 0..n {
            gradient[j] /= diag[self.permutation[j]];
        }
        let gnorm = gradient.norm();
        let mut lambda_upper = gnorm / delta;
        if lambda_upper == 0.0 {
            lambda_upper = f64::MIN_POSITIVE / delta.min(0.1);
        }
        let mut lambda = initial_lambda.max(lambda_lower).min(lambda_upper);
        if lambda == 0.0 {
            lambda = gnorm / diag_p_norm;
        }
        for iteration in 1.. {
            if lambda == 0.0 {
                lambda = f64::MIN_POSITIVE.max(0.001 * lambda_upper);
            }
            p = self.solve_with_diagonal(&(diag * lambda.sqrt()));
            diag_p = p.component_mul(diag);
            diag_p_norm = diag_p.norm();
            if iteration == 10 {
                break;
            }
            let fp_old = fp;
            fp = diag_p_norm - delta;
            if fp.abs() <= 0.1 * delta || (lambda_lower == 0.0 && fp <= fp_old && fp_old < 0.0) {
                break;
            }
            let correction = newton(self, &diag_p, diag_p_norm, fp, true);
            if fp > 0.0 {
                lambda_lower = lambda_lower.max(lambda);
            } else {
                lambda_upper = lambda_upper.min(lambda);
            }
            lambda = lambda_lower.max(lambda + correction);
        }
        (p, lambda, diag_p_norm)
    }
}

/// What `minimize` needs of a least-squares problem in parameters `u`; `None` stops it.
pub(super) trait Problem {
    /// `|f(u)|`.
    fn residual_norm(&mut self, u: &DVector<f64>) -> Option<f64>;
    /// The pivoted QR of the Jacobian at `u` with `Q^T f(u)` (one evaluation of both).
    fn reduced(&mut self, u: &DVector<f64>) -> Option<Reduced>;
}

/// Minimizes `|f(u)|` from `u` (the crate's `LevenbergMarquardt::minimize` with its default
/// tolerances and `patience`: at most `patience * (n + 1)` residual evaluations). Returns the last
/// accepted point; the caller may track a better one it evaluated.
pub(super) fn minimize(
    problem: &mut impl Problem,
    mut x: DVector<f64>,
    patience: usize,
) -> DVector<f64> {
    let n = x.len();
    let Some(mut fnorm) = problem.residual_norm(&x) else {
        return x;
    };
    if n == 0 || !fnorm.is_finite() || fnorm <= f64::MIN_POSITIVE {
        return x;
    }
    let max_evaluations = patience * (n + 1);
    let mut evaluations = 1;
    let mut diag = DVector::from_element(n, 1.0);
    let (mut delta, mut lambda, mut xnorm) = (0.0, 0.0, 0.0);
    let mut first_update = true;
    let mut first_iteration = true;
    loop {
        let Some(mut lls) = problem.reduced(&x) else {
            return x;
        };
        let Some(gnorm) = lls.max_a_t_b_scaled(fnorm) else {
            return x;
        };
        if gnorm <= TOLERANCE {
            return x;
        }
        if first_update {
            for (d, norm) in diag.iter_mut().zip(lls.column_norms.iter()) {
                *d = if *norm == 0.0 { 1.0 } else { *norm };
            }
            xnorm = diag.component_mul(&x).norm();
            if !xnorm.is_finite() {
                return x;
            }
            delta = if xnorm == 0.0 {
                STEP_BOUND
            } else {
                STEP_BOUND * xnorm
            };
            first_update = false;
        } else {
            for (d, norm) in diag.iter_mut().zip(lls.column_norms.iter()) {
                *d = norm.max(*d);
            }
        }
        loop {
            let (step, new_lambda, pnorm) = lls.step(&diag, delta, lambda);
            lambda = new_lambda;
            if !pnorm.is_finite() {
                return x;
            }
            let temp1 = (lls.a_x_norm(&step) / fnorm).powi(2);
            let temp2 = (lambda.sqrt() * pnorm / fnorm).powi(2);
            if !temp1.is_finite() || !temp2.is_finite() {
                return x;
            }
            let predicted = temp1 + temp2 / 0.5;
            let directional = -(temp1 + temp2);
            if first_iteration && pnorm < delta {
                delta = pnorm;
            }
            first_iteration = false;
            let trial = &x - &step;
            evaluations += 1;
            let Some(new_norm) = problem.residual_norm(&trial) else {
                return x;
            };
            let actual = if 0.1 * new_norm < fnorm {
                1.0 - (new_norm / fnorm).powi(2)
            } else {
                -1.0
            };
            let ratio = if predicted == 0.0 {
                0.0
            } else {
                actual / predicted
            };
            if ratio <= 0.25 {
                let mut temp = if actual >= 0.0 {
                    0.5
                } else {
                    0.5 * directional / (directional + 0.5 * actual)
                };
                if 0.1 * new_norm >= fnorm || temp < 0.1 {
                    temp = 0.1;
                }
                delta = temp * delta.min(pnorm * 10.0);
                lambda /= temp;
            } else if lambda == 0.0 || ratio >= 0.75 {
                delta = pnorm / 0.5;
                lambda *= 0.5;
            }
            let good = ratio >= 1e-4;
            if good {
                x = trial;
                xnorm = diag.component_mul(&x).norm();
                if !xnorm.is_finite() {
                    return x;
                }
                fnorm = new_norm;
            }
            let converged = fnorm <= f64::MIN_POSITIVE
                || (actual.abs() <= TOLERANCE && predicted <= TOLERANCE && ratio * 0.5 <= 1.0)
                || delta <= TOLERANCE * xnorm;
            let stuck =
                (actual.abs() <= f64::EPSILON && predicted <= f64::EPSILON && ratio * 0.5 <= 1.0)
                    || delta <= f64::EPSILON * xnorm
                    || gnorm <= f64::EPSILON;
            if converged || evaluations >= max_evaluations || stuck {
                return x;
            }
            if good {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows added in pieces of any size (with zero rows and refolds of the triangle) give the QR of
    // the whole matrix: `R^T R = A^T A` and `R^T Q^T b = A^T b`.
    #[test]
    fn folding_row_blocks_gives_the_qr_of_the_stacked_rows() {
        let (rows, n) = (1500, 7);
        let mut state = 12345_u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        let mut a = DMatrix::from_fn(rows, n, |_, _| next());
        for i in (0..rows).step_by(3) {
            a.row_mut(i).fill(0.0);
        }
        let b = DVector::from_fn(rows, |_, _| next());
        let mut folder = Folder::new(n);
        let mut start = 0;
        for size in [1, 2, 300, 7, 600, 590] {
            let block = a.rows(start, size).into_owned();
            folder.add(block.as_slice(), b.rows(start, size).as_slice());
            start += size;
        }
        assert_eq!(start, rows);
        let triangle = folder.finish().unwrap();
        let reduced = Reduced::new(&[triangle], &[], &[], n, rows);
        let (r, qtb) = reduced.unpivoted();
        let information = a.tr_mul(&a);
        assert!((r.tr_mul(&r) - &information).amax() < 1e-11 * information.amax());
        let gradient = a.tr_mul(&b);
        assert!((r.tr_mul(&qtb) - &gradient).amax() < 1e-11 * gradient.amax());
    }
}
