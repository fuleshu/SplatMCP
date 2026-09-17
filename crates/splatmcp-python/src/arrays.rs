//! The Gaussian array contract Python authors fill in and Rust validates.
//!
//! Every field is *activated* data, not file-native encodings, so a script writes the
//! values it reasons about and no hidden conversion can hide a malformed array:
//!
//! | field       | shape | unit / range                          |
//! |-------------|-------|---------------------------------------|
//! | `positions` | (N,3) | metres, document space                |
//! | `scales`    | (N,3) | strictly positive radii, metres      |
//! | `rotations` | (N,4) | quaternion `(w,x,y,z)`, unit length   |
//! | `colors`    | (N,3) | linear RGB in `0..=1`                 |
//! | `opacities` | (N)   | `0..=1`                               |
//!
//! Validation happens on the raw values, before the core constructors that clamp, so a
//! `NaN`, a zero radius or a `(0,0,0,0)` quaternion is reported as an invalid batch
//! instead of being silently repaired.

use serde::{Deserialize, Serialize};
use splatmcp_core::{Bounds, Splat, SplatPoint};

use crate::{PythonError, Result};

/// Largest batch a single job may produce.
///
/// The core model tops out at [`splatmcp_core::MAX_POINTS`]; the Python side keeps the
/// same ceiling so a runaway script fails with a budget error instead of exhausting
/// memory.
pub const MAX_BATCH_POINTS: usize = splatmcp_core::MAX_POINTS;

/// Tolerance for colours and opacities that a float round trip may push just outside
/// `0..=1`. Anything further out is rejected rather than silently clamped.
pub const RANGE_TOLERANCE: f32 = 1e-3;

/// Quaternion lengths accepted as already normalised; anything else is rescaled.
pub const QUATERNION_TOLERANCE: f32 = 1e-3;

/// Smallest quaternion length that can be normalised; below this it is treated as
/// degenerate and rejected.
pub const QUATERNION_MIN_NORM: f32 = 1e-6;

/// Provenance a script may attach to a batch: which component it is, which recipe
/// produced it, and which seed it used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BatchMetadata {
    /// Named component this batch replaces or creates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Recipe or helper that produced the geometry, for reproducibility records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// Seed the script used, when it differs from the job seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// A detached candidate result: plain Rust-owned arrays, never a live scene buffer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GaussianBatch {
    pub positions: Vec<[f32; 3]>,
    pub scales: Vec<[f32; 3]>,
    pub rotations: Vec<[f32; 4]>,
    pub colors: Vec<[f32; 3]>,
    pub opacities: Vec<f32>,
    pub metadata: BatchMetadata,
}

impl GaussianBatch {
    /// Empty batch with room for `points` gaussians, so a loop does not reallocate.
    pub fn with_capacity(points: usize) -> Self {
        Self {
            positions: Vec::with_capacity(points),
            scales: Vec::with_capacity(points),
            rotations: Vec::with_capacity(points),
            colors: Vec::with_capacity(points),
            opacities: Vec::with_capacity(points),
            metadata: BatchMetadata::default(),
        }
    }

    /// Number of gaussians, taken from the position array.
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Appends one gaussian.
    pub fn push(
        &mut self,
        position: [f32; 3],
        scale: [f32; 3],
        rotation: [f32; 4],
        color: [f32; 3],
        opacity: f32,
    ) {
        self.positions.push(position);
        self.scales.push(scale);
        self.rotations.push(rotation);
        self.colors.push(color);
        self.opacities.push(opacity);
    }

    /// Checks the whole contract and the `max_points` budget.
    ///
    /// The first failure is reported with the index of the offending gaussian, because a
    /// 500k point batch is unusable to debug without knowing *where* it broke.
    pub fn validate(&self, max_points: usize) -> Result<()> {
        let count = self.len();
        if count == 0 {
            return Err(PythonError::InvalidBatch(
                "the batch has no gaussians".to_owned(),
            ));
        }
        if count > max_points {
            return Err(PythonError::BudgetExceeded(format!(
                "the batch holds {count} gaussians, above the {max_points} point budget"
            )));
        }
        check_lengths(self)?;
        for index in 0..count {
            check_position(index, self.positions[index])?;
            check_scale(index, self.scales[index])?;
            check_rotation(index, self.rotations[index])?;
            check_color(index, self.colors[index])?;
            check_opacity(index, self.opacities[index])?;
        }
        Ok(())
    }

    /// Converts a validated batch into core model points.
    ///
    /// Colours and opacities inside the tolerance band are clamped, and quaternions are
    /// normalised; everything else was already rejected by [`Self::validate`].
    pub fn to_splat(&self, max_points: usize) -> Result<Splat> {
        self.validate(max_points)?;
        let points = (0..self.len())
            .map(|index| {
                SplatPoint::new(
                    self.positions[index],
                    self.scales[index],
                    self.colors[index].map(|value| value.clamp(0.0, 1.0)),
                    self.opacities[index].clamp(0.0, 1.0),
                    normalize_rotation(self.rotations[index]),
                )
            })
            .collect();
        Ok(Splat::from_points(points))
    }

