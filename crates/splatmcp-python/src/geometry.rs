//! Rust geometry helpers the Python bindings expose: parametric surfaces, curves and
//! tangent frames.
//!
//! These helpers exist so a script does not have to re-derive orientation maths in Python
//! and cannot disagree with the core about it: a frame becomes a `(w,x,y,z)` quaternion
//! here, once, using the same convention [`crate::arrays`] validates.
//!
//! The local frame of a surface gaussian is the one a viewer expects: local `+X` along the
//! surface tangent, local `+Z` along the surface normal, and local `+Y` completing the
//! right-handed set. The scale on each axis is the radius of the flattened disc that
//! follows the surface, which is what makes a sampled surface read as a sheet instead of a
//! cloud of spheres.

use serde::Deserialize;
use splatmcp_core::Rng;

use crate::arrays::{BatchMetadata, GaussianBatch, MAX_BATCH_POINTS};
use crate::{PythonError, Result};

/// Largest sample count per axis, so a resolution typo cannot allocate unbounded memory.
pub const MAX_RESOLUTION: u32 = 512;

/// Shape of a sampled surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    /// Unit-radius sphere around `center`.
    Sphere,
    /// Flat sheet in the `XZ` plane, `radius` wide in each direction.
    Plane,
    /// Tube along `+Y`, `radius` wide and `length` tall.
    Cylinder,
    /// Ring of main radius `radius` and tube radius `secondary_radius` around `+Y`.
    Torus,
}

/// A surface to sample into gaussians.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SurfaceSpec {
    pub kind: SurfaceKind,
    /// Samples per axis; the batch holds `resolution[0] * resolution[1]` gaussians.
    pub resolution: [u32; 2],
    /// Main radius, in metres.
    pub radius: f32,
    /// Tube radius, used by [`SurfaceKind::Torus`].
    pub secondary_radius: f32,
    /// Height of a cylinder, in metres.
    pub length: f32,
    /// Centre of the surface, in the authoring space's Y-down document space.
    pub center: [f32; 3],
    /// Local radii of each gaussian.
    pub scale: [f32; 3],
    pub color: [f32; 3],
    /// Random per-gaussian colour spread, seeded by [`SurfaceSpec::seed`].
    pub color_variation: f32,
    pub opacity: f32,
    /// Random offset applied to each position, seeded by [`SurfaceSpec::seed`].
    pub jitter: f32,
    pub seed: u64,
}

impl Default for SurfaceSpec {
    fn default() -> Self {
        Self {
            kind: SurfaceKind::Sphere,
            resolution: [48, 24],
            radius: 1.0,
            secondary_radius: 0.3,
            length: 1.0,
            center: [0.0; 3],
            scale: [0.02; 3],
            color: [0.8, 0.8, 0.8],
            color_variation: 0.0,
            opacity: 1.0,
            jitter: 0.0,
            seed: 0,
        }
    }
}

impl SurfaceSpec {
    /// Number of gaussians this spec produces.
    pub fn count(&self) -> usize {
        self.resolution[0] as usize * self.resolution[1] as usize
    }

