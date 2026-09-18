//! Edit operations over an existing splat.
//!
//! Every operation selects a subset of the points (all of them by default), applies a
//! change and reports how many gaussians it touched. Selection happens against the
//! *original* values of an operation, so `set_color` after `set_color` cannot compound
//! unexpectedly, and a filter followed by a change behaves the way the parameter reads.
//!
//! Colour is fixed per gaussian - there are no spherical harmonics - so `set_color` is
//! the only colour operation and it writes RGB directly.

use crate::contract::{multiply_quaternions, rotate_vector};
use crate::{Result, Splat, SplatError, SplatPoint, normalize_quat};

/// Axis-aligned region used to select points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Box3 {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Box3 {
    /// Builds a box from two opposite corners, tolerating either order per axis.
    pub fn from_corners(a: [f32; 3], b: [f32; 3]) -> Self {
        let mut min = [0.0; 3];
        let mut max = [0.0; 3];
        for axis in 0..3 {
            min[axis] = a[axis].min(b[axis]);
            max[axis] = a[axis].max(b[axis]);
        }
        Self { min, max }
    }

    /// Box grown by `radius` on every side, so a gaussian that overlaps it counts.
    pub fn grown(self, radius: f32) -> Self {
        Self {
            min: self.min.map(|value| value - radius),
            max: self.max.map(|value| value + radius),
        }
    }

    pub fn contains(&self, point: [f32; 3]) -> bool {
        (0..3).all(|axis| point[axis] >= self.min[axis] && point[axis] <= self.max[axis])
    }

    pub fn is_finite(&self) -> bool {
        self.min
            .into_iter()
            .chain(self.max)
            .all(|value| value.is_finite())
    }
}

/// Which points an operation applies to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    /// Keep points inside this box.
    pub within: Option<Box3>,
    /// Keep points *outside* this box.
    pub outside: Option<Box3>,
    /// Keep points whose mean colour is at least this value per channel.
    pub color_min: Option<[f32; 3]>,
    /// Keep points whose mean colour is at most this value per channel.
    pub color_max: Option<[f32; 3]>,
    /// Keep points whose opacity is at least this value.
    pub opacity_min: Option<f32>,
    /// Keep points whose largest radius is at most this value.
    pub max_radius: Option<f32>,
    /// Keep the first N points.
    pub first: Option<usize>,
}

impl Selection {
    /// True when nothing is filtered, i.e. every point is selected.
    pub fn is_all(&self) -> bool {
        self.within.is_none()
            && self.outside.is_none()
            && self.color_min.is_none()
            && self.color_max.is_none()
            && self.opacity_min.is_none()
            && self.max_radius.is_none()
            && self.first.is_none()
    }

    /// Indexes of the selected points.
    pub fn indices(&self, splat: &Splat) -> Vec<usize> {
        let mut selected: Vec<usize> = (0..splat.len())
            .filter(|index| self.matches(&splat.points[*index]))
            .collect();
        if let Some(limit) = self.first {
            selected.truncate(limit);
        }
        selected
    }

    fn matches(&self, point: &SplatPoint) -> bool {
        if let Some(within) = self.within {
            if !within.contains(point.position) {
                return false;
            }
        }
        if let Some(outside) = self.outside {
            if outside.contains(point.position) {
                return false;
            }
        }
        if let Some(min) = self.color_min {
            if (0..3).any(|axis| point.color[axis] < min[axis]) {
                return false;
            }
        }
        if let Some(max) = self.color_max {
            if (0..3).any(|axis| point.color[axis] > max[axis]) {
                return false;
            }
        }
        if let Some(min) = self.opacity_min {
            if point.opacity < min {
                return false;
            }
        }
        if let Some(max) = self.max_radius {
            if point
                .scale
                .iter()
                .fold(0.0f32, |acc, value| acc.max(*value))
                > max
            {
                return false;
            }
        }
        true
    }
}