    /// Copies a core splat into a batch, so a source snapshot and a generated candidate
    /// travel through the same representation.
    pub fn from_splat(splat: &Splat, metadata: BatchMetadata) -> Self {
        let mut batch = Self::with_capacity(splat.len());
        for point in &splat.points {
            batch.push(
                point.position,
                point.scale,
                point.rotation,
                point.color,
                point.opacity,
            );
        }
        batch.metadata = metadata;
        batch
    }

    /// Concatenates several batches into one, for a job that builds several parts.
    ///
    /// Empty input is rejected: a job that produced nothing is a failure, not an empty
    /// document.
    pub fn merge(batches: Vec<GaussianBatch>) -> Result<Self> {
        let mut merged = Self::default();
        let mut components = Vec::new();
        for batch in batches {
            merged.positions.extend_from_slice(&batch.positions);
            merged.scales.extend_from_slice(&batch.scales);
            merged.rotations.extend_from_slice(&batch.rotations);
            merged.colors.extend_from_slice(&batch.colors);
            merged.opacities.extend_from_slice(&batch.opacities);
            if let Some(component) = batch.metadata.component_id {
                components.push(component);
            }
        }
        if merged.is_empty() {
            return Err(PythonError::InvalidBatch(
                "no batch returned any gaussians".to_owned(),
            ));
        }
        merged.metadata = BatchMetadata {
            component_id: components.first().cloned(),
            recipe: Some(format!("merge of {} batches", components.len())),
            seed: None,
        };
        Ok(merged)
    }

    /// Bounds of the batch, using the same padding rule as the core model.
    pub fn bounds(&self) -> Option<Bounds> {
        let first = self.positions.first()?;
        let mut min = *first;
        let mut max = *first;
        for (position, scale) in self.positions.iter().zip(self.scales.iter()) {
            let pad = scale.iter().copied().fold(0.0f32, f32::max);
            for axis in 0..3 {
                min[axis] = min[axis].min(position[axis] - pad);
                max[axis] = max[axis].max(position[axis] + pad);
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
}

/// Serializable bounds, mirroring [`splatmcp_core::Bounds`].
///
/// The core model deliberately carries no serde dependency, so a bound that has to travel
/// in a job reply is converted into this type at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundsOut {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    /// Longest half extent.
    pub radius: f32,
}

impl From<Bounds> for BoundsOut {
    fn from(bounds: Bounds) -> Self {
        Self {
            min: bounds.min,
            max: bounds.max,
            center: bounds.center,
            radius: bounds.radius,
        }
    }
}

/// Rescales a quaternion to unit length.
///
/// The core model applies the same policy, so a batch that validated here cannot change
/// meaning on the way into the document.
pub fn normalize_rotation(rotation: [f32; 4]) -> [f32; 4] {
    let norm = rotation
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm <= QUATERNION_MIN_NORM || !norm.is_finite() {
        return [1.0, 0.0, 0.0, 0.0];
    }
    rotation.map(|value| value / norm)
}

fn check_lengths(batch: &GaussianBatch) -> Result<()> {
    let count = batch.len();
    let lengths = [
        ("scales", batch.scales.len()),
        ("rotations", batch.rotations.len()),
        ("colors", batch.colors.len()),
        ("opacities", batch.opacities.len()),
    ];
    for (name, actual) in lengths {
        if actual != count {
            return Err(PythonError::InvalidBatch(format!(
                "{name} holds {actual} entries but positions holds {count}; \
                 every array must describe the same gaussians"
            )));
        }
    }
    Ok(())
}

fn check_position(index: usize, position: [f32; 3]) -> Result<()> {
    if !position.iter().all(|value| value.is_finite()) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-finite position {position:?}"
        )));
    }
    Ok(())
}

fn check_scale(index: usize, scale: [f32; 3]) -> Result<()> {
    if !scale.iter().all(|value| value.is_finite()) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-finite scale {scale:?}"
        )));
    }
    if scale.iter().any(|value| *value <= 0.0) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-positive scale {scale:?}; scales are activated \
             radii, not PLY log-scales"
        )));
    }
    Ok(())
}

fn check_rotation(index: usize, rotation: [f32; 4]) -> Result<()> {
    if !rotation.iter().all(|value| value.is_finite()) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-finite quaternion {rotation:?}"
        )));
    }
    let norm = rotation
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm <= QUATERNION_MIN_NORM {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a {norm} length quaternion {rotation:?}; quaternions are \
             (w,x,y,z) and must not be all zero"
        )));
    }
    Ok(())
}

fn check_color(index: usize, color: [f32; 3]) -> Result<()> {
    if !color.iter().all(|value| value.is_finite()) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-finite colour {color:?}"
        )));
    }
    if color
        .iter()
        .any(|value| !(-RANGE_TOLERANCE..=1.0 + RANGE_TOLERANCE).contains(value))
    {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a colour {color:?} outside the linear RGB 0..=1 range"
        )));
    }
    Ok(())
}

