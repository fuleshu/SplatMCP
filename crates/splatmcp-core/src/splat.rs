use crate::{Result, SplatError, color_to_dc, dc_to_color, inv_sigmoid, sigmoid};

/// One Gaussian: an anisotropic ellipsoid with a fixed RGB colour.
///
/// Field units are chosen to be directly meaningful to a caller (an LLM writing
/// JSON) rather than file-native; see the conversions in [`crate`]'s docs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplatPoint {
    /// World-space centre in metres.
    pub position: [f32; 3],
    /// Ellipsoid radius per axis in metres (the activated scale, always >= 0).
    pub scale: [f32; 3],
    /// Linear RGB in `0..=1`.
    pub color: [f32; 3],
    /// Opacity in `0..=1`.
    pub opacity: f32,
    /// Unit quaternion in `(w, x, y, z)` order.
    pub rotation: [f32; 4],
}

impl Default for SplatPoint {
    fn default() -> Self {
        Self {
            position: [0.0, 0.0, 0.0],
            scale: [0.01, 0.01, 0.01],
            color: [0.5, 0.5, 0.5],
            opacity: 1.0,
            rotation: [1.0, 0.0, 0.0, 0.0],
        }
    }
}

impl SplatPoint {
    /// Point with the identity rotation, normalised so file round trips stay stable.
    pub fn new(
        position: [f32; 3],
        scale: [f32; 3],
        color: [f32; 3],
        opacity: f32,
        rotation: [f32; 4],
    ) -> Self {
        Self {
            position,
            scale: scale.map(|value| value.max(0.0)),
            color: color.map(|value| value.clamp(0.0, 1.0)),
            opacity: opacity.clamp(0.0, 1.0),
            rotation: normalize_quat(rotation),
        }
    }

    /// PLY `ln(scale)`; PLY stores the log of the radius.
    pub(crate) fn log_scale(&self) -> [f32; 3] {
        self.scale
            .map(|value| value.max(f32::MIN_POSITIVE).ln())
    }

    /// Sets the radius from `ln(scale)`.
    pub(crate) fn set_log_scale(&mut self, log_scale: [f32; 3]) {
        self.scale = log_scale.map(f32::exp);
    }

    /// PLY `f_dc_*` coefficients.
    pub(crate) fn dc(&self) -> [f32; 3] {
        self.color.map(|value| color_to_dc(value))
    }

    /// Sets RGB from PLY `f_dc_*` coefficients.
    pub(crate) fn set_dc(&mut self, dc: [f32; 3]) {
        self.color = dc.map(dc_to_color);
    }

    /// PLY `opacity` (sigmoid logit).
    pub(crate) fn opacity_logit(&self) -> f32 {
        inv_sigmoid(self.opacity)
    }

    /// Sets opacity from a sigmoid logit.
    pub(crate) fn set_opacity_logit(&mut self, logit: f32) {
        self.opacity = sigmoid(logit);
    }
}

/// Axis-aligned bounds of a splat, inflated by each point's largest radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    /// Longest half extent, useful as a framing radius.
    pub radius: f32,
}

/// Cheap summary of a splat, returned by inspection tools.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplatStats {
    pub point_count: usize,
    pub bounds: Option<Bounds>,
    pub min_opacity: f32,
    pub max_opacity: f32,
    pub mean_color: [f32; 3],
}

/// An editable collection of Gaussians.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Splat {
    pub points: Vec<SplatPoint>,
}

impl Splat {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_points(points: Vec<SplatPoint>) -> Self {
        Self { points }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Rejects values that would not survive a file round trip.
    pub fn validate(&self) -> Result<()> {
        if self.points.is_empty() {
            return Err(SplatError::Format("splat has no points".to_owned()));
        }
        for (index, point) in self.points.iter().enumerate() {
            let finite = point
                .position
                .into_iter()
                .chain(point.scale)
                .chain(point.color)
                .chain([point.opacity])
                .all(|value| value.is_finite());
            if !finite {
                return Err(SplatError::Format(format!(
                    "point {index} has a non-finite value"
                )));
            }
            if point.scale.iter().any(|value| *value <= 0.0) {
                return Err(SplatError::Format(format!(
                    "point {index} has a non-positive scale"
                )));
            }
        }
        Ok(())
    }

