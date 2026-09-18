//! Authoring tools: `create_splat`, and the shared description of where a splat goes.
//!
//! A tool parameters struct is mapped onto [`SplatParams`] here, keeping the wire schema
//! flat and every field optional so a model can start with `{}` and refine.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use splatmcp_bridge::protocol::ViewerStatus;
use splatmcp_bridge::{Method, load_ply_params, replace_ply_params};
use splatmcp_core::validation::{IssueRecorder, ValidationIssue};
use splatmcp_core::{MAX_POINTS, Shape, Splat, SplatParams, SplatPoint, build, splat_from_points};
use std::path::{Path, PathBuf};

use crate::bridge::AppLink;
use crate::tools::edit::DocumentIdentity;
use crate::tools::{Factor, SplatReply};

/// Default seed. Zero is a fine default because the generator is deterministic anyway.
pub const DEFAULT_SEED: u64 = 1;

/// Parameters of `create_splat`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CreateInput {
    /// `sphere`, `cube`, `plane`, `line`, `shell`, `ring` or `grid`. Default `sphere`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    /// Number of gaussians (1 to 2,000,000). Default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// Centre in world metres `[x, y, z]`. Default `[0, 0, 0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<[f32; 3]>,
    /// Half extent in metres. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Linear RGB in `0..=1`. Default a muted red.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// Opacity in `0..=1`. Default 0.9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Gaussian radius in metres. Default 0.02.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    /// Position jitter as a fraction of `size`, for an irregular look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<f32>,
    /// Per-channel colour variation in `0..=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_variation: Option<f32>,
    /// Seed for jitter and variation; the same seed always gives the same splat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Explicit points, used instead of `shape` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<PointInput>>,
    /// Write the result to this `.ply` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Show the result in the SplatMCP window. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// One explicit gaussian.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, JsonSchema)]
pub struct PointInput {
    /// World position in metres `[x, y, z]`.
    pub position: [f32; 3],
    /// Linear RGB in `0..=1`. Default a muted red.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// Opacity in `0..=1`. Default 0.9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Ellipsoid radius in metres: one number or `[x, y, z]`. Default 0.02.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Factor>,
    /// Orientation as a `(w, x, y, z)` quaternion. Default identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 4]>,
}

/// Colour used when a point does not give one.
pub const DEFAULT_POINT_COLOR: [f32; 3] = [0.85, 0.25, 0.2];
/// Opacity used when a point does not give one.
pub const DEFAULT_POINT_OPACITY: f32 = 0.9;
/// Radius used when a point does not give one, in metres.
pub const DEFAULT_POINT_RADIUS: f32 = 0.02;

impl PointInput {
    /// New point at `position` with the documented defaults.
    pub fn at(position: [f32; 3]) -> Self {
        Self {
            position,
            color: None,
            opacity: None,
            scale: None,
            rotation: None,
        }
    }

    /// The values this input stands for, with the documented defaults filled in.
    ///
    /// The defaults live here rather than in `#[serde(default)]` attributes: a `serde`
    /// default of an `f32` is re-emitted in the tool schema as a widened `f64`
    /// (`0.8500000238418579`), which is both misleading and wasteful.
    fn resolved(&self) -> ([f32; 3], [f32; 3], [f32; 3], f32, [f32; 4]) {
        (
            self.position,
            self.scale
                .unwrap_or(Factor::All(DEFAULT_POINT_RADIUS))
                .axes(),
            self.color.unwrap_or(DEFAULT_POINT_COLOR),
            self.opacity.unwrap_or(DEFAULT_POINT_OPACITY),
            self.rotation.unwrap_or([1.0, 0.0, 0.0, 0.0]),
        )
    }

    /// Model-side point, checked against the contract before anything is clamped.
    ///
    /// A tool call is a boundary, so a non-finite value, a zero radius, an out-of-range
    /// colour or opacity or a degenerate quaternion is reported - with the index of the
    /// gaussian - instead of being silently repaired into a different splat. The rotation
    /// is normalised, which is the contract's documented policy and loses nothing.
    pub fn checked_point(&self, index: usize) -> Result<SplatPoint, ValidationIssue> {
        let (position, scale, color, opacity, rotation) = self.resolved();
        if let Some(issue) = splatmcp_core::validation::check_gaussian(
            index, position, scale, color, opacity, rotation,
        ) {
            return Err(issue);
        }
        Ok(SplatPoint {
            position,
            scale,
            color,
            opacity,
            rotation: splatmcp_core::contract::normalized_quaternion(rotation)
                .unwrap_or(splatmcp_core::contract::IDENTITY_QUATERNION),
        })
    }
}

