//! Which shots of a synthetic scenario capture are baseline observations and which is the trial.
//!
//! The roles come from what the scenario generator recorded (capture metadata `scenario.params`
//! lists every training launch and the trial launch; decisions carry stage names), so they
//! survive regenerating the scenarios. Live captures have no roles.
use super::*;

struct Launch {
    x: f64,
    y: f64,
    angle: f64,
    speed: f64,
}

impl Launch {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            x: value["launch_x"].as_f64()?,
            y: value["launch_y"].as_f64()?,
            angle: value["angle_degrees"].as_f64()?,
            speed: value["speed"].as_f64()?,
        })
    }

    fn matches(&self, shot: &Value) -> bool {
        let first = &shot["points"][0];
        shot["angle"]
            .as_f64()
            .is_some_and(|angle| (angle - self.angle).abs() < 1e-6)
            && shot["velocity"]
                .as_f64()
                .is_some_and(|speed| (speed - self.speed).abs() < 1e-6)
            && first["x"]
                .as_f64()
                .is_some_and(|x| (x - self.x).abs() < 0.5)
            && first["y"]
                .as_f64()
                .is_some_and(|y| (y - self.y).abs() < 0.5)
    }
}

pub struct ShotRoles {
    /// Training launches the scenario recorded, in order. (The recorded `trial` launch is the
    /// target witness, not the shot the policy then fired, so it identifies nothing.)
    training: Vec<Launch>,
    /// `(decision sequence, stage)` pairs: the policy's trial shot is the one after `target`.
    stages: Vec<(i64, String)>,
}

impl ShotRoles {
    /// Roles for `capture_id`; empty (no labels) for anything but a synthetic scenario.
    pub fn load(connection: &Connection, capture_id: i64) -> Result<Self, String> {
        let metadata: String = connection
            .query_row(
                "SELECT metadata_json FROM captures WHERE id=?1",
                [capture_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        let mut roles = Self {
            training: Vec::new(),
            stages: Vec::new(),
        };
        if metadata["host"] != "synthetic-scenario" {
            return Ok(roles);
        }
        let params = &metadata["scenario"]["params"];
        roles.training = params["training_paths"]
            .as_array()
            .map(|paths| paths.iter().filter_map(Launch::from_json).collect())
            .unwrap_or_default();
        {
            let mut statement = connection
                .prepare("SELECT sequence, json_extract(data_json, '$.scenario.stage') FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([capture_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|error| error.to_string())?;
            for row in rows {
                if let (sequence, Some(stage)) = row.map_err(|error| error.to_string())? {
                    roles.stages.push((sequence, stage));
                }
            }
        }
        Ok(roles)
    }

    /// "training 2", "trial", or `None` for a shot that is neither (or any live shot or non-shot).
    pub fn label(&self, sequence: i64, shot: &Value) -> Option<String> {
        if shot["type"] != "shot_fin" {
            return None;
        }
        // The shot right after the `target` decision is the policy's trial, even when it repeats
        // a training launch.
        let latest_stage = self
            .stages
            .iter()
            .rev()
            .find(|(decision, _)| *decision < sequence)
            .map(|(_, stage)| stage.as_str());
        if latest_stage == Some("target") {
            return Some("trial".to_owned());
        }
        if let Some(index) = self.training.iter().position(|launch| launch.matches(shot)) {
            return Some(format!("training {}", index + 1));
        }
        // Stage names (older captures have no recorded launches): a shot belongs to the
        // `training_N` decision that follows it.
        self.stages
            .iter()
            .find(|(decision, stage)| *decision > sequence && stage.starts_with("training_"))
            .map(|(_, stage)| stage.replace('_', " "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(x: f64, y: f64, angle: f64, speed: f64) -> Value {
        json!({"type": "shot_fin", "angle": angle, "velocity": speed, "points": [{"x": x, "y": y}]})
    }

    #[test]
    fn recorded_launches_label_training_shots_and_stages_label_the_trial() {
        let roles = ShotRoles {
            training: vec![
                Launch {
                    x: 200.0,
                    y: 500.0,
                    angle: 0.0,
                    speed: 8.0,
                },
                Launch {
                    x: 600.0,
                    y: 100.0,
                    angle: -90.0,
                    speed: 8.0,
                },
            ],
            stages: vec![(9, "training_2".into()), (21, "target".into())],
        };
        assert_eq!(
            roles.label(4, &shot(600.0, 100.0, -90.0, 8.0)).as_deref(),
            Some("training 2")
        );
        // The policy's shot is not any recorded launch; it follows the `target` decision.
        assert_eq!(
            roles.label(22, &shot(200.0, 350.0, -30.0, 12.0)).as_deref(),
            Some("trial")
        );
        assert_eq!(
            roles.label(3, &shot(1.0, 1.0, 0.0, 8.0)).as_deref(),
            Some("training 2")
        );
    }

    #[test]
    fn stage_names_label_older_captures() {
        let roles = ShotRoles {
            training: Vec::new(),
            stages: vec![
                (5, "training_1".into()),
                (9, "training_2".into()),
                (21, "target".into()),
            ],
        };
        assert_eq!(
            roles.label(4, &shot(0.0, 0.0, 0.0, 1.0)).as_deref(),
            Some("training 1")
        );
        assert_eq!(
            roles.label(8, &shot(0.0, 0.0, 0.0, 1.0)).as_deref(),
            Some("training 2")
        );
        assert_eq!(
            roles.label(22, &shot(0.0, 0.0, 0.0, 1.0)).as_deref(),
            Some("trial")
        );
    }
}
