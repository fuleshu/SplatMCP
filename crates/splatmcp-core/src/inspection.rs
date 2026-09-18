//! Bounded inspection summaries: counts, bounds, distributions and ownership.
//!
//! A tool that describes a splat must not serialise it. A summary here is a fixed number
//! of scalars - a handful of distributions, one bounds box, one bounded issue list - so
//! inspecting a 500 000 gaussian fixture costs the same reply size as inspecting three. It
//! is computed in a single pass together with the validation report, because both walk the
//! same gaussians.
//!
//! Non-finite values are excluded from the distributions (a `NaN` in a mean would hide
//! every real value) and reported through [`InspectionReport::all_finite`] and the
//! validation report instead.

use std::fmt;

use crate::contract::{self, ATTRIBUTES};
use crate::validation::{self, IssueRecorder, ValidationLimits, ValidationReport};
use crate::{Bounds, Splat, SplatPoint};

/// Distribution of one scalar over a splat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Distribution {
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    /// Number of finite values the distribution was built from.
    pub finite_count: usize,
}

impl Distribution {
    /// Empty distribution, used when nothing was measurable.
    pub const EMPTY: Self = Self {
        min: 0.0,
        max: 0.0,
        mean: 0.0,
        finite_count: 0,
    };
}

impl fmt::Display for Distribution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "min {} max {} mean {}",
            self.min, self.max, self.mean
        )
    }
}

/// Bytes a splat owns, so a caller can see what a document costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnedBuffers {
    /// Bytes of gaussian data in use.
    pub points: usize,
    /// Elements the point vector has allocated for.
    pub capacity_points: usize,
    /// Bytes the vector has allocated, used or not.
    pub allocated: usize,
    /// Total bytes this model owns.
    pub total: usize,
}

/// Bounded description of a splat: never carries the points themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectionReport {
    /// Version of the contract the data was checked against.
    pub contract_version: u32,
    /// Attributes the model stores, from the contract.
    pub attributes: &'static [&'static str],
    pub point_count: usize,
    /// Bounds padded by each gaussian's largest radius, or `None` for an empty splat.
    pub bounds: Option<Bounds>,
    /// Radius distribution per local axis.
    pub scale: [Distribution; 3],
    /// Distribution of the largest radius of each gaussian.
    pub largest_radius: Distribution,
    pub opacity: Distribution,
    /// Linear RGB distribution per channel.
    pub color: [Distribution; 3],
    pub mean_color: [f32; 3],
    /// True when every stored value was finite.
    pub all_finite: bool,
    /// Contract checks, including the point budget the caller applied.
    pub validation: ValidationReport,
    /// What the data costs in memory.
    pub owned: OwnedBuffers,
}

impl InspectionReport {
    /// One line, bounded, for a log or a reply.
    pub fn summary(&self) -> String {
        let bounds = self
            .bounds
            .map(|bounds| {
                format!(
                    "radius {} around [{}, {}, {}]",
                    bounds.radius, bounds.center[0], bounds.center[1], bounds.center[2]
                )
            })
            .unwrap_or_else(|| "no bounds".to_owned());
        format!(
            "{} gaussians, {}, scale {}..{}, opacity {}..{}, {}",
            self.point_count,
            bounds,
            self.largest_radius.min,
            self.largest_radius.max,
            self.opacity.min,
            self.opacity.max,
            self.validation.summary()
        )
    }
}

impl fmt::Display for InspectionReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.summary())
    }
}

/// Running min/max/mean of one scalar channel.
#[derive(Debug, Clone, Copy)]
struct Accumulator {
    min: f32,
    max: f32,
    sum: f64,
    finite_count: usize,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            min: 0.0,
            max: 0.0,
            sum: 0.0,
            finite_count: 0,
        }
    }

    /// Folds one sample in, ignoring values that cannot be summarised.
    fn push(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        if self.finite_count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += f64::from(value);
        self.finite_count += 1;
    }

    fn distribution(&self) -> Distribution {
        if self.finite_count == 0 {
            return Distribution::EMPTY;
        }
        Distribution {
            min: self.min,
            max: self.max,
            mean: (self.sum / self.finite_count as f64) as f32,
            finite_count: self.finite_count,
        }
    }
}

/// Everything one pass over the gaussians has to collect.
struct Sweep {
    scale: [Accumulator; 3],
    largest_radius: Accumulator,
    opacity: Accumulator,
    color: [Accumulator; 3],
    min: [f32; 3],
    max: [f32; 3],
    bounded: bool,
    non_finite: usize,
    recorder: IssueRecorder,
}

