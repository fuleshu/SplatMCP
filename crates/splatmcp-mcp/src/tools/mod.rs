//! Tool implementation helpers.
//!
//! The `#[tool]` entry points live in `lib.rs` so a single tool router is generated; the
//! parameter types and the work they do live here, where they can be tested without an
//! MCP session.

pub mod author;
pub mod edit;
pub mod python;
pub mod viewer;

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde::Serialize;
use splatmcp_bridge::InspectionSummary;
use splatmcp_core::{Bounds, Splat, SplatPoint};

use crate::tools::edit::DocumentIdentity;

/// A factor that may be given as one number or one per axis.
///
/// Accepting both keeps the schema small and the call obvious: `"factor": 2` for a
/// uniform scale is what a caller writes first, and `[1, 2, 1]` still works. The same
/// shape is used for a gaussian radius, so a caller only learns one convention.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Factor {
    /// Same value on every axis.
    All(f32),
    /// One value per axis.
    PerAxis([f32; 3]),
}

impl Factor {
    /// One value per axis.
    pub fn axes(self) -> [f32; 3] {
        match self {
            Self::All(value) => [value; 3],
            Self::PerAxis(values) => values,
        }
    }

    /// The first axis' value, for operations that only take one number.
    pub fn single(self) -> f32 {
        self.axes()[0]
    }
}

/// Rounds a float to three decimals for tool replies.
///
/// The values are for a human or a model reading JSON, so full `f32` noise only wastes
/// context. Negative zero is folded to zero so replies stay clean.
pub fn round3(value: f32) -> f32 {
    let rounded = (value * 1000.0).round() / 1000.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

/// Rounded copy of a vector.
pub fn round3_vec(values: [f32; 3]) -> [f32; 3] {
    values.map(round3)
}

/// Compact description of a splat, returned by every tool that produces or reads one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplatSummary {
    pub point_count: usize,
    pub center: [f32; 3],
    pub radius: f32,
    pub min_opacity: f32,
    pub max_opacity: f32,
    pub mean_color: [f32; 3],
}

impl SplatSummary {
    /// Summarises a document from an inspection reply instead of its gaussians.
    ///
    /// `splat_info` reads the displayed document as bounded metadata when no sample was
    /// asked for, and the reply keeps the same summary fields either way.
    pub fn of_inspection(inspection: &InspectionSummary) -> Self {
        let bounds = inspection.bounds.unwrap_or(splatmcp_bridge::BoundsSummary {
            min: [0.0; 3],
            max: [0.0; 3],
            center: [0.0; 3],
            radius: 0.0,
        });
        Self {
            point_count: inspection.point_count,
            center: round3_vec(bounds.center),
            radius: round3(bounds.radius),
            min_opacity: round3(inspection.opacity.min),
            max_opacity: round3(inspection.opacity.max),
            mean_color: round3_vec(inspection.mean_color),
        }
    }

    /// Summarises a splat, using zeroed bounds for an empty one.
    pub fn of(splat: &Splat) -> Self {
        let stats = splat.stats();
        let bounds = stats.bounds.unwrap_or(Bounds {
            min: [0.0; 3],
            max: [0.0; 3],
            center: [0.0; 3],
            radius: 0.0,
        });
        Self {
            point_count: stats.point_count,
            center: round3_vec(bounds.center),
            radius: round3(bounds.radius),
            min_opacity: round3(stats.min_opacity),
            max_opacity: round3(stats.max_opacity),
            mean_color: round3_vec(stats.mean_color),
        }
    }
}

/// Reply of a tool that produced or edited a splat.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplatReply {
    #[serde(flatten)]
    pub summary: SplatSummary,
    /// Identity of the document revision the app now displays.
    ///
    /// Present when the app reported it, so a caller can quote the revision back instead of
    /// guessing which document its edit landed in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentIdentity>,
    /// File the splat was written to, when the call asked for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// True when the desktop app is now showing this splat.
    pub displayed: bool,
}

impl SplatReply {
    pub fn new(splat: &Splat, path: Option<&std::path::Path>, displayed: bool) -> Self {
        Self {
            summary: SplatSummary::of(splat),
            document: None,
            path: path.map(|path| path.to_string_lossy().to_string()),
            displayed,
        }
    }

    /// Same reply, reporting the identity the app resolved.
    pub fn with_document(mut self, document: Option<DocumentIdentity>) -> Self {
        self.document = document;
        self
    }
}

