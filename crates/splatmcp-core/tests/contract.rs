//! Contract-level tests: conventions, round trips, diagnostics and bounded summaries.
//!
//! These are the checks task 11 is accepted against at the core boundary: an asymmetric
//! labelled fixture that survives PLY, a rotated anisotropic gaussian whose *covariance*
//! survives, activated values round tripping within stated tolerances, invalid raw input
//! refused with indexed reasons, and a 500 000 point inspection that stays bounded.
//!
//! The native PlayCanvas capture is not reproducible in a unit test; the viewer side of the
//! contract is pinned here by the transform itself ([`contract::to_viewer_space`]) and by
//! `ui/viewer.js`, which applies that one transform when it attaches a splat.

use splatmcp_core::contract::{self, CONTRACT, IDENTITY_QUATERNION};
use splatmcp_core::fixtures;
use splatmcp_core::validation::{
    self, IssueRecorder, ValidationLimits, ValidationReason,
};
use splatmcp_core::{
    MAX_REPORTED_ISSUES, PlyImportPolicy, Splat, SplatPoint, read_ply, read_ply_repairing,
    read_ply_with_policy,
    write_ply,
};

/// PLY property names, in the order the reader resolves them and this file's rows use.
const ASCII_PROPERTIES: [&str; 14] = [
    "x",
    "y",
    "z",
    "f_dc_0",
    "f_dc_1",
    "f_dc_2",
    "opacity",
    "scale_0",
    "scale_1",
    "scale_2",
    "rot_0",
    "rot_1",
    "rot_2",
    "rot_3",
];

/// An ASCII PLY with one vertex row and the canonical property order.
fn ascii_ply(row: &str) -> Vec<u8> {
    let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 1\n");
    for name in ASCII_PROPERTIES {
        header.push_str(&format!("property float {name}\n"));
    }
    header.push_str(&format!("end_header\n{row}\n"));
    header.into_bytes()
}

#[test]
fn the_contract_is_versioned_and_reported_as_data() {
    assert_eq!(CONTRACT.version, contract::CONTRACT_VERSION);
    assert_eq!(CONTRACT.sh_degree, 0);
    assert_eq!(CONTRACT.up_axis, "-Y");
    assert_eq!(CONTRACT.forward_axis, "+Z");
    assert_eq!(CONTRACT.quaternion_order, "wxyz");
    assert!(CONTRACT.rotation_semantics.starts_with("active"));
    assert!(CONTRACT.color_space.starts_with("linear RGB"));
    assert_eq!(CONTRACT.viewer_flip_degrees, 180);
}

#[test]
fn the_axis_fixture_keeps_its_labels_through_ply_and_the_viewer_flip() {
    let fixture = fixtures::axis_fixture();
    let (loaded, report) = read_ply_with_policy(&write_ply(&fixture).unwrap(), PlyImportPolicy::Strict).unwrap();

    assert_eq!(loaded.len(), fixture.len());
    assert!(fixtures::labels_match_positions(&loaded), "an axis moved");
    assert_eq!(report.total_repairs, 0, "a clean fixture needs no repair");
    // The canonical writer emits placeholder normals the model never keeps; they are
    // reported as dropped rather than silently ignored.
    assert_eq!(report.discarded_names(), vec!["nx", "ny", "nz"]);

    for (before, after) in fixture.points.iter().zip(&loaded.points) {
        for axis in 0..3 {
            assert!((before.position[axis] - after.position[axis]).abs() < 1e-6);
            assert!((before.color[axis] - after.color[axis]).abs() < 1e-6);
            assert!((before.scale[axis] - after.scale[axis]).abs() < 1e-6);
        }
    }

    // The viewer is Y-up: the document +Y arrow is up on screen after exactly one flip, the
    // X axis is untouched, and document +Z points out of the screen. Applying the flip
    // twice would put the arrow back down, which is the mistake this catches.
    let arrow = |axis: usize| {
        loaded
            .points
            .iter()
            .find(|point| point.color == fixtures::AXIS_COLORS[axis])
            .expect("the axis arrow is present")
    };
    let up = contract::to_viewer_space(arrow(1).position);
    assert!(up[1] < 0.0, "document +Y is up in the viewer");
    assert_eq!(up[0], arrow(1).position[0]);
    let right = contract::to_viewer_space(arrow(0).position);
    assert!(right[0] > 0.0, "document +X is still right");
    assert_eq!(right[1].abs(), 0.0);
    let forward = contract::to_viewer_space(arrow(2).position);
    assert!(forward[2] < 0.0, "document +Z points out of the screen");
    assert_eq!(
        contract::to_viewer_space(up),
        arrow(1).position,
        "one flip, not two"
    );
}