/// One change to apply.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Add `offset` to the selected positions.
    Translate { by: [f32; 3] },
    /// Rotate positions and orientations around a centre.
    Rotate {
        /// Rotation axis; normalised internally.
        axis: [f32; 3],
        /// Rotation angle in degrees.
        degrees: f32,
        /// Point the rotation happens around.
        center: [f32; 3],
    },
    /// Multiply positions relative to a centre.
    Scale { center: [f32; 3], factor: [f32; 3] },
    /// Multiply the gaussian radii by `factor`.
    SetRadius { factor: f32 },
    /// Add per-channel deltas to the colour.
    AdjustColor { delta: [f32; 3] },
    /// Replace the colour, blended by `mix` (`1` replaces it outright).
    SetColor { color: [f32; 3], mix: f32 },
    /// Multiply the opacity.
    SetOpacity { factor: f32 },
    /// Copy the selection and offset the copies.
    Duplicate { by: [f32; 3] },
    /// Delete the selection.
    Remove,
    /// Add gaussians at the end, keeping the rest untouched.
    Merge { points: Vec<SplatPoint> },
}

/// Result of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpReport {
    /// Gaussians the operation changed, or for `Remove`, how many it deleted.
    pub affected: usize,
    /// Gaussians in the splat afterwards.
    pub remaining: usize,
}

impl OpReport {
    fn new(affected: usize, splat: &Splat) -> Self {
        Self {
            affected,
            remaining: splat.len(),
        }
    }
}

/// A single edit step: what to change and which points it applies to.
#[derive(Debug, Clone, PartialEq)]
pub struct EditStep {
    pub op: EditOp,
    pub selection: Selection,
}

impl EditStep {
    pub fn new(op: EditOp) -> Self {
        Self {
            op,
            selection: Selection::default(),
        }
    }

    pub fn with_selection(op: EditOp, selection: Selection) -> Self {
        Self { op, selection }
    }
}

/// Applies one step to a splat in place.
pub fn apply(splat: &mut Splat, step: &EditStep) -> Result<OpReport> {
    validate_op(&step.op)?;
    validate_selection(&step.selection)?;

    // `Merge` appends rather than selecting, so it does not go through the selection.
    if let EditOp::Merge { points } = &step.op {
        if points.is_empty() {
            return Err(SplatError::Format(
                "merge needs at least one point".to_owned(),
            ));
        }
        splat.points.extend(points.iter().copied());
        splat.validate()?;
        return Ok(OpReport::new(points.len(), splat));
    }

    let indices = step.selection.indices(splat);
    if indices.is_empty() {
        return Err(SplatError::Format(
            "no gaussian matched the selection; nothing was changed".to_owned(),
        ));
    }
    let affected = indices.len();

    apply_to_indices(splat, &indices, &step.op)?;
    splat.validate()?;
    Ok(OpReport::new(affected, splat))
}