fn check_opacity(index: usize, opacity: f32) -> Result<()> {
    if !opacity.is_finite() {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has a non-finite opacity {opacity}"
        )));
    }
    if !(-RANGE_TOLERANCE..=1.0 + RANGE_TOLERANCE).contains(&opacity) {
        return Err(PythonError::InvalidBatch(format!(
            "gaussian {index} has opacity {opacity} outside 0..=1; opacity is the \
             activated value, not a logit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_batch() -> GaussianBatch {
        let mut batch = GaussianBatch::with_capacity(1);
        batch.push(
            [1.0, 2.0, 3.0],
            [0.1, 0.2, 0.3],
            [1.0, 0.0, 0.0, 0.0],
            [0.25, 0.5, 0.75],
            0.5,
        );
        batch
    }

    #[test]
    fn a_well_formed_batch_converts_to_the_core_model() {
        let batch = unit_batch();
        batch.validate(MAX_BATCH_POINTS).unwrap();
        let splat = batch.to_splat(MAX_BATCH_POINTS).unwrap();
        assert_eq!(splat.len(), 1);
        assert_eq!(splat.points[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(splat.points[0].scale, [0.1, 0.2, 0.3]);
        assert_eq!(splat.points[0].color, [0.25, 0.5, 0.75]);
        assert_eq!(splat.points[0].opacity, 0.5);
    }

    #[test]
    fn an_empty_batch_is_rejected() {
        let error = GaussianBatch::default().validate(MAX_BATCH_POINTS).unwrap_err();
        assert_eq!(error.code(), "invalid_batch");
    }

    #[test]
    fn inconsistent_lengths_name_the_mismatched_array() {
        let mut batch = unit_batch();
        batch.opacities.clear();
        let error = batch.validate(MAX_BATCH_POINTS).unwrap_err();
        assert!(error.to_string().contains("opacities holds 0"));
    }

    /// One damaged batch plus the label of the damage, for the rejection table.
    type DamageCase = (&'static str, fn(&mut GaussianBatch));

    #[test]
    fn malformed_values_are_rejected_before_any_clamping() {
        let cases: [DamageCase; 5] = [
            ("non-finite position", |batch| {
                batch.positions[0] = [f32::NAN, 0.0, 0.0];
            }),
            ("non-positive scale", |batch| {
                batch.scales[0] = [0.1, 0.0, 0.1];
            }),
            ("zero-norm quaternion", |batch| {
                batch.rotations[0] = [0.0, 0.0, 0.0, 0.0];
            }),
            ("colour out of range", |batch| {
                batch.colors[0] = [1.4, 0.0, 0.0];
            }),
            ("opacity out of range", |batch| {
                batch.opacities[0] = 3.0;
            }),
        ];
        for (name, damage) in cases {
            let mut batch = unit_batch();
            damage(&mut batch);
            let error = batch.validate(MAX_BATCH_POINTS).unwrap_err();
            assert_eq!(error.code(), "invalid_batch", "{name} should be rejected");
        }
    }

    #[test]
    fn the_point_budget_is_enforced_as_a_budget_error() {
        let batch = unit_batch();
        let error = batch.validate(0).unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");
    }

    #[test]
    fn quaternions_are_normalised_and_degenerate_ones_default_to_identity() {
        let normalized = normalize_rotation([0.0, 4.0, 0.0, 0.0]);
        assert_eq!(normalized, [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(normalize_rotation([0.0, 0.0, 0.0, 0.0]), [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_splat_round_trips_through_a_batch() {
        let splat = unit_batch().to_splat(MAX_BATCH_POINTS).unwrap();
        let batch = GaussianBatch::from_splat(&splat, BatchMetadata::default());
        let again = batch.to_splat(MAX_BATCH_POINTS).unwrap();
        assert_eq!(splat, again);
    }

    #[test]
    fn batches_merge_into_one_and_keep_a_component_name() {
        let mut first = unit_batch();
        first.metadata.component_id = Some("spire".to_owned());
        let second = unit_batch();
        let merged = GaussianBatch::merge(vec![first, second]).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged.metadata.component_id.as_deref(), Some("spire"));
        assert!(merged.validate(MAX_BATCH_POINTS).is_ok());
        assert_eq!(
            GaussianBatch::merge(Vec::new()).unwrap_err().code(),
            "invalid_batch"
        );
    }

    #[test]
    fn bounds_pad_by_the_largest_radius() {
        let batch = unit_batch();
        let bounds = batch.bounds().unwrap();
        // Padding uses the largest radius per axis: 0.3 here.
        assert_eq!(bounds.min, [0.7, 1.7, 2.7]);
        assert_eq!(bounds.center, [1.0, 2.0, 3.0]);
        assert!((bounds.radius - 0.3).abs() < 1e-5);
    }
}
