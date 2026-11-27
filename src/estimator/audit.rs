//! Background audit of the maintained map: a from-scratch map (`rebuild::from_scratch`) on a
//! snapshot of the paths, adopted only if its evidence beats the maintained map's on all the rows
//! known when the rebuild is done, under the noise of that moment. Both halves (the rebuild, then
//! the comparison) run on their own threads, so an audit never delays the publication of a map;
//! the estimator collects a finished half between slices.
use super::Shot;
use super::fit::{Budget, Fit, any_overlap, theta_of};
use super::noise::{Noise, with_noise};
use super::rebuild::from_scratch;
use super::search::refit;
use crate::simulation::WorldParameters;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// Evaluations of each map's refit on all rows before the comparison.
const REFIT_EVALUATIONS: u64 = 1500;
/// The first audit runs at this many paths, then every `AUDIT_EVERY`.
const FIRST_AUDIT: usize = 8;
const AUDIT_EVERY: usize = 16;

/// Whether an audit starts when the `paths`-th path has arrived.
pub(super) fn due(paths: usize) -> bool {
    paths >= FIRST_AUDIT && (paths - FIRST_AUDIT) % AUDIT_EVERY == 0
}

/// Which half of the audit a thread runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// The from-scratch map (minutes).
    Rebuild,
    /// The comparison with the maintained map on the paths known now (seconds).
    Compare,
}

pub(super) struct Audit {
    pub stage: Stage,
    thread: Option<JoinHandle<Option<Vec<[f64; 3]>>>>,
    cancel: Arc<AtomicBool>,
}

impl Audit {
    /// Start building a map from scratch on the paths `shots` (floor noise, full rows) under
    /// `noise`. The result is compared with the maintained map by `compare`, on the paths known
    /// when it is done: a map built from the paths of minutes ago must not be compared (or
    /// adopted) on those.
    pub(super) fn rebuild(
        shots: Vec<Shot>,
        noise: Noise,
        max_planets: usize,
        threads: usize,
    ) -> Self {
        Self::spawn(Stage::Rebuild, move |cancelled| {
            from_scratch(&shots, &noise, max_planets, threads, cancelled)
        })
    }

    /// Start comparing `candidate` with `maintained` on all of `shots` under `noise` (both maps
    /// see the same noise): both refit on all rows, and the result is the refit candidate if its
    /// evidence is higher.
    pub(super) fn compare(
        shots: Vec<Shot>,
        noise: Noise,
        maintained: Vec<[f64; 3]>,
        candidate: Vec<[f64; 3]>,
        threads: usize,
    ) -> Self {
        Self::spawn(Stage::Compare, move |cancelled| {
            let world = WorldParameters::default();
            let rows = with_noise(&shots, &noise);
            let joint = Fit::new(&rows, world, cancelled, true, vec![]);
            let budget = Budget {
                evaluations: REFIT_EVALUATIONS,
                deadline: None,
                threads,
            };
            let refit_of = |start: Vec<[f64; 3]>| {
                refit(&joint, &start, &budget)
                    .map(|mode| mode.planets.iter().map(theta_of).collect())
                    .unwrap_or(start)
            };
            let (maintained, candidate) = (refit_of(maintained), refit_of(candidate));
            let evidence = |theta: &[[f64; 3]]| joint.evidence(theta).map(|e| e.log_evidence);
            let better = match (evidence(&maintained), evidence(&candidate)) {
                (Some(old), Some(new)) => new > old,
                (None, Some(_)) => true,
                _ => false,
            };
            // `refit_of` falls back to the unrefit start, which must not overlap either.
            (better && !any_overlap(&candidate) && !cancelled.load(Ordering::Relaxed))
                .then_some(candidate)
        })
    }

    fn spawn(
        stage: Stage,
        work: impl FnOnce(&AtomicBool) -> Option<Vec<[f64; 3]>> + Send + 'static,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        Self {
            stage,
            thread: Some(std::thread::spawn(move || work(&flag))),
            cancel,
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Wait for the audit; `Some` is a map to adopt.
    pub(super) fn join(mut self) -> Option<Vec<[f64; 3]>> {
        let thread = self.thread.take().expect("joined once");
        thread.join().unwrap_or_else(|_| {
            eprintln!("estimator: the audit panicked; the maintained map stays");
            None
        })
    }
}

impl Drop for Audit {
    /// Dropping an audit (reset, shutdown) cancels it; the thread ends at its next check.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audits_start_at_eight_paths_and_then_every_sixteen() {
        let due_at: Vec<usize> = (1..=60).filter(|n| due(*n)).collect();
        assert_eq!(due_at, vec![8, 24, 40, 56]);
    }
}