/// Applies one operation to the given point rows, without resolving a selection.
///
/// `indices` must be ascending and in range, which is what [`Selection::indices`] produces and
/// what the transaction runner maintains through stable point identities. Nothing here
/// validates the splat as a whole, so a batch of steps is validated once, at its end; a single
/// [`apply`] call adds that validation itself.
///
/// `Merge` is *not* handled here: it appends rather than selects, so it never has point rows
/// to act on. [`apply`] handles it before resolving a selection, and the transaction runner
/// handles it as an append.
pub(crate) fn apply_to_indices(splat: &mut Splat, indices: &[usize], op: &EditOp) -> Result<()> {
    debug_assert!(
        indices.windows(2).all(|pair| pair[0] < pair[1]),
        "indices must be ascending and distinct"
    );
    debug_assert!(
        indices.iter().all(|index| *index < splat.len()),
        "indices must be inside the splat"
    );
    match op {
        EditOp::Merge { .. } => {
            return Err(SplatError::Format(
                "merge appends points; it is not applied to a selection".to_owned(),
            ));
        }
        EditOp::Translate { by } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                for axis in 0..3 {
                    point.position[axis] += by[axis];
                }
            }
        }
        EditOp::Rotate {
            axis,
            degrees,
            center,
        } => {
            let rotation = rotation_quaternion(*axis, *degrees);
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                let local = [
                    point.position[0] - center[0],
                    point.position[1] - center[1],
                    point.position[2] - center[2],
                ];
                let rotated = rotate_vector(local, rotation);
                point.position = [
                    center[0] + rotated[0],
                    center[1] + rotated[1],
                    center[2] + rotated[2],
                ];
                point.rotation = normalize_quat(multiply_quaternions(rotation, point.rotation));
            }
        }
        EditOp::Scale { center, factor } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                for axis in 0..3 {
                    point.position[axis] =
                        center[axis] + (point.position[axis] - center[axis]) * factor[axis];
                    point.scale[axis] = (point.scale[axis] * factor[axis]).max(f32::MIN_POSITIVE);
                }
            }
        }
        EditOp::SetRadius { factor } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                point.scale = point
                    .scale
                    .map(|value| (value * factor).clamp(f32::MIN_POSITIVE, 1.0e6));
            }
        }
        EditOp::AdjustColor { delta } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                point.color = (0..3)
                    .map(|axis| (point.color[axis] + delta[axis]).clamp(0.0, 1.0))
                    .collect::<Vec<f32>>()
                    .try_into()
                    .expect("three channels");
            }
        }
        EditOp::SetColor { color, mix } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                for axis in 0..3 {
                    point.color[axis] =
                        (point.color[axis] * (1.0 - mix) + color[axis] * mix).clamp(0.0, 1.0);
                }
            }
        }
        EditOp::SetOpacity { factor } => {
            for index in indices.iter().copied() {
                let point = &mut splat.points[index];
                point.opacity = (point.opacity * factor).clamp(0.0, 1.0);
            }
        }
        EditOp::Duplicate { by } => {
            let copies: Vec<SplatPoint> = indices
                .iter()
                .map(|index| {
                    let mut copy = splat.points[*index];
                    for axis in 0..3 {
                        copy.position[axis] += by[axis];
                    }
                    copy
                })
                .collect();
            splat.points.extend(copies);
        }
        EditOp::Remove => {
            // Remove from the back so earlier indices stay valid.
            for index in indices.iter().rev() {
                splat.points.remove(*index);
            }
        }
    }

    Ok(())
}

/// Applies several steps in order, stopping at the first failure.
pub fn apply_all(splat: &mut Splat, steps: &[EditStep]) -> Result<Vec<OpReport>> {
    if steps.is_empty() {
        return Err(SplatError::Format(
            "no operations given; pass at least one step".to_owned(),
        ));
    }
    let mut reports = Vec::with_capacity(steps.len());
    for step in steps {
        reports.push(apply(splat, step)?);
    }
    Ok(reports)
}

