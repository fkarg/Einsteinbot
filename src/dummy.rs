use crate::protocol::ClientCommand;
use rand::RngExt;

pub fn commands_for_energy<R: RngExt + ?Sized>(
    energy: f64,
    rng: &mut R,
) -> Option<[ClientCommand; 2]> {
    let maximum_velocity = maximum_velocity(energy)?;
    let velocity = rng.random_range(2..=maximum_velocity);
    let angle = rng.random_range(-180..=180);
    commands_for_samples(energy, velocity, angle)
}

fn maximum_velocity(energy: f64) -> Option<i32> {
    if !energy.is_finite() || energy < 2.0 {
        return None;
    }
    Some(energy.floor().min(8.0) as i32)
}

fn commands_for_samples(energy: f64, velocity: i32, angle: i32) -> Option<[ClientCommand; 2]> {
    let maximum_velocity = maximum_velocity(energy)?;
    if !(2..=maximum_velocity).contains(&velocity) || !(-180..=180).contains(&angle) {
        return None;
    }
    Some([
        ClientCommand::SetVelocity {
            velocity: velocity as f64,
        },
        ClientCommand::SetAngle {
            angle: angle as f64,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_energy_below_two_or_non_finite() {
        assert!(commands_for_samples(1.99, 2, 0).is_none());
        assert!(commands_for_samples(f64::NAN, 2, 0).is_none());
        assert!(commands_for_samples(f64::INFINITY, 2, 0).is_none());
    }

    #[test]
    fn commands_obey_energy_velocity_and_angle_bounds() {
        assert!(commands_for_samples(3.9, 2, -180).is_some());
        assert!(commands_for_samples(3.9, 3, 180).is_some());
        assert!(commands_for_samples(3.9, 4, 0).is_none());
        assert!(commands_for_samples(100.0, 8, 0).is_some());
        assert!(commands_for_samples(100.0, 9, 0).is_none());
        assert!(commands_for_samples(5.0, 2, 181).is_none());
    }
}