#[test]
fn a_rotated_anisotropic_gaussian_keeps_its_covariance_through_ply() {
    let splat = fixtures::rotated_fixture();
    let point = splat.points[0];
    let (loaded, report) = read_ply_with_policy(&write_ply(&splat).unwrap(), PlyImportPolicy::Strict).unwrap();
    let loaded_point = loaded.points[0];

    assert_eq!(report.total_repairs, 0);
    let before = contract::covariance(point.scale, point.rotation);
    let after = contract::covariance(loaded_point.scale, loaded_point.rotation);
    for row in 0..3 {
        for column in 0..3 {
            assert!(
                (before[row][column] - after[row][column]).abs() < 1e-6,
                "covariance[{row}][{column}]: {} vs {}",
                before[row][column],
                after[row][column]
            );
        }
    }

    // Orientation, not just the centre: the long radius still points along document +Y.
    let axis = contract::dominant_axis(loaded_point.scale, loaded_point.rotation).unwrap();
    assert!(axis[1] > 0.999, "{axis:?}");
    assert!(axis[0].abs() < 1e-3, "{axis:?}");
    // The scales themselves stayed anisotropic.
    assert!(loaded_point.scale[0] > loaded_point.scale[1]);
    assert!(loaded_point.scale[1] > loaded_point.scale[2]);
}

#[test]
fn activated_values_round_trip_within_the_stated_tolerances() {
    let splat = Splat::from_points(vec![
        SplatPoint::new(
            [1.0, -2.0, 3.0],
            [1.0e-4, 1.0, 100.0],
            [0.0, 0.5, 1.0],
            1.0,
            [1.0, 0.0, 0.0, 0.0],
        ),
        SplatPoint::new(
            [-1.0, 0.5, 0.0],
            [0.01, 0.02, 0.03],
            [1.0, 0.25, 0.0],
            0.0,
            [0.0, 1.0, 0.0, 0.0],
        ),
    ]);
    let (loaded, report) = read_ply_with_policy(&write_ply(&splat).unwrap(), PlyImportPolicy::Strict).unwrap();
    assert_eq!(report.total_repairs, 0);

    // Colour is a linear SH coefficient: no gamma is applied on either side.
    for channel in 0..3 {
        assert!((loaded.points[0].color[channel] - splat.points[0].color[channel]).abs() < 1e-6);
    }
    // Scale is stored as `ln(radius)`, so the tolerance is relative.
    for axis in 0..3 {
        let expected = splat.points[0].scale[axis];
        let actual = loaded.points[0].scale[axis];
        assert!(
            (expected - actual).abs() <= expected * 1e-4 + 1e-9,
            "scale[{axis}]: {expected} vs {actual}"
        );
    }
    // Opacity is stored as a logit whose endpoints are pulled in by half a 1/255 step; that
    // is the documented serialized convention, so it is not reported as a repair.
    let opaque_endpoint = 1.0 - 0.5 / 255.0;
    let clear_endpoint = 0.5 / 255.0;
    assert!((loaded.points[0].opacity - opaque_endpoint).abs() < 1e-6);
    assert!((loaded.points[1].opacity - clear_endpoint).abs() < 1e-6);
    // Mid-range opacity is preserved far more tightly.
    let mid = Splat::from_points(vec![SplatPoint::new(
        [0.0; 3],
        [0.1; 3],
        [0.5; 3],
        0.4,
        IDENTITY_QUATERNION,
    )]);
    let mid_loaded = read_ply(&write_ply(&mid).unwrap()).unwrap();
    assert!((mid_loaded.points[0].opacity - 0.4).abs() < 1e-6);
}

