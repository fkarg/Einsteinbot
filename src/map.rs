use crate::protocol::Planet;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn norm_squared(self) -> f64 {
        self.x * self.x + self.y * self.y
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix2 {
    pub xx: f64,
    pub xy: f64,
    pub yx: f64,
    pub yy: f64,
}

impl Default for Matrix2 {
    fn default() -> Self {
        Self {
            xx: 0.0,
            xy: 0.0,
            yx: 0.0,
            yy: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tensor2 {
    /// `values[component][first_derivative][second_derivative]`.
    pub values: [[[f64; 2]; 2]; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanetMap {
    pub planets: Vec<Planet>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SingularFieldError {
    pub planet_index: usize,
}

impl std::fmt::Display for SingularFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "field is singular at planet {} center",
            self.planet_index
        )
    }
}

impl std::error::Error for SingularFieldError {}

impl PlanetMap {
    pub fn acceleration(&self, position: Vec2) -> Result<Vec2, SingularFieldError> {
        let mut result = Vec2::default();
        for (planet_index, planet) in self.planets.iter().enumerate() {
            let dx = planet.x - position.x;
            let dy = planet.y - position.y;
            let distance_squared = dx * dx + dy * dy;
            if distance_squared == 0.0 {
                return Err(SingularFieldError { planet_index });
            }
            let scale = planet.mass / (distance_squared * distance_squared.sqrt());
            result.x += scale * dx;
            result.y += scale * dy;
        }
        Ok(result)
    }

    /// Spatial Jacobian of acceleration: row is output component, column is position axis.
    pub fn acceleration_jacobian(&self, position: Vec2) -> Result<Matrix2, SingularFieldError> {
        let mut result = Matrix2::default();
        for (planet_index, planet) in self.planets.iter().enumerate() {
            let dx = planet.x - position.x;
            let dy = planet.y - position.y;
            let distance_squared = dx * dx + dy * dy;
            if distance_squared == 0.0 {
                return Err(SingularFieldError { planet_index });
            }
            let inverse_r3 = 1.0 / (distance_squared * distance_squared.sqrt());
            let inverse_r5 = inverse_r3 / distance_squared;
            let mass = planet.mass;
            result.xx += mass * (-inverse_r3 + 3.0 * dx * dx * inverse_r5);
            result.xy += mass * (3.0 * dx * dy * inverse_r5);
            result.yx += mass * (3.0 * dy * dx * inverse_r5);
            result.yy += mass * (-inverse_r3 + 3.0 * dy * dy * inverse_r5);
        }
        Ok(result)
    }

    /// Second spatial derivative of acceleration, indexed as [component][axis][axis].
    pub fn acceleration_hessian(&self, position: Vec2) -> Result<Tensor2, SingularFieldError> {
        let mut result = Tensor2 {
            values: [[[0.0; 2]; 2]; 2],
        };
        for (planet_index, planet) in self.planets.iter().enumerate() {
            let d = [planet.x - position.x, planet.y - position.y];
            let distance_squared = d[0] * d[0] + d[1] * d[1];
            if distance_squared == 0.0 {
                return Err(SingularFieldError { planet_index });
            }
            let inverse_r5 = 1.0 / (distance_squared * distance_squared.sqrt() * distance_squared);
            for component in 0..2 {
                for first_axis in 0..2 {
                    for second_axis in 0..2 {
                        let delta_if = f64::from(component == first_axis);
                        let delta_is = f64::from(component == second_axis);
                        let delta_fs = f64::from(first_axis == second_axis);
                        let derivative = planet.mass
                            * (-3.0 * delta_if * d[second_axis]
                                - 3.0 * delta_is * d[first_axis]
                                - 3.0 * d[component] * delta_fs
                                + 15.0 * d[component] * d[first_axis] * d[second_axis]
                                    / distance_squared);
                        result.values[component][first_axis][second_axis] +=
                            derivative * inverse_r5;
                    }
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planet(x: f64, y: f64, radius: f64, mass: f64) -> Planet {
        Planet { x, y, radius, mass }
    }

    #[test]
    fn acceleration_and_spatial_derivatives_match_point_source_law() {
        let map = PlanetMap {
            planets: vec![planet(0.0, 0.0, 1.0, 8.0)],
        };
        let position = Vec2::new(2.0, 0.0);
        let acceleration = map.acceleration(position).unwrap();
        let jacobian = map.acceleration_jacobian(position).unwrap();

        assert!((acceleration.x + 2.0).abs() < 1e-12);
        assert!(acceleration.y.abs() < 1e-12);
        assert!((jacobian.xx - 2.0).abs() < 1e-12);
        assert!(jacobian.xy.abs() < 1e-12);

        let h = 1e-4;
        let x_plus = map
            .acceleration(Vec2::new(position.x + h, position.y))
            .unwrap();
        let x_minus = map
            .acceleration(Vec2::new(position.x - h, position.y))
            .unwrap();
        assert!((((x_plus.x - x_minus.x) / (2.0 * h)) - jacobian.xx).abs() < 1e-7);

        let hessian = map.acceleration_hessian(position).unwrap();
        let jac_plus = map
            .acceleration_jacobian(Vec2::new(position.x + h, position.y))
            .unwrap();
        let jac_minus = map
            .acceleration_jacobian(Vec2::new(position.x - h, position.y))
            .unwrap();
        let numeric_second = (jac_plus.xx - jac_minus.xx) / (2.0 * h);
        assert!((numeric_second - hessian.values[0][0][0]).abs() < 1e-6);
        let numeric_mixed = (jac_plus.xy - jac_minus.xy) / (2.0 * h);
        assert!((numeric_mixed - hessian.values[0][0][1]).abs() < 1e-6);
    }
}
