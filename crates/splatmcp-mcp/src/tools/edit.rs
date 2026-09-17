//! Edit tools: `edit_splat`, `load_splat` and `splat_info`.
//!
//! A splat to work on comes from one of three places: the desktop app (`viewer`), a PLY
//! file (`path:<file>` or a bare path) or an empty splat (`new`). The resolved splat is
//! read, edited through `splatmcp_core`, optionally written and optionally shown - so a
//! tool call never has a second, divergent copy of the document.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::protocol::ViewerStatus;
use splatmcp_bridge::{LoadPlyRequest, Method};
use splatmcp_core::{
    Box3, EditOp, EditStep, Selection, Splat, apply_all, read_ply, write_ply,
};

use crate::bridge::AppLink;
use crate::tools::{Factor, PointOut, SplatSummary, round3};

/// Source of the splat an edit applies to.
pub const SOURCE_VIEWER: &str = "viewer";
/// Source keyword for an empty splat, to be filled by edit steps.
pub const SOURCE_NEW: &str = "new";

/// Parameters of `edit_splat`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct EditInput {
    /// Where the splat comes from: `viewer` (the displayed one, default), `new` (empty),
    /// or a `.ply` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Steps applied in order.
    pub ops: Vec<EditOpInput>,
    /// Write the result to this `.ply` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Show the result in the SplatMCP window. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// The edit operations a step can ask for.
///
/// An enum rather than a free string: the schema then carries the choice list, so the
/// explanation does not have to be repeated in prose, and a typo is rejected by the
/// parser instead of by a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditOpKind {
    /// Move the selection by `by`.
    Translate,
    /// Turn the selection by `degrees` around `axis` through `center`.
    Rotate,
    /// Scale the selection around `center` by `factor`.
    Scale,
    /// Multiply the gaussian radii by `factor`.
    SetRadius,
    /// Add `delta` per colour channel.
    AdjustColor,
    /// Move the colour towards `color` by `mix`.
    SetColor,
    /// Multiply the opacity by `factor`.
    SetOpacity,
    /// Copy the selection, offset by `by`.
    Duplicate,
    /// Delete the selection.
    Remove,
    /// Append `points`.
    Merge,
}

/// One edit step, with its optional selection.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct EditOpInput {
    /// Operation to apply.
    pub op: EditOpKind,
    /// Offset in metres, for `translate` and `duplicate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<[f32; 3]>,
    /// Rotation axis, for `rotate`. Default up `[0, 1, 0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<[f32; 3]>,
    /// Rotation angle in degrees, for `rotate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degrees: Option<f32>,
    /// Centre for `rotate` and `scale`. Default `[0, 0, 0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<[f32; 3]>,
    /// `scale`, `set_radius` or `set_opacity` multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor: Option<Factor>,
    /// Per-channel colour change, for `adjust_color`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<[f32; 3]>,
    /// Target colour, for `set_color`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// How far to move towards `color`, `0..=1`. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<f32>,
    /// Points to append, for `merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<crate::tools::author::PointInput>>,
    /// Act only on points inside this box, `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Vec<f32>>,
    /// Act only on points outside this box, same shape as `within`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<Vec<f32>>,
    /// Act only on points with at least this opacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity_min: Option<f32>,
    /// Act only on points whose largest radius is at most this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_radius: Option<f32>,
    /// Act only on the first N points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<usize>,
}

/// Parameters of `load_splat`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct LoadInput {
    /// `.ply` file to display.
    pub path: String,
}

/// Parameters of `splat_info`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct InfoInput {
    /// `viewer` (default) or a `.ply` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Include the first `n` points in the reply, for detailed inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<usize>,
}

/// Edits applied to a splat, with one report per step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EditReply {
    #[serde(flatten)]
    pub summary: SplatSummary,
    /// Points touched by each step, in order.
    pub steps: Vec<StepReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub displayed: bool,
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StepReport {
    pub op_index: usize,
    pub affected: usize,
    pub remaining: usize,
}