#[test]
fn invalid_raw_input_is_refused_with_an_indexed_reason_at_every_core_entry_point() {
    // 1. The raw constructor, used by a boundary that has not clamped anything yet.
    let error = SplatPoint::try_new_at(
        3,
        [0.0; 3],
        [0.1, 0.0, 0.1],
        [0.5; 3],
        0.5,
        IDENTITY_QUATERNION,
    )
    .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("point 3 scale"), "{text}");
    assert!(text.contains("positive radius"), "{text}");

    // 2. A batch check, which a boundary runs before it stores anything.
    let mut recorder = IssueRecorder::new();
    for (index, scale) in [[0.1; 3], [0.1, f32::NAN, 0.1], [0.1; 3]].iter().enumerate() {
        if let Some(issue) = validation::check_gaussian(
            index,
            [index as f32, 0.0, 0.0],
            *scale,
            [0.5; 3],
            0.5,
            IDENTITY_QUATERNION,
        ) {
            recorder.record(issue);
        }
    }
    let error = recorder.error(3).unwrap();
    assert_eq!(error.offending_points, 1);
    let text = error.to_string();
    assert!(text.contains("point 1 scale"), "{text}");
    assert!(text.contains("finite"), "{text}");

    // 3. The document invariant, which the app and the writers apply. The forgiving
    //    constructor repairs this colour silently, which is exactly why a boundary checks
    //    the raw value first.
    let repaired = SplatPoint::new(
        [0.0; 3],
        [0.1; 3],
        [1.5, 0.0, 0.0],
        0.5,
        IDENTITY_QUATERNION,
    );
    assert_eq!(repaired.color[0], 1.0, "the forgiving constructor clamps");

    let broken = Splat::from_points(vec![SplatPoint {
        position: [0.0; 3],
        scale: [0.1; 3],
        color: [1.5, 0.0, 0.0],
        opacity: 0.5,
        rotation: IDENTITY_QUATERNION,
    }]);
    let text = broken.validate().unwrap_err().to_string();
    assert!(text.contains("point 0 color"), "{text}");

    let raw = validation::check_values([0.0; 3], [0.1; 3], [1.5, 0.0, 0.0], 0.5, IDENTITY_QUATERNION)
        .expect("an out-of-range colour is invalid");
    assert_eq!(raw.reason, ValidationReason::ColorOutOfRange);
    assert_eq!(raw.reason.code(), "color_out_of_range");

    // 4. The importer, which must not carry an unreadable value into a document.
    let error = read_ply(&ascii_ply("NaN 0 0 0 0 0 0 -8 -8 -8 1 0 0 0"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("vertex 0"), "{error}");
    assert!(error.contains("non-finite position"), "{error}");

    // 5. The importer reports what it had to repair instead of hiding it.
    let (repaired, report) = read_ply_with_policy(
        &ascii_ply("0 0 0 0 0 0 0 NaN NaN NaN 1 0 0 0"),
        PlyImportPolicy::Repair,
    )
        .unwrap();
    // One repair per damaged field of the row: the radius field, not one per axis.
    assert_eq!(report.total_repairs, 1);
    assert_eq!(report.changed_values(), 1);
    assert_eq!(repaired.points[0].scale, [f32::MIN_POSITIVE; 3]);
    assert!(report.repairs.iter().all(|repair| repair.field == "scale"));
    // The same file is refused when repair was not asked for, and the refusal is indexed.
    let refused = read_ply(&ascii_ply("0 0 0 0 0 0 0 NaN NaN NaN 1 0 0 0"))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("point 0 scale"), "{refused}");
    assert!(refused.contains("repair"), "{refused}");
}

#[test]
fn a_report_never_changes_the_document_and_the_limit_is_reported_separately() {
    let splat = fixtures::axis_fixture();
    let before = splat.clone();

    let report = splat.check(ValidationLimits::with_max_points(4));
    assert!(report.is_valid(), "the fixture itself is valid");
    assert!(!report.within_limits, "19 gaussians exceed a limit of 4");
    assert_eq!(report.applied_limit(), Some(4));
    assert!(report.limit_message().unwrap().contains("applied limit of 4"));
    assert!(
        report.issues.is_empty(),
        "a policy limit is not a contract issue"
    );
    assert_eq!(splat, before, "checking is read-only");

    let error = splat.check_strict(ValidationLimits::MATHEMATICAL);
    assert!(error.is_ok());
}

