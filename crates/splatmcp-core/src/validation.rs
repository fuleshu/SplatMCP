//! Structured, bounded validation of raw gaussian data.
//!
//! The contract ([`crate::contract`]) says what a value may be; this module says *what
//! was wrong, where, and how much of it there was*. It is deliberately the one place
//! that decides whether a value is acceptable, so MCP, the desktop app and the Python
//! adapter cannot drift apart on what "a valid gaussian" means.
//!
//! Two rules shape the API:
//!
//! - **Raw values are checked before any clamping.** [`crate::SplatPoint::new`] is a
//!   forgiving convenience constructor and clamps; a boundary that receives caller input
//!   uses [`check_gaussian`] or [`crate::SplatPoint::try_new`] instead, so a silent
//!   repair can never be the default.
//! - **Reports are bounded.** A 500 000 point fixture must not be able to make a reply
//!   large, so a report carries at most [`MAX_REPORTED_ISSUES`] located issues while
//!   still counting every issue and every offending gaussian.
//!
//! Policy limits (a maximum point count) are kept out of [`ValidationReport::issues`]:
//! [`ValidationReport::within_limits`], [`ValidationReport::limits`] and
//! [`ValidationReport::limit_message`] report the limit *actually applied*, separately
//! from mathematical validity.

use std::fmt;

use crate::{MAX_POINTS, SplatPoint};

/// Largest number of located issues one report carries.
pub const MAX_REPORTED_ISSUES: usize = 32;

/// Why a value failed the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationReason {
    /// A required number was `NaN` or infinite.
    NonFiniteValue,
    /// A radius was zero, negative or unreadable.
    NonPositiveScale,
    /// A quaternion was all zero, or too short to have a direction.
    DegenerateQuaternion,
    /// A colour channel was outside the linear RGB range.
    ColorOutOfRange,
    /// An opacity was outside the activated range.
    OpacityOutOfRange,
    /// The splat holds no gaussians at all.
    Empty,
}

impl ValidationReason {
    /// Stable machine readable name, for structured replies.
    pub fn code(self) -> &'static str {
        match self {
            Self::NonFiniteValue => "non_finite_value",
            Self::NonPositiveScale => "non_positive_scale",
            Self::DegenerateQuaternion => "degenerate_quaternion",
            Self::ColorOutOfRange => "color_out_of_range",
            Self::OpacityOutOfRange => "opacity_out_of_range",
            Self::Empty => "empty",
        }
    }

    /// What the value should have been, as the tail of an error message.
    pub fn message(self) -> &'static str {
        match self {
            Self::NonFiniteValue => "must be a finite number",
            Self::NonPositiveScale => {
                "must be a positive radius in metres (the activated scale, not a PLY log-scale)"
            }
            Self::DegenerateQuaternion => "must be a non-zero (w, x, y, z) quaternion",
            Self::ColorOutOfRange => "must be linear RGB in 0..=1",
            Self::OpacityOutOfRange => "must be an activated opacity in 0..=1",
            Self::Empty => "the splat has no points",
        }
    }
}

/// One failed contract check, with its location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// Field that failed: `position`, `scale`, `color`, `opacity`, `rotation` or `points`.
    pub field: &'static str,
    /// Index of the offending gaussian, when the issue belongs to one.
    pub point: Option<usize>,
    pub reason: ValidationReason,
    /// Bounded rendering of the offending value, e.g. `[0.1, 0, 0.1]`.
    pub detail: String,
}

impl ValidationIssue {
    /// Issue for one gaussian, with a rendered value.
    pub fn new(
        field: &'static str,
        point: Option<usize>,
        reason: ValidationReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            field,
            point,
            reason,
            detail: detail.into(),
        }
    }

    /// Same issue, located at `index`.
    pub fn at(mut self, index: usize) -> Self {
        self.point = Some(index);
        self
    }

    /// Where the failure is, for a reply that lists issues.
    pub fn location(&self) -> String {
        match self.point {
            Some(index) => format!("point {index} {}", self.field),
            None => self.field.to_owned(),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            return write!(formatter, "{}: {}", self.field, self.reason.message());
        }
        write!(
            formatter,
            "{} {} {}",
            self.location(),
            self.detail,
            self.reason.message()
        )
    }
}

/// Limits a caller applies on top of the contract, kept separate from validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationLimits {
    /// Largest allowed gaussian count; `None` checks mathematics only.
    pub max_points: Option<usize>,
}

impl ValidationLimits {
    /// No policy limit: only the contract is checked.
    pub const MATHEMATICAL: Self = Self { max_points: None };