/// Turns tool parameters into builder parameters, rejecting what the builder cannot use.
pub fn splat_params(input: &CreateInput) -> Result<SplatParams, String> {
    let shape = match input.shape.as_deref() {
        None => Shape::Sphere,
        Some(name) => Shape::parse(name).ok_or_else(|| {
            format!(
                "unknown shape '{name}'; use one of {}",
                Shape::NAMES.join(", ")
            )
        })?,
    };
    let grid = if shape == Shape::Grid {
        // A grid is square, so `count` is honoured by rounding its square root up.
        let requested = input.count.unwrap_or(1024);
        ((requested as f64).sqrt().ceil() as usize).max(1)
    } else {
        1
    };
    let defaults = SplatParams::default();
    let params = SplatParams {
        shape,
        count: if shape == Shape::Grid {
            grid * grid
        } else {
            input.count.unwrap_or(defaults.count)
        },
        center: input.center.unwrap_or(defaults.center),
        size: input.size.unwrap_or(defaults.size),
        color: input.color.unwrap_or(defaults.color),
        opacity: input.opacity.unwrap_or(defaults.opacity),
        grid,
        jitter: input.jitter.unwrap_or(defaults.jitter),
        radius: input.radius.unwrap_or(defaults.radius),
        color_variation: input.color_variation.unwrap_or(defaults.color_variation),
        seed: input.seed.unwrap_or(DEFAULT_SEED),
        random_rotation: input.color_variation.unwrap_or(0.0) > 0.0,
    };
    if params.count > MAX_POINTS {
        return Err(format!(
            "count {} is above the {MAX_POINTS} point limit of one call; build a smaller \
             splat and add to it with edit_splat",
            params.count
        ));
    }
    params.validate().map_err(|error| error.to_string())?;
    Ok(params)
}

/// Builds the splat a `create_splat` call describes.
pub fn build_splat(input: &CreateInput) -> Result<Splat, String> {
    match &input.points {
        Some(points) => {
            if points.is_empty() {
                return Err("points is empty; omit it to use shape instead".to_owned());
            }
            if points.len() > MAX_POINTS {
                return Err(format!(
                    "points holds {} entries, above the {MAX_POINTS} point limit",
                    points.len()
                ));
            }
            let mut recorder = IssueRecorder::new();
            let mut built = Vec::with_capacity(points.len());
            for (index, input) in points.iter().enumerate() {
                match input.checked_point(index) {
                    Ok(point) => built.push(point),
                    Err(issue) => recorder.record(issue),
                }
            }
            if let Some(error) = recorder.error(points.len()) {
                return Err(format!("points: {error}"));
            }
            splat_from_points(built).map_err(|error| error.to_string())
        }
        None => build(&splat_params(input)?).map_err(|error| error.to_string()),
    }
}