pub(crate) fn validate_op(op: &EditOp) -> Result<()> {
    let finite = |values: &[f32], name: &str| -> Result<()> {
        if values.iter().all(|value| value.is_finite()) {
            Ok(())
        } else {
            Err(SplatError::Format(format!("{name} must be finite numbers")))
        }
    };
    match op {
        EditOp::Translate { by } => finite(by, "translate.by"),
        EditOp::Rotate {
            axis,
            degrees,
            center,
        } => {
            let norm = axis.iter().map(|value| value * value).sum::<f32>();
            if !norm.is_finite() || norm < 1e-12 {
                return Err(SplatError::Format(
                    "rotate.axis must be a non-zero direction".to_owned(),
                ));
            }
            if !degrees.is_finite() {
                return Err(SplatError::Format(
                    "rotate.degrees must be finite".to_owned(),
                ));
            }
            finite(center, "rotate.center")
        }
        EditOp::Scale { center, factor } => {
            finite(center, "scale.center")?;
            finite(factor, "scale.factor")?;
            if factor.iter().any(|value| *value <= 0.0) {
                return Err(SplatError::Format(
                    "scale.factor must be positive on every axis".to_owned(),
                ));
            }
            Ok(())
        }
        EditOp::SetRadius { factor } => {
            if !factor.is_finite() || *factor <= 0.0 {
                return Err(SplatError::Format(
                    "set_radius.factor must be a positive number".to_owned(),
                ));
            }
            Ok(())
        }
        EditOp::AdjustColor { delta } => finite(delta, "adjust_color.delta"),
        EditOp::SetColor { color, mix } => {
            finite(color, "set_color.color")?;
            if !mix.is_finite() || !(0.0..=1.0).contains(mix) {
                return Err(SplatError::Format(
                    "set_color.mix must be in 0..=1".to_owned(),
                ));
            }
            Ok(())
        }
        EditOp::SetOpacity { factor } => {
            if !factor.is_finite() || *factor < 0.0 {
                return Err(SplatError::Format(
                    "set_opacity.factor must be a non-negative number".to_owned(),
                ));
            }
            Ok(())
        }
        EditOp::Duplicate { by } => finite(by, "duplicate.by"),
        EditOp::Remove => Ok(()),
        EditOp::Merge { points } => {
            if points.len() > crate::MAX_POINTS {
                return Err(SplatError::Format(format!(
                    "merge holds {} points, above the {} point limit",
                    points.len(),
                    crate::MAX_POINTS
                )));
            }
            Ok(())
        }
    }
}

