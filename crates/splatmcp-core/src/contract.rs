//! The versioned Gaussian data and coordinate contract.
//!
//! Everything in this crate - authoring, edits, PLY import and export, and the adapters
//! around it - speaks this one contract. It is stated here as data ([`CONTRACT`]) and as
//! the conversion helpers below, so an adapter never has to restate a convention from
//! memory. [`CONTRACT_VERSION`] is bumped when a convention changes meaning.
//!
//! # Document space
//!
//! - Right-handed, [`crate::SplatPoint::position`] in **metres**.
//! - `+X` right, `+Y` **down**, `+Z` forward. Up is therefore `-Y` and forward is `+Z`.
//!   This is the space PLY files are authored in (3DGS training and the INRIA exporter
//!   both work Y-down), so an imported file needs no axis conversion to become the model.
//! - The stored values are **activated**: `scale` is an ellipsoid radius in metres,
//!   `opacity` is in `0..=1`, and `color` is linear RGB in `0..=1`.
//!
//! # Rotation
//!
//! [`crate::SplatPoint::rotation`] is a unit quaternion in `(w, x, y, z)` order - SciPy's
//! `(x, y, z, w)` order is available through [`to_scipy_quaternion`] and
//! [`from_scipy_quaternion`]. It is an **active** rotation of the gaussian's own frame:
//! it maps a local axis onto a document axis, which is exactly what
//! [`rotate_vector`] does and what the renderer does when it builds a covariance from
//! `scale` and `rotation` ([`covariance`]).
//!
//! # Colour
//!
//! `color` is *linear* RGB. It is not sRGB, and no gamma conversion happens on import,
//! export or edit - the PLY field is an SH degree 0 coefficient that is linear in the
//! same way. [`linear_to_srgb`] and [`srgb_to_linear`] exist for images a caller writes
//! or reads (a screenshot, a swatch file), never for the model or the file format, so a
//! value can never be gamma converted twice.
//!
//! # Serialisation
//!
//! PLY is the only file format (see `ply`). Writing emits binary little-endian `float`
//! properties at SH degree 0, so a gaussian stores
//!
//! | model field | PLY property                | stored value                     |
//! |-------------|-----------------------------|----------------------------------|
//! | `position`  | `x`, `y`, `z`               | metres, unchanged                |
//! | `scale`     | `scale_0..2`                | `ln(radius)`                     |
//! | `color`     | `f_dc_0..2`                 | `(channel - 0.5) / SH_C0`        |
//! | `opacity`   | `opacity`                   | logistic logit of the activated value |
//! | `rotation`  | `rot_0..3`                  | `(w, x, y, z)`, unchanged        |
//!
//! Higher SH bands (`f_rest_*`) and normals (`nx`, `ny`, `nz`) are never stored and are
//! dropped on import; [`ply_attribute_use`] says what happens to each attribute and the
//! reader reports what it discarded.
//!
//! # Viewer space
//!
//! The PlayCanvas viewer is Y-up, so `ui/viewer.js` rotates an imported splat by
//! [`VIEWER_X_FLIP_DEGREES`] about X. Viewer space is therefore
//! [`to_viewer_space`] of document space, i.e. `(x, -y, -z)`, and the transform is its
//! own inverse. It is applied exactly once, when the PLY is attached to the scene: a
//! model that is already Y-up must not be flipped again, which is what the asymmetric
//! fixture in [`crate::fixtures`] makes visible.

use crate::SH_C0;

/// Version of the Gaussian contract implemented by this crate.
pub const CONTRACT_VERSION: u32 = 1;

/// Spherical-harmonic degree the model stores. Fixed RGB colour only.
pub const SH_DEGREE: u32 = 0;

/// Attributes every gaussian carries, in the order the PLY writer emits them.
pub const ATTRIBUTES: [&str; 5] = ["position", "scale", "color", "opacity", "rotation"];

/// Identity rotation, used wherever a gaussian has no orientation of its own.
pub const IDENTITY_QUATERNION: [f32; 4] = [1.0, 0.0, 0.0, 0.0];

/// Lowest PLY `f_dc_*` coefficient that still maps into linear RGB `0..=1`.
pub const DC_MIN: f32 = -0.5 / SH_C0;

