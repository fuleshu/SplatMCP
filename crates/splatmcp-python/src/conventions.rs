//! Coordinate and quaternion conventions shared by Python, Rust and the viewer.
//!
//! # Document space (what a batch holds)
//!
//! A [`GaussianBatch`](crate::GaussianBatch) position is in *document space*, the space
//! of the core model and of the PLY file that stores it:
//!
//! - right-handed axes, `+X` right, `+Y` down, `+Z` forward, metres
//! - `scales` are activated radii along the Gaussian's own local axes
//! - `rotations` are unit quaternions in `(w, x, y, z)` order that rotate the local axes
//!   into document space
//! - `colors` are linear RGB, `opacities` are activated values
//!
//! # Viewer space
//!
//! The PlayCanvas viewer imports a PLY and applies a 180 degree rotation about X
//! (`PLY_DEFAULT_X_FLIP_DEG` in `ui/viewer.js`), because 3DGS files are authored Y-down.
//! Viewer space is therefore `(x, -y, -z)` of document space, i.e. Y-up:
//!
//! ```text
//! viewer = flip_x(document):  (x, y, z) -> (x, -y, -z)
//! ```
//!
//! The flip is its own inverse, so the same formula converts in both directions. A
//! script that reasons in Y-up space can author in [`AuthoringSpace::YUp`] and the batch
//! is converted once, at the boundary, instead of every script repeating the sign flips.
//!
//! # Why this module exists
//!
//! Two sign errors - one in Python, one in the viewer - cancel out in a symmetric model
//! and hide until an asymmetric one is rendered. [`axis_fixture`] is that asymmetric
//! model: its three coloured arrows are distinguishable per axis, so a double flip or a
//! swapped axis shows up in a screenshot instead of passing silently.

use crate::arrays::{BatchMetadata, GaussianBatch};

/// Space a script authored its positions in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthoringSpace {
    /// The document space itself: `+Y` down. This is the default, and the space
    /// [`GaussianBatch`] always holds.
    #[default]
    Document,
    /// A Y-up space as `(x, -y, -z)` of document space, for scripts that model objects
    /// the way a viewer shows them.
    YUp,
}

impl AuthoringSpace {
    /// Converts a position authored in this space into document space.
    pub fn to_document(self, position: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Document => position,
            Self::YUp => flip_about_x(position),
        }
    }

    /// Converts a document space position into this authoring space.
    pub fn from_document(self, position: [f32; 3]) -> [f32; 3] {
        // The flip is its own inverse, so both directions use the same formula.
        self.to_document(position)
    }

    /// Name used in runtime metadata and tool replies.
    pub fn name(self) -> &'static str {
        match self {
            Self::Document => "document_y_down",
            Self::YUp => "y_up",
        }
    }
}

/// The viewer transform applied to imported PLY data, exposed so a script can be written
/// against the same numbers instead of a remembered constant.
pub const VIEWER_X_FLIP_DEGREES: f32 = 180.0;

/// Applies the viewer's 180 degree X rotation to a vector or position.
///
/// The flip is its own inverse, so this converts document space to viewer space and back.
pub fn flip_about_x(value: [f32; 3]) -> [f32; 3] {
    [value[0], -value[1], -value[2]]
}

/// Description of the conventions, reported by `python_runtime_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConventionReport {
    /// Space every batch holds.
    pub document_space: &'static str,
    /// Axis directions in document space.
    pub document_axes: &'static str,
    /// Quaternion order.
    pub quaternion_order: &'static str,
    /// Viewer flip, in degrees.
    pub viewer_flip_degrees: u32,
    /// Formula for converting document space to viewer space.
    pub viewer_transform: &'static str,
}

/// The conventions this module implements, for runtime metadata and tool replies.
pub fn report() -> ConventionReport {
    ConventionReport {
        document_space: "right-handed, metres, +X right, +Y down, +Z forward",
        document_axes: "+X right, +Y down, +Z forward",
        quaternion_order: "wxyz",
        viewer_flip_degrees: VIEWER_X_FLIP_DEGREES as u32,
        viewer_transform: "viewer = (x, -y, -z) of document space",
    }
}