#[test]
fn an_inspection_of_five_hundred_thousand_gaussians_is_bounded() {
    const COUNT: usize = 500_000;
    let splat = Splat::from_points(
        (0..COUNT)
            .map(|index| {
                // Every 500th gaussian is damaged, so the report has to stay bounded while
                // still counting everything.
                let radius = if index % 500 == 0 { 0.0 } else { 0.01 };
                SplatPoint::new(
                    [index as f32 * 0.001, 0.0, 0.0],
                    [radius; 3],
                    [0.5, 0.5, 0.5],
                    0.5,
                    IDENTITY_QUATERNION,
                )
            })
            .collect(),
    );

    let report = splat.inspection(ValidationLimits::default());
    assert_eq!(report.point_count, COUNT);
    assert_eq!(report.validation.offending_points, COUNT / 500);
    assert_eq!(report.validation.issues.len(), MAX_REPORTED_ISSUES);
    assert!(report.validation.truncated);
    assert_eq!(report.owned.points, COUNT * std::mem::size_of::<SplatPoint>());
    assert!(report.largest_radius.max > 0.0);

    // Bounded metadata: a fixed-size report and a short summary, never the points.
    assert!(std::mem::size_of::<splatmcp_core::InspectionReport>() < 1024);
    let summary = report.summary();
    assert!(summary.len() < 400, "{summary}");
    assert!(summary.contains("1000 of 500000 gaussians"), "{summary}");
}

#[test]
fn a_valid_file_from_elsewhere_reports_nothing_to_fix() {
    // `tests/data/external_grid.ply` is authored outside this crate; the report must not
    // invent repairs for it.
    let bytes = include_bytes!("data/external_grid.ply");
    let (splat, report) = read_ply_with_policy(bytes, PlyImportPolicy::Strict).unwrap();
    assert_eq!(splat.len(), 189);
    assert_eq!(report.vertex_count, 189);
    assert_eq!(report.total_repairs, 0, "{}", report.summary());
    assert!(!report.ascii, "the sample is a binary PLY");
    assert_eq!(splat, read_ply(bytes).unwrap());
}

#[test]
fn the_reviewed_invalid_quaternion_file_is_refused_strictly_and_repaired_on_request() {
    // The exact file the review reproduced the defect with: five gaussians, the first
    // carrying a [0, 0, 0, 0] quaternion, which used to become the identity rotation in
    // silence - the load succeeded and nothing in the reply said a value had been replaced.
    let bytes = include_bytes!("data/invalid_quaternion.ply");

    // Strict, which is now the default: refused, indexed, and told how to proceed.
    let error = read_ply(bytes).unwrap_err().to_string();
    assert!(error.contains("point 0 rotation"), "{error}");
    assert!(error.contains("1 of 5 gaussians are invalid"), "{error}");
    assert!(error.contains("[0, 0, 0, 0]"), "{error}");
    assert!(error.contains("repair"), "{error}");

    // Repair, which is now an explicit decision the caller makes and sees.
    let (splat, report) = read_ply_repairing(bytes).unwrap();
    assert_eq!(splat.len(), 5);
    assert_eq!(report.total_repairs, 1, "exactly the one damaged value");
    assert_eq!(report.repairs[0].point, 0);
    assert_eq!(report.repairs[0].field, "rotation");
    assert_eq!(splat.points[0].rotation, [1.0, 0.0, 0.0, 0.0], "the value that replaced it");
    // Everything else is untouched: the quarter turn on the last gaussian survives, and
    // an ordinary unit quaternion is not reported as rescaled.
    assert!((splat.points[4].rotation[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4);
    assert_eq!(report.total_normalized, 0, "float rounding stays quiet");
    assert_eq!(splat.points[1].rotation, [1.0, 0.0, 0.0, 0.0]);
    splat.validate().unwrap();

    // The attributes the model cannot keep are named too, so a caller sees the whole
    // import rather than only the repair.
    assert_eq!(report.discarded_names(), vec!["nx", "ny", "nz"]);
    assert_eq!(report.vertex_count, 5);

    // And the wire summary a tool reply carries says the same thing.
    let summary = splatmcp_core::PlyReport::summary(&report);
    assert!(summary.contains("1 value(s) repaired"), "{summary}");
}