/// Highest PLY `f_dc_*` coefficient that still maps into linear RGB `0..=1`.
pub const DC_MAX: f32 = 0.5 / SH_C0;

/// Rotation the viewer applies about X when it attaches an imported PLY, in degrees.
pub const VIEWER_X_FLIP_DEGREES: f32 = 180.0;

/// Shortest quaternion length that still has a direction; below it the value is refused
/// rather than repaired.
pub const QUATERNION_MIN_NORM: f32 = 1e-6;

/// Tolerance allowed for activated colours and opacities that a float round trip pushed
/// just outside `0..=1`. Anything further out is refused, never silently clamped.
pub const RANGE_TOLERANCE: f32 = 1e-3;

/// How far a quaternion's length may differ from 1 before an importer reports that it had to
/// rescale it.
///
/// Float rounding leaves a stored unit quaternion at `1 ± 1e-7`, so this keeps an ordinary
/// file quiet while a genuinely scaled quaternion is reported instead of silently normalised.
pub const QUATERNION_LENGTH_TOLERANCE: f32 = 1e-3;

/// Machine readable statement of the contract, for tool replies and runtime metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contract {
    pub version: u32,
    /// Handedness and axis directions of document space.
    pub handedness: &'static str,
    pub axes: &'static str,
    pub up_axis: &'static str,
    pub forward_axis: &'static str,
    /// Unit of every position and radius.
    pub length_unit: &'static str,
    /// Quaternion component order as written.
    pub quaternion_order: &'static str,
    /// What the rotation means.
    pub rotation_semantics: &'static str,
    /// Colour space of `color`.
    pub color_space: &'static str,
    /// Meaning of `scale`.
    pub scale_semantics: &'static str,
    /// Meaning of `opacity`.
    pub opacity_semantics: &'static str,
    /// Serialized precision of the numbers in a file.
    pub serialized_precision: &'static str,
    /// Spherical-harmonic degree stored.
    pub sh_degree: u32,
    /// Transform from document space to viewer space.
    pub viewer_transform: &'static str,
    pub viewer_flip_degrees: u32,
}

/// The contract, as a value: reply fields and diagnostics are built from this rather than
/// from repeated string literals.
pub const CONTRACT: Contract = Contract {
    version: CONTRACT_VERSION,
    handedness: "right-handed",
    axes: "+X right, +Y down, +Z forward",
    up_axis: "-Y",
    forward_axis: "+Z",
    length_unit: "metres",
    quaternion_order: "wxyz",
    rotation_semantics: "active: rotates the gaussian's local axes into document space",
    color_space: "linear RGB in 0..=1 (sRGB only for images, never for the model)",
    scale_semantics: "activated ellipsoid radius in metres; PLY stores ln(scale)",
    opacity_semantics: "activated 0..=1; PLY stores the sigmoid logit",
    serialized_precision: "f32, written as PLY float (binary little endian)",
    sh_degree: SH_DEGREE,
    viewer_transform: "viewer = (x, -y, -z) of document space",
    viewer_flip_degrees: VIEWER_X_FLIP_DEGREES as u32,
};

/// Converts a document-space position into viewer space.
///
/// The viewer applies [`VIEWER_X_FLIP_DEGREES`] about X, and a 180 degree rotation is its
/// own inverse, so this same formula also converts viewer space back to document space.
pub fn to_viewer_space(position: [f32; 3]) -> [f32; 3] {
    [position[0], -position[1], -position[2]]
}

/// Converts a viewer-space position into document space.
pub fn from_viewer_space(position: [f32; 3]) -> [f32; 3] {
    to_viewer_space(position)
}

/// Reorders `(w, x, y, z)` into SciPy's `(x, y, z, w)` layout.
///
/// `scipy.spatial.transform.Rotation` uses XYZW, so a script exchanging a quaternion
/// with this crate converts once, at the boundary, instead of guessing per call.
pub fn to_scipy_quaternion(rotation: [f32; 4]) -> [f32; 4] {
    [rotation[1], rotation[2], rotation[3], rotation[0]]
}