/// Description of a splat, optionally with sample points.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InfoReply {
    #[serde(flatten)]
    pub summary: SplatSummary,
    /// Sampled points, when the call asked for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<Vec<PointOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn parse_box(values: &[f32], name: &str) -> Result<Box3, String> {
    let (min, max) = match values.len() {
        6 => (
            [values[0], values[1], values[2]],
            [values[3], values[4], values[5]],
        ),
        2 => (
            [values[0], 0.0, 0.0],
            [values[1], 0.0, 0.0],
        ),
        other => {
            return Err(format!(
                "{name} needs 6 numbers [min_x, min_y, min_z, max_x, max_y, max_z], got {other}"
            ));
        }
    };
    if min.iter().chain(max.iter()).any(|value| !value.is_finite()) {
        return Err(format!("{name} must hold finite numbers"));
    }
    Ok(Box3::from_corners(min, max))
}

/// Turns one tool step into a core edit step.
pub fn to_step(input: &EditOpInput, index: usize) -> Result<EditStep, String> {
    let name = input.op;
    let op = match name {
        EditOpKind::Translate => EditOp::Translate {
            by: input
                .by
                .ok_or_else(|| format!("op {index} (translate) needs by = [dx, dy, dz]"))?,
        },
        EditOpKind::Rotate => EditOp::Rotate {
            axis: input.axis.unwrap_or([0.0, 1.0, 0.0]),
            degrees: input
                .degrees
                .ok_or_else(|| format!("op {index} (rotate) needs degrees"))?,
            center: input.center.unwrap_or([0.0; 3]),
        },
        EditOpKind::Scale => {
            let factor = input.factor.ok_or_else(|| {
                format!("op {index} (scale) needs factor as one number or [x, y, z]")
            })?;
            EditOp::Scale {
                center: input.center.unwrap_or([0.0; 3]),
                factor: factor.axes(),
            }
        }
        EditOpKind::SetRadius => EditOp::SetRadius {
            factor: input
                .factor
                .ok_or_else(|| format!("op {index} (set_radius) needs factor"))?
                .single(),
        },
        EditOpKind::AdjustColor => EditOp::AdjustColor {
            delta: input
                .delta
                .ok_or_else(|| format!("op {index} (adjust_color) needs delta = [r, g, b]"))?,
        },
        EditOpKind::SetColor => EditOp::SetColor {
            color: input
                .color
                .ok_or_else(|| format!("op {index} (set_color) needs color = [r, g, b]"))?,
            mix: input.mix.unwrap_or(1.0),
        },
        EditOpKind::SetOpacity => EditOp::SetOpacity {
            factor: input
                .factor
                .ok_or_else(|| format!("op {index} (set_opacity) needs factor"))?
                .single(),
        },
        EditOpKind::Duplicate => EditOp::Duplicate {
            by: input.by.unwrap_or([0.0; 3]),
        },
        EditOpKind::Remove => EditOp::Remove,
        EditOpKind::Merge => EditOp::Merge {
            points: input
                .points
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .copied()
                .map(crate::tools::author::PointInput::to_point)
                .collect(),
        },
    };

    let selection = Selection {
        within: match input.within.as_deref() {
            Some(values) => Some(parse_box(values, &format!("op {index} within"))?),
            None => None,
        },
        outside: match input.outside.as_deref() {
            Some(values) => Some(parse_box(values, &format!("op {index} outside"))?),
            None => None,
        },
        opacity_min: input.opacity_min,
        max_radius: input.max_radius,
        first: input.first,
        ..Selection::default()
    };
    Ok(EditStep::with_selection(op, selection))
}

/// Reads a splat from a `.ply` file.
pub fn read_splat_file(path: &str) -> Result<Splat, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("could not read {path}: {error}"))?;
    read_ply(&bytes).map_err(|error| format!("{path} is not a readable splat: {error}"))
}

/// `true` when the source names a file rather than a keyword.
fn looks_like_path(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    lower.ends_with(".ply") || lower.contains('/') || lower.contains('\\')
}

