//! Background audit of the maintained map: a from-scratch map (`rebuild::from_scratch`) on a
//! snapshot of the paths, adopted only if its evidence beats the maintained map's on every row.
//! It runs on its own thread, so it never delays the publication of a map; the estimator adopts a
//! finished audit between slices.
use super::Shot;
use super::fit::{Budget, Fit};
use super::rebuild::from_scratch;
use super::search::{LEVELS, refit};
use crate::simulation::WorldParameters;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// Evaluations of the maintained map's refit on all rows before the comparison.
const MAINTAINED_REFIT_EVALUATIONS: u64 = 1500;
/// The first audit runs at this many paths, then every `AUDIT_EVERY`.
const FIRST_AUDIT: usize = 8;
const AUDIT_EVERY: usize = 16;

/// Whether an audit starts when the `paths`-th path has arrived.
pub(super) fn due(paths: usize) -> bool {
    paths >= FIRST_AUDIT && (paths - FIRST_AUDIT) % AUDIT_EVERY == 0
}

pub(super) struct Audit {
    thread: Option<JoinHandle<Option<Vec<[f64; 3]>>>>,
    cancel: Arc<AtomicBool>,
}

impl Audit {
    /// Start auditing `maintained` on the paths `shots` (exact, full rows).
    pub(super) fn spawn(
        shots: Vec<Shot>,
        maintained: Vec<[f64; 3]>,
        max_planets: usize,
        threads: usize,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let thread = std::thread::spawn(move || {
            let cancelled = &*flag;
            let candidate = from_scratch(&shots, max_planets, threads, cancelled)?;
            // Both maps on all rows at the finest relative level, the maintained one refit there
            // first (the candidate's polish already ran at that level).
            let world = WorldParameters::default();
            let level = LEVELS[LEVELS.len() - 1];
            let rows: Vec<Shot> = shots
                .iter()
                .map(|shot| shot.at_relative_level(world, level))
                .collect();
            let joint = Fit::new(&rows, world, cancelled, true, vec![]);
            let budget = Budget {
                evaluations: MAINTAINED_REFIT_EVALUATIONS,
                deadline: None,
                threads,
            };
            let maintained = refit(&joint, &maintained, &budget)
                .map(|mode| mode.planets.iter().map(super::fit::theta_of).collect())
                .unwrap_or(maintained);
            let evidence = |theta: &[[f64; 3]]| joint.evidence(theta).map(|e| e.log_evidence);
            let better = match (evidence(&maintained), evidence(&candidate)) {
                (Some(old), Some(new)) => new > old,
                (None, Some(_)) => true,
                _ => false,
            };
            (better && !cancelled.load(Ordering::Relaxed)).then_some(candidate)
        });
        Self {
            thread: Some(thread),
            cancel,
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Wait for the audit; `Some` is a map to adopt.
    pub(super) fn join(mut self) -> Option<Vec<[f64; 3]>> {
        let thread = self.thread.take().expect("joined once");
        thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
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