/// Reorders SciPy's `(x, y, z, w)` layout back into `(w, x, y, z)`.
pub fn from_scipy_quaternion(rotation: [f32; 4]) -> [f32; 4] {
    [rotation[3], rotation[0], rotation[1], rotation[2]]
}

/// Length of a quaternion.
pub fn quaternion_norm(rotation: [f32; 4]) -> f32 {
    rotation
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt()
}

/// True when the quaternion is finite and long enough to carry a direction.
pub fn is_usable_quaternion(rotation: [f32; 4]) -> bool {
    let norm = quaternion_norm(rotation);
    norm.is_finite() && norm > QUATERNION_MIN_NORM
}

/// Rescales a quaternion to unit length, or returns `None` when it has no direction.
///
/// This is the documented normalisation policy: finite, non-degenerate quaternions are
/// rescaled (their length carries no information), and degenerate ones are refused by the
/// caller instead of being silently replaced with an identity rotation.
pub fn normalized_quaternion(rotation: [f32; 4]) -> Option<[f32; 4]> {
    if !is_usable_quaternion(rotation) {
        return None;
    }
    let norm = quaternion_norm(rotation);
    Some(rotation.map(|value| value / norm))
}

/// Rotates a vector by a unit quaternion: the active rotation of the contract.
///
/// `v' = v + 2 * cross(q.xyz, cross(q.xyz, v) + w * v)`, which is the same orientation the
/// renderer applies, so a covariance computed here matches a rendered gaussian.
pub fn rotate_vector(vector: [f32; 3], rotation: [f32; 4]) -> [f32; 3] {
    let [w, x, y, z] = rotation;
    let axis = [x, y, z];
    let cross = [
        axis[1] * vector[2] - axis[2] * vector[1],
        axis[2] * vector[0] - axis[0] * vector[2],
        axis[0] * vector[1] - axis[1] * vector[0],
    ];
    let scaled = [
        cross[0] + w * vector[0],
        cross[1] + w * vector[1],
        cross[2] + w * vector[2],
    ];
    [
        vector[0] + 2.0 * (axis[1] * scaled[2] - axis[2] * scaled[1]),
        vector[1] + 2.0 * (axis[2] * scaled[0] - axis[0] * scaled[2]),
        vector[2] + 2.0 * (axis[0] * scaled[1] - axis[1] * scaled[0]),
    ]
}

/// Hamilton product `a * b`: `b` applied in `a`'s frame.
pub fn multiply_quaternions(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [aw, ax, ay, az] = a;
    let [bw, bx, by, bz] = b;
    [
        aw * bw - ax * bx - ay * by - az * bz,
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
    ]
}

/// The gaussian's own axes in document space: local `X`, `Y` then `Z`.
pub fn local_axes(rotation: [f32; 4]) -> [[f32; 3]; 3] {
    [
        rotate_vector([1.0, 0.0, 0.0], rotation),
        rotate_vector([0.0, 1.0, 0.0], rotation),
        rotate_vector([0.0, 0.0, 1.0], rotation),
    ]
}

/// Covariance matrix `R * diag(scale^2) * R^T` of one gaussian.
///
/// This is what a renderer builds from an anisotropic gaussian, so a test that compares
/// covariances verifies scale *and* orientation rather than a point centre. `matrix[row]
/// [column]` is the document-space covariance entry.
pub fn covariance(scale: [f32; 3], rotation: [f32; 4]) -> [[f32; 3]; 3] {
    let axes = local_axes(rotation);
    let variances = scale.map(|value| value * value);
    let mut matrix = [[0.0f32; 3]; 3];
    for (axis, direction) in axes.iter().enumerate() {
        for row in 0..3 {
            for column in 0..3 {
                matrix[row][column] += variances[axis] * direction[row] * direction[column];
            }
        }
    }
    matrix
}