/// Resolves where a splat comes from and loads it.
///
/// Returns the splat, a description of the source, and the file it was read from (if any).
pub fn resolve_source(
    link: &AppLink,
    source: Option<&str>,
) -> Result<(Splat, String, Option<String>), String> {
    let source = source.unwrap_or(SOURCE_VIEWER).trim().to_owned();
    if source.eq_ignore_ascii_case(SOURCE_NEW) {
        return Ok((Splat::new(), "new".to_owned(), None));
    }
    if source.eq_ignore_ascii_case(SOURCE_VIEWER) {
        let status: ViewerStatus = link.request_typed(Method::ViewerStatus, Value::Null)?;
        if !status.loaded {
            return Err(
                "no splat is displayed in the SplatMCP window; create one with create_splat, \
                 load one with load_splat, or pass source as a .ply path"
                    .to_owned(),
            );
        }
        let bytes = document_ply_bytes(link)?;
        let splat = read_ply(&bytes)
            .map_err(|error| format!("the displayed splat could not be read: {error}"))?;
        return Ok((splat, "viewer".to_owned(), None));
    }
    if looks_like_path(&source) {
        let splat = read_splat_file(&source)?;
        return Ok((splat, format!("path:{source}"), Some(source)));
    }
    Err(format!(
        "unknown source '{source}'; use '{SOURCE_VIEWER}', '{SOURCE_NEW}', or a .ply path"
    ))
}