/// Writes PLY bytes to `path`, creating the parent directory when needed.
pub fn write_splat_file(path: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("path is empty".to_owned());
    }
    let path = PathBuf::from(trimmed);
    if path.extension().is_none() {
        return Err(format!(
            "{} has no file extension; give a .ply path",
            path.display()
        ));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Sends PLY bytes to the desktop app and waits until the viewer shows them.
///
/// With no `target` the bytes become a new document: that is what creating a splat or loading
/// a file means. With a `target` the load is an explicit replacement of that document at that
/// revision, which is how an edit keeps the identity it edited - and how a stale edit is
/// refused instead of overwriting newer work.
pub fn display_splat(
    link: &AppLink,
    file_name: &str,
    bytes: &[u8],
    target: Option<&DocumentIdentity>,
) -> Result<ViewerStatus, String> {
    let encoded = BASE64.encode(bytes);
    let params = match target {
        Some(target) => {
            replace_ply_params(encoded, &target.document_id, target.revision, Some(false))
        }
        None => load_ply_params(encoded, Some(file_name.to_owned())),
    };
    link.request_typed(Method::ViewerLoadPly, params)
}

/// Standard reply for a tool that produced a splat.
pub fn splat_reply(
    splat: &Splat,
    path: Option<&Path>,
    displayed: bool,
    status: Option<&ViewerStatus>,
) -> SplatReply {
    let document = status
        .and_then(|status| status.document.as_ref())
        .and_then(|summary| DocumentIdentity::of_summary(Some(summary)));
    SplatReply::new(splat, path, displayed).with_document(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-point ASCII PLY whose first quaternion is all zero.
    fn ascii_with_zero_quaternion() -> Vec<u8> {
        const PROPERTIES: [&str; 14] = [
            "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
            "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
        ];
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 2\n");
        for name in PROPERTIES {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str("end_header\n");
        header.push_str("0 0 0 0 0 0 0 -8 -8 -8 0 0 0 0\n");
        header.push_str("1 0 0 0 0 0 0 -8 -8 -8 1 0 0 0\n");
        header.into_bytes()
    }

    #[test]
    fn a_bare_call_builds_the_documented_default() {
        let splat = build_splat(&CreateInput::default()).unwrap();
        assert_eq!(splat.len(), 1000);
        let summary = crate::tools::SplatSummary::of(&splat);
        // The sample centre of a random ball sits near, but not exactly at, the origin.
        for axis in 0..3 {
            assert!(
                summary.center[axis].abs() < 0.05,
                "axis {axis}: {:?}",
                summary.center
            );
        }
        assert!((summary.radius - 1.02).abs() < 0.05, "{:?}", summary.radius);
        assert_eq!(summary.mean_color, [0.85, 0.25, 0.2]);
    }

    #[test]
    fn shape_names_are_validated_with_a_helpful_message() {
        let error = build_splat(&CreateInput {
            shape: Some("sprinkle".to_owned()),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(error.contains("unknown shape 'sprinkle'"), "{error}");
        assert!(error.contains("sphere"), "{error}");
        assert!(error.contains("grid"), "{error}");

        let alias = build_splat(&CreateInput {
            shape: Some("BOX".to_owned()),
            count: Some(8),
            ..CreateInput::default()
        })
        .unwrap();
        assert_eq!(alias.len(), 8);
    }

    #[test]
    fn a_grid_call_reports_a_square_point_count() {
        let splat = build_splat(&CreateInput {
            shape: Some("grid".to_owned()),
            count: Some(50),
            size: Some(2.0),
            ..CreateInput::default()
        })
        .unwrap();
        // ceil(sqrt(50)) = 8, so 64 points.
        assert_eq!(splat.len(), 64);
        let bounds = splat.bounds().unwrap();
        assert!((bounds.max[0] - 2.02).abs() < 0.01, "{:?}", bounds.max);
    }

    #[test]
    fn explicit_points_win_over_the_shape() {
        let splat = build_splat(&CreateInput {
            shape: Some("cube".to_owned()),
            count: Some(100),
            points: Some(vec![
                PointInput {
                    position: [1.0, 2.0, 3.0],
                    color: Some([1.0, 0.0, 0.0]),
                    opacity: Some(0.5),
                    scale: Some(Factor::PerAxis([0.1, 0.2, 0.3])),
                    rotation: None,
                },
                PointInput::at([-1.0, 0.0, 0.0]),
            ]),
            ..CreateInput::default()
        })
        .unwrap();
        assert_eq!(splat.len(), 2);
        assert_eq!(splat.points[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(splat.points[0].scale, [0.1, 0.2, 0.3]);
        assert_eq!(splat.points[0].opacity, 0.5);
        // The defaults of a bare point are the documented ones.
        assert_eq!(splat.points[1].scale, [DEFAULT_POINT_RADIUS; 3]);
        assert_eq!(splat.points[1].color, DEFAULT_POINT_COLOR);
        assert_eq!(splat.points[1].opacity, DEFAULT_POINT_OPACITY);
        assert_eq!(splat.points[1].rotation, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_point_radius_accepts_one_number_or_three() {
        let uniform = PointInput {
            scale: Some(Factor::All(0.5)),
            ..PointInput::at([0.0; 3])
        };
        assert_eq!(uniform.checked_point(0).unwrap().scale, [0.5; 3]);

        let per_axis: PointInput =
            serde_json::from_str(r#"{"position":[0,0,0],"scale":[0.01,0.02,0.03]}"#).unwrap();
        assert_eq!(per_axis.checked_point(0).unwrap().scale, [0.01, 0.02, 0.03]);

        // Colour, opacity and rotation can be omitted entirely.
        let bare: PointInput = serde_json::from_str(r#"{"position":[1,2,3]}"#).unwrap();
        let point = bare.checked_point(0).unwrap();
        assert_eq!(point.position, [1.0, 2.0, 3.0]);
        assert_eq!(point.color, DEFAULT_POINT_COLOR);
    }

    #[test]
    fn an_empty_points_array_is_rejected() {
        let error = build_splat(&CreateInput {
            points: Some(Vec::new()),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(error.contains("points is empty"), "{error}");
    }

    #[test]
    fn a_bad_parameter_names_itself() {
        let error = build_splat(&CreateInput {
            radius: Some(0.0),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(error.contains("radius"), "{error}");

        let error = build_splat(&CreateInput {
            count: Some(MAX_POINTS + 1),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(error.contains("point limit"), "{error}");
    }

    #[test]
    fn a_file_without_an_extension_is_refused() {
        let error = write_splat_file("C:/tmp/splat", b"data").unwrap_err();
        assert!(error.contains("extension"), "{error}");
        let error = write_splat_file("   ", b"data").unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn the_reply_reports_the_summary_and_where_the_splat_went() {
        let splat = build_splat(&CreateInput::default()).unwrap();
        let reply = splat_reply(&splat, Some(Path::new("C:/tmp/a.ply")), true, None);
        assert_eq!(reply.summary.point_count, 1000);
        assert!(reply.displayed);
        assert!(reply.path.as_deref().unwrap().ends_with("a.ply"));

        // The reply is flattened into one object and keeps short float forms.
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(
            encoded.starts_with("{\"point_count\":1000,\"center\":["),
            "{encoded}"
        );
        assert!(encoded.contains("\"displayed\":true"));
        assert!(!encoded.contains("0.8999999"), "{encoded}");

        let bare = splat_reply(&splat, None, false, None);
        let encoded = serde_json::to_string(&bare).unwrap();
        assert!(!encoded.contains("path"), "{encoded}");
        assert!(encoded.contains("\"displayed\":false"));
        assert!(encoded.contains("\"max_opacity\":0.9"), "{encoded}");
    }

    #[test]
    fn a_reply_reports_the_identity_the_app_resolved() {
        let splat = build_splat(&CreateInput::default()).unwrap();
        let status = ViewerStatus {
            viewer_ready: true,
            loaded: true,
            point_count: 1000,
            canvas_width: 800,
            canvas_height: 600,
            camera: None,
            document: Some(splatmcp_bridge::DocumentSummary {
                document_id: "doc-4f2a-2".to_owned(),
                revision: 3,
                point_count: 1000,
                ..splatmcp_bridge::DocumentSummary::default()
            }),
            import: None,
        };
        let reply = splat_reply(&splat, None, true, Some(&status));
        let document = reply.document.expect("the identity travels with the reply");
        assert_eq!(document.document_id, "doc-4f2a-2");
        assert_eq!(document.revision, 3);

        // An app that reports no identity leaves the field out rather than inventing one.
        let mut silent = status;
        silent.document = None;
        let reply = splat_reply(&splat, None, true, Some(&silent));
        assert!(reply.document.is_none());
    }

    #[test]
    fn a_reply_reports_what_an_import_did_to_a_file() {
        let splat = build_splat(&CreateInput::default()).unwrap();
        let plain = splat_reply(&splat, None, true, None);
        let encoded = serde_json::to_string(&plain).unwrap();
        assert!(!encoded.contains("import"), "{encoded}");

        let (_, report) = splatmcp_core::read_ply_repairing(&ascii_with_zero_quaternion()).unwrap();
        let summary = splatmcp_bridge::PlyImportSummary::of(&report);
        let reply = plain.with_import(summary);
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("\"import\""), "{encoded}");
        assert!(encoded.contains("point 0 rotation"), "{encoded}");
        assert!(encoded.contains("\"policy\":\"repair\""), "{encoded}");
    }
    #[test]
    fn explicit_points_are_checked_before_they_are_clamped() {
        let zero_radius = build_splat(&CreateInput {
            points: Some(vec![
                PointInput::at([0.0, 0.0, 0.0]),
                PointInput {
                    scale: Some(Factor::All(0.0)),
                    ..PointInput::at([1.0, 0.0, 0.0])
                },
            ]),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(zero_radius.contains("point 1 scale"), "{zero_radius}");
        assert!(zero_radius.contains("positive radius"), "{zero_radius}");

        let out_of_range = build_splat(&CreateInput {
            points: Some(vec![PointInput {
                color: Some([1.4, 0.0, 0.0]),
                ..PointInput::at([0.0; 3])
            }]),
            ..CreateInput::default()
        })
        .unwrap_err();
        assert!(out_of_range.contains("linear RGB"), "{out_of_range}");

        // A usable, non-unit quaternion is normalised rather than refused.
        let normalized = build_splat(&CreateInput {
            points: Some(vec![PointInput {
                rotation: Some([0.0, 4.0, 0.0, 0.0]),
                ..PointInput::at([0.0; 3])
            }]),
            ..CreateInput::default()
        })
        .unwrap();
        assert_eq!(normalized.points[0].rotation, [0.0, 1.0, 0.0, 0.0]);
    }
}
