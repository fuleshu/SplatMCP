//! End-to-end check against a PLY file authored by hand (Python), i.e. bytes that
//! this crate did not produce. Guards the reader against real 3DGS exports.

use std::fs;

use splatmcp_core::{Splat, read_ply, write_ply};

const SAMPLE: &[u8] = include_bytes!("data/external_grid.ply");

#[test]
fn reads_an_externally_authored_ply() {
    let splat = read_ply(SAMPLE).expect("sample should parse");
    assert_eq!(splat.len(), 189);

    // First point of the generated grid: position (-1.5, -0.5, -1.5), red-ish.
    let first = &splat.points[0];
    for (axis, expected) in [-1.5_f32, -0.5, -1.5].iter().enumerate() {
        assert!((first.position[axis] - expected).abs() < 1e-6, "axis {axis}");
    }
    assert!((first.color[0] - 0.85).abs() < 1e-5, "red {}", first.color[0]);
    assert!((first.color[1] - 0.15).abs() < 1e-5, "green {}", first.color[1]);
    assert!((first.color[2] - 0.10).abs() < 1e-5, "blue {}", first.color[2]);
    assert!((first.scale[0] - 0.06).abs() < 1e-6);
    assert!((first.opacity - 0.9).abs() < 1e-4, "opacity {}", first.opacity);
    assert_eq!(first.rotation, [1.0, 0.0, 0.0, 0.0]);

    // The splat is valid and its bounds cover the whole grid.
    splat.validate().expect("sample should validate");
    let bounds = splat.bounds().expect("non-empty splat has bounds");
    assert!(bounds.min[0] <= -1.5 && bounds.max[0] >= 1.5);
}

#[test]
fn re_exporting_an_externally_authored_ply_keeps_the_points() {
    let splat = read_ply(SAMPLE).expect("sample should parse");
    let bytes = write_ply(&splat).expect("re-export should succeed");
    let round_tripped = read_ply(&bytes).expect("re-export should parse back");

    assert_eq!(round_tripped.len(), splat.len());
    let stats_before = splat.stats();
    let stats_after = round_tripped.stats();
    assert_eq!(stats_before.point_count, stats_after.point_count);
    assert!((stats_before.mean_color[0] - stats_after.mean_color[0]).abs() < 1e-5);
    assert!((stats_before.min_opacity - stats_after.min_opacity).abs() < 1e-4);
    let before = stats_before.bounds.expect("non-empty splat has bounds");
    let after = stats_after.bounds.expect("non-empty splat has bounds");
    for axis in 0..3 {
        assert!((before.min[axis] - after.min[axis]).abs() < 1e-4);
        assert!((before.max[axis] - after.max[axis]).abs() < 1e-4);
    }
}

#[test]
fn an_empty_splat_is_rejected_rather_than_written() {
    // A splat with no points cannot be rendered, so the core refuses to write one.
    let error = write_ply(&Splat::new()).expect_err("empty splat must not be written");
    assert!(error.to_string().contains("no points"), "{error}");
}
