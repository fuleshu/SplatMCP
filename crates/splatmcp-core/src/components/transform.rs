//! Local frames and the maths that keeps an anisotropic gaussian anisotropic.
//!
//! A component's explicit frame is translation, rotation and positive per-axis scale
//! (`A = R · S`). Applying it to a gaussian is not "multiply the radii": the covariance
//! `C = R_g · diag(scale²) · R_gᵀ` is transformed as `C' = A · C · Aᵀ` and then decomposed back
//! into a scale/orientation pair, which is the only way a rotated, anisotropic gaussian keeps
//! its shape. Reflections and singular transforms are refused rather than repaired.

use crate::SplatPoint;
use crate::contract::{IDENTITY_QUATERNION, covariance, normalized_quaternion, rotate_vector};

use super::SelectionError;

/// Smallest determinant of a transform's linear part that is still invertible.
pub const MIN_TRANSFORM_DETERMINANT: f32 = 1e-9;

/// An affine local frame: translation, rotation and positive per-axis scale (`A = R · S`).
///
/// The supported class is exactly "invertible affine with positive determinant", which is what
/// an anisotropic gaussian can be mapped through without changing handedness or collapsing an
/// axis. Reflections (`det < 0`) and singular transforms (`|det| <= MIN_TRANSFORM_DETERMINANT`)
/// are refused with [`SelectionError::UnsupportedTransform`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTransform {
    pub translation: [f32; 3],
    /// Unit quaternion `(w, x, y, z)`; the same active rotation the contract defines.
    pub rotation: [f32; 4],
    /// Positive per-axis scale, applied before the rotation (`A = R · S`).
    pub scale: [f32; 3],
}

impl Default for LocalTransform {
    fn default() -> Self {
        Self::identity()
    }
}

impl LocalTransform {
    /// The identity frame: position and orientation pass through unchanged.
    pub fn identity() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: IDENTITY_QUATERNION,
            scale: [1.0; 3],
        }
    }

    /// Translation only.
    pub fn translation(by: [f32; 3]) -> Self {
        Self {
            translation: by,
            ..Self::identity()
        }
    }

    /// Checks the transform is finite, usable and invertible, and normalises its rotation.
    pub fn validate(&self) -> Result<Self, SelectionError> {
        if !self.translation.iter().all(|value| value.is_finite()) {
            return Err(SelectionError::UnsupportedTransform(
                "translation must be finite".to_owned(),
            ));
        }
        let rotation = normalized_quaternion(self.rotation).ok_or_else(|| {
            SelectionError::UnsupportedTransform(
                "rotation must be a finite, non-degenerate quaternion".to_owned(),
            )
        })?;
        if !self
            .scale
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
        {
            return Err(SelectionError::UnsupportedTransform(
                "scale must be positive and finite on every axis".to_owned(),
            ));
        }
        let candidate = Self {
            translation: self.translation,
            rotation,
            scale: self.scale,
        };
        let determinant = candidate.determinant();
        if !determinant.is_finite() || determinant <= MIN_TRANSFORM_DETERMINANT {
            return Err(SelectionError::UnsupportedTransform(
                "the transform must be invertible and must not reflect".to_owned(),
            ));
        }
        Ok(candidate)
    }

    /// The linear part `A = R · S`, row major.
    ///
    /// Column `axis` is the image of local axis `axis`: `scale[axis] * R[:, axis]`.
    pub fn linear(&self) -> [[f32; 3]; 3] {
        let rotation = [
            rotate_vector([1.0, 0.0, 0.0], self.rotation),
            rotate_vector([0.0, 1.0, 0.0], self.rotation),
            rotate_vector([0.0, 0.0, 1.0], self.rotation),
        ];
        let mut matrix = [[0.0f32; 3]; 3];
        for column in 0..3 {
            for row in 0..3 {
                matrix[row][column] = rotation[column][row] * self.scale[column];
            }
        }
        matrix
    }

    /// Determinant of the linear part.
    pub fn determinant(&self) -> f32 {
        let m = self.linear();
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Maps a document-space position into this frame.
    pub fn to_local(&self, position: [f32; 3]) -> Result<[f32; 3], SelectionError> {
        let transform = self.validate()?;
        let inverse = inverse_with_determinant(transform.linear(), transform.determinant())?;
        let shifted = [
            position[0] - transform.translation[0],
            position[1] - transform.translation[1],
            position[2] - transform.translation[2],
        ];
        Ok(apply_matrix(inverse, shifted))
    }

    /// Maps a local point into document space: `A · p + t`.
    pub fn to_world(&self, point: [f32; 3]) -> [f32; 3] {
        let mapped = apply_matrix(self.linear(), point);
        [
            mapped[0] + self.translation[0],
            mapped[1] + self.translation[1],
            mapped[2] + self.translation[2],
        ]
    }

    /// Transforms one gaussian: position, and covariance for an anisotropic orientation.
    ///
    /// The orientation is not "multiplied" component-wise: the covariance is transformed as
    /// `C' = A · C · Aᵀ` and decomposed back into a scale/orientation pair, so a rotated
    /// anisotropic gaussian stays a rotated anisotropic gaussian under a non-uniform map.
    pub fn apply_point(&self, point: &SplatPoint) -> Result<SplatPoint, SelectionError> {
        let transform = self.validate()?;
        let a = transform.linear();
        let position = transform.to_world(point.position);
        let c = covariance(point.scale, point.rotation);
        let transformed = transform_covariance(a, c);
        let (scale, rotation) = decompose_covariance(transformed).ok_or_else(|| {
            SelectionError::UnsupportedTransform(
                "the transformed covariance is degenerate; the transform cannot be applied"
                    .to_owned(),
            )
        })?;
        Ok(SplatPoint {
            position,
            scale,
            color: point.color,
            opacity: point.opacity,
            rotation,
        })
    }
}