/// PLY bytes of the splat the app displays, read back over the bridge.
pub fn document_ply_bytes(link: &AppLink) -> Result<Vec<u8>, String> {
    let value = link.request(Method::DocumentGetPly, Value::Null)?;
    let encoded = value
        .get("ply_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "the app did not return the displayed splat".to_owned())?;
    BASE64
        .decode(encoded.as_bytes())
        .map_err(|error| format!("the app returned an unreadable splat: {error}"))
}

/// Applies `ops` to `splat` and reports each step.
pub fn apply_edits(splat: &mut Splat, ops: &[EditOpInput]) -> Result<Vec<StepReport>, String> {
    if ops.is_empty() {
        return Err("ops is empty; pass at least one edit step".to_owned());
    }
    let steps: Vec<EditStep> = ops
        .iter()
        .enumerate()
        .map(|(index, input)| to_step(input, index))
        .collect::<Result<_, _>>()?;
    let reports = apply_all(splat, &steps).map_err(|error| format!("edit failed: {error}"))?;
    Ok(reports
        .iter()
        .enumerate()
        .map(|(index, report)| StepReport {
            op_index: index,
            affected: report.affected,
            remaining: report.remaining,
        })
        .collect())
}

/// Writes PLY bytes for a splat and shows it.
pub fn save_and_display(
    link: &AppLink,
    splat: &Splat,
    path: Option<&str>,
    display: bool,
) -> Result<(Option<String>, bool), String> {
    let bytes = write_ply(splat).map_err(|error| format!("could not encode the splat: {error}"))?;
    let written = match path {
        Some(path) => Some(
            crate::tools::author::write_splat_file(path, &bytes)?
                .to_string_lossy()
                .to_string(),
        ),
        None => None,
    };
    if display {
        let file_name = written
            .as_deref()
            .and_then(|path| std::path::Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned();
        let request = LoadPlyRequest {
            ply_base64: BASE64.encode(&bytes),
            file_name: Some(file_name),
            frame: Some(false),
        };
        let params = serde_json::to_value(&request)
            .map_err(|error| format!("could not encode the splat: {error}"))?;
        // Editing keeps the current camera, so the caller's view is not thrown away.
        link.request_typed::<ViewerStatus>(Method::ViewerLoadPly, params)?;
    }
    Ok((written, display))
}

/// Builds the reply of `edit_splat`.
pub fn edit_reply(
    splat: &Splat,
    steps: Vec<StepReport>,
    path: Option<String>,
    displayed: bool,
) -> EditReply {
    EditReply {
        summary: SplatSummary::of(splat),
        steps,
        path,
        displayed,
    }
}

/// Builds the reply of `splat_info`.
pub fn info_reply(splat: &Splat, source: String, sample: Option<usize>) -> InfoReply {
    let sample = sample.map(|count| {
        splat
            .points
            .iter()
            .take(count.min(splat.len()))
            .map(PointOut::of)
            .collect()
    });
    InfoReply {
        summary: SplatSummary::of(splat),
        sample,
        source: Some(source),
    }
}

/// Rounded opacity range, used by tests to describe a splat compactly.
pub fn opacity_range(splat: &Splat) -> [f32; 2] {
    let stats = splat.stats();
    [round3(stats.min_opacity), round3(stats.max_opacity)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{Splat as CoreSplat, SplatPoint};

    fn grid(count: usize) -> CoreSplat {
        CoreSplat::from_points(
            (0..count)
                .map(|index| {
                    SplatPoint::new(
                        [index as f32, 0.0, 0.0],
                        [0.1; 3],
                        [0.5; 3],
                        0.8,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        )
    }

    fn step(op: EditOpKind) -> EditOpInput {
        EditOpInput {
            op,
            by: None,
            axis: None,
            degrees: None,
            center: None,
            factor: None,
            delta: None,
            color: None,
            mix: None,
            points: None,
            within: None,
            outside: None,
            opacity_min: None,
            max_radius: None,
            first: None,
        }
    }

    #[test]
    fn a_translate_step_needs_an_offset() {
        let error = to_step(&step(EditOpKind::Translate), 0).unwrap_err();
        assert!(error.contains("needs by"), "{error}");
        let ok = to_step(
            &EditOpInput {
                by: Some([1.0, 2.0, 3.0]),
                ..step(EditOpKind::Translate)
            },
            1,
        )
        .unwrap();
        assert_eq!(
            ok.op,
            EditOp::Translate {
                by: [1.0, 2.0, 3.0]
            }
        );
    }

    #[test]
    fn an_unknown_op_is_rejected_by_the_schema() {
        // The operation is an enum, so a typo is refused while the arguments are decoded
        // rather than surfacing as a handler error.
        let decoded: Result<EditOpInput, _> = serde_json::from_str(r#"{"op":"shrink"}"#);
        assert!(decoded.is_err(), "an unknown op must not deserialise");

        // Every name the enum accepts round trips.
        for (text, expected) in [
            ("translate", EditOpKind::Translate),
            ("rotate", EditOpKind::Rotate),
            ("scale", EditOpKind::Scale),
            ("set_radius", EditOpKind::SetRadius),
            ("adjust_color", EditOpKind::AdjustColor),
            ("set_color", EditOpKind::SetColor),
            ("set_opacity", EditOpKind::SetOpacity),
            ("duplicate", EditOpKind::Duplicate),
            ("remove", EditOpKind::Remove),
            ("merge", EditOpKind::Merge),
        ] {
            let decoded: EditOpInput =
                serde_json::from_str(&format!("{{\"op\":\"{text}\"}}")).unwrap();
            assert_eq!(decoded.op, expected);
        }
    }

    #[test]
    fn a_scale_factor_accepts_one_number_or_three() {
        let uniform = to_step(
            &EditOpInput {
                factor: Some(Factor::All(2.0)),
                ..step(EditOpKind::Scale)
            },
            0,
        )
        .unwrap();
        assert_eq!(
            uniform.op,
            EditOp::Scale {
                center: [0.0; 3],
                factor: [2.0; 3]
            }
        );

        let per_axis = to_step(
            &EditOpInput {
                factor: Some(Factor::PerAxis([1.0, 2.0, 3.0])),
                center: Some([1.0, 0.0, 0.0]),
                ..step(EditOpKind::Scale)
            },
            0,
        )
        .unwrap();
        assert_eq!(
            per_axis.op,
            EditOp::Scale {
                center: [1.0, 0.0, 0.0],
                factor: [1.0, 2.0, 3.0]
            }
        );

        let missing = to_step(&step(EditOpKind::Scale), 4).unwrap_err();
        assert!(missing.contains("one number or [x, y, z]"), "{missing}");
    }

    #[test]
    fn a_factor_deserialises_from_a_number_or_an_array() {
        let one: Factor = serde_json::from_str("2").unwrap();
        assert_eq!(one, Factor::All(2.0));
        assert_eq!(one.axes(), [2.0; 3]);
        assert_eq!(one.single(), 2.0);

        let three: Factor = serde_json::from_str("[1, 2, 3]").unwrap();
        assert_eq!(three, Factor::PerAxis([1.0, 2.0, 3.0]));
        assert_eq!(three.single(), 1.0);

        // A scalar factor is what a single-number operation expects, without an array.
        let op: EditOpInput =
            serde_json::from_str(r#"{"op":"set_opacity","factor":0.5}"#).unwrap();
        assert_eq!(op.factor, Some(Factor::All(0.5)));
        assert_eq!(to_step(&op, 0).unwrap().op, EditOp::SetOpacity { factor: 0.5 });
    }

    #[test]
    fn a_box_selection_accepts_six_or_two_numbers() {
        let six = to_step(
            &EditOpInput {
                op: EditOpKind::Remove,
                within: Some(vec![-1.0, -1.0, -1.0, 1.0, 1.0, 1.0]),
                ..step(EditOpKind::Remove)
            },
            0,
        )
        .unwrap();
        assert_eq!(
            six.selection.within,
            Some(Box3::from_corners(
                [-1.0, -1.0, -1.0],
                [1.0, 1.0, 1.0]
            ))
        );

        let two = to_step(
            &EditOpInput {
                op: EditOpKind::Remove,
                outside: Some(vec![2.0, 5.0]),
                ..step(EditOpKind::Remove)
            },
            0,
        )
        .unwrap();
        assert_eq!(
            two.selection.outside,
            Some(Box3::from_corners([2.0, 0.0, 0.0], [5.0, 0.0, 0.0]))
        );

        let error = to_step(
            &EditOpInput {
                op: EditOpKind::Remove,
                within: Some(vec![1.0, 2.0, 3.0, 4.0]),
                ..step(EditOpKind::Remove)
            },
            0,
        )
        .unwrap_err();
        assert!(error.contains("6 numbers"), "{error}");
    }

    #[test]
    fn merge_carries_explicit_points() {
        let merged = to_step(
            &EditOpInput {
                op: EditOpKind::Merge,
                points: Some(vec![crate::tools::author::PointInput {
                    position: [1.0, 2.0, 3.0],
                    color: Some([1.0, 0.0, 0.0]),
                    opacity: Some(1.0),
                    scale: None,
                    rotation: None,
                }]),
                ..step(EditOpKind::Remove)
            },
            0,
        )
        .unwrap();
        match merged.op {
            EditOp::Merge { points } => {
                assert_eq!(points.len(), 1);
                assert_eq!(points[0].position, [1.0, 2.0, 3.0]);
            }
            other => panic!("expected merge, got {other:?}"),
        }
    }

    #[test]
    fn edits_apply_in_order_and_report_each_step() {
        let mut splat = grid(4);
        let reports = apply_edits(
            &mut splat,
            &[
                EditOpInput {
                    op: EditOpKind::Translate,
                    by: Some([0.0, 1.0, 0.0]),
                    ..step(EditOpKind::Remove)
                },
                EditOpInput {
                    op: EditOpKind::Remove,
                    first: Some(1),
                    ..step(EditOpKind::Remove)
                },
                EditOpInput {
                    op: EditOpKind::Duplicate,
                    by: Some([0.0, 0.0, 2.0]),
                    ..step(EditOpKind::Remove)
                },
            ],
        )
        .unwrap();
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].affected, 4);
        assert_eq!(reports[0].remaining, 4);
        assert_eq!(reports[1].affected, 1);
        assert_eq!(reports[1].remaining, 3);
        assert_eq!(reports[2].remaining, 6);
        assert!(splat.points.iter().all(|point| point.position[1] == 1.0));
    }

    #[test]
    fn an_empty_op_list_is_refused_with_guidance() {
        let mut splat = grid(2);
        let error = apply_edits(&mut splat, &[]).unwrap_err();
        assert!(error.contains("ops is empty"), "{error}");
        assert_eq!(splat.len(), 2);
    }

    #[test]
    fn a_failing_step_keeps_the_earlier_state() {
        let mut splat = grid(2);
        let error = apply_edits(
            &mut splat,
            &[
                EditOpInput {
                    op: EditOpKind::Translate,
                    by: Some([0.0, 5.0, 0.0]),
                    ..step(EditOpKind::Remove)
                },
                EditOpInput {
                    op: EditOpKind::SetRadius,
                    factor: Some(Factor::All(0.0)),
                    ..step(EditOpKind::Remove)
                },
            ],
        )
        .unwrap_err();
        assert!(error.contains("edit failed"), "{error}");
        // Steps are applied in place, so the successful first step is kept and the
        // caller sees the failure for the whole call.
        assert_eq!(splat.points[0].position[1], 5.0);
        assert_eq!(splat.len(), 2);
    }

    #[test]
    fn source_keywords_are_validated() {
        // No app is running here, so `viewer` fails on the bridge, not on the name.
        let link = AppLink::new(false);
        let error = resolve_source(&link, Some("gallery")).unwrap_err();
        assert!(error.contains("unknown source 'gallery'"), "{error}");
        assert!(error.contains("viewer"), "{error}");

        let (empty, source, path) = resolve_source(&link, Some("new")).unwrap();
        assert!(empty.is_empty());
        assert_eq!(source, "new");
        assert_eq!(path, None);

        let error = resolve_source(&link, Some("C:/nowhere/missing.ply")).unwrap_err();
        assert!(error.contains("could not read"), "{error}");
    }

    #[test]
    fn a_path_source_is_recognised() {
        assert!(looks_like_path("C:/tmp/a.ply"));
        assert!(looks_like_path("sub/dir/file.ply"));
        assert!(looks_like_path("sub\\dir\\file.ply"));
        assert!(!looks_like_path("viewer"));
        assert!(!looks_like_path("new"));
    }

    #[test]
    fn the_info_reply_samples_points_on_request() {
        let splat = grid(5);
        let reply = info_reply(&splat, "new".to_owned(), Some(2));
        assert_eq!(reply.summary.point_count, 5);
        let sample = reply.sample.expect("a sample was requested");
        assert_eq!(sample.len(), 2);
        assert_eq!(sample[0].position[0], 0.0);
        assert_eq!(sample[1].scale[0], 0.1);
        // The sample is serialised with short float forms, which matters because a
        // reply can carry many points.
        let encoded = serde_json::to_string(&sample).unwrap();
        assert!(encoded.starts_with("[{\"position\":[0.0,0.0,0.0]"), "{encoded}");
        assert!(encoded.contains("\"opacity\":0.8"), "{encoded}");
        assert!(!encoded.contains("0.80000001"), "{encoded}");

        // Asking for more points than exist is not an error.
        let all = info_reply(&splat, "new".to_owned(), Some(100));
        assert_eq!(all.sample.unwrap().len(), 5);

        // Without a request the sample is omitted from the JSON entirely.
        let plain = info_reply(&splat, "new".to_owned(), None);
        let encoded = serde_json::to_string(&plain).unwrap();
        assert!(!encoded.contains("sample"), "{encoded}");
        assert_eq!(plain.source.as_deref(), Some("new"));
    }

    #[test]
    fn the_edit_reply_is_compact_and_typed() {
        let splat = grid(3);
        let reply = edit_reply(
            &splat,
            vec![StepReport {
                op_index: 0,
                affected: 3,
                remaining: 3,
            }],
            Some("C:/tmp/a.ply".to_owned()),
            true,
        );
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.starts_with("{\"point_count\":3"), "{encoded}");
        assert!(encoded.contains("\"steps\":[{\"op_index\":0,\"affected\":3,\"remaining\":3}]"));
        assert!(encoded.contains("\"displayed\":true"));
        assert!(!encoded.contains("0.80000001"), "{encoded}");
    }

    #[test]
    fn the_opacity_range_helper_rounds() {
        let splat = CoreSplat::from_points(vec![
            SplatPoint::new([0.0; 3], [0.1; 3], [0.5; 3], 0.123_456, [1.0, 0.0, 0.0, 0.0]),
            SplatPoint::new([1.0, 0.0, 0.0], [0.1; 3], [0.5; 3], 0.987_65, [1.0, 0.0, 0.0, 0.0]),
        ]);
        assert_eq!(opacity_range(&splat), [0.123, 0.988]);
    }
}
