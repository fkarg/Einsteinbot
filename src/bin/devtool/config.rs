//! Evaluation configuration: estimator settings and targeting constants, loadable from JSON.
//! Missing keys take the production defaults; unknown keys are errors.
use einsteinbot::estimator::Settings;
use einsteinbot::strategy::TargetingSettings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub estimator: Settings,
    pub targeting: TargetingSettings,
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    /// The effective configuration (defaults filled in) as canonical JSON.
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("config serializes")
    }

    /// FNV-1a 64 of the canonical JSON: the same effective configuration has the same hash
    /// whether its values were written out or defaulted.
    pub fn hash(&self) -> String {
        format!("{:016x}", fnv1a(self.canonical().as_bytes()))
    }
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hash_like_an_empty_file_and_unknown_keys_fail() {
        let empty: Config = serde_json::from_str("{}").unwrap();
        let explicit: Config =
            serde_json::from_str(r#"{"estimator": {"slices_per_path": 6}}"#).unwrap();
        assert_eq!(empty.hash(), Config::default().hash());
        assert_eq!(explicit.hash(), Config::default().hash());
        let changed: Config =
            serde_json::from_str(r#"{"estimator": {"search": {"row_cap": 1500}}}"#).unwrap();
        assert_ne!(changed.hash(), Config::default().hash());
        assert_eq!(changed.estimator.search.row_cap, 1500);
        for bad in [
            r#"{"estimator": {"slices": 6}}"#,
            r#"{"estimator": {"search": {"rowcap": 1}}}"#,
            r#"{"targeting": {"doom": 0.2}}"#,
            r#"{"other": {}}"#,
        ] {
            assert!(serde_json::from_str::<Config>(bad).is_err(), "{bad}");
        }
        let ceiling: Config =
            serde_json::from_str(r#"{"estimator": {"slice_ceiling_ms": null}}"#).unwrap();
        assert_eq!(ceiling.estimator.slice_ceiling, None);
    }
}