/// `M · v`.
fn apply_matrix(matrix: [[f32; 3]; 3], vector: [f32; 3]) -> [f32; 3] {
    let mut result = [0.0f32; 3];
    for row in 0..3 {
        result[row] =
            matrix[row][0] * vector[0] + matrix[row][1] * vector[1] + matrix[row][2] * vector[2];
    }
    result
}

/// Inverse of a matrix whose determinant is already known.
fn inverse_with_determinant(
    m: [[f32; 3]; 3],
    determinant: f32,
) -> Result<[[f32; 3]; 3], SelectionError> {
    if !determinant.is_finite() || determinant.abs() <= MIN_TRANSFORM_DETERMINANT {
        return Err(SelectionError::UnsupportedTransform(
            "the transform is singular and has no inverse".to_owned(),
        ));
    }
    let mut inverse = [[0.0f32; 3]; 3];
    inverse[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) / determinant;
    inverse[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) / determinant;
    inverse[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) / determinant;
    inverse[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) / determinant;
    inverse[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) / determinant;
    inverse[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) / determinant;
    inverse[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) / determinant;
    inverse[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) / determinant;
    inverse[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) / determinant;
    Ok(inverse)
}

/// `A · C · Aᵀ`.
fn transform_covariance(a: [[f32; 3]; 3], c: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut ac = [[0.0f32; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            ac[row][column] = (0..3).map(|k| a[row][k] * c[k][column]).sum();
        }
    }
    let mut result = [[0.0f32; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            result[row][column] = (0..3).map(|k| ac[row][k] * a[column][k]).sum();
        }
    }
    result
}