/// Direction of the gaussian's longest axis in document space, normalised.
///
/// `None` when the quaternion is degenerate or the scales are not finite, so a caller can
/// report a bad input instead of comparing nonsense.
pub fn dominant_axis(scale: [f32; 3], rotation: [f32; 4]) -> Option<[f32; 3]> {
    if !scale.iter().all(|value| value.is_finite()) || !is_usable_quaternion(rotation) {
        return None;
    }
    let longest = (0..3).max_by(|a, b| scale[*a].total_cmp(&scale[*b]))?;
    let unit = normalized_quaternion(rotation)?;
    let mut local = [0.0f32; 3];
    local[longest] = 1.0;
    let direction = rotate_vector(local, unit);
    let norm = direction
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm <= 0.0 {
        return None;
    }
    Some(direction.map(|value| value / norm))
}

/// sRGB transfer function for one linear channel in `0..=1`.
///
/// For images only: importing, exporting or editing never converts colour.
pub fn linear_to_srgb(value: f32) -> f32 {
    let clamped = value.clamp(0.0, 1.0);
    let encoded = if clamped <= 0.003_130_8 {
        12.92 * clamped
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    encoded.clamp(0.0, 1.0)
}

/// Inverse sRGB transfer function for one encoded channel in `0..=1`.
pub fn srgb_to_linear(value: f32) -> f32 {
    let clamped = value.clamp(0.0, 1.0);
    if clamped <= 0.040_45 {
        clamped / 12.92
    } else {
        ((clamped + 0.055) / 1.055).powf(2.4)
    }
}

/// What the PLY importer does with one file attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlyAttributeUse {
    /// The attribute is read into the model, with the contract field it feeds.
    Interpreted(&'static str),
    /// The attribute is dropped, with the reason reported to the caller.
    Discarded(&'static str),
}

/// Classifies one PLY property name against the contract.
///
/// The reader uses this to build its import report, so the reason a property was dropped
/// is never invented twice.
pub fn ply_attribute_use(name: &str) -> PlyAttributeUse {
    use PlyAttributeUse::{Discarded, Interpreted};
    match name {
        "x" | "y" | "z" => Interpreted("position"),
        "scale_0" | "scale_1" | "scale_2" => Interpreted("scale"),
        "f_dc_0" | "f_dc_1" | "f_dc_2" => Interpreted("color"),
        "opacity" => Interpreted("opacity"),
        "rot_0" | "rot_1" | "rot_2" | "rot_3" => Interpreted("rotation"),
        "nx" | "ny" | "nz" => Discarded("normals are not part of the contract"),
        _ if name.starts_with("f_rest_") => {
            Discarded("higher spherical-harmonic bands are dropped; the model stores degree 0")
        }
        _ => Discarded("not part of the gaussian contract"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_names_every_convention_once() {
        let contract = CONTRACT;
        assert_eq!(contract.version, CONTRACT_VERSION);
        assert_eq!(contract.quaternion_order, "wxyz");
        assert_eq!(contract.up_axis, "-Y");
        assert_eq!(contract.forward_axis, "+Z");
        assert_eq!(contract.handedness, "right-handed");
        assert_eq!(contract.length_unit, "metres");
        assert_eq!(contract.sh_degree, 0);
        assert_eq!(contract.viewer_flip_degrees, 180);
        assert!(contract.color_space.contains("linear RGB"));
        assert!(contract.scale_semantics.contains("activated"));
        assert!(contract.opacity_semantics.contains("activated"));
        assert_eq!(ATTRIBUTES.len(), 5);
    }

    #[test]
    fn the_viewer_flip_is_its_own_inverse() {
        let point = [1.0, 2.0, 3.0];
        assert_eq!(to_viewer_space(point), [1.0, -2.0, -3.0]);
        assert_eq!(from_viewer_space(to_viewer_space(point)), point);
        // A 180 degree X rotation is what the two sign flips mean.
        assert_eq!(VIEWER_X_FLIP_DEGREES, 180.0);
    }

    #[test]
    fn scipy_order_round_trips() {
        let wxyz = [0.5, 0.5, 0.5, 0.5];
        assert_eq!(to_scipy_quaternion(wxyz), [0.5, 0.5, 0.5, 0.5]);
        let scipy = [0.1, 0.2, 0.3, 0.9];
        assert_eq!(from_scipy_quaternion(scipy), [0.9, 0.1, 0.2, 0.3]);
        assert_eq!(to_scipy_quaternion(from_scipy_quaternion(scipy)), scipy);
    }

    #[test]
    fn quaternion_normalisation_refuses_only_degenerate_values() {
        assert_eq!(normalized_quaternion([0.0, 0.0, 0.0, 0.0]), None);
        assert_eq!(normalized_quaternion([f32::NAN, 0.0, 0.0, 0.0]), None);
        let normalized = normalized_quaternion([0.0, 4.0, 0.0, 0.0]).unwrap();
        assert_eq!(normalized, [0.0, 1.0, 0.0, 0.0]);
        assert!(is_usable_quaternion([1.0, 0.0, 0.0, 0.0]));
    }

    #[test]
    fn a_ninety_degree_rotation_moves_the_dominant_axis() {
        // 90 degrees about Z sends local +X onto document +Y.
        let half = std::f32::consts::FRAC_PI_4;
        let rotation = [half.cos(), 0.0, 0.0, half.sin()];
        let direction = dominant_axis([0.3, 0.1, 0.05], rotation).unwrap();
        assert!(direction[0].abs() < 1e-6, "{direction:?}");
        assert!((direction[1] - 1.0).abs() < 1e-6, "{direction:?}");
        assert!(direction[2].abs() < 1e-6, "{direction:?}");
    }

    #[test]
    fn covariance_is_anisotropic_and_follows_the_rotation() {
        let half = std::f32::consts::FRAC_PI_4;
        let rotation = [half.cos(), 0.0, 0.0, half.sin()];
        let matrix = covariance([0.3, 0.1, 0.05], rotation);
        // Variances move with the axes: 0.09 along document Y, 0.01 along X, 0.0025 on Z.
        assert!((matrix[0][0] - 0.01).abs() < 1e-6, "{matrix:?}");
        assert!((matrix[1][1] - 0.09).abs() < 1e-6, "{matrix:?}");
        assert!((matrix[2][2] - 0.0025).abs() < 1e-6, "{matrix:?}");
        assert!(matrix[0][1].abs() < 1e-6, "{matrix:?}");

        // Without a rotation the same scales stay on their own axes.
        let axis_aligned = covariance([0.3, 0.1, 0.05], IDENTITY_QUATERNION);
        assert!((axis_aligned[0][0] - 0.09).abs() < 1e-6);
        assert!((axis_aligned[1][1] - 0.01).abs() < 1e-6);
    }

    #[test]
    fn colour_helpers_are_inverse_and_do_not_change_the_model() {
        for linear in [0.0_f32, 0.02, 0.25, 0.5, 1.0] {
            let round_trip = srgb_to_linear(linear_to_srgb(linear));
            assert!(
                (round_trip - linear).abs() < 1e-4,
                "{linear} -> {round_trip}"
            );
        }
        // Linear 0.5 is sRGB ~0.735: the two spaces are not interchangeable.
        assert!((linear_to_srgb(0.5) - 0.735_36).abs() < 1e-4);
        assert!(linear_to_srgb(1.5) > 0.999);
        assert!(linear_to_srgb(-1.0) < 0.001);
    }

    #[test]
    fn ply_attributes_are_classified_with_a_reason() {
        assert_eq!(
            ply_attribute_use("x"),
            PlyAttributeUse::Interpreted("position")
        );
        assert_eq!(
            ply_attribute_use("rot_3"),
            PlyAttributeUse::Interpreted("rotation")
        );
        assert!(matches!(
            ply_attribute_use("f_rest_17"),
            PlyAttributeUse::Discarded(reason) if reason.contains("degree 0")
        ));
        assert!(matches!(
            ply_attribute_use("ny"),
            PlyAttributeUse::Discarded(reason) if reason.contains("normals")
        ));
        assert!(matches!(
            ply_attribute_use("temperature"),
            PlyAttributeUse::Discarded(_)
        ));
    }

    #[test]
    fn the_dc_range_matches_the_linear_colour_range() {
        // The endpoints of the representable linear range.
        assert!((SH_C0 * DC_MIN + 0.5).abs() < 1e-6);
        assert!((SH_C0 * DC_MAX + 0.5 - 1.0).abs() < 1e-6);
    }
}