    /// Rejects a spec that would produce an unusable or oversized batch.
    pub fn validate(&self) -> Result<()> {
        for (axis, value) in self.resolution.iter().enumerate() {
            if *value == 0 || *value > MAX_RESOLUTION {
                return Err(PythonError::InvalidBatch(format!(
                    "resolution[{axis}] is {value}; it must be between 1 and {MAX_RESOLUTION}"
                )));
            }
        }
        if self.count() > MAX_BATCH_POINTS {
            return Err(PythonError::BudgetExceeded(format!(
                "{} samples are above the {MAX_BATCH_POINTS} point budget",
                self.count()
            )));
        }
        if !self.radius.is_finite() || self.radius <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "radius must be a positive, finite number".to_owned(),
            ));
        }
        if self.kind == SurfaceKind::Torus && self.secondary_radius <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "secondary_radius must be positive for a torus".to_owned(),
            ));
        }
        if self.kind == SurfaceKind::Cylinder && self.length <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "length must be positive for a cylinder".to_owned(),
            ));
        }
        if self.scale.iter().any(|value| *value <= 0.0) {
            return Err(PythonError::InvalidBatch(
                "scale is the local radius of each gaussian and must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Shape of a sampled curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurveKind {
    /// Straight segment along `+X`.
    Line,
    /// Circle in the `XZ` plane.
    Circle,
    /// Helix along `+Y`.
    Helix,
}

/// A curve to sample into gaussians.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CurveSpec {
    pub kind: CurveKind,
    /// Samples along the curve.
    pub steps: u32,
    /// Segment length for [`CurveKind::Line`], height for [`CurveKind::Helix`].
    pub length: f32,
    /// Radius for [`CurveKind::Circle`] and [`CurveKind::Helix`].
    pub radius: f32,
    /// Number of turns for [`CurveKind::Helix`].
    pub turns: f32,
    pub center: [f32; 3],
    pub scale: [f32; 3],
    pub color: [f32; 3],
    pub color_variation: f32,
    pub opacity: f32,
    pub jitter: f32,
    pub seed: u64,
}

impl Default for CurveSpec {
    fn default() -> Self {
        Self {
            kind: CurveKind::Line,
            steps: 64,
            length: 1.0,
            radius: 0.5,
            turns: 3.0,
            center: [0.0; 3],
            scale: [0.02; 3],
            color: [0.8, 0.8, 0.8],
            color_variation: 0.0,
            opacity: 1.0,
            jitter: 0.0,
            seed: 0,
        }
    }
}

impl CurveSpec {
    /// Number of gaussians this spec produces.
    pub fn count(&self) -> usize {
        self.steps as usize
    }

    /// Rejects a spec that would produce an unusable or oversized batch.
    pub fn validate(&self) -> Result<()> {
        if self.steps == 0 || self.steps > MAX_RESOLUTION * 8 {
            return Err(PythonError::InvalidBatch(format!(
                "steps is {}; it must be between 1 and {}",
                self.steps,
                MAX_RESOLUTION * 8
            )));
        }
        if !self.length.is_finite() || self.length <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "length must be a positive, finite number".to_owned(),
            ));
        }
        if matches!(self.kind, CurveKind::Circle | CurveKind::Helix) && self.radius <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "radius must be positive for a circle or helix".to_owned(),
            ));
        }
        if self.kind == CurveKind::Helix && self.turns <= 0.0 {
            return Err(PythonError::InvalidBatch(
                "turns must be positive for a helix".to_owned(),
            ));
        }
        if self.scale.iter().any(|value| *value <= 0.0) {
            return Err(PythonError::InvalidBatch(
                "scale is the local radius of each gaussian and must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Samples a parametric surface into gaussians oriented by its tangent frames.
pub fn sample_surface(spec: &SurfaceSpec) -> Result<GaussianBatch> {
    spec.validate()?;
    let count = spec.count();
    let mut batch = GaussianBatch::with_capacity(count);
    batch.metadata = BatchMetadata {
        component_id: None,
        recipe: Some(format!("sample_surface:{:?}", spec.kind)),
        seed: Some(spec.seed),
    };
    let mut rng = Rng::new(spec.seed);
    let (u_steps, v_steps) = (spec.resolution[0].max(1), spec.resolution[1].max(1));

    for u_index in 0..u_steps {
        for v_index in 0..v_steps {
            let u = u_index as f32 / u_steps as f32;
            let v = v_index as f32 / v_steps as f32;
            let (position, normal) = surface_point(spec, u, v);
            let position = jittered(position, spec.jitter, &mut rng);
            let rotation = frame_quaternion(tangent_of(spec, u, v), normal);
            batch.push(
                position,
                spec.scale,
                rotation,
                varied_color(spec.color, spec.color_variation, &mut rng),
                spec.opacity,
            );
        }
    }
    Ok(batch)
}

/// Samples a curve into gaussians oriented along its tangent.
pub fn sample_curve(spec: &CurveSpec) -> Result<GaussianBatch> {
    spec.validate()?;
    let steps = spec.steps.max(1);
    let mut batch = GaussianBatch::with_capacity(steps as usize);
    batch.metadata = BatchMetadata {
        component_id: None,
        recipe: Some(format!("sample_curve:{:?}", spec.kind)),
        seed: Some(spec.seed),
    };
    let mut rng = Rng::new(spec.seed);
    for index in 0..steps {
        // Endpoints are inclusive for a sampled curve, so a line really is `length` long.
        let t = if steps > 1 {
            index as f32 / (steps - 1) as f32
        } else {
            0.5
        };
        let (position, tangent) = curve_point(spec, t);
        let position = jittered(position, spec.jitter, &mut rng);
        // A curve has no surface normal, so an arbitrary one perpendicular to the tangent
        // keeps the frame well defined without twisting the gaussian around its axis.
        let normal = perpendicular(tangent);
        batch.push(
            position,
            spec.scale,
            frame_quaternion(tangent, normal),
            varied_color(spec.color, spec.color_variation, &mut rng),
            spec.opacity,
        );
    }
    Ok(batch)
}

/// Builds the `(w,x,y,z)` quaternion of a frame with local `+X` along `tangent` and local
/// `+Z` along `normal`.
///
/// The quaternion comes from the rotation matrix whose columns are the frame axes, so a
/// script gets the same numbers the core would compute.
pub fn frame_quaternion(tangent: [f32; 3], normal: [f32; 3]) -> [f32; 4] {
    let x = normalize(tangent);
    let z = normalize(normal);
    // Gram-Schmidt: the caller's two vectors need not be perpendicular.
    let y = cross(z, x);
    let y = normalize(y);
    let z = cross(x, y);
    quaternion_from_axes(x, y, z)
}

/// Converts an orthonormal basis into a `(w,x,y,z)` quaternion.
pub fn quaternion_from_axes(x: [f32; 3], y: [f32; 3], z: [f32; 3]) -> [f32; 4] {
    // Standard rotation-matrix to quaternion conversion, with the matrix columns being the
    // images of the local axes.
    let trace = x[0] + y[1] + z[2];
    let quaternion = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [
            0.25 * s,
            (y[2] - z[1]) / s,
            (z[0] - x[2]) / s,
            (x[1] - y[0]) / s,
        ]
    } else if x[0] > y[1] && x[0] > z[2] {
        let s = (1.0 + x[0] - y[1] - z[2]).sqrt() * 2.0;
        [
            (y[2] - z[1]) / s,
            0.25 * s,
            (y[0] + x[1]) / s,
            (z[0] + x[2]) / s,
        ]
    } else if y[1] > z[2] {
        let s = (1.0 + y[1] - x[0] - z[2]).sqrt() * 2.0;
        [
            (z[0] - x[2]) / s,
            (y[0] + x[1]) / s,
            0.25 * s,
            (z[1] + y[2]) / s,
        ]
    } else {
        let s = (1.0 + z[2] - x[0] - y[1]).sqrt() * 2.0;
        [
            (x[1] - y[0]) / s,
            (z[0] + x[2]) / s,
            (z[1] + y[2]) / s,
            0.25 * s,
        ]
    };
    let norm = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm <= 1e-6 || !norm.is_finite() {
        return [1.0, 0.0, 0.0, 0.0];
    }
    quaternion.map(|value| value / norm)
}

/// Position and outward normal of a surface at `(u, v)` in `0..=1`.
fn surface_point(spec: &SurfaceSpec, u: f32, v: f32) -> ([f32; 3], [f32; 3]) {
    let center = spec.center;
    let two_pi = std::f32::consts::TAU;
    match spec.kind {
        SurfaceKind::Sphere => {
            let phi = two_pi * u;
            let theta = std::f32::consts::PI * v;
            let normal = [
                theta.sin() * phi.cos(),
                theta.cos(),
                theta.sin() * phi.sin(),
            ];
            (offset(center, scale3(normal, spec.radius)), normal)
        }
        SurfaceKind::Plane => {
            let x = (u - 0.5) * 2.0 * spec.radius;
            let z = (v - 0.5) * 2.0 * spec.radius;
            ([center[0] + x, center[1], center[2] + z], [0.0, -1.0, 0.0])
        }
        SurfaceKind::Cylinder => {
            let phi = two_pi * u;
            let normal = [phi.cos(), 0.0, phi.sin()];
            let y = (v - 0.5) * spec.length;
            (
                [
                    center[0] + normal[0] * spec.radius,
                    center[1] + y,
                    center[2] + normal[2] * spec.radius,
                ],
                normal,
            )
        }
        SurfaceKind::Torus => {
            let phi = two_pi * u;
            let psi = two_pi * v;
            let ring = spec.radius + spec.secondary_radius * psi.cos();
            let normal = [
                psi.cos() * phi.cos(),
                psi.sin(),
                psi.cos() * phi.sin(),
            ];
            (
                [
                    center[0] + ring * phi.cos(),
                    center[1] + spec.secondary_radius * psi.sin(),
                    center[2] + ring * phi.sin(),
                ],
                normal,
            )
        }
    }
}

/// Tangent along increasing `u`, used as the gaussian's local `+X`.
fn tangent_of(spec: &SurfaceSpec, u: f32, v: f32) -> [f32; 3] {
    let step = 1e-3;
    let ahead = surface_point(spec, (u + step) % 1.0, v).0;
    let behind = surface_point(spec, (u - step).rem_euclid(1.0), v).0;
    let mut tangent = [
        ahead[0] - behind[0],
        ahead[1] - behind[1],
        ahead[2] - behind[2],
    ];
    if length(tangent) < 1e-9 {
        // Degenerate wrap-around (1-dimensional surfaces): fall back to the v direction.
        let ahead = surface_point(spec, u, (v + step).min(1.0)).0;
        let behind = surface_point(spec, u, (v - step).max(0.0)).0;
        tangent = [
            ahead[0] - behind[0],
            ahead[1] - behind[1],
            ahead[2] - behind[2],
        ];
    }
    normalize(tangent)
}

/// Position at `t` in `0..=1` and its unit tangent.
fn curve_point(spec: &CurveSpec, t: f32) -> ([f32; 3], [f32; 3]) {
    let center = spec.center;
    let two_pi = std::f32::consts::TAU;
    match spec.kind {
        CurveKind::Line => {
            let x = (t - 0.5) * spec.length;
            ([center[0] + x, center[1], center[2]], [1.0, 0.0, 0.0])
        }
        CurveKind::Circle => {
            let phi = two_pi * t;
            (
                [
                    center[0] + spec.radius * phi.cos(),
                    center[1],
                    center[2] + spec.radius * phi.sin(),
                ],
                [-phi.sin(), 0.0, phi.cos()],
            )
        }
        CurveKind::Helix => {
            let phi = two_pi * spec.turns * t;
            let y = (t - 0.5) * spec.length;
            let tangent_scale = spec.radius * two_pi * spec.turns;
            let tangent = [
                -phi.sin() * tangent_scale,
                spec.length,
                phi.cos() * tangent_scale,
            ];
            (
                [
                    center[0] + spec.radius * phi.cos(),
                    center[1] + y,
                    center[2] + spec.radius * phi.sin(),
                ],
                normalize(tangent),
            )
        }
    }
}

/// A unit vector perpendicular to `value`, chosen to stay stable for axis-aligned input.
fn perpendicular(value: [f32; 3]) -> [f32; 3] {
    let axis = if value[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    normalize(cross(value, axis))
}

fn varied_color(base: [f32; 3], variation: f32, rng: &mut Rng) -> [f32; 3] {
    if variation <= 0.0 {
        return base;
    }
    base.map(|value| (value + rng.jitter(variation)).clamp(0.0, 1.0))
}

fn jittered(position: [f32; 3], spread: f32, rng: &mut Rng) -> [f32; 3] {
    if spread <= 0.0 {
        return position;
    }
    [
        position[0] + rng.jitter(spread),
        position[1] + rng.jitter(spread),
        position[2] + rng.jitter(spread),
    ]
}

fn offset(center: [f32; 3], delta: [f32; 3]) -> [f32; 3] {
    [
        center[0] + delta[0],
        center[1] + delta[1],
        center[2] + delta[2],
    ]
}

fn scale3(value: [f32; 3], factor: f32) -> [f32; 3] {
    [value[0] * factor, value[1] * factor, value[2] * factor]
}

fn length(value: [f32; 3]) -> f32 {
    (value[0] * value[0] + value[1] * value[1] + value[2] * value[2]).sqrt()
}

fn normalize(value: [f32; 3]) -> [f32; 3] {
    let norm = length(value);
    if norm <= 1e-9 || !norm.is_finite() {
        return [0.0, 0.0, 1.0];
    }
    [value[0] / norm, value[1] / norm, value[2] / norm]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    #[test]
    fn a_sphere_sits_on_its_radius() {
        let spec = SurfaceSpec {
            kind: SurfaceKind::Sphere,
            resolution: [12, 6],
            radius: 2.0,
            ..SurfaceSpec::default()
        };
        let batch = sample_surface(&spec).unwrap();
        assert_eq!(batch.len(), 72);
        batch.validate(MAX_BATCH_POINTS).unwrap();
        for position in &batch.positions {
            let distance = length(*position);
            assert!((distance - 2.0).abs() < 0.05, "distance {distance}");
        }
    }

    #[test]
    fn a_plane_is_flat_and_faces_down_in_document_space() {
        let spec = SurfaceSpec {
            kind: SurfaceKind::Plane,
            resolution: [4, 4],
            radius: 1.0,
            ..SurfaceSpec::default()
        };
        let batch = sample_surface(&spec).unwrap();
        assert!(batch.positions.iter().all(|position| position[1] == 0.0));
        // Local +Z is the normal, so the frame must leave +Z pointing along -Y.
        let rotation = frame_quaternion([1.0, 0.0, 0.0], [0.0, -1.0, 0.0]);
        let z_axis = rotate(rotation, [0.0, 0.0, 1.0]);
        assert!(dot(z_axis, [0.0, -1.0, 0.0]) > 0.99);
    }

    #[test]
    fn a_cylinder_keeps_its_radius_and_height() {
        let spec = SurfaceSpec {
            kind: SurfaceKind::Cylinder,
            resolution: [8, 4],
            radius: 0.5,
            length: 3.0,
            ..SurfaceSpec::default()
        };
        let batch = sample_surface(&spec).unwrap();
        for position in &batch.positions {
            let radial = (position[0] * position[0] + position[2] * position[2]).sqrt();
            assert!((radial - 0.5).abs() < 1e-4);
            assert!(position[1].abs() <= 1.5 + 1e-4);
        }
    }

    #[test]
    fn a_torus_uses_both_radii() {
        let spec = SurfaceSpec {
            kind: SurfaceKind::Torus,
            resolution: [16, 8],
            radius: 1.0,
            secondary_radius: 0.25,
            ..SurfaceSpec::default()
        };
        let batch = sample_surface(&spec).unwrap();
        for position in &batch.positions {
            let radial = (position[0] * position[0] + position[2] * position[2]).sqrt();
            assert!((0.75 - 1e-4..=1.25 + 1e-4).contains(&radial), "radial {radial}");
        }
    }

    #[test]
    fn a_curve_follows_its_kind() {
        let line = CurveSpec {
            kind: CurveKind::Line,
            steps: 5,
            length: 4.0,
            ..CurveSpec::default()
        };
        let batch = sample_curve(&line).unwrap();
        assert_eq!(batch.len(), 5);
        assert!((batch.positions[0][0] + 2.0).abs() < 1e-4);
        assert!((batch.positions[4][0] - 2.0).abs() < 1e-4);
        assert_eq!(batch.positions[2][0], 0.0);

        let helix = CurveSpec {
            kind: CurveKind::Helix,
            steps: 24,
            radius: 0.4,
            turns: 2.0,
            length: 1.0,
            ..CurveSpec::default()
        };
        let batch = sample_curve(&helix).unwrap();
        batch.validate(MAX_BATCH_POINTS).unwrap();
        for position in &batch.positions {
            let radial = (position[0] * position[0] + position[2] * position[2]).sqrt();
            assert!((radial - 0.4).abs() < 1e-4);
        }
    }

    #[test]
    fn the_same_seed_produces_the_same_jitter() {
        let spec = SurfaceSpec {
            kind: SurfaceKind::Sphere,
            resolution: [6, 3],
            jitter: 0.02,
            color_variation: 0.05,
            seed: 42,
            ..SurfaceSpec::default()
        };
        let first = sample_surface(&spec).unwrap();
        let second = sample_surface(&spec).unwrap();
        assert_eq!(first, second);
        let other = sample_surface(&SurfaceSpec { seed: 43, ..spec }).unwrap();
        assert_ne!(first.positions, other.positions);
    }

    #[test]
    fn frames_are_orthonormal_and_survive_a_degenerate_input() {
        let rotation = frame_quaternion([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
        let x_axis = rotate(rotation, [1.0, 0.0, 0.0]);
        let z_axis = rotate(rotation, [0.0, 0.0, 1.0]);
        assert!((length(x_axis) - 1.0).abs() < 1e-5);
        assert!((length(z_axis) - 1.0).abs() < 1e-5);
        assert!(dot(x_axis, z_axis).abs() < 1e-4);

        // Parallel vectors cannot define a frame; the fallback must still be a unit
        // quaternion rather than a NaN.
        let degenerate = frame_quaternion([0.0, 1.0, 0.0], [0.0, 1.0, 0.0]);
        let norm = degenerate.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
    }

    #[test]
    fn bad_specs_are_refused_with_their_reason() {
        let too_fine = SurfaceSpec {
            resolution: [MAX_RESOLUTION + 1, 1],
            ..SurfaceSpec::default()
        };
        assert_eq!(too_fine.validate().unwrap_err().code(), "invalid_batch");

        let no_scale = SurfaceSpec {
            scale: [0.0, 0.1, 0.1],
            ..SurfaceSpec::default()
        };
        assert!(no_scale.validate().unwrap_err().to_string().contains("scale"));

        let bad_helix = CurveSpec {
            kind: CurveKind::Helix,
            turns: 0.0,
            ..CurveSpec::default()
        };
        assert!(bad_helix.validate().unwrap_err().to_string().contains("turns"));
    }

    #[test]
    fn a_batch_metadata_records_the_recipe_and_seed() {
        let spec = SurfaceSpec {
            seed: 5,
            ..SurfaceSpec::default()
        };
        let batch = sample_surface(&spec).unwrap();
        assert_eq!(batch.metadata.seed, Some(5));
        assert!(
            batch
                .metadata
                .recipe
                .as_deref()
                .unwrap()
                .contains("sample_surface")
        );
    }

    /// Rotates a vector by a `(w,x,y,z)` quaternion, the same way a viewer would.
    fn rotate(quaternion: [f32; 4], value: [f32; 3]) -> [f32; 3] {
        let [w, x, y, z] = quaternion;
        let basis = [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - w * z),
                2.0 * (x * z + w * y),
            ],
            [
                2.0 * (x * y + w * z),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - w * x),
            ],
            [
                2.0 * (x * z - w * y),
                2.0 * (y * z + w * x),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ];
        [dot(basis[0], value), dot(basis[1], value), dot(basis[2], value)]
    }
}