    /// Axis-aligned bounds, or `None` for an empty splat.
    pub fn bounds(&self) -> Option<Bounds> {
        let first = self.points.first()?;
        let mut min = first.position;
        let mut max = first.position;
        for point in &self.points {
            let pad = point.scale.into_iter().fold(0.0f32, f32::max);
            for axis in 0..3 {
                min[axis] = min[axis].min(point.position[axis] - pad);
                max[axis] = max[axis].max(point.position[axis] + pad);
            }
        }
        let center = [
            (min[0] + max[0]) * 0.5,
            (min[1] + max[1]) * 0.5,
            (min[2] + max[2]) * 0.5,
        ];
        let radius = (max[0] - min[0])
            .max(max[1] - min[1])
            .max(max[2] - min[2])
            * 0.5;
        Some(Bounds {
            min,
            max,
            center,
            radius: radius.max(0.0),
        })
    }

    pub fn stats(&self) -> SplatStats {
        let count = self.points.len();
        if count == 0 {
            return SplatStats {
                point_count: 0,
                bounds: None,
                min_opacity: 0.0,
                max_opacity: 0.0,
                mean_color: [0.0, 0.0, 0.0],
            };
        }
        let mut min_opacity = f32::MAX;
        let mut max_opacity = f32::MIN;
        let mut sum = [0.0f64; 3];
        for point in &self.points {
            min_opacity = min_opacity.min(point.opacity);
            max_opacity = max_opacity.max(point.opacity);
            for channel in 0..3 {
                sum[channel] += f64::from(point.color[channel]);
            }
        }
        let mean_color = sum.map(|value| (value / count as f64) as f32);
        SplatStats {
            point_count: count,
            bounds: self.bounds(),
            min_opacity,
            max_opacity,
            mean_color,
        }
    }
}

/// Rescales a quaternion, defaulting to identity for a zero-length input.
pub(crate) fn normalize_quat(rotation: [f32; 4]) -> [f32; 4] {
    let norm = rotation.into_iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm < 1e-9 || !norm.is_finite() {
        return [1.0, 0.0, 0.0, 0.0];
    }
    rotation.map(|value| value / norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f32, scale: f32, color: f32) -> SplatPoint {
        SplatPoint::new(
            [x, 0.0, 0.0],
            [scale, scale, scale],
            [color, color, color],
            1.0,
            [1.0, 0.0, 0.0, 0.0],
        )
    }

    #[test]
    fn rejects_empty_and_bad_values() {
        assert!(Splat::new().validate().is_err());
        assert!(
            Splat::from_points(vec![point(0.0, 0.0, 0.5)])
                .validate()
                .is_err()
        );
        assert!(Splat::from_points(vec![point(0.0, 0.1, 0.5)]).validate().is_ok());
    }

    #[test]
    fn bounds_pad_by_the_largest_radius() {
        let splat = Splat::from_points(vec![point(0.0, 0.5, 0.5)]);
        let bounds = splat.bounds().unwrap();
        assert_eq!(bounds.min[0], -0.5);
        assert_eq!(bounds.max[0], 0.5);
        assert_eq!(bounds.center, [0.0, 0.0, 0.0]);
        assert_eq!(bounds.radius, 0.5);
    }

    #[test]
    fn zero_quaternion_becomes_identity() {
        assert_eq!(normalize_quat([0.0, 0.0, 0.0, 0.0]), [1.0, 0.0, 0.0, 0.0]);
        let normalized = normalize_quat([0.0, 2.0, 0.0, 0.0]);
        assert_eq!(normalized, [0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn clamps_into_representable_ranges() {
        let point = SplatPoint::new([0.0; 3], [-1.0, 2.0, 1.0], [2.0, -3.0, 0.5], 9.0, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(point.scale[0], 0.0);
        assert_eq!(point.color[0], 1.0);
        assert_eq!(point.color[1], 0.0);
        assert_eq!(point.opacity, 1.0);
    }
}