impl Sweep {
    fn new() -> Self {
        Self {
            scale: [Accumulator::new(); 3],
            largest_radius: Accumulator::new(),
            opacity: Accumulator::new(),
            color: [Accumulator::new(); 3],
            min: [0.0; 3],
            max: [0.0; 3],
            bounded: false,
            non_finite: 0,
            recorder: IssueRecorder::new(),
        }
    }

    /// Folds one gaussian in, checking it in the same pass.
    fn push(&mut self, index: usize, point: &SplatPoint) {
        let values = [
            point.position[0],
            point.position[1],
            point.position[2],
            point.scale[0],
            point.scale[1],
            point.scale[2],
            point.color[0],
            point.color[1],
            point.color[2],
            point.opacity,
        ];
        let non_finite = values.iter().filter(|value| !value.is_finite()).count()
            + point
                .rotation
                .iter()
                .filter(|value| !value.is_finite())
                .count();
        self.non_finite += non_finite;

        for axis in 0..3 {
            self.scale[axis].push(point.scale[axis]);
            self.color[axis].push(point.color[axis]);
        }
        self.opacity.push(point.opacity);

        // Padding uses the largest finite radius, so one damaged axis cannot poison the
        // bounds of an otherwise readable splat.
        let radius = point
            .scale
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(None, |best: Option<f32>, value| {
                Some(best.map_or(value, |best| best.max(value)))
            });
        if let Some(radius) = radius {
            self.largest_radius.push(radius);
        }

        if point.position.iter().all(|value| value.is_finite()) {
            let pad = radius.unwrap_or(0.0).max(0.0);
            for axis in 0..3 {
                let low = point.position[axis] - pad;
                let high = point.position[axis] + pad;
                if !self.bounded {
                    self.min[axis] = low;
                    self.max[axis] = high;
                } else {
                    self.min[axis] = self.min[axis].min(low);
                    self.max[axis] = self.max[axis].max(high);
                }
            }
            self.bounded = true;
        }

        if let Some(issue) = validation::check_point(index, point) {
            self.recorder.record(issue);
        }
    }

    /// Finishes the report, adding the whole-splat "nothing here" issue for an empty model.
    fn finish(
        mut self,
        point_count: usize,
        capacity_points: usize,
        limits: ValidationLimits,
    ) -> InspectionReport {
        if point_count == 0 {
            self.recorder.record_splat(validation::ValidationIssue::new(
                "points",
                None,
                validation::ValidationReason::Empty,
                "",
            ));
        }
        let color = [
            self.color[0].distribution(),
            self.color[1].distribution(),
            self.color[2].distribution(),
        ];
        let scale = [
            self.scale[0].distribution(),
            self.scale[1].distribution(),
            self.scale[2].distribution(),
        ];
        let bounds = self.bounds();
        let point_bytes = std::mem::size_of::<SplatPoint>();
        let owned = OwnedBuffers {
            points: point_count * point_bytes,
            capacity_points,
            allocated: capacity_points * point_bytes,
            total: capacity_points * point_bytes,
        };
        InspectionReport {
            contract_version: contract::CONTRACT_VERSION,
            attributes: &ATTRIBUTES,
            point_count,
            bounds,
            scale,
            largest_radius: self.largest_radius.distribution(),
            opacity: self.opacity.distribution(),
            color,
            mean_color: [color[0].mean, color[1].mean, color[2].mean],
            all_finite: self.non_finite == 0,
            validation: self.recorder.report(point_count, limits),
            owned,
        }
    }

    fn bounds(&self) -> Option<Bounds> {
        if !self.bounded {
            return None;
        }
        let center = [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ];
        let radius = (self.max[0] - self.min[0])
            .max(self.max[1] - self.min[1])
            .max(self.max[2] - self.min[2])
            * 0.5;
        Some(Bounds {
            min: self.min,
            max: self.max,
            center,
            radius: radius.max(0.0),
        })
    }
}

/// Inspects a splat in one pass, bounded by the caller's limits.
pub fn inspect(splat: &Splat, limits: ValidationLimits) -> InspectionReport {
    let mut sweep = Sweep::new();
    for (index, point) in splat.points.iter().enumerate() {
        sweep.push(index, point);
    }
    sweep.finish(splat.points.len(), splat.points.capacity(), limits)
}