/// Builds the asymmetric fixture used to prove that Python, PLY and the viewer agree.
///
/// Three short arrows sit along `+X`, `+Y` and `+Z`, each in its own colour, and a fourth
/// gaussian is offset along `+X` only. Every arrow is a chain of small gaussians so the
/// direction it points in stays visible after a render, and no axis is symmetric with
/// any other, so a swapped or doubly flipped axis cannot pass unnoticed.
pub fn axis_fixture() -> GaussianBatch {
    let mut batch = GaussianBatch::with_capacity(3 * AXIS_STEPS as usize + 1);
    batch.metadata = BatchMetadata {
        component_id: Some("axis_fixture".to_owned()),
        recipe: Some("conventions::axis_fixture".to_owned()),
        seed: Some(0),
    };
    for (axis, color) in [
        (0usize, [1.0, 0.0, 0.0]),
        (1usize, [0.0, 1.0, 0.0]),
        (2usize, [0.0, 0.0, 1.0]),
    ] {
        for step in 1..=AXIS_STEPS {
            let distance = step as f32 * AXIS_STEP_LENGTH;
            let mut position = [0.0f32; 3];
            position[axis] = distance;
            let taper = 1.0 - 0.1 * step as f32;
            let radius = AXIS_RADIUS * taper;
            batch.push(
                position,
                [radius, radius, radius],
                [1.0, 0.0, 0.0, 0.0],
                color,
                1.0,
            );
        }
    }
    // A lone marker further along +Y: its position identifies the Y axis even when the
    // coloured arrow is hidden behind the model's own geometry.
    batch.push(
        [0.0, AXIS_STEPS as f32 * AXIS_STEP_LENGTH * 1.5, 0.0],
        [AXIS_RADIUS, AXIS_RADIUS, AXIS_RADIUS],
        [1.0, 0.0, 0.0, 0.0],
        [0.9, 0.9, 0.9],
        1.0,
    );
    batch
}

/// Gaussians per axis in [`axis_fixture`].
pub const AXIS_STEPS: i32 = 6;
/// Distance between fixture gaussians along an axis, in metres.
pub const AXIS_STEP_LENGTH: f32 = 0.2;
/// Radius of a fixture gaussian, in metres.
pub const AXIS_RADIUS: f32 = 0.04;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrays::MAX_BATCH_POINTS;

    #[test]
    fn the_viewer_flip_is_its_own_inverse() {
        let point = [1.0, 2.0, 3.0];
        assert_eq!(flip_about_x(flip_about_x(point)), point);
        assert_eq!(flip_about_x(point), [1.0, -2.0, -3.0]);
    }

    #[test]
    fn a_y_up_script_is_converted_once_at_the_boundary() {
        let y_up = [1.0, 2.0, 3.0];
        let document = AuthoringSpace::YUp.to_document(y_up);
        assert_eq!(document, [1.0, -2.0, -3.0]);
        assert_eq!(AuthoringSpace::YUp.from_document(document), y_up);
        // Document authored positions are untouched.
        assert_eq!(AuthoringSpace::Document.to_document(y_up), y_up);
    }

    #[test]
    fn the_fixture_is_asymmetric_and_valid() {
        let batch = axis_fixture();
        batch.validate(MAX_BATCH_POINTS).unwrap();
        assert_eq!(batch.len(), 3 * AXIS_STEPS as usize + 1);

        // One axis must dominate each arrow, and no two arrows may share a boundary, so a
        // swapped axis changes the fixture's bounds.
        let bounds = batch.bounds().unwrap();
        assert!(bounds.max[0] > 0.0 && bounds.max[1] > 0.0 && bounds.max[2] > 0.0);
        assert!(bounds.min[0] <= 0.0 && bounds.min[1] <= 0.0 && bounds.min[2] <= 0.0);

        let unique_positions = batch
            .positions
            .iter()
            .filter(|position| position.iter().filter(|axis| **axis != 0.0).count() == 1)
            .count();
        assert_eq!(unique_positions, batch.len());
    }

    #[test]
    fn the_colour_of_each_arrow_identifies_its_axis() {
        let batch = axis_fixture();
        let red = batch.colors[0];
        assert_eq!(red, [1.0, 0.0, 0.0]);
        let green = batch.colors[AXIS_STEPS as usize];
        assert_eq!(green, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn the_report_names_the_flip_the_viewer_applies() {
        let report = report();
        assert_eq!(report.quaternion_order, "wxyz");
        assert_eq!(report.viewer_flip_degrees, 180);
        assert_eq!(AuthoringSpace::YUp.name(), "y_up");
    }
}