    /// The contract plus a point budget.
    pub const fn with_max_points(max_points: usize) -> Self {
        Self {
            max_points: Some(max_points),
        }
    }

    /// The limit that will actually be applied, for reporting.
    pub fn applied_max_points(self) -> Option<usize> {
        self.max_points
    }
}

impl Default for ValidationLimits {
    /// The limits a caller gets when it does not choose: the crate's point ceiling.
    fn default() -> Self {
        Self::with_max_points(MAX_POINTS)
    }
}

/// Result of checking gaussians against the contract and a caller's limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    /// Version of the contract the values were checked against.
    pub contract_version: u32,
    /// Gaussians that were checked.
    pub point_count: usize,
    /// Bounded list of located issues; [`Self::total_issues`] counts all of them.
    pub issues: Vec<ValidationIssue>,
    /// Issues found, including any beyond [`MAX_REPORTED_ISSUES`].
    pub total_issues: usize,
    /// Gaussians with at least one issue.
    pub offending_points: usize,
    /// True when issues were dropped from the list.
    pub truncated: bool,
    /// Limits applied.
    pub limits: ValidationLimits,
    /// Whether the point count is inside the applied limit.
    pub within_limits: bool,
}

impl ValidationReport {
    /// True when every value satisfied the contract.
    pub fn is_valid(&self) -> bool {
        self.total_issues == 0
    }

    /// True when the values are correct and inside the caller's point limit.
    pub fn is_acceptable(&self) -> bool {
        self.is_valid() && self.within_limits
    }

    /// First located issue, which is the one worth naming in an error message.
    pub fn first_issue(&self) -> Option<&ValidationIssue> {
        self.issues.first()
    }

    /// The point limit that was applied, if any.
    pub fn applied_limit(&self) -> Option<usize> {
        self.limits.applied_max_points()
    }

    /// Policy note, when the point count is above the applied limit.
    pub fn limit_message(&self) -> Option<String> {
        let limit = self.limits.applied_max_points()?;
        if self.within_limits {
            return None;
        }
        Some(format!(
            "{} gaussians is above the applied limit of {limit}; the limit was applied, not \
             the data changed",
            self.point_count
        ))
    }

    /// One line, bounded, for a log or a tool reply.
    pub fn summary(&self) -> String {
        let mut text = if self.is_valid() {
            format!(
                "{} gaussians, no contract issues (version {})",
                self.point_count, self.contract_version
            )
        } else {
            let first = self
                .first_issue()
                .map(ValidationIssue::to_string)
                .unwrap_or_else(|| "unreported".to_owned());
            format!(
                "{} of {} gaussians failed the contract ({} issues); first: {first}",
                self.offending_points, self.point_count, self.total_issues
            )
        };
        if let Some(note) = self.limit_message() {
            text.push_str("; ");
            text.push_str(&note);
        }
        text
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.summary())
    }
}

/// A refusal that carries its structured issues, so every adapter renders it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Located issues, bounded by [`MAX_REPORTED_ISSUES`].
    pub issues: Vec<ValidationIssue>,
    /// Total issues, including any beyond the bounded list.
    pub total_issues: usize,
    /// Gaussians with at least one issue.
    pub offending_points: usize,
    /// Gaussians that were checked.
    pub point_count: usize,
}

impl ValidationError {
    /// Error for a single failing gaussian or value.
    pub fn from_issue(issue: ValidationIssue) -> Self {
        Self {
            issues: vec![issue],
            total_issues: 1,
            offending_points: 1,
            point_count: 1,
        }
    }

    /// Error for a report that found something, or `None` when it found nothing.
    ///
    /// A policy limit is not part of this: it is reported through
    /// [`ValidationReport::limit_message`] and stays visible in the reply.
    pub fn from_report(report: &ValidationReport) -> Option<Self> {
        if report.is_valid() {
            return None;
        }
        Some(Self {
            issues: report.issues.clone(),
            total_issues: report.total_issues,
            offending_points: report.offending_points,
            point_count: report.point_count,
        })
    }

