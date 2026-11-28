//! Tunable constants of the live planet search (`search.rs`), so offline tools can load them from
//! a configuration file (`devtool eval --config`). The defaults are the production values; the
//! from-scratch cascade of the audit (`rebuild.rs`) keeps the defaults.
use super::search;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchTuning {
    /// Windows less significant than this (whitened magnitude) never seed a planet.
    pub seed_significance: f64,
    /// Seeds (window positions) taken per seeding round.
    pub max_seeds: usize,
    /// Candidates kept between rounds (new seeds first, then the best fitted).
    pub max_candidates: usize,
    /// Best distinct candidates that go on to a joint refit.
    pub joint_refits: usize,
    /// Weakest planets tried as deaths after a birth (and at the cap).
    pub death_trials: usize,
    /// Rows kept by the live search's thinning.
    pub row_cap: usize,
    /// A new path's local refit frees the planets within this distance of its rows.
    pub local_radius: f64,
    /// Evaluations of a new path's local refit.
    pub local_evaluations: u64,
    /// A joint partial still improving after this many slices of work counts as finished.
    pub max_rounds: usize,
}

impl Default for SearchTuning {
    fn default() -> Self {
        Self {
            seed_significance: search::SEED_SIGNIFICANCE,
            max_seeds: search::MAX_SEEDS,
            max_candidates: search::MAX_CANDIDATES,
            joint_refits: search::JOINT_REFITS,
            death_trials: search::DEATH_TRIALS,
            row_cap: search::ROW_CAP,
            local_radius: search::LOCAL_RADIUS,
            local_evaluations: search::LOCAL_EVALUATIONS,
            max_rounds: search::MAX_ROUNDS,
        }
    }
}

/// `Option<Duration>` as whole milliseconds (`null`: none).
pub(super) mod optional_ms {
    use super::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(d) => s.serialize_some(&(d.as_millis() as u64)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
    }
}