/// Recovers `(scale, rotation)` from a covariance matrix.
///
/// Eigenvalues of a covariance are the squared radii and its eigenvectors are the gaussian's
/// own axes, so this is the decomposition the contract's `covariance` function inverts. The
/// basis is forced right-handed, which keeps the recovered rotation a rotation rather than a
/// reflection.
pub fn decompose_covariance(c: [[f32; 3]; 3]) -> Option<([f32; 3], [f32; 4])> {
    let (values, mut basis) = symmetric_eigen(c)?;
    if values
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return None;
    }
    let scale = [
        values[0].sqrt().max(f32::MIN_POSITIVE),
        values[1].sqrt().max(f32::MIN_POSITIVE),
        values[2].sqrt().max(f32::MIN_POSITIVE),
    ];
    if determinant3(basis) < 0.0 {
        for row in 0..3 {
            basis[row][2] = -basis[row][2];
        }
    }
    let rotation = quaternion_from_basis(basis)?;
    Some((scale, rotation))
}

/// Cyclic Jacobi eigen decomposition of a symmetric 3x3 matrix.
///
/// Returns eigenvalues in descending order with the matching unit eigenvectors as the columns
/// of the second value. `None` for a non-finite input.
fn symmetric_eigen(matrix: [[f32; 3]; 3]) -> Option<([f32; 3], [[f32; 3]; 3])> {
    if matrix.iter().flatten().any(|value| !value.is_finite()) {
        return None;
    }
    let mut a = matrix;
    let mut v = [[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..32 {
        // Largest off-diagonal magnitude decides whether another rotation is needed.
        let mut off = 0.0f32;
        for (row, column) in [(0, 1), (0, 2), (1, 2)] {
            off = off.max(a[row][column].abs());
        }
        let scale = a
            .iter()
            .flatten()
            .fold(0.0f32, |acc, value| acc.max(value.abs()));
        if off <= 1e-12 * scale.max(1e-30) {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q].abs() <= f32::EPSILON * scale {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for k in 0..3 {
                let akp = a[k][p];
                let akq = a[k][q];
                a[k][p] = c * akp - s * akq;
                a[k][q] = s * akp + c * akq;
            }
            for k in 0..3 {
                let apk = a[p][k];
                let aqk = a[q][k];
                a[p][k] = c * apk - s * aqk;
                a[q][k] = s * apk + c * aqk;
            }
            for k in 0..3 {
                let vkp = v[k][p];
                let vkq = v[k][q];
                v[k][p] = c * vkp - s * vkq;
                v[k][q] = s * vkp + c * vkq;
            }
        }
    }
    let mut pairs = [
        (a[0][0], [v[0][0], v[1][0], v[2][0]]),
        (a[1][1], [v[0][1], v[1][1], v[2][1]]),
        (a[2][2], [v[0][2], v[1][2], v[2][2]]),
    ];
    pairs.sort_by(|left, right| right.0.total_cmp(&left.0));
    let values = [pairs[0].0, pairs[1].0, pairs[2].0];
    let basis = [
        [pairs[0].1[0], pairs[1].1[0], pairs[2].1[0]],
        [pairs[0].1[1], pairs[1].1[1], pairs[2].1[1]],
        [pairs[0].1[2], pairs[1].1[2], pairs[2].1[2]],
    ];
    Some((values, orthonormalize(basis)?))
}

/// Gram-Schmidt, so rounding in the sweeps cannot leave a non-orthonormal basis.
fn orthonormalize(basis: [[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let columns = [
        [basis[0][0], basis[1][0], basis[2][0]],
        [basis[0][1], basis[1][1], basis[2][1]],
        [basis[0][2], basis[1][2], basis[2][2]],
    ];
    let mut out = [[0.0f32; 3]; 3];
    let mut done: Vec<[f32; 3]> = Vec::with_capacity(3);
    for column in columns {
        let mut vector = column;
        for previous in &done {
            let projection: f32 = (0..3).map(|axis| vector[axis] * previous[axis]).sum();
            for axis in 0..3 {
                vector[axis] -= projection * previous[axis];
            }
        }
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if !norm.is_finite() || norm <= 1e-12 {
            return None;
        }
        let unit = vector.map(|value| value / norm);
        done.push(unit);
        out[0][done.len() - 1] = unit[0];
        out[1][done.len() - 1] = unit[1];
        out[2][done.len() - 1] = unit[2];
    }
    Some(out)
}

fn determinant3(basis: [[f32; 3]; 3]) -> f32 {
    basis[0][0] * (basis[1][1] * basis[2][2] - basis[1][2] * basis[2][1])
        - basis[0][1] * (basis[1][0] * basis[2][2] - basis[1][2] * basis[2][0])
        + basis[0][2] * (basis[1][0] * basis[2][1] - basis[1][1] * basis[2][0])
}

/// Quaternion `(w, x, y, z)` of a rotation matrix given by orthonormal columns.
fn quaternion_from_basis(basis: [[f32; 3]; 3]) -> Option<[f32; 4]> {
    let trace = basis[0][0] + basis[1][1] + basis[2][2];
    let quaternion = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [
            0.25 * s,
            (basis[2][1] - basis[1][2]) / s,
            (basis[0][2] - basis[2][0]) / s,
            (basis[1][0] - basis[0][1]) / s,
        ]
    } else if basis[0][0] > basis[1][1] && basis[0][0] > basis[2][2] {
        let s = (1.0 + basis[0][0] - basis[1][1] - basis[2][2]).sqrt() * 2.0;
        [
            (basis[2][1] - basis[1][2]) / s,
            0.25 * s,
            (basis[0][1] + basis[1][0]) / s,
            (basis[0][2] + basis[2][0]) / s,
        ]
    } else if basis[1][1] > basis[2][2] {
        let s = (1.0 + basis[1][1] - basis[0][0] - basis[2][2]).sqrt() * 2.0;
        [
            (basis[0][2] - basis[2][0]) / s,
            (basis[0][1] + basis[1][0]) / s,
            0.25 * s,
            (basis[1][2] + basis[2][1]) / s,
        ]
    } else {
        let s = (1.0 + basis[2][2] - basis[0][0] - basis[1][1]).sqrt() * 2.0;
        [
            (basis[1][0] - basis[0][1]) / s,
            (basis[0][2] + basis[2][0]) / s,
            (basis[1][2] + basis[2][1]) / s,
            0.25 * s,
        ]
    };
    normalized_quaternion(quaternion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplatPoint;
    use crate::contract::dominant_axis;
    use crate::fixtures::rotated_gaussian;

    #[test]
    fn an_anisotropic_rotated_gaussian_survives_a_non_uniform_transform() {
        // The fixture's longest radius (0.3 m) lies along document +Y after its own 90 degree
        // rotation, so its covariance is diagonal: 0.01 on X, 0.09 on Y, 0.0025 on Z.
        let point = rotated_gaussian();
        let local = LocalTransform {
            translation: [0.0; 3],
            rotation: IDENTITY_QUATERNION,
            // Squash X, stretch Y: the long axis must stay the long axis.
            scale: [0.5, 2.0, 1.0],
        };
        let mapped = local.apply_point(&point).unwrap();
        let before = covariance(point.scale, point.rotation);
        let after = covariance(mapped.scale, mapped.rotation);
        assert!((before[1][1] - 0.09).abs() < 1e-6, "{before:?}");

        // A · C · Aᵀ: the variances follow the squared scale factors, and the shape stays
        // anisotropic rather than collapsing into radii multiplied in world space.
        assert!((after[0][0] - 0.01 * 0.25).abs() < 1e-7, "{after:?}");
        assert!((after[1][1] - 0.09 * 4.0).abs() < 1e-5, "{after:?}");
        assert!((after[2][2] - 0.0025).abs() < 1e-7, "{after:?}");
        for (row, column) in [(0, 1), (0, 2), (1, 2)] {
            assert!(after[row][column].abs() < 1e-6, "{after:?}");
        }
        assert!(mapped.scale[0] > mapped.scale[1] && mapped.scale[1] > mapped.scale[2]);

        // The recovered longest axis still lies along document Y, as it did before the
        // transform. Only the axis line is observable: an eigenvector and its negation describe
        // the same ellipsoid, so the assertion is on the direction's magnitude, not its sign.
        let axis = dominant_axis(mapped.scale, mapped.rotation).unwrap();
        assert!(axis[0].abs() < 1e-3, "{axis:?}");
        assert!((axis[1].abs() - 1.0).abs() < 1e-3, "{axis:?}");
        assert!(axis[2].abs() < 1e-3, "{axis:?}");

        // Translation moves the position and leaves the shape alone.
        let moved = LocalTransform::translation([1.0, 2.0, 3.0])
            .apply_point(&point)
            .unwrap();
        assert_eq!(moved.position, [1.5, 2.0, 3.25]);
        let moved_covariance = covariance(moved.scale, moved.rotation);
        for row in 0..3 {
            for column in 0..3 {
                assert!((moved_covariance[row][column] - before[row][column]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn a_rotation_in_the_local_frame_updates_the_covariance_and_keeps_it_valid() {
        let point = rotated_gaussian();
        // 90 degrees about Z on top of the gaussian's own 90 degree rotation: +Y -> -X.
        let half = std::f32::consts::FRAC_PI_4;
        let transform = LocalTransform {
            translation: [0.0; 3],
            rotation: [half.cos(), 0.0, 0.0, half.sin()],
            scale: [1.0; 3],
        };
        let mapped = transform.apply_point(&point).unwrap();
        // Before: the 0.3 radius lay along document Y (variance 0.09). After a further 90
        // degrees about Z it lies along document X instead - the *shape* is what a covariance
        // comparison can prove, and an eigenvector's sign is not observable.
        let axis = dominant_axis(mapped.scale, mapped.rotation).unwrap();
        assert!(axis[1].abs() < 1e-3, "{axis:?}");
        assert!(axis[2].abs() < 1e-3, "{axis:?}");
        assert!((axis[0].abs() - 1.0).abs() < 1e-3, "{axis:?}");
        let after = covariance(mapped.scale, mapped.rotation);
        assert!((after[0][0] - 0.09).abs() < 1e-6, "{after:?}");
        assert!((after[1][1] - 0.01).abs() < 1e-6, "{after:?}");
        assert!(mapped.scale[0] > mapped.scale[1] && mapped.scale[1] > mapped.scale[2]);
        assert!(mapped.color == point.color && mapped.opacity == point.opacity);
    }

    #[test]
    fn unsupported_transforms_are_refused_instead_of_repaired() {
        let singular = LocalTransform {
            translation: [0.0; 3],
            rotation: IDENTITY_QUATERNION,
            scale: [1.0, 0.0, 1.0],
        };
        assert_eq!(
            singular.validate().unwrap_err().code(),
            "unsupported_transform"
        );

        let reflecting = LocalTransform {
            translation: [0.0; 3],
            rotation: IDENTITY_QUATERNION,
            scale: [-1.0, 1.0, 1.0],
        };
        assert_eq!(
            reflecting.validate().unwrap_err().code(),
            "unsupported_transform"
        );

        let degenerate = LocalTransform {
            translation: [0.0; 3],
            rotation: [0.0; 4],
            scale: [1.0; 3],
        };
        assert_eq!(
            degenerate.validate().unwrap_err().code(),
            "unsupported_transform"
        );

        // A non-finite translation is refused too.
        assert!(
            LocalTransform::translation([f32::NAN, 0.0, 0.0])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn a_decomposed_covariance_round_trips_through_the_contract() {
        for point in [rotated_gaussian(), SplatPoint::default()] {
            let (scale, rotation) =
                decompose_covariance(covariance(point.scale, point.rotation)).unwrap();
            let rebuilt = covariance(scale, rotation);
            let original = covariance(point.scale, point.rotation);
            for row in 0..3 {
                for column in 0..3 {
                    assert!(
                        (rebuilt[row][column] - original[row][column]).abs() < 1e-6,
                        "{rebuilt:?} vs {original:?}"
                    );
                }
            }
        }
    }
}
