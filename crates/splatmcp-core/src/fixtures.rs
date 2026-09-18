//! Reference fixtures that make a convention error visible.
//!
//! A symmetric model hides sign and axis mistakes: flipping it twice, or swapping two
//! axes, still produces something that looks plausible. These fixtures are deliberately
//! asymmetric, so a mistake changes bounds, colours or covariance:
//!
//! - [`axis_fixture`] holds three short arrows along `+X`, `+Y` and `+Z`, each in its own
//!   colour, plus one grey marker further along `+Y`. The colour answers "which axis is
//!   this?" and the position answers "does the colour agree?" - so a swapped or doubly
//!   flipped axis is a *reported mismatch*, not a judgement call.
//! - [`rotated_gaussian`] is a single non-spherical gaussian turned 90 degrees about Z, so
//!   its longest radius lies along document `+Y`. Comparing point centres cannot detect a
//!   lost rotation; comparing [`crate::contract::covariance`] can.
//!
//! They are plain model data, so a test, a tool call or a captured frame can all use the
//! same numbers - there is no second definition of the fixture for the viewer.

use crate::contract::IDENTITY_QUATERNION;
use crate::{Splat, SplatPoint};

/// Gaussians per laboured axis in [`axis_fixture`].
pub const AXIS_STEPS: usize = 6;
/// Distance between fixture gaussians along an axis, in metres.
pub const AXIS_STEP_LENGTH: f32 = 0.2;
/// Radius of the first gaussian of an arrow, in metres.
pub const AXIS_RADIUS: f32 = 0.04;
/// Colour of each labelled arrow, in document axis order `X`, `Y`, `Z`.
pub const AXIS_COLORS: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
/// Colour of the extra `+Y` marker, which labels nothing.
pub const MARKER_COLOR: [f32; 3] = [0.9, 0.9, 0.9];
/// Opacity of every fixture gaussian: opaque, so a capture shows it plainly.
pub const FIXTURE_OPACITY: f32 = 1.0;

/// The three-arrow, one-marker fixture described in the module docs.
pub fn axis_fixture() -> Splat {
    let mut points = Vec::with_capacity(3 * AXIS_STEPS + 1);
    for (axis, color) in AXIS_COLORS.iter().enumerate() {
        for step in 1..=AXIS_STEPS {
            let mut position = [0.0f32; 3];
            position[axis] = step as f32 * AXIS_STEP_LENGTH;
            // Each arrow tapers, so its tip is identifiable in a frame as well.
            let radius = AXIS_RADIUS * (1.0 - 0.1 * step as f32);
            points.push(SplatPoint {
                position,
                scale: [radius; 3],
                color: *color,
                opacity: FIXTURE_OPACITY,
                rotation: IDENTITY_QUATERNION,
            });
        }
    }
    points.push(SplatPoint {
        position: [0.0, AXIS_STEPS as f32 * AXIS_STEP_LENGTH * 1.5, 0.0],
        scale: [AXIS_RADIUS; 3],
        color: MARKER_COLOR,
        opacity: FIXTURE_OPACITY,
        rotation: IDENTITY_QUATERNION,
    });
    Splat::from_points(points)
}

/// A single non-spherical gaussian, rotated 90 degrees about Z.
///
/// Its local `X` radius (0.3 m) is the longest, and the rotation sends local `+X` onto
/// document `+Y`, so an inspection or a render must show the long axis pointing along
/// `+Y` - not along `+X`, and not along `-Y`.
pub fn rotated_gaussian() -> SplatPoint {
    // 90 degrees about Z: cos(45) + sin(45) * k.
    let half = std::f32::consts::FRAC_PI_4;
    SplatPoint {
        position: [0.5, 0.0, 0.25],
        scale: [0.3, 0.1, 0.05],
        color: [0.9, 0.4, 0.1],
        opacity: 0.85,
        rotation: [half.cos(), 0.0, 0.0, half.sin()],
    }
}

/// [`rotated_gaussian`] as a one-point splat.
pub fn rotated_fixture() -> Splat {
    Splat::from_points(vec![rotated_gaussian()])
}

/// Index of the document axis a fixture arrow is labelled with, from its colour.
///
/// `None` for the grey marker and for anything that is not fixture data.
pub fn labelled_axis(point: &SplatPoint) -> Option<usize> {
    AXIS_COLORS.iter().position(|color| point.color == *color)
}

