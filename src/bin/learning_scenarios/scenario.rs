use super::*;
use einsteinbot::event_store::{EventLog, ScenarioCaptureMetadata};
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::strategy::{BotState, MapBelief};

#[derive(Clone, Copy)]
pub(super) struct Launch {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) angle: i32,
    pub(super) speed: i32,
}
#[derive(Clone)]
pub(super) struct Scenario {
    pub(super) name: &'static str,
    pub(super) family: &'static str,
    pub(super) defect_class: &'static str,
    /// Truth-map planet indices this decision is designed to clarify or evaluate.
    pub(super) truth_intended_planet_indices: &'static [usize],
    pub(super) truth: PlanetMap,
    pub(super) observations: Vec<(Launch, Termination)>,
    pub(super) trial: Launch,
    pub(super) target_tick: usize,
}

pub(super) struct ScenarioCapture {
    log: EventLog,
    timestamp_ms: i64,
}

impl ScenarioCapture {
    pub(super) fn event(&mut self, event: &ServerEvent, bot: Option<&str>) {
        self.log
            .append_for_bot(
                self.timestamp_ms,
                bot,
                &RecordedEvent::Inbound(event.clone()),
            )
            .expect("write scenario event");
        self.timestamp_ms += 1;
    }

    pub(super) fn decision(
        &mut self,
        scenario: &Scenario,
        state: &BotState,
        belief: Option<&MapBelief>,
        choice: Option<einsteinbot::strategy::ShotChoice>,
        stage: &str,
        observations_seen: usize,
        fit_ms: Option<u128>,
    ) {
        let belief_json = belief.map(|belief| serde_json::json!({
            "planets": belief.map.planets,
            "local_field": belief.local_field,
            "field_hypothesis": belief.field_hypothesis,
            "hypothesis_local_field": belief.hypothesis_local_field,
            "contact_points": belief.contact_points,
            "covariance": belief.parameter_covariance.as_ref().map(|matrix| {
                (0..matrix.nrows()).map(|row| (0..matrix.ncols()).map(|col| matrix[(row, col)]).collect::<Vec<_>>()).collect::<Vec<_>>()
            }),
            "covariance_unavailable_reason": belief.covariance_unavailable_reason,
            "fit_rank": belief.fit_rank,
            "fit_parameter_count": belief.fit_parameter_count,
            "fit_independent_path_count": belief.fit_independent_path_count,
            "field_evidence": belief.field_evidence.as_ref().map(|evidence| serde_json::json!({
                "resolution": (0..evidence.resolution.nrows()).map(|row| (0..evidence.resolution.ncols()).map(|column| evidence.resolution[(row, column)]).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "parameter_to_physical": (0..evidence.parameter_to_physical.nrows()).map(|row| (0..evidence.parameter_to_physical.ncols()).map(|column| evidence.parameter_to_physical[(row, column)]).collect::<Vec<_>>()).collect::<Vec<_>>(),
            })),
            "model_position_rmse": belief.model_position_rmse,
            "fit_converged": belief.fit_converged,
            "planet_contact_counts": belief.planet_contact_counts,
            "uncertain_associations": belief.uncertain_associations,
        }));
        let choice_json = choice.map(|choice| {
            serde_json::json!({
                "reason": format!("{:?}", choice.reason),
                "velocity": choice.velocity,
                "angle_degrees": choice.angle_degrees,
                "miss_distance": choice.miss_distance,
                "predicted_hit": choice.predicted_hit,
                "predicted_victim": choice.predicted_victim,
                "used_field_hypothesis": choice.used_field_hypothesis,
                "contact_before_target": choice.contact_before_target,
            })
        });
        self.log
            .append_diagnostic(
                self.timestamp_ms,
                "scenario",
                "target_decision",
                &serde_json::json!({
                    "energy": 20.0,
                    "map_updating": false,
                    "map_epoch": 0,
                    "map_submitted_watermark": observations_seen,
                    "map_processed_watermark": observations_seen,
                    "map_pending_events": 0,
                    "map_source": "online",
                    "fit_ms": fit_ms,
                    "map_belief": belief_json,
                    "bot_state": state.diagnostic_snapshot(),
                    "scenario": scenario.diagnostic_metadata(stage),
                    "choice": choice_json,
                }),
            )
            .expect("write scenario diagnostic");
        self.timestamp_ms += 1;
    }

    pub(super) fn finish(mut self) {
        self.log
            .finish(self.timestamp_ms)
            .expect("finish scenario capture");
    }
}

impl Scenario {
    pub(super) fn capture_metadata(&self) -> ScenarioCaptureMetadata {
        let contacted_planets = self
            .observations
            .iter()
            .filter_map(|(_, termination)| match termination {
                Termination::PlanetCollision(index) => Some(*index),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        ScenarioCaptureMetadata {
            name: self.name.into(),
            family: self.family.into(),
            defect_class: self.defect_class.into(),
            params: serde_json::json!({
                "truth_planet_count": self.truth.planets.len(),
                "training_path_count": self.observations.len(),
                "contacted_planet_indices": contacted_planets,
                "training_paths": self.observations.iter().map(|(launch, termination)| {
                    serde_json::json!({
                        "launch_x": launch.x,
                        "launch_y": launch.y,
                        "angle_degrees": launch.angle,
                        "speed": launch.speed,
                        "expected_termination": match termination {
                            Termination::PlanetCollision(index) => serde_json::json!({"planet_collision": index}),
                            Termination::PlayerCollision(player_id) => serde_json::json!({"player_collision": player_id}),
                            Termination::BattlefieldMargin => serde_json::json!("battlefield_margin"),
                            Termination::SampleLimit => serde_json::json!("sample_limit"),
                        },
                    })
                }).collect::<Vec<_>>(),
                "trial": {
                    "launch_x": self.trial.x,
                    "launch_y": self.trial.y,
                    "angle_degrees": self.trial.angle,
                    "speed": self.trial.speed,
                    "target_tick": self.target_tick,
                },
            }),
            // These fixtures are hand-authored and deterministic; the original source
            // did not record an objective difficulty ranking.
            seed: if self.family == "W7" {
                Some(super::w7::SEED)
            } else {
                None
            },
            difficulty: None,
        }
    }

    pub(super) fn diagnostic_metadata(&self, stage: &str) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "stage": stage,
            "intended": self.truth_intended_planet_indices,
            "truth_planets": self.truth.planets,
        })
    }
}

impl ScenarioCapture {
    pub(super) fn create(path: &std::path::Path, scenario: &Scenario) -> Result<Self, String> {
        use einsteinbot::event_store::CaptureMetadata;
        use std::time::{SystemTime, UNIX_EPOCH};

        let started_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis() as i64;
        let metadata = CaptureMetadata {
            started_at_unix_ms,
            process_id: std::process::id(),
            host: "synthetic-scenario".into(),
            port: 0,
            client_name: scenario.name.into(),
            protocol_version: 8,
        };
        let scenario_metadata = scenario.capture_metadata();
        let log = EventLog::create_with_scenario_metadata(path, &metadata, &scenario_metadata)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            log,
            timestamp_ms: started_at_unix_ms,
        })
    }
}
