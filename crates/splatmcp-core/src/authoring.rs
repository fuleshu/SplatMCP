//! Deterministic splat builders.
//!
//! Everything here is a pure function of its parameters: the same request produces the
//! same splat, byte for byte, because scatter uses a seeded generator with a fixed
//! algorithm rather than the platform's random source. That is what makes an MCP tool
//! call reproducible.
//!
//! Units follow the model in [`crate`]: positions in metres, `scale` is the activated
//! radius in metres, colours are linear RGB in `0..=1`, and opacity is `0..=1`.

use crate::{Result, Splat, SplatError, SplatPoint};

/// A seeded, dependency-free RNG (SplitMix64).
///
/// Chosen over a general random crate because the stream must never change: a caller
/// asking for the same seed twice expects the same splat, including across releases.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    /// Next 64 raw bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        // 24 bits is exactly the mantissa of an f32.
        ((self.next_u64() >> 40) as f32) / (1u32 << 24) as f32
    }

    /// Uniform in `[min, max)`.
    pub fn range(&mut self, min: f32, max: f32) -> f32 {
        min + (max - min) * self.unit()
    }

    /// Uniform in `[-spread, spread]`.
    pub fn jitter(&mut self, spread: f32) -> f32 {
        self.range(-spread, spread)
    }

    /// Random unit-length quaternion.
    pub fn quaternion(&mut self) -> [f32; 4] {
        // Shoemake's method: three uniforms give a uniform orientation.
        let u1 = self.unit();
        let u2 = self.unit();
        let u3 = self.unit();
        let two_pi = std::f32::consts::TAU;
        let (r1, r2) = ((1.0 - u1).sqrt(), u1.sqrt());
        let (t1, t2) = (two_pi * u2, two_pi * u3);
        [
            r2 * (t2 * 0.5).cos(),
            r1 * (t1 * 0.5).sin(),
            r1 * (t1 * 0.5).cos(),
            r2 * (t2 * 0.5).sin(),
        ]
    }
}

/// Shape of a generated splat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Uniformly filled ball of radius `size`.
    Sphere,
    /// Uniformly filled cube with edge `2 * size`.
    Cube,
    /// Flat square sheet in the XZ plane.
    Plane,
    /// Points spread evenly along one axis.
    Line,
    /// Points on the surface of a ball, for a hollow look.
    Shell,
    /// Ring in the XZ plane.
    Ring,
    /// Rectangular grid of points in the XZ plane.
    Grid,
}

impl Shape {
    /// Parses a tool's shape name, case-insensitively.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "sphere" | "ball" | "cloud" => Self::Sphere,
            "cube" | "box" => Self::Cube,
            "plane" | "sheet" | "floor" => Self::Plane,
            "line" | "axis" => Self::Line,
            "shell" | "hollow_sphere" => Self::Shell,
            "ring" | "torus" => Self::Ring,
            "grid" => Self::Grid,
            _ => return None,
        })
    }

    /// Names accepted by [`Shape::parse`], for error messages and docs.
    pub const NAMES: [&'static str; 7] =
        ["sphere", "cube", "plane", "line", "shell", "ring", "grid"];
}

/// Parameters of a generated splat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplatParams {
    pub shape: Shape,
    /// Number of gaussians to place.
    pub count: usize,
    /// Centre of the shape in world metres.
    pub center: [f32; 3],
    /// Half extent of the shape in metres.
    pub size: f32,
    /// Linear RGB in `0..=1`.
    pub color: [f32; 3],
    /// Opacity in `0..=1`.
    pub opacity: f32,
    /// Gaussians per axis for [`Shape::Grid`].
    pub grid: usize,
    /// Position jitter as a fraction of `size`.
    pub jitter: f32,
    /// Gaussian radius in metres.
    pub radius: f32,
    /// Hue variation in `0..=1`, applied per channel.
    pub color_variation: f32,
    /// Seed of the generator.
    pub seed: u64,
    /// Give each point a random orientation; otherwise they are axis aligned.
    pub random_rotation: bool,
}