/// Inspects a borrowed slice of gaussians, for adapters that hold their own arrays.
pub fn inspect_slice(points: &[SplatPoint], limits: ValidationLimits) -> InspectionReport {
    let mut sweep = Sweep::new();
    for (index, point) in points.iter().enumerate() {
        sweep.push(index, point);
    }
    sweep.finish(points.len(), points.len(), limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::IDENTITY_QUATERNION;

    fn splat_of(points: &[([f32; 3], [f32; 3], f32)]) -> Splat {
        Splat::from_points(
            points
                .iter()
                .map(|(position, scale, opacity)| SplatPoint {
                    position: *position,
                    scale: *scale,
                    color: [0.5, 0.5, 0.5],
                    opacity: *opacity,
                    rotation: IDENTITY_QUATERNION,
                })
                .collect(),
        )
    }

    #[test]
    fn an_inspection_carries_distributions_and_bounds() {
        let splat = splat_of(&[
            ([0.0, 0.0, 0.0], [0.1, 0.1, 0.1], 1.0),
            ([2.0, 0.0, 0.0], [0.5, 0.2, 0.05], 0.5),
        ]);
        let report = inspect(&splat, ValidationLimits::MATHEMATICAL);
        assert_eq!(report.point_count, 2);
        assert!(report.all_finite);
        assert_eq!(report.validation.total_issues, 0);
        assert_eq!(report.attributes, contract::ATTRIBUTES);

        let bounds = report.bounds.unwrap();
        assert_eq!(bounds.min[0], -0.1);
        assert_eq!(bounds.max[0], 2.5);
        assert_eq!(report.largest_radius.min, 0.1);
        assert_eq!(report.largest_radius.max, 0.5);
        assert!((report.largest_radius.mean - 0.3).abs() < 1e-6);
        assert_eq!(report.scale[1].min, 0.1);
        assert_eq!(report.scale[1].max, 0.2);
        assert_eq!(report.opacity.min, 0.5);
        assert_eq!(report.opacity.max, 1.0);
        assert!((report.opacity.mean - 0.75).abs() < 1e-6);
        assert_eq!(report.color[0].mean, 0.5);
        assert_eq!(report.mean_color, [0.5, 0.5, 0.5]);
        assert_eq!(report.owned.points, 2 * std::mem::size_of::<SplatPoint>());
        assert!(report.owned.total >= report.owned.points);
    }

    #[test]
    fn an_empty_splat_inspects_without_bounds() {
        let report = inspect(&Splat::new(), ValidationLimits::default());
        assert_eq!(report.point_count, 0);
        assert!(report.bounds.is_none());
        assert_eq!(report.largest_radius, Distribution::EMPTY);
        assert_eq!(report.scale[0], Distribution::EMPTY);
        assert_eq!(report.owned.points, 0);
        assert!(!report.validation.is_valid());
        assert!(report.validation.to_string().contains("no points"));
    }

    #[test]
    fn damaged_values_are_counted_but_left_out_of_the_distributions() {
        let mut splat = splat_of(&[([0.0, 0.0, 0.0], [0.2, 0.2, 0.2], 0.5)]);
        splat.points.push(SplatPoint {
            position: [f32::NAN, 0.0, 0.0],
            scale: [0.4, f32::NAN, 0.4],
            color: [0.5, 0.5, 0.5],
            opacity: 0.5,
            rotation: IDENTITY_QUATERNION,
        });
        let report = inspect(&splat, ValidationLimits::MATHEMATICAL);
        assert!(!report.all_finite);
        assert_eq!(report.validation.offending_points, 1);
        assert_eq!(report.scale[0].finite_count, 2, "both X radii are finite");
        assert_eq!(report.scale[1].finite_count, 1, "the NaN radius is left out");
        assert_eq!(report.largest_radius.max, 0.4);
        assert_eq!(report.bounds.unwrap().max[0], 0.2, "the NaN position is skipped");
    }

    #[test]
    fn a_large_inspection_stays_bounded() {
        let count = 20_000;
        let splat = Splat::from_points(
            (0..count)
                .map(|index| SplatPoint {
                    position: [index as f32 * 0.001, 0.0, 0.0],
                    scale: [0.01, 0.01, 0.01],
                    color: [0.5, 0.5, 0.5],
                    opacity: 0.5,
                    rotation: IDENTITY_QUATERNION,
                })
                .collect(),
        );
        let report = inspect(&splat, ValidationLimits::with_max_points(1000));
        assert_eq!(report.point_count, count);
        assert_eq!(report.validation.issues.len(), 0);
        assert!(!report.validation.within_limits);
        assert!(report.summary().len() < 300, "{}", report.summary());
    }

    #[test]
    fn the_slice_entry_point_matches_the_splat_entry_point() {
        let splat = splat_of(&[([1.0, 2.0, 3.0], [0.1, 0.2, 0.3], 0.9)]);
        let from_splat = inspect(&splat, ValidationLimits::MATHEMATICAL);
        let from_slice = inspect_slice(&splat.points, ValidationLimits::MATHEMATICAL);
        assert_eq!(from_splat.point_count, from_slice.point_count);
        assert_eq!(from_splat.bounds, from_slice.bounds);
        assert_eq!(from_splat.scale, from_slice.scale);
        assert_eq!(from_splat.mean_color, from_slice.mean_color);
    }
}