pub(crate) fn validate_selection(selection: &Selection) -> Result<()> {
    for (name, box3) in [("within", selection.within), ("outside", selection.outside)] {
        if let Some(box3) = box3 {
            if !box3.is_finite() {
                return Err(SplatError::Format(format!(
                    "{name} must hold finite corner values"
                )));
            }
        }
    }
    for (name, color) in [
        ("color_min", selection.color_min),
        ("color_max", selection.color_max),
    ] {
        if let Some(color) = color {
            if color.iter().any(|value| !value.is_finite()) {
                return Err(SplatError::Format(format!("{name} must be finite")));
            }
        }
    }
    if let Some(opacity) = selection.opacity_min {
        if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
            return Err(SplatError::Format(
                "opacity_min must be in 0..=1".to_owned(),
            ));
        }
    }
    if let Some(radius) = selection.max_radius {
        if !radius.is_finite() || radius <= 0.0 {
            return Err(SplatError::Format(
                "max_radius must be a positive number".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Shared rotation math lives in [`crate::contract`]: the contract owns the meaning of a
/// quaternion, so an edit and a renderer cannot disagree about which way a rotation turns.
///
/// Quaternion `(w, x, y, z)` for a rotation of `degrees` around `axis`.
fn rotation_quaternion(axis: [f32; 3], degrees: f32) -> [f32; 4] {
    let norm = axis.iter().map(|value| value * value).sum::<f32>().sqrt();
    let unit = axis.map(|value| value / norm);
    let half = degrees.to_radians() * 0.5;
    let (sin, cos) = half.sin_cos();
    [cos, unit[0] * sin, unit[1] * sin, unit[2] * sin]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(count: usize, spacing: f32) -> Splat {
        Splat::from_points(
            (0..count)
                .map(|index| {
                    let x = index as f32 * spacing;
                    SplatPoint::new(
                        [x, 0.0, 0.0],
                        [0.1, 0.1, 0.1],
                        [0.5, 0.5, 0.5],
                        0.8,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn translate_moves_every_point_by_default() {
        let mut splat = grid(3, 1.0);
        let report = apply(
            &mut splat,
            &EditStep::new(EditOp::Translate {
                by: [0.0, 1.0, 0.0],
            }),
        )
        .unwrap();
        assert_eq!(report.affected, 3);
        assert_eq!(report.remaining, 3);
        assert!(splat.points.iter().all(|point| point.position[1] == 1.0));
    }

    #[test]
    fn a_box_selection_narrows_the_operation() {
        let mut splat = grid(5, 1.0);
        // Points sit at x = 0, 1, 2, 3, 4 and the box bounds are inclusive, so the
        // selection is x = 1 and x = 2.
        let selection = Selection {
            within: Some(Box3::from_corners([0.5, -1.0, -1.0], [2.5, 1.0, 1.0])),
            ..Selection::default()
        };
        let report = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::Translate {
                    by: [0.0, 0.0, 5.0],
                },
                selection,
            ),
        )
        .unwrap();
        assert_eq!(report.affected, 2);
        assert_eq!(splat.points[0].position[2], 0.0);
        assert_eq!(splat.points[1].position[2], 5.0);
        assert_eq!(splat.points[2].position[2], 5.0);
        assert_eq!(splat.points[3].position[2], 0.0);
        assert_eq!(splat.points[4].position[2], 0.0);
    }

    #[test]
    fn an_outside_box_inverts_the_selection() {
        let mut splat = grid(5, 1.0);
        let selection = Selection {
            outside: Some(Box3::from_corners([0.5, -1.0, -1.0], [3.5, 1.0, 1.0])),
            ..Selection::default()
        };
        let report = apply(
            &mut splat,
            &EditStep::with_selection(EditOp::Remove, selection),
        )
        .unwrap();
        // x = 1, 2, 3 fall inside the box, so x = 0 and x = 4 are removed.
        assert_eq!(report.affected, 2);
        assert_eq!(splat.len(), 3);
        assert_eq!(splat.points[0].position[0], 1.0);
        assert_eq!(splat.points.last().unwrap().position[0], 3.0);
    }

    #[test]
    fn colour_and_opacity_filters_select_as_documented() {
        let mut splat = Splat::from_points(vec![
            SplatPoint::new(
                [0.0; 3],
                [0.1; 3],
                [1.0, 0.0, 0.0],
                1.0,
                [1.0, 0.0, 0.0, 0.0],
            ),
            SplatPoint::new(
                [1.0, 0.0, 0.0],
                [0.1; 3],
                [0.0, 1.0, 0.0],
                0.2,
                [1.0, 0.0, 0.0, 0.0],
            ),
            SplatPoint::new(
                [2.0, 0.0, 0.0],
                [0.5; 3],
                [0.0, 0.0, 1.0],
                0.9,
                [1.0, 0.0, 0.0, 0.0],
            ),
        ]);
        // Keep only the opaque gaussians.
        let report = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::Remove,
                Selection {
                    opacity_min: Some(0.5),
                    ..Selection::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(report.affected, 2);
        assert_eq!(splat.len(), 1);
        assert_eq!(splat.points[0].color, [0.0, 1.0, 0.0]);

        // Keep only the red one by colour, using max_radius to exclude nothing.
        let mut splat = grid(3, 1.0);
        splat.points[0].color = [1.0, 0.0, 0.0];
        let report = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::SetOpacity { factor: 0.5 },
                Selection {
                    color_min: Some([0.9, 0.0, 0.0]),
                    max_radius: Some(1.0),
                    ..Selection::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(report.affected, 1);
        assert!((splat.points[0].opacity - 0.4).abs() < 1e-6);
        assert!((splat.points[1].opacity - 0.8).abs() < 1e-6);
    }

    #[test]
    fn first_limits_the_selection_to_a_prefix() {
        let mut splat = grid(4, 1.0);
        let report = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::Remove,
                Selection {
                    first: Some(2),
                    ..Selection::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(report.affected, 2);
        assert_eq!(splat.len(), 2);
        assert_eq!(splat.points[0].position[0], 2.0);
    }

    #[test]
    fn rotate_turns_positions_and_orientations_around_a_centre() {
        let mut splat = Splat::from_points(vec![SplatPoint::new(
            [1.0, 0.0, 0.0],
            [0.1; 3],
            [0.5; 3],
            1.0,
            [1.0, 0.0, 0.0, 0.0],
        )]);
        apply(
            &mut splat,
            &EditStep::new(EditOp::Rotate {
                axis: [0.0, 1.0, 0.0],
                degrees: 90.0,
                center: [0.0; 3],
            }),
        )
        .unwrap();
        let position = splat.points[0].position;
        assert!(position[0].abs() < 1e-6, "{position:?}");
        assert!((position[2] + 1.0).abs() < 1e-6, "{position:?}");
        // The orientation picked up the same rotation.
        let [w, x, y, z] = splat.points[0].rotation;
        assert!((w - 0.7071).abs() < 1e-3, "{w}");
        assert!((y - 0.7071).abs() < 1e-3, "{y}");
        assert!(x.abs() < 1e-6 && z.abs() < 1e-6);
    }

    #[test]
    fn scale_resizes_positions_and_radii() {
        let mut splat = grid(2, 2.0);
        apply(
            &mut splat,
            &EditStep::new(EditOp::Scale {
                center: [0.0; 3],
                factor: [2.0, 1.0, 1.0],
            }),
        )
        .unwrap();
        assert_eq!(splat.points[0].position[0], 0.0);
        assert_eq!(splat.points[1].position[0], 4.0);
        assert!((splat.points[1].scale[0] - 0.2).abs() < 1e-6);
        assert!((splat.points[1].scale[1] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn set_radius_scales_every_axis() {
        let mut splat = grid(1, 1.0);
        apply(
            &mut splat,
            &EditStep::new(EditOp::SetRadius { factor: 3.0 }),
        )
        .unwrap();
        assert_eq!(splat.points[0].scale, [0.30000001192092896; 3]);
    }

    #[test]
    fn colour_operations_are_clamped() {
        let mut splat = grid(1, 1.0);
        apply(
            &mut splat,
            &EditStep::new(EditOp::AdjustColor {
                delta: [0.8, -0.8, 0.0],
            }),
        )
        .unwrap();
        assert_eq!(splat.points[0].color, [1.0, 0.0, 0.5]);

        apply(
            &mut splat,
            &EditStep::new(EditOp::SetColor {
                color: [0.0, 1.0, 0.0],
                mix: 0.5,
            }),
        )
        .unwrap();
        assert!((splat.points[0].color[0] - 0.5).abs() < 1e-6);
        assert!((splat.points[0].color[1] - 0.5).abs() < 1e-6);
        assert!((splat.points[0].color[2] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn duplicate_appends_offset_copies() {
        let mut splat = grid(2, 1.0);
        let report = apply(
            &mut splat,
            &EditStep::new(EditOp::Duplicate {
                by: [0.0, 2.0, 0.0],
            }),
        )
        .unwrap();
        assert_eq!(report.affected, 2);
        assert_eq!(splat.len(), 4);
        assert_eq!(splat.points[0].position, [0.0, 0.0, 0.0]);
        assert_eq!(splat.points[2].position, [0.0, 2.0, 0.0]);
        assert_eq!(splat.points[3].position, [1.0, 2.0, 0.0]);
    }

    #[test]
    fn remove_leaves_the_other_points_untouched() {
        let mut splat = grid(4, 1.0);
        // Points at x = 0, 1, 2, 3; the box covers x = 2 only.
        let report = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::Remove,
                Selection {
                    within: Some(Box3::from_corners([1.5, -1.0, -1.0], [2.5, 1.0, 1.0])),
                    ..Selection::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(report.affected, 1);
        assert_eq!(report.remaining, 3);
        assert_eq!(splat.points[0].position[0], 0.0);
        assert_eq!(splat.points[1].position[0], 1.0);
        assert_eq!(splat.points[2].position[0], 3.0);
    }

    #[test]
    fn merge_appends_points() {
        let mut splat = grid(1, 1.0);
        let report = apply(
            &mut splat,
            &EditStep::new(EditOp::Merge {
                points: vec![SplatPoint::new(
                    [5.0, 5.0, 5.0],
                    [0.05; 3],
                    [0.1, 0.2, 0.3],
                    0.5,
                    [1.0, 0.0, 0.0, 0.0],
                )],
            }),
        )
        .unwrap();
        assert_eq!(report.affected, 1);
        assert_eq!(report.remaining, 2);
        assert_eq!(splat.points[1].position, [5.0, 5.0, 5.0]);
    }

    #[test]
    fn a_selection_that_matches_nothing_is_reported() {
        let mut splat = grid(2, 1.0);
        let error = apply(
            &mut splat,
            &EditStep::with_selection(
                EditOp::Remove,
                Selection {
                    within: Some(Box3::from_corners([10.0; 3], [11.0; 3])),
                    ..Selection::default()
                },
            ),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no gaussian matched"), "{error}");
        assert_eq!(splat.len(), 2, "a failed op must not modify the splat");
    }

    #[test]
    fn invalid_operations_are_refused_before_anything_changes() {
        let mut splat = grid(1, 1.0);
        let cases = [
            EditOp::Translate {
                by: [f32::NAN, 0.0, 0.0],
            },
            EditOp::Rotate {
                axis: [0.0; 3],
                degrees: 45.0,
                center: [0.0; 3],
            },
            EditOp::Scale {
                center: [0.0; 3],
                factor: [0.0, 1.0, 1.0],
            },
            EditOp::SetRadius { factor: -1.0 },
            EditOp::SetColor {
                color: [0.5; 3],
                mix: 1.5,
            },
            EditOp::SetOpacity { factor: -0.5 },
            EditOp::Merge { points: Vec::new() },
        ];
        for op in cases {
            let error = apply(&mut splat, &EditStep::new(op.clone())).unwrap_err();
            assert!(
                !error.to_string().is_empty(),
                "{op:?} should explain itself: {error}"
            );
        }
        assert_eq!(splat.len(), 1);
    }

    #[test]
    fn steps_apply_in_order_and_stop_at_the_first_failure() {
        let mut splat = grid(2, 1.0);
        let reports = apply_all(
            &mut splat,
            &[
                EditStep::new(EditOp::Translate {
                    by: [0.0, 1.0, 0.0],
                }),
                EditStep::new(EditOp::Duplicate {
                    by: [0.0, 0.0, 1.0],
                }),
            ],
        )
        .unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].remaining, 2);
        assert_eq!(reports[1].remaining, 4);

        let error = apply_all(&mut splat, &[]).unwrap_err().to_string();
        assert!(error.contains("no operations"), "{error}");

        let error = apply_all(
            &mut splat,
            &[
                EditStep::new(EditOp::Translate {
                    by: [0.0, 0.0, 1.0],
                }),
                EditStep::new(EditOp::SetRadius { factor: 0.0 }),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("positive"), "{error}");
    }

    #[test]
    fn a_box_grows_by_a_radius() {
        let box3 = Box3::from_corners([1.0, 2.0, 3.0], [-1.0, -2.0, -3.0]);
        assert_eq!(box3.min, [-1.0, -2.0, -3.0]);
        assert_eq!(box3.max, [1.0, 2.0, 3.0]);
        let grown = box3.grown(0.5);
        assert_eq!(grown.min, [-1.5, -2.5, -3.5]);
        assert!(grown.contains([1.4, 0.0, 0.0]));
        assert!(!box3.contains([1.4, 0.0, 0.0]));
    }

    #[test]
    fn a_default_selection_is_all_points() {
        assert!(Selection::default().is_all());
        assert_eq!(Selection::default().indices(&grid(3, 1.0)).len(), 3);
        assert!(
            !Selection {
                first: Some(1),
                ..Selection::default()
            }
            .is_all()
        );
    }
}