impl Default for SplatParams {
    fn default() -> Self {
        Self {
            shape: Shape::Sphere,
            count: 1000,
            center: [0.0; 3],
            size: 1.0,
            color: [0.85, 0.25, 0.2],
            opacity: 0.9,
            grid: 32,
            jitter: 0.0,
            radius: 0.02,
            color_variation: 0.0,
            seed: 0,
            random_rotation: false,
        }
    }
}

/// Largest splat a single tool call may build, so a typo cannot exhaust memory.
pub const MAX_POINTS: usize = 2_000_000;

impl SplatParams {
    /// Checks the parameters before any memory is reserved.
    pub fn validate(&self) -> Result<()> {
        if self.count == 0 {
            return Err(SplatError::Format("count must be at least 1".to_owned()));
        }
        if self.count > MAX_POINTS {
            return Err(SplatError::Format(format!(
                "count {} is above the {MAX_POINTS} point limit of one call",
                self.count
            )));
        }
        if !self.size.is_finite() || self.size < 0.0 {
            return Err(SplatError::Format(format!(
                "size must be a finite, non-negative number (got {})",
                self.size
            )));
        }
        if !self.radius.is_finite() || self.radius <= 0.0 {
            return Err(SplatError::Format(format!(
                "radius must be a finite, positive number (got {})",
                self.radius
            )));
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(SplatError::Format(format!(
                "opacity must be in 0..=1 (got {})",
                self.opacity
            )));
        }
        if !self.jitter.is_finite() || self.jitter < 0.0 {
            return Err(SplatError::Format(
                "jitter must be a finite, non-negative fraction".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&self.color_variation) {
            return Err(SplatError::Format(
                "color_variation must be in 0..=1".to_owned(),
            ));
        }
        if self.grid == 0 {
            return Err(SplatError::Format("grid must be at least 1".to_owned()));
        }
        if self.center.iter().any(|value| !value.is_finite()) {
            return Err(SplatError::Format(
                "center must be three finite numbers".to_owned(),
            ));
        }
        if self.color.iter().any(|value| !value.is_finite()) {
            return Err(SplatError::Format(
                "color must be three finite numbers".to_owned(),
            ));
        }
        Ok(())
    }

    /// Builds the splat.
    pub fn build(&self) -> Result<Splat> {
        self.validate()?;
        let mut rng = Rng::new(self.seed);
        let mut points = Vec::with_capacity(self.count);
        match self.shape {
            Shape::Grid => points.extend(self.grid_points()),
            other => {
                for index in 0..self.count {
                    let base = self.base_position(other, index, self.count);
                    points.push(self.point(base, &mut rng));
                }
            }
        }
        let splat = Splat::from_points(points);
        splat.validate()?;
        Ok(splat)
    }

    /// Position of one generated point, before jitter.
    fn base_position(&self, shape: Shape, index: usize, count: usize) -> [f32; 3] {
        let mut rng = Rng::new(self.seed ^ (index as u64).wrapping_mul(0x9E37_79B9));
        let size = self.size;
        let local = match shape {
            Shape::Sphere => {
                // Rejection sampling would loop; scaling a unit vector by cbrt keeps it
                // uniform and needs exactly three samples.
                let direction = unit_vector(&mut rng);
                let radius = size * rng.unit().cbrt();
                [
                    direction[0] * radius,
                    direction[1] * radius,
                    direction[2] * radius,
                ]
            }
            Shape::Shell => {
                let direction = unit_vector(&mut rng);
                [
                    direction[0] * size,
                    direction[1] * size,
                    direction[2] * size,
                ]
            }
            Shape::Cube => [
                rng.range(-size, size),
                rng.range(-size, size),
                rng.range(-size, size),
            ],
            Shape::Plane => [rng.range(-size, size), 0.0, rng.range(-size, size)],
            Shape::Line => {
                let t = if count <= 1 {
                    0.5
                } else {
                    index as f32 / (count - 1) as f32
                };
                [
                    rng.range(-size, size) * 0.0 + (t * 2.0 - 1.0) * size,
                    0.0,
                    0.0,
                ]
            }
            Shape::Ring => {
                let angle = std::f32::consts::TAU * (index as f32 / count as f32);
                [angle.cos() * size, 0.0, angle.sin() * size]
            }
            Shape::Grid => unreachable!("grid is generated row by row"),
        };
        [
            self.center[0] + local[0],
            self.center[1] + local[1],
            self.center[2] + local[2],
        ]
    }

    /// Points on a `grid x grid` sheet in the XZ plane.
    fn grid_points(&self) -> Vec<SplatPoint> {
        let mut rng = Rng::new(self.seed);
        let last = self.grid.max(2) - 1;
        let mut points = Vec::with_capacity(self.grid * self.grid);
        for row in 0..self.grid {
            for column in 0..self.grid {
                let x = (column as f32 / last as f32) * 2.0 - 1.0;
                let z = (row as f32 / last as f32) * 2.0 - 1.0;
                let base = [
                    self.center[0] + x * self.size,
                    self.center[1],
                    self.center[2] + z * self.size,
                ];
                points.push(self.point(base, &mut rng));
            }
        }
        points
    }

    /// Turns a base position into a point, applying jitter, radius, colour and rotation.
    fn point(&self, base: [f32; 3], rng: &mut Rng) -> SplatPoint {
        let spread = self.jitter * self.size;
        let position = if spread > 0.0 {
            [
                base[0] + rng.jitter(spread),
                base[1] + rng.jitter(spread),
                base[2] + rng.jitter(spread),
            ]
        } else {
            base
        };
        let color = if self.color_variation > 0.0 {
            self.color
                .map(|channel| (channel + rng.jitter(self.color_variation)).clamp(0.0, 1.0))
        } else {
            self.color
        };
        let rotation = if self.random_rotation {
            rng.quaternion()
        } else {
            [1.0, 0.0, 0.0, 0.0]
        };
        SplatPoint::new(
            position,
            [self.radius, self.radius, self.radius],
            color,
            self.opacity,
            rotation,
        )
    }
}

/// Uniformly distributed unit vector.
fn unit_vector(rng: &mut Rng) -> [f32; 3] {
    // Marsaglia's method: rejection against the unit disc.
    loop {
        let x1 = rng.range(-1.0, 1.0);
        let x2 = rng.range(-1.0, 1.0);
        let sum = x1 * x1 + x2 * x2;
        if sum > 0.0 && sum < 1.0 {
            let factor = 2.0 * (1.0 - sum).sqrt();
            return [x1 * factor, x2 * factor, 1.0 - 2.0 * sum];
        }
    }
}

/// Builds a splat from explicit points, keeping the caller's values as given.
pub fn splat_from_points(points: Vec<SplatPoint>) -> Result<Splat> {
    let splat = Splat::from_points(points);
    splat.validate()?;
    Ok(splat)
}

/// Builds a splat from a shape description.
pub fn build(params: &SplatParams) -> Result<Splat> {
    params.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(shape: Shape) -> SplatParams {
        SplatParams {
            shape,
            count: 64,
            grid: 8,
            ..SplatParams::default()
        }
    }

    #[test]
    fn the_generator_is_reproducible_and_seed_sensitive() {
        let first = build(&SplatParams::default()).unwrap();
        let second = build(&SplatParams::default()).unwrap();
        assert_eq!(first, second, "the same seed must give the same splat");

        let other = build(&SplatParams {
            seed: 7,
            ..SplatParams::default()
        })
        .unwrap();
        assert_ne!(first, other);
    }

    #[test]
    fn unit_stays_inside_the_unit_interval() {
        let mut rng = Rng::new(3);
        for _ in 0..10_000 {
            let value = rng.unit();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn a_sphere_fills_its_radius_without_leaving_it() {
        let params = SplatParams {
            shape: Shape::Sphere,
            count: 2000,
            size: 2.0,
            radius: 0.05,
            ..SplatParams::default()
        };
        let splat = build(&params).unwrap();
        assert_eq!(splat.len(), 2000);
        let bounds = splat.bounds().unwrap();
        // Bounds include the gaussian radius and must not exceed it beyond the shape.
        for axis in 0..3 {
            assert!(
                bounds.min[axis] >= -2.0 - 0.05 - 1e-3,
                "axis {axis}: {:?}",
                bounds.min
            );
            assert!(
                bounds.max[axis] <= 2.0 + 0.05 + 1e-3,
                "axis {axis}: {:?}",
                bounds.max
            );
        }
        // A filled ball reaches close to its radius on every axis.
        assert!(bounds.max[0] > 1.5 && bounds.max[1] > 1.5 && bounds.max[2] > 1.5);
    }

    #[test]
    fn a_shell_is_hollow() {
        let splat = build(&SplatParams {
            shape: Shape::Shell,
            count: 2000,
            size: 1.0,
            radius: 0.01,
            ..SplatParams::default()
        })
        .unwrap();
        // Every point sits on the surface, so none is near the centre.
        assert!(
            splat
                .points
                .iter()
                .all(|point| distance(point.position, [0.0; 3]) > 0.9)
        );
    }

    #[test]
    fn a_plane_is_flat_and_the_centre_is_honoured() {
        let splat = build(&SplatParams {
            shape: Shape::Plane,
            count: 500,
            size: 3.0,
            center: [10.0, 5.0, -2.0],
            radius: 0.01,
            ..SplatParams::default()
        })
        .unwrap();
        assert!(splat.points.iter().all(|point| point.position[1] == 5.0));
        let bounds = splat.bounds().unwrap();
        assert!((bounds.center[0] - 10.0).abs() < 0.2);
        assert!((bounds.center[2] + 2.0).abs() < 0.2);
    }

    #[test]
    fn a_line_is_evenly_spread_along_one_axis() {
        let splat = build(&SplatParams {
            shape: Shape::Line,
            count: 5,
            size: 2.0,
            radius: 0.01,
            ..SplatParams::default()
        })
        .unwrap();
        let xs: Vec<f32> = splat.points.iter().map(|point| point.position[0]).collect();
        assert_eq!(xs, vec![-2.0, -1.0, 0.0, 1.0, 2.0]);
        assert!(splat.points.iter().all(|point| point.position[1] == 0.0));
    }

    #[test]
    fn a_grid_places_one_point_per_cell() {
        let splat = build(&SplatParams {
            shape: Shape::Grid,
            grid: 6,
            count: 1,
            size: 1.0,
            radius: 0.02,
            ..SplatParams::default()
        })
        .unwrap();
        assert_eq!(splat.len(), 36);
        let bounds = splat.bounds().unwrap();
        // The outermost cells sit at -size and +size, and bounds are padded by the
        // gaussian radius (0.02).
        assert!((bounds.min[0] + 1.02).abs() < 1e-3, "{:?}", bounds.min);
        assert!((bounds.max[2] - 1.02).abs() < 1e-3, "{:?}", bounds.max);
        assert!(splat.points.iter().all(|point| point.position[1] == 0.0));
    }

    #[test]
    fn a_ring_lies_on_its_circle() {
        let splat = build(&SplatParams {
            shape: Shape::Ring,
            count: 64,
            size: 2.0,
            radius: 0.01,
            ..SplatParams::default()
        })
        .unwrap();
        for point in &splat.points {
            let radius = (point.position[0].powi(2) + point.position[2].powi(2)).sqrt();
            assert!((radius - 2.0).abs() < 1e-3, "{radius}");
            assert_eq!(point.position[1], 0.0);
        }
    }

    #[test]
    fn jitter_stays_within_the_requested_spread() {
        let splat = build(&SplatParams {
            shape: Shape::Cube,
            count: 1000,
            size: 0.0,
            jitter: 0.5,
            radius: 0.01,
            ..SplatParams::default()
        })
        .unwrap();
        // A zero-size cube puts every point at the centre, so jitter alone is visible.
        for point in &splat.points {
            for axis in 0..3 {
                assert!(point.position[axis].abs() <= 0.5 + 1e-6);
            }
        }
    }

    #[test]
    fn colour_variation_and_rotation_are_applied() {
        let flat = build(&SplatParams {
            shape: Shape::Sphere,
            count: 32,
            random_rotation: false,
            ..SplatParams::default()
        })
        .unwrap();
        assert!(
            flat.points
                .iter()
                .all(|point| point.rotation == [1.0, 0.0, 0.0, 0.0])
        );
        assert!(
            flat.points
                .iter()
                .all(|point| point.color == [0.85, 0.25, 0.2])
        );

        let varied = build(&SplatParams {
            shape: Shape::Sphere,
            count: 64,
            color_variation: 0.1,
            random_rotation: true,
            ..SplatParams::default()
        })
        .unwrap();
        assert!(
            varied
                .points
                .iter()
                .any(|point| point.color != [0.85, 0.25, 0.2])
        );
        assert!(varied.points.iter().all(|point| {
            point
                .color
                .iter()
                .all(|channel| (0.0..=1.0).contains(channel))
        }));
        for point in &varied.points {
            let norm = point
                .rotation
                .iter()
                .map(|value| value * value)
                .sum::<f32>();
            assert!((norm - 1.0).abs() < 1e-4, "rotation not unit: {norm}");
        }
    }

    #[test]
    fn quaternions_are_unit_length() {
        let mut rng = Rng::new(11);
        for _ in 0..500 {
            let quaternion = rng.quaternion();
            let norm = quaternion.iter().map(|value| value * value).sum::<f32>();
            assert!((norm - 1.0).abs() < 1e-4, "{norm}");
        }
    }

    #[test]
    fn parameter_errors_are_explained() {
        let too_many = SplatParams {
            count: MAX_POINTS + 1,
            ..SplatParams::default()
        };
        let error = build(&too_many).unwrap_err().to_string();
        assert!(error.contains("point limit"), "{error}");

        assert!(
            build(&SplatParams {
                count: 0,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                radius: 0.0,
                ..SplatParams::default()
            })
            .unwrap_err()
            .to_string()
            .contains("radius")
        );
        assert!(
            build(&SplatParams {
                size: -1.0,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                opacity: 1.5,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                grid: 0,
                shape: Shape::Grid,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                jitter: -0.5,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                color_variation: 2.0,
                ..SplatParams::default()
            })
            .is_err()
        );
        assert!(
            build(&SplatParams {
                center: [f32::NAN, 0.0, 0.0],
                ..SplatParams::default()
            })
            .is_err()
        );
    }

    #[test]
    fn shape_names_parse_with_aliases_and_reject_typos() {
        assert_eq!(Shape::parse("Sphere"), Some(Shape::Sphere));
        assert_eq!(Shape::parse(" box "), Some(Shape::Cube));
        assert_eq!(Shape::parse("cloud"), Some(Shape::Sphere));
        assert_eq!(Shape::parse("hollow_sphere"), Some(Shape::Shell));
        assert_eq!(Shape::parse("sprinkle"), None);
        for name in Shape::NAMES {
            assert!(Shape::parse(name).is_some(), "{name}");
        }
    }

    #[test]
    fn explicit_points_are_kept_as_given() {
        let point = SplatPoint::new(
            [1.0, 2.0, 3.0],
            [0.1, 0.2, 0.3],
            [0.1, 0.2, 0.3],
            0.4,
            [1.0, 0.0, 0.0, 0.0],
        );
        let splat = splat_from_points(vec![point]).unwrap();
        assert_eq!(splat.len(), 1);
        assert_eq!(splat.points[0], point);
        assert!(splat_from_points(Vec::new()).is_err());
    }

    #[test]
    fn every_shape_builds_the_requested_count() {
        for shape in [
            Shape::Sphere,
            Shape::Cube,
            Shape::Plane,
            Shape::Line,
            Shape::Shell,
            Shape::Ring,
        ] {
            let splat = build(&params(shape)).unwrap();
            assert_eq!(splat.len(), 64, "{shape:?}");
        }
        let grid = build(&params(Shape::Grid)).unwrap();
        assert_eq!(grid.len(), 64, "grid uses grid * grid points");
    }

    fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }
}
