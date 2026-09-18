use crate::contract;
use crate::inspection::{self, InspectionReport};
use crate::validation::{self, ValidationError, ValidationLimits, ValidationReport};
use crate::{Result, SplatError, color_to_dc, dc_to_color, inv_sigmoid, sigmoid};

/// One Gaussian: an anisotropic ellipsoid with a fixed RGB colour.
///
/// Field units are chosen to be directly meaningful to a caller (an LLM writing
/// JSON) rather than file-native; the contract that defines them - axes, quaternion
/// order, colour space and the activated-versus-serialized distinction - is
/// [`crate::contract`]. See the conversions in [`crate`]'s docs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplatPoint {
    /// World-space centre in metres.
    pub position: [f32; 3],
    /// Ellipsoid radius per axis in metres (the activated scale, always > 0).
    pub scale: [f32; 3],
    /// Linear RGB in `0..=1`.
    pub color: [f32; 3],
    /// Opacity in `0..=1`.
    pub opacity: f32,
    /// Unit quaternion in `(w, x, y, z)` order that rotates the local axes into document
    /// space (an active rotation).
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
    /// Point with the identity rotation, **clamping** values into the contract ranges.
    ///
    /// This is the forgiving convenience constructor used inside the builders: it repairs
    /// a value rather than reporting it, so a boundary that receives caller input uses
    /// [`SplatPoint::try_new`] instead and reports what was wrong.
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

    /// Strict constructor: validates the raw values and normalises the rotation.
    ///
    /// Nothing is clamped or defaulted. A non-finite value, a zero radius, an
    /// out-of-range colour or opacity, or a degenerate quaternion is refused with the
    /// contract reason, so a caller can report it instead of silently storing a repaired
    /// gaussian.
    pub fn try_new(
        position: [f32; 3],
        scale: [f32; 3],
        color: [f32; 3],
        opacity: f32,
        rotation: [f32; 4],
    ) -> std::result::Result<Self, ValidationError> {
        if let Some(issue) = validation::check_values(position, scale, color, opacity, rotation) {
            return Err(ValidationError::from_issue(issue));
        }
        Ok(Self {
            position,
            scale,
            // A finite, non-degenerate quaternion is rescaled: its length carries no
            // information, and the contract stores unit rotations.
            rotation: contract::normalized_quaternion(rotation)
                .expect("a usable quaternion was checked above"),
            color,
            opacity,
        })
    }

    /// Strict constructor for one gaussian of a batch, with its index in the message.
    pub fn try_new_at(
        index: usize,
        position: [f32; 3],
        scale: [f32; 3],
        color: [f32; 3],
        opacity: f32,
        rotation: [f32; 4],
    ) -> std::result::Result<Self, ValidationError> {
        if let Some(issue) =
            validation::check_gaussian(index, position, scale, color, opacity, rotation)
        {
            return Err(ValidationError::from_issue(issue));
        }
        Ok(Self {
            position,
            scale,
            rotation: contract::normalized_quaternion(rotation)
                .expect("a usable quaternion was checked above"),
            color,
            opacity,
        })
    }

    /// PLY `ln(scale)`; PLY stores the log of the radius.
    pub(crate) fn log_scale(&self) -> [f32; 3] {
        self.scale.map(|value| value.max(f32::MIN_POSITIVE).ln())
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
///
/// The diagnostic version of the same information - distributions, contract issues,
/// ownership - is [`Splat::inspection`].
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
    ///
    /// This is the document invariant: it is checked before writing and after every edit,
    /// and it reports the first problem as a plain message. A caller that receives
    /// untrusted values wants [`Splat::check`] or [`Splat::inspection`], which report
    /// *where* and *how many*.
    pub fn validate(&self) -> Result<()> {
        match self.check(ValidationLimits::MATHEMATICAL).first_issue() {
            Some(issue) => Err(SplatError::Format(issue.to_string())),
            None => Ok(()),
        }
    }

    /// Contract report for this splat: bounded, indexed and explicit about limits.
    ///
    /// The point budget is policy rather than mathematics, so it is reported through
    /// [`ValidationReport::within_limits`] and never mixed into the issues.
    pub fn check(&self, limits: ValidationLimits) -> ValidationReport {
        validation::check_splat(&self.points, limits)
    }

    /// Same check, as a structured error, or `Ok` when every gaussian is valid.
    pub fn check_strict(
        &self,
        limits: ValidationLimits,
    ) -> std::result::Result<(), ValidationError> {
        match ValidationError::from_report(&self.check(limits)) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Bounded inspection summary: count, bounds, distributions, diagnostics and cost.
    ///
    /// One pass over the gaussians, and a fixed-size result: inspecting a 500 000 point
    /// splat returns the same shape of data as inspecting three.
    pub fn inspection(&self, limits: ValidationLimits) -> InspectionReport {
        inspection::inspect(self, limits)
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
        let radius = (max[0] - min[0]).max(max[1] - min[1]).max(max[2] - min[2]) * 0.5;
        Some(Bounds {
            min,
            max,
            center,
            radius: radius.max(0.0),
        })
    }

    /// Compact summary of a splat, returned by tool replies.
    ///
    /// Built from [`Splat::inspection`] so the two summaries can never disagree: it is the
    /// same single pass, keeping only the fields a caller reads first.
    pub fn stats(&self) -> SplatStats {
        let report = self.inspection(ValidationLimits::MATHEMATICAL);
        SplatStats {
            point_count: report.point_count,
            bounds: report.bounds,
            min_opacity: report.opacity.min,
            max_opacity: report.opacity.max,
            mean_color: report.mean_color,
        }
    }
}

/// Rescales a quaternion to unit length, defaulting to identity when it has no direction.
///
/// This is the forgiving form of the documented policy in [`crate::contract`]; the strict
/// form is [`crate::contract::normalized_quaternion`], which returns `None` so a caller can
/// refuse a degenerate value instead of inventing a rotation.
pub(crate) fn normalize_quat(rotation: [f32; 4]) -> [f32; 4] {
    contract::normalized_quaternion(rotation).unwrap_or(contract::IDENTITY_QUATERNION)
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
        assert!(
            Splat::from_points(vec![point(0.0, 0.1, 0.5)])
                .validate()
                .is_ok()
        );
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
        let point = SplatPoint::new(
            [0.0; 3],
            [-1.0, 2.0, 1.0],
            [2.0, -3.0, 0.5],
            9.0,
            [1.0, 0.0, 0.0, 0.0],
        );
        assert_eq!(point.scale[0], 0.0);
        assert_eq!(point.color[0], 1.0);
        assert_eq!(point.color[1], 0.0);
        assert_eq!(point.opacity, 1.0);
    }

    #[test]
    fn the_strict_constructor_refuses_what_the_forgiving_one_repairs() {
        let repaired = SplatPoint::new([0.0; 3], [0.0, 0.1, 0.1], [2.0, 0.0, 0.0], 4.0, [0.0; 4]);
        assert_eq!(repaired.scale[0], 0.0);
        assert_eq!(repaired.color[0], 1.0);
        assert_eq!(repaired.opacity, 1.0);
        assert_eq!(repaired.rotation, [1.0, 0.0, 0.0, 0.0]);

        let error = SplatPoint::try_new(
            [0.0; 3],
            [0.0, 0.1, 0.1],
            [0.5; 3],
            0.5,
            [1.0, 0.0, 0.0, 0.0],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("positive radius"), "{error}");
        assert!(
            SplatPoint::try_new(
                [0.0; 3],
                [0.1; 3],
                [2.0, 0.0, 0.0],
                0.5,
                [1.0, 0.0, 0.0, 0.0]
            )
            .is_err()
        );
        assert!(
            SplatPoint::try_new([0.0; 3], [0.1; 3], [0.5; 3], 0.5, [0.0; 4]).is_err(),
            "a zero quaternion has no orientation to store"
        );

        // A usable quaternion is stored normalised, and the index shows up in the message.
        let point =
            SplatPoint::try_new([0.0; 3], [0.1; 3], [0.5; 3], 0.5, [0.0, 4.0, 0.0, 0.0]).unwrap();
        assert_eq!(point.rotation, [0.0, 1.0, 0.0, 0.0]);
        let indexed =
            SplatPoint::try_new_at(5, [0.0; 3], [0.1; 3], [0.5; 3], 2.0, [1.0, 0.0, 0.0, 0.0])
                .unwrap_err()
                .to_string();
        assert!(indexed.contains("point 5"), "{indexed}");
    }

    #[test]
    fn check_reports_where_and_how_much_and_validate_reports_the_first_problem() {
        let splat = Splat::from_points(vec![
            point(0.0, 0.0, 0.5),
            point(1.0, 0.0, 0.5),
            point(2.0, 0.1, 0.5),
        ]);
        let report = splat.check(ValidationLimits::MATHEMATICAL);
        assert_eq!(report.point_count, 3);
        assert_eq!(report.offending_points, 2);
        assert_eq!(report.total_issues, 2);
        assert_eq!(report.contract_version, crate::contract::CONTRACT_VERSION);
        assert_ne!(report.first_issue().unwrap().point, Some(2));

        let error = splat
            .check_strict(ValidationLimits::MATHEMATICAL)
            .unwrap_err()
            .to_string();
        assert!(error.contains("2 of 3 gaussians are invalid"), "{error}");

        // `validate` keeps the terse message the write paths have always produced.
        let text = splat.validate().unwrap_err().to_string();
        assert!(text.contains("point 0"), "{text}");
    }

    #[test]
    fn inspection_and_stats_describe_the_same_splat() {
        let splat = Splat::from_points(vec![point(2.0, 0.5, 0.2), point(0.0, 0.0, 0.6)]);
        let stats = splat.stats();
        let report = splat.inspection(ValidationLimits::default());
        assert_eq!(stats.point_count, report.point_count);
        assert_eq!(stats.bounds, report.bounds);
        assert_eq!(stats.min_opacity, report.opacity.min);
        assert_eq!(stats.max_opacity, report.opacity.max);
        assert_eq!(stats.mean_color, report.mean_color);
        assert_eq!(report.attributes, crate::contract::ATTRIBUTES);
        assert!(!report.validation.is_valid());
    }
}