/// One gaussian in a JSON reply.
///
/// A typed struct rather than `serde_json::Value`: widening an `f32` through `Value`
/// prints `0.10000000149011612` where the caller wrote `0.1`, which is pure noise in a
/// reply that can hold many points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PointOut {
    pub position: [f32; 3],
    pub scale: [f32; 3],
    pub color: [f32; 3],
    pub opacity: f32,
    pub rotation: [f32; 4],
}

impl PointOut {
    /// Rounded copy of a model point.
    pub fn of(point: &SplatPoint) -> Self {
        Self {
            position: round3_vec(point.position),
            scale: round3_vec(point.scale),
            color: round3_vec(point.color),
            opacity: round3(point.opacity),
            rotation: point.rotation.map(round3),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_keeps_three_decimals() {
        assert_eq!(round3(0.123_456), 0.123);
        assert_eq!(round3_vec([1.0, 1.234_56, -2.0]), [1.0, 1.235, -2.0]);
    }

    #[test]
    fn a_point_keeps_short_float_forms() {
        let point = SplatPoint::new(
            [0.1, 0.2, 0.3],
            [0.01, 0.02, 0.03],
            [0.4, 0.5, 0.6],
            0.7,
            [1.0, 0.0, 0.0, 0.0],
        );
        let encoded = serde_json::to_string(&PointOut::of(&point)).unwrap();
        assert_eq!(
            encoded,
            "{\"position\":[0.1,0.2,0.3],\"scale\":[0.01,0.02,0.03],\
             \"color\":[0.4,0.5,0.6],\"opacity\":0.7,\"rotation\":[1.0,0.0,0.0,0.0]}"
        );
    }

    #[test]
    fn a_summary_reports_bounds_and_colour() {
        let splat = Splat::from_points(vec![
            SplatPoint::new(
                [0.0, 0.0, 0.0],
                [0.5, 0.5, 0.5],
                [1.0, 0.0, 0.0],
                1.0,
                [1.0, 0.0, 0.0, 0.0],
            ),
            SplatPoint::new(
                [2.0, 0.0, 0.0],
                [0.5, 0.5, 0.5],
                [0.0, 0.0, 1.0],
                0.5,
                [1.0, 0.0, 0.0, 0.0],
            ),
        ]);
        let summary = SplatSummary::of(&splat);
        assert_eq!(summary.point_count, 2);
        assert_eq!(summary.center, [1.0, 0.0, 0.0]);
        assert_eq!(summary.radius, 1.5);
        assert_eq!(summary.mean_color, [0.5, 0.0, 0.5]);
        assert_eq!(summary.min_opacity, 0.5);
        let encoded = serde_json::to_string(&summary).unwrap();
        assert!(!encoded.contains("opacity_range"));
    }

    #[test]
    fn an_empty_splat_summarises_to_zeroes() {
        let summary = SplatSummary::of(&Splat::new());
        assert_eq!(summary.point_count, 0);
        assert_eq!(summary.center, [0.0, 0.0, 0.0]);
        assert_eq!(summary.radius, 0.0);
    }

    #[test]
    fn a_reply_can_name_the_document_it_landed_in() {
        let splat = Splat::from_points(vec![SplatPoint::new(
            [0.0; 3],
            [0.1; 3],
            [0.5; 3],
            0.5,
            [1.0, 0.0, 0.0, 0.0],
        )]);
        let plain = SplatReply::new(&splat, None, true);
        let encoded = serde_json::to_string(&plain).unwrap();
        assert!(!encoded.contains("document"), "{encoded}");

        let named = plain.with_document(Some(DocumentIdentity {
            document_id: "doc-1-2".to_owned(),
            revision: 4,
        }));
        let encoded = serde_json::to_string(&named).unwrap();
        assert!(
            encoded.contains("\"document\":{\"document_id\":\"doc-1-2\",\"revision\":4}"),
            "{encoded}"
        );
    }

    #[test]
    fn a_summary_can_be_built_from_bounded_metadata() {
        let splat = splatmcp_core::fixtures::axis_fixture();
        let inspection =
            InspectionSummary::from(&splat.inspection(splatmcp_core::ValidationLimits::default()));
        let from_metadata = SplatSummary::of_inspection(&inspection);
        let from_splat = SplatSummary::of(&splat);
        assert_eq!(from_metadata, from_splat);

        // An empty document summarises to zeroes, exactly as an empty splat does.
        let empty = InspectionSummary::default();
        assert_eq!(SplatSummary::of_inspection(&empty).point_count, 0);
        assert_eq!(SplatSummary::of_inspection(&empty).radius, 0.0);
    }
}