    /// The first issue, for a message that names one thing.
    pub fn first_issue(&self) -> Option<&ValidationIssue> {
        self.issues.first()
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first = self
            .first_issue()
            .map(ValidationIssue::to_string)
            .unwrap_or_else(|| "an unreported contract failure".to_owned());
        write!(
            formatter,
            "{} of {} gaussians are invalid: {first}",
            self.offending_points, self.point_count
        )?;
        let hidden = self.total_issues.saturating_sub(1);
        if hidden > 0 {
            write!(formatter, " (and {hidden} more issues)")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

/// Collects issues while a caller walks its own arrays, bounded by
/// [`MAX_REPORTED_ISSUES`].
///
/// An adapter that validates a batch it built (the Python arrays, an MCP point list) walks
/// its own rows once and records the first failure of each gaussian, so the report is
/// indexed without a second pass.
#[derive(Debug, Clone)]
pub struct IssueRecorder {
    issues: Vec<ValidationIssue>,
    total_issues: usize,
    offending_points: usize,
    cap: usize,
}

impl Default for IssueRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl IssueRecorder {
    /// Empty recorder.
    pub fn new() -> Self {
        Self {
            issues: Vec::new(),
            total_issues: 0,
            offending_points: 0,
            cap: MAX_REPORTED_ISSUES,
        }
    }

    /// Records the first failure of one gaussian.
    ///
    /// Call it at most once per gaussian: the counts stay meaningful because one gaussian
    /// contributes at most one offending point.
    pub fn record(&mut self, issue: ValidationIssue) {
        debug_assert!(
            issue.point.is_some(),
            "a per-gaussian issue should carry its index"
        );
        self.total_issues += 1;
        self.offending_points += 1;
        if self.issues.len() < self.cap {
            self.issues.push(issue);
        }
    }

    /// Records an issue about the whole batch, which no single gaussian owns.
    pub fn record_splat(&mut self, issue: ValidationIssue) {
        debug_assert!(
            issue.point.is_none(),
            "a whole-batch issue has no gaussian index"
        );
        self.total_issues += 1;
        if self.issues.len() < self.cap {
            self.issues.push(issue);
        }
    }

    /// True once something was recorded.
    pub fn has_issues(&self) -> bool {
        self.total_issues > 0
    }

    /// Finishes the report for a batch of `point_count` gaussians.
    pub fn report(self, point_count: usize, limits: ValidationLimits) -> ValidationReport {
        let within_limits = limits
            .applied_max_points()
            .map(|limit| point_count <= limit)
            .unwrap_or(true);
        ValidationReport {
            contract_version: crate::contract::CONTRACT_VERSION,
            point_count,
            truncated: self.total_issues > self.issues.len(),
            issues: self.issues,
            total_issues: self.total_issues,
            offending_points: self.offending_points,
            limits,
            within_limits,
        }
    }

    /// Finishes as a structured error, or `None` when nothing was recorded.
    pub fn error(self, point_count: usize) -> Option<ValidationError> {
        if self.total_issues == 0 {
            return None;
        }
        Some(ValidationError {
            issues: self.issues,
            total_issues: self.total_issues,
            offending_points: self.offending_points,
            point_count,
        })
    }
}

/// Renders the offending values compactly, so a reply stays readable.
fn values_text(values: &[f32]) -> String {
    let rendered: Vec<String> = values.iter().map(|value| format!("{value}")).collect();
    format!("[{}]", rendered.join(", "))
}

/// The first contract failure in one gaussian's raw activated values, if any.
///
/// Values are checked as given: nothing is clamped, defaulted or normalised here, so the
/// caller decides whether to refuse the input or to ask for an explicit repair. The issue
/// carries no point index; use [`check_gaussian`] when the index is known.
pub fn check_values(
    position: [f32; 3],
    scale: [f32; 3],
    color: [f32; 3],
    opacity: f32,
    rotation: [f32; 4],
) -> Option<ValidationIssue> {
    if !position.iter().all(|value| value.is_finite()) {
        return Some(ValidationIssue::new(
            "position",
            None,
            ValidationReason::NonFiniteValue,
            values_text(&position),
        ));
    }
    if !scale.iter().all(|value| value.is_finite()) {
        return Some(ValidationIssue::new(
            "scale",
            None,
            ValidationReason::NonFiniteValue,
            values_text(&scale),
        ));
    }
    if scale.iter().any(|value| *value <= 0.0) {
        return Some(ValidationIssue::new(
            "scale",
            None,
            ValidationReason::NonPositiveScale,
            values_text(&scale),
        ));
    }
    if !color.iter().all(|value| value.is_finite()) {
        return Some(ValidationIssue::new(
            "color",
            None,
            ValidationReason::NonFiniteValue,
            values_text(&color),
        ));
    }
    if color.iter().any(|value| {
        !(-crate::contract::RANGE_TOLERANCE..=1.0 + crate::contract::RANGE_TOLERANCE)
            .contains(value)
    }) {
        return Some(ValidationIssue::new(
            "color",
            None,
            ValidationReason::ColorOutOfRange,
            values_text(&color),
        ));
    }
    if !opacity.is_finite() {
        return Some(ValidationIssue::new(
            "opacity",
            None,
            ValidationReason::NonFiniteValue,
            format!("{opacity}"),
        ));
    }
    if !(-crate::contract::RANGE_TOLERANCE..=1.0 + crate::contract::RANGE_TOLERANCE)
        .contains(&opacity)
    {
        return Some(ValidationIssue::new(
            "opacity",
            None,
            ValidationReason::OpacityOutOfRange,
            format!("{opacity}"),
        ));
    }
    if !rotation.iter().all(|value| value.is_finite()) {
        return Some(ValidationIssue::new(
            "rotation",
            None,
            ValidationReason::NonFiniteValue,
            values_text(&rotation),
        ));
    }
    if !crate::contract::is_usable_quaternion(rotation) {
        return Some(ValidationIssue::new(
            "rotation",
            None,
            ValidationReason::DegenerateQuaternion,
            values_text(&rotation),
        ));
    }
    None
}

/// Same as [`check_values`], located at `index`.
pub fn check_gaussian(
    index: usize,
    position: [f32; 3],
    scale: [f32; 3],
    color: [f32; 3],
    opacity: f32,
    rotation: [f32; 4],
) -> Option<ValidationIssue> {
    check_values(position, scale, color, opacity, rotation).map(|issue| issue.at(index))
}

/// The first contract failure in a model gaussian, if any.
pub fn check_point(index: usize, point: &SplatPoint) -> Option<ValidationIssue> {
    check_gaussian(
        index,
        point.position,
        point.scale,
        point.color,
        point.opacity,
        point.rotation,
    )
}

/// Checks every gaussian and the point budget in a single pass.
pub fn check_splat(points: &[SplatPoint], limits: ValidationLimits) -> ValidationReport {
    let mut recorder = IssueRecorder::new();
    if points.is_empty() {
        recorder.record_splat(ValidationIssue::new(
            "points",
            None,
            ValidationReason::Empty,
            "",
        ));
    }
    for (index, point) in points.iter().enumerate() {
        if let Some(issue) = check_point(index, point) {
            recorder.record(issue);
        }
    }
    recorder.report(points.len(), limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::IDENTITY_QUATERNION;

    fn point(scale: f32, color: f32, opacity: f32) -> SplatPoint {
        SplatPoint {
            position: [0.0; 3],
            scale: [scale; 3],
            color: [color; 3],
            opacity,
            rotation: IDENTITY_QUATERNION,
        }
    }

    #[test]
    fn a_correct_gaussian_has_no_issue() {
        assert!(
            check_values(
                [1.0, 2.0, 3.0],
                [0.1, 0.2, 0.3],
                [0.25, 0.5, 0.75],
                0.5,
                [0.5, 0.5, 0.5, 0.5]
            )
            .is_none()
        );
    }

    #[test]
    fn each_reason_is_reported_once_with_the_offending_field() {
        let cases = [
            (
                ValidationReason::NonFiniteValue,
                check_values(
                    [f32::NAN, 0.0, 0.0],
                    [0.1; 3],
                    [0.5; 3],
                    0.5,
                    IDENTITY_QUATERNION,
                ),
            ),
            (
                ValidationReason::NonPositiveScale,
                check_values(
                    [0.0; 3],
                    [0.1, 0.0, 0.1],
                    [0.5; 3],
                    0.5,
                    IDENTITY_QUATERNION,
                ),
            ),
            (
                ValidationReason::DegenerateQuaternion,
                check_values([0.0; 3], [0.1; 3], [0.5; 3], 0.5, [0.0; 4]),
            ),
            (
                ValidationReason::ColorOutOfRange,
                check_values(
                    [0.0; 3],
                    [0.1; 3],
                    [1.4, 0.0, 0.0],
                    0.5,
                    IDENTITY_QUATERNION,
                ),
            ),
            (
                ValidationReason::OpacityOutOfRange,
                check_values([0.0; 3], [0.1; 3], [0.5; 3], 3.0, IDENTITY_QUATERNION),
            ),
        ];
        for (expected, issue) in cases {
            let issue = issue.expect("a damaged value must be reported");
            assert_eq!(issue.reason, expected);
            assert!(!issue.detail.is_empty());
            assert!(issue.point.is_none());
        }
    }

    #[test]
    fn a_located_issue_names_its_gaussian() {
        let issue = check_gaussian(
            7,
            [0.0; 3],
            [0.1, 0.0, 0.1],
            [0.5; 3],
            0.5,
            IDENTITY_QUATERNION,
        )
        .unwrap();
        assert_eq!(issue.point, Some(7));
        assert_eq!(issue.location(), "point 7 scale");
        let text = issue.to_string();
        assert!(text.starts_with("point 7 scale [0.1, 0, 0.1]"), "{text}");
        assert!(text.contains("positive radius"), "{text}");
    }

    #[test]
    fn a_report_counts_more_than_it_lists() {
        let points: Vec<SplatPoint> = (0..MAX_REPORTED_ISSUES + 8)
            .map(|_| point(0.0, 0.5, 0.5))
            .collect();
        let report = check_splat(&points, ValidationLimits::MATHEMATICAL);
        assert!(!report.is_valid());
        assert_eq!(report.total_issues, MAX_REPORTED_ISSUES + 8);
        assert_eq!(report.offending_points, MAX_REPORTED_ISSUES + 8);
        assert_eq!(report.issues.len(), MAX_REPORTED_ISSUES);
        assert!(report.truncated);
        assert!(report.within_limits, "the budget was not exceeded");
        assert!(
            !report.is_acceptable(),
            "validity is not washed out by the budget"
        );
        let summary = report.summary();
        assert!(summary.contains("40 of 40 gaussians"), "{summary}");
        assert!(summary.len() < 200, "{summary}");
    }

    #[test]
    fn an_empty_splat_is_reported_in_the_words_callers_worked_out_from() {
        let report = check_splat(&[], ValidationLimits::MATHEMATICAL);
        assert!(!report.is_valid());
        assert_eq!(report.total_issues, 1);
        assert_eq!(report.offending_points, 0);
        let text = report.to_string();
        assert!(text.contains("no points"), "{text}");
    }

    #[test]
    fn the_point_limit_is_reported_separately_from_validity() {
        let points = vec![point(0.1, 0.5, 0.5); 5];
        let report = check_splat(&points, ValidationLimits::with_max_points(3));
        assert!(report.is_valid(), "the values are fine");
        assert!(!report.within_limits);
        assert!(!report.is_acceptable());
        assert_eq!(report.applied_limit(), Some(3));
        let note = report.limit_message().unwrap();
        assert!(note.contains("applied limit of 3"), "{note}");
        let text = report.to_string();
        assert!(text.contains("no contract issues"), "{text}");
        assert!(text.contains("applied limit of 3"), "{text}");

        // No limit chosen means no limit applied.
        let unlimited = check_splat(&points, ValidationLimits::MATHEMATICAL);
        assert!(unlimited.within_limits);
        assert_eq!(unlimited.applied_limit(), None);
        assert!(unlimited.limit_message().is_none());
    }

    #[test]
    fn the_recorder_turns_issues_into_a_structured_error() {
        let mut recorder = IssueRecorder::new();
        assert!(recorder.clone().error(3).is_none());
        recorder.record(
            ValidationIssue::new(
                "scale",
                None,
                ValidationReason::NonPositiveScale,
                "[0, 0, 0]",
            )
            .at(2),
        );
        recorder.record(
            ValidationIssue::new(
                "color",
                None,
                ValidationReason::ColorOutOfRange,
                "[2, 0, 0]",
            )
            .at(5),
        );
        assert!(recorder.has_issues());
        let report = recorder.clone().report(6, ValidationLimits::default());
        assert_eq!(report.offending_points, 2);
        assert_eq!(report.total_issues, 2);
        let error = recorder.error(6).unwrap();
        assert_eq!(error.offending_points, 2);
        assert_eq!(error.point_count, 6);
        let text = error.to_string();
        assert!(text.starts_with("2 of 6 gaussians are invalid:"), "{text}");
        assert!(text.contains("point 2 scale"), "{text}");
        assert!(text.contains("and 1 more"), "{text}");
        let splat_error: crate::SplatError = error.into();
        assert!(matches!(splat_error, crate::SplatError::Invalid(_)));
    }

    #[test]
    fn tolerance_admits_a_rounded_float_but_not_a_wrong_one() {
        // A float round trip may push 1.0 just past the range; 1.05 is a mistake.
        assert!(
            check_values(
                [0.0; 3],
                [0.1; 3],
                [1.0005, 0.5, 0.5],
                0.5,
                IDENTITY_QUATERNION
            )
            .is_none()
        );
        assert!(
            check_values(
                [0.0; 3],
                [0.1; 3],
                [1.05, 0.5, 0.5],
                0.5,
                IDENTITY_QUATERNION
            )
            .is_some()
        );
    }

    #[test]
    fn reason_codes_are_stable_for_adapters() {
        assert_eq!(
            ValidationReason::NonPositiveScale.code(),
            "non_positive_scale"
        );
        assert_eq!(ValidationReason::Empty.code(), "empty");
        assert_eq!(
            ValidationReason::DegenerateQuaternion.code(),
            "degenerate_quaternion"
        );
    }
}