/// Index of the document axis a fixture arrow sits on, from its position.
///
/// `None` for the grey marker, which is offset along `+Y` but labels nothing, and for a
/// position that is not on exactly one axis.
pub fn positioned_axis(point: &SplatPoint) -> Option<usize> {
    if point.color == MARKER_COLOR {
        return None;
    }
    let nonzero: Vec<usize> = (0..3).filter(|axis| point.position[*axis] != 0.0).collect();
    match nonzero.as_slice() {
        [axis] => Some(*axis),
        _ => None,
    }
}

/// True when every arrow's colour still names the axis it sits on.
///
/// A test, a tool or a captured frame all answer the same question with this: if a flip or
/// a swap happened anywhere between authoring and display, the label no longer matches.
pub fn labels_match_positions(splat: &Splat) -> bool {
    splat.points.iter().all(|point| match labelled_axis(point) {
        Some(label) => positioned_axis(point) == Some(label),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_axis_fixture_is_asymmetric_and_labelled() {
        let splat = axis_fixture();
        assert_eq!(splat.len(), 3 * AXIS_STEPS + 1);
        assert!(labels_match_positions(&splat));
        assert!(splat.validate().is_ok());

        // Every arrow reaches its own positive axis and no arrow reaches another's.
        let bounds = splat.bounds().unwrap();
        for axis in 0..3 {
            assert!(bounds.max[axis] > 0.0, "axis {axis} is not reached");
        }
        for (axis, color) in AXIS_COLORS.iter().enumerate() {
            let tips: Vec<&SplatPoint> = splat
                .points
                .iter()
                .filter(|point| point.color == *color && point.position[axis] > 0.0)
                .collect();
            assert_eq!(tips.len(), AXIS_STEPS, "axis {axis} arrow is incomplete");
            assert!(
                tips.iter().all(|point| point
                    .position
                    .iter()
                    .filter(|value| **value != 0.0)
                    .count()
                    == 1),
                "an arrow left its axis"
            );
        }

        // The grey marker sits beyond every arrow, and labels nothing.
        let marker = splat
            .points
            .iter()
            .find(|point| point.color == MARKER_COLOR)
            .expect("the marker exists");
        assert_eq!(positioned_axis(marker), None);
        assert_eq!(labelled_axis(marker), None);
        assert!(marker.position[1] > bounds.center[1]);
    }

    #[test]
    fn the_fixture_colour_identifies_the_positioned_axis() {
        let splat = axis_fixture();
        for point in &splat.points {
            match labelled_axis(point) {
                Some(label) => assert_eq!(positioned_axis(point), Some(label)),
                None => assert_eq!(positioned_axis(point), None),
            }
        }
        // A deliberately swapped fixture is detected.
        let mut swapped = axis_fixture();
        swapped.points[0].color = AXIS_COLORS[1];
        assert!(!labels_match_positions(&swapped));
    }

    #[test]
    fn the_rotated_gaussian_keeps_its_anisotropy_and_orientation() {
        let point = rotated_gaussian();
        assert!(point.scale[0] > point.scale[1] && point.scale[1] > point.scale[2]);
        let axis = crate::contract::dominant_axis(point.scale, point.rotation).unwrap();
        assert!(axis[0].abs() < 1e-6, "{axis:?}");
        assert!((axis[1] - 1.0).abs() < 1e-6, "{axis:?}");

        let matrix = crate::contract::covariance(point.scale, point.rotation);
        assert!((matrix[1][1] - 0.09).abs() < 1e-6, "{matrix:?}");
        assert!((matrix[0][0] - 0.01).abs() < 1e-6, "{matrix:?}");
        assert!((matrix[2][2] - 0.0025).abs() < 1e-6, "{matrix:?}");
    }

    #[test]
    fn the_fixtures_survive_the_viewer_flip_there_and_back() {
        // A double flip is the classic mistake; the transform is its own inverse, so this
        // is a statement about the contract rather than about the fixture.
        for point in axis_fixture().points {
            let viewer = crate::contract::to_viewer_space(point.position);
            assert_eq!(crate::contract::from_viewer_space(viewer), point.position);
        }
    }

    #[test]
    fn the_rotated_fixture_is_a_single_valid_gaussian() {
        let splat = rotated_fixture();
        assert_eq!(splat.len(), 1);
        assert!(splat.validate().is_ok());
    }
}
