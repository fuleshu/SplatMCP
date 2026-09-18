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
use splatmcp_bridge::{
    BatchOpParams, BatchPointParams, CommitPreviewRequest, ComponentsRequest,
    DocumentTargetRequest, EditBatchRequest, InspectResult, InspectionSummary, Method,
    PlyImportSummary, SelectionParams, load_ply_params, replace_ply_params,
};
use splatmcp_core::validation::IssueRecorder;
use splatmcp_core::{
    Box3, EditOp, EditStep, PlyImportPolicy, PlyReport, Selection, Splat, apply_all,
    read_ply_with_policy, write_ply,
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
    /// Accept a file source that needs repair, reporting every change it makes.
    ///
    /// Default false: a damaged file is refused with indexed diagnostics instead of being
    /// edited as if it were intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
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
    /// Minimum mean colour per channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_min: Option<[f32; 3]>,
    /// Maximum mean colour per channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_max: Option<[f32; 3]>,
    /// Component whose members are targeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// Exact point ids (`pt-7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_ids: Option<Vec<String>>,
    /// Saved selection handle id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_handle: Option<u64>,
    /// `world` (default) or `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// `[cx, cy, cz, radius]` sphere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sphere: Option<[f32; 4]>,
}

/// A selection filter for `splat_components`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct SelectionInput {
    /// Box to keep points inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Vec<f32>>,
    /// Box to keep points outside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<Vec<f32>>,
    /// `[cx, cy, cz, radius]` sphere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sphere: Option<[f32; 4]>,
    /// `world` (default) or `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// Minimum opacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity_min: Option<f32>,
    /// Maximum largest radius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_radius: Option<f32>,
    /// Keep the first N rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<usize>,
    /// Minimum mean colour per channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_min: Option<[f32; 3]>,
    /// Maximum mean colour per channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_max: Option<[f32; 3]>,
    /// Component whose members are targeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// Exact point ids (`pt-7`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub point_ids: Vec<String>,
    /// Saved selection handle id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_handle: Option<u64>,
}

/// Parameters of `edit_batch`: one atomic, previewable edit transaction.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct EditBatchInput {
    /// Steps applied in order by one transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<EditOpInput>>,
    /// Commit this dry run's candidate instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_id: Option<u64>,
    /// Report a dry run and commit nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Retry-safe request identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// `stable` (default) or `sequential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Document to edit; default displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Required with `document_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Show the result. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// Parameters of `edit_history`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct HistoryInput {
    /// `status` (default), `undo` or `redo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Document to act on; default displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Required with `document_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Show the result. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// Parameters of `splat_components`: one component or selection action.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct ComponentsInput {
    /// `list` (default), `create`, `rename`, `remove`, `transform`, `members`,
    /// `apply_transform` or `select`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Document to act on; default displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Required with `document_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Target component id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Display name; names are not identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Frame translation, in metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<[f32; 3]>,
    /// Frame rotation `(w, x, y, z)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 4]>,
    /// Frame scale, positive per axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 3]>,
    /// Clear the frame instead of setting one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_transform: Option<bool>,
    /// What `members` or `select` resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionInput>,
    /// Transform members through the frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_transform: Option<bool>,
}

/// Which app call a batch input maps to.
#[derive(Debug, Clone, PartialEq)]
pub enum BatchCall {
    /// Run (or dry-run) a batch.
    Batch(EditBatchRequest),
    /// Commit a retained preview candidate.
    CommitPreview(CommitPreviewRequest),
}

/// The selection a step targets, or `None` when it targets everything.
fn selection_params(input: &EditOpInput) -> Result<Option<SelectionParams>, String> {
    let present = input.within.is_some()
        || input.outside.is_some()
        || input.sphere.is_some()
        || input.frame.is_some()
        || input.opacity_min.is_some()
        || input.max_radius.is_some()
        || input.first.is_some()
        || input.color_min.is_some()
        || input.color_max.is_some()
        || input.component.is_some()
        || input.point_ids.as_ref().is_some_and(|ids| !ids.is_empty())
        || input.selection_handle.is_some();
    if !present {
        return Ok(None);
    }
    // The box shapes are checked here as well as in the app, so a malformed request is refused
    // before it costs a round trip.
    if let Some(values) = &input.within {
        parse_box(values, "within")?;
    }
    if let Some(values) = &input.outside {
        parse_box(values, "outside")?;
    }
    Ok(Some(SelectionParams {
        within: input.within.clone(),
        outside: input.outside.clone(),
        sphere: input.sphere,
        frame: input.frame.clone(),
        opacity_min: input.opacity_min,
        max_radius: input.max_radius,
        first: input.first,
        color_min: input.color_min,
        color_max: input.color_max,
        component: input.component.clone(),
        point_ids: input.point_ids.clone().unwrap_or_default(),
        selection_handle: input.selection_handle,
    }))
}

/// Projects a validated core operation onto the wire shape the app runs.
fn op_params(op: &EditOp) -> BatchOpParams {
    let mut params = BatchOpParams {
        op: String::new(),
        by: None,
        axis: None,
        degrees: None,
        center: None,
        factor: None,
        delta: None,
        color: None,
        mix: None,
        points: Vec::new(),
        selection: None,
    };
    match op {
        EditOp::Translate { by } => {
            params.op = "translate".to_owned();
            params.by = Some(*by);
        }
        EditOp::Rotate {
            axis,
            degrees,
            center,
        } => {
            params.op = "rotate".to_owned();
            params.axis = Some(*axis);
            params.degrees = Some(*degrees);
            params.center = Some(*center);
        }
        EditOp::Scale { center, factor } => {
            params.op = "scale".to_owned();
            params.center = Some(*center);
            params.factor = Some(*factor);
        }
        EditOp::SetRadius { factor } => {
            params.op = "set_radius".to_owned();
            params.factor = Some([*factor; 3]);
        }
        EditOp::AdjustColor { delta } => {
            params.op = "adjust_color".to_owned();
            params.delta = Some(*delta);
        }
        EditOp::SetColor { color, mix } => {
            params.op = "set_color".to_owned();
            params.color = Some(*color);
            params.mix = Some(*mix);
        }
        EditOp::SetOpacity { factor } => {
            params.op = "set_opacity".to_owned();
            params.factor = Some([*factor; 3]);
        }
        EditOp::Duplicate { by } => {
            params.op = "duplicate".to_owned();
            params.by = Some(*by);
        }
        EditOp::Remove => params.op = "remove".to_owned(),
        EditOp::Merge { points } => {
            params.op = "merge".to_owned();
            params.points = points
                .iter()
                .map(|point| BatchPointParams {
                    position: point.position,
                    color: Some(point.color),
                    opacity: Some(point.opacity),
                    scale: Some(point.scale),
                    rotation: Some(point.rotation),
                })
                .collect();
        }
    }
    params
}

/// Translates one tool step into the wire shape, validating it on the way.
pub fn step_params(input: &EditOpInput, index: usize) -> Result<BatchOpParams, String> {
    let step = to_step(input, index)?;
    let mut params = op_params(&step.op);
    params.selection = selection_params(input)?;
    Ok(params)
}

/// Turns the tool input into the app call it describes.
pub fn batch_call(input: &EditBatchInput) -> Result<BatchCall, String> {
    if let Some(preview_id) = input.preview_id {
        if input.steps.as_ref().is_some_and(|steps| !steps.is_empty()) {
            return Err(
                "pass either steps or preview_id, not both: committing a preview applies exactly                  the candidate the dry run reported"
                    .to_owned(),
            );
        }
        return Ok(BatchCall::CommitPreview(CommitPreviewRequest {
            preview_id,
            document_id: input.document_id.clone(),
            expected_revision: input.expected_revision,
            display: input.display,
        }));
    }
    let steps = input.steps.as_deref().unwrap_or(&[]);
    if steps.is_empty() {
        return Err("steps is empty; pass at least one edit step or a preview_id".to_owned());
    }
    let steps = steps
        .iter()
        .enumerate()
        .map(|(index, step)| step_params(step, index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BatchCall::Batch(EditBatchRequest {
        document_id: input.document_id.clone(),
        expected_revision: input.expected_revision,
        operation_id: input.operation_id.clone(),
        resolution: input.resolution.clone(),
        dry_run: input.dry_run,
        display: input.display,
        steps,
    }))
}

/// Turns the component tool input into the app request it describes.
pub fn components_request(input: &ComponentsInput) -> Result<ComponentsRequest, String> {
    let action = input.action.clone().unwrap_or_else(|| "list".to_owned());
    let selection = match &input.selection {
        Some(filter) => {
            if let Some(values) = &filter.within {
                parse_box(values, "within")?;
            }
            if let Some(values) = &filter.outside {
                parse_box(values, "outside")?;
            }
            Some(SelectionParams {
                within: filter.within.clone(),
                outside: filter.outside.clone(),
                sphere: filter.sphere,
                frame: filter.frame.clone(),
                opacity_min: filter.opacity_min,
                max_radius: filter.max_radius,
                first: filter.first,
                color_min: filter.color_min,
                color_max: filter.color_max,
                component: filter.component.clone(),
                point_ids: filter.point_ids.clone(),
                selection_handle: filter.selection_handle,
            })
        }
        None => None,
    };
    Ok(ComponentsRequest {
        action,
        document_id: input.document_id.clone(),
        expected_revision: input.expected_revision,
        component_id: input.component_id.clone(),
        name: input.name.clone(),
        translation: input.translation,
        rotation: input.rotation,
        scale: input.scale,
        clear_transform: input.clear_transform,
        selection,
        apply_transform: input.apply_transform,
    })
}

/// The target a history call names.
pub fn history_target(input: &HistoryInput) -> DocumentTargetRequest {
    DocumentTargetRequest {
        document_id: input.document_id.clone(),
        expected_revision: input.expected_revision,
        display: input.display,
    }
}

/// Parameters of `load_splat`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct LoadInput {
    /// `.ply` file to display.
    pub path: String,
    /// Accept a file that needs repair, reporting every change it makes.
    ///
    /// Default false: a damaged file is refused with indexed diagnostics rather than
    /// loaded as a quietly repaired document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
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
    /// Accept a file that needs repair, reporting every change it makes.
    ///
    /// Default false: a damaged file is refused with indexed diagnostics instead of being
    /// described as if it were intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
}

/// Edits applied to a splat, with one report per step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EditReply {
    #[serde(flatten)]
    pub summary: SplatSummary,
    /// Identity of the document revision the edit landed in.
    ///
    /// The app reports it, so a caller can quote that revision back for the next edit - or
    /// finds out that it edited a different document than it assumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentIdentity>,
    /// Points touched by each step, in order.
    pub steps: Vec<StepReport>,
    /// What the import of a file source did, when it was not lossless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import: Option<PlyImportSummary>,
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
    /// Bounded inspection: distributions, contract diagnostics and buffer sizes.
    ///
    /// A fixed-size object, so describing a 500 000 gaussian document costs no more than
    /// describing three.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection: Option<InspectionSummary>,
    /// Sampled points, when the call asked for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<Vec<PointOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Identity of the displayed document, when the call read one.
    ///
    /// A Python edit has to state `expected_revision`, and the caller reads it here. It is
    /// an addition to the reply, so a caller that only wanted the summary is unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentIdentity>,
    /// What the import of a file source did, when it was not lossless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import: Option<PlyImportSummary>,
}

/// Identity and revision of the document a reply describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentIdentity {
    pub document_id: String,
    pub revision: u64,
}

impl DocumentIdentity {
    /// Reads the identity out of a bridge document summary.
    ///
    /// `None` when the app reported none - an older build, or a viewer reply that never went
    /// through the document service - so a caller is never handed an invented identity.
    pub fn of_summary(summary: Option<&splatmcp_bridge::DocumentSummary>) -> Option<Self> {
        let summary = summary?;
        if summary.document_id.is_empty() {
            return None;
        }
        Some(Self {
            document_id: summary.document_id.clone(),
            revision: summary.revision,
        })
    }
}

fn parse_box(values: &[f32], name: &str) -> Result<Box3, String> {
    let (min, max) = match values.len() {
        6 => (
            [values[0], values[1], values[2]],
            [values[3], values[4], values[5]],
        ),
        2 => ([values[0], 0.0, 0.0], [values[1], 0.0, 0.0]),
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
        EditOpKind::Merge => {
            // Merged points are caller input, so they are checked before anything is
            // clamped: a silent repair here would edit a document differently from what
            // the call asked for.
            let points = input.points.as_deref().unwrap_or(&[]);
            let mut recorder = IssueRecorder::new();
            let mut built = Vec::with_capacity(points.len());
            for (point_index, point) in points.iter().enumerate() {
                match point.checked_point(point_index) {
                    Ok(point) => built.push(point),
                    Err(issue) => recorder.record(issue),
                }
            }
            if let Some(error) = recorder.error(points.len()) {
                return Err(format!("op {index} (merge) points: {error}"));
            }
            EditOp::Merge { points: built }
        }
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

/// What a file source produced: the geometry plus what the import reported.
pub struct FileSource {
    pub splat: Splat,
    pub report: PlyReport,
}

/// Reads a splat from a `.ply` file under `policy`, reporting what the import did.
///
/// Strict by default: a file that would need repair is refused with indexed diagnostics, so
/// a tool call cannot silently change a caller's data. `repair: true` accepts the file and
/// the reply then names every value that was changed.
pub fn read_splat_file(path: &str, policy: PlyImportPolicy) -> Result<FileSource, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("could not read {path}: {error}"))?;
    let (splat, report) = read_ply_with_policy(&bytes, policy)
        .map_err(|error| format!("{path} is not a readable splat: {error}"))?;
    Ok(FileSource { splat, report })
}

/// `true` when the source names a file rather than a keyword.
fn looks_like_path(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    lower.ends_with(".ply") || lower.contains('/') || lower.contains('\\')
}

/// Where a splat came from, as `resolve_source` reports it.
#[derive(Debug)]
pub struct ResolvedSource {
    pub splat: Splat,
    /// Human readable description of the source.
    pub source: String,
    /// File it was read from, when it came from disk.
    pub path: Option<String>,
    /// Identity of the displayed document, when the source was the app.
    pub document: Option<DocumentIdentity>,
    /// What reading a file source did, when it was not lossless.
    pub import: Option<PlyImportSummary>,
}

/// Resolves where a splat comes from and loads it under `policy`.
///
/// The policy only applies to a file source: the displayed document is served by the app as
/// bytes this crate wrote, which already satisfy the contract.
pub fn resolve_source(
    link: &AppLink,
    source: Option<&str>,
    policy: PlyImportPolicy,
) -> Result<ResolvedSource, String> {
    let source = source.unwrap_or(SOURCE_VIEWER).trim().to_owned();
    if source.eq_ignore_ascii_case(SOURCE_NEW) {
        return Ok(ResolvedSource {
            splat: Splat::new(),
            source: "new".to_owned(),
            path: None,
            document: None,
            import: None,
        });
    }
    if source.eq_ignore_ascii_case(SOURCE_VIEWER) {
        // The viewer status says whether anything is displayed; the document reply carries
        // the bytes *and* the identity a Python edit has to quote.
        let status: ViewerStatus = link.request_typed(Method::ViewerStatus, Value::Null)?;
        if !status.loaded {
            return Err(
                "no splat is displayed in the SplatMCP window; create one with create_splat, \
                 load one with load_splat, or pass source as a .ply path"
                    .to_owned(),
            );
        }
        let document = document_ply(link)?;
        let splat = read_ply_with_policy(&document.1, PlyImportPolicy::Strict)
            .map_err(|error| format!("the displayed splat could not be read: {error}"))?
            .0;
        return Ok(ResolvedSource {
            splat,
            source: "viewer".to_owned(),
            path: None,
            document: document.0,
            import: None,
        });
    }
    if looks_like_path(&source) {
        let file = read_splat_file(&source, policy)?;
        return Ok(ResolvedSource {
            splat: file.splat,
            source: format!("path:{source}"),
            path: Some(source),
            document: None,
            import: PlyImportSummary::of(&file.report),
        });
    }
    Err(format!(
        "unknown source '{source}'; use '{SOURCE_VIEWER}', '{SOURCE_NEW}', or a .ply path"
    ))
}

/// PLY bytes of the splat the app displays, read back over the bridge.
pub fn document_ply_bytes(link: &AppLink) -> Result<Vec<u8>, String> {
    Ok(document_ply(link)?.1)
}

/// The displayed document's identity and bytes, in one bridge round trip.
fn document_ply(link: &AppLink) -> Result<(Option<DocumentIdentity>, Vec<u8>), String> {
    let value = link.request(Method::DocumentGetPly, Value::Null)?;
    let encoded = value
        .get("ply_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "the app did not return the displayed splat".to_owned())?;
    let bytes = BASE64
        .decode(encoded.as_bytes())
        .map_err(|error| format!("the app returned an unreadable splat: {error}"))?;
    // Identity is additive: an older app that does not report it still answers the bytes, and
    // a reply that names no document is not turned into an invented identity.
    let document = value
        .get("document")
        .and_then(|document| {
            serde_json::from_value::<splatmcp_bridge::DocumentSummary>(document.clone()).ok()
        })
        .and_then(|summary| DocumentIdentity::of_summary(Some(&summary)));
    Ok((document, bytes))
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

/// What a save-and-display step did, including the identity it resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedEdit {
    /// File the splat was written to, when the call asked for one.
    pub path: Option<String>,
    /// True when the app was asked to display the result.
    pub displayed: bool,
    /// Identity of the revision the app resolved, when it reported one.
    pub document: Option<DocumentIdentity>,
}

/// Writes PLY bytes for a splat and shows it.
///
/// `target` is the document the edit started from: when it is known, the load is an explicit
/// replacement of that document at that revision, so the edit keeps its identity and a stale
/// edit is refused. A `None` target means the geometry came from somewhere else, and the
/// result becomes a document of its own.
pub fn save_and_display(
    link: &AppLink,
    splat: &Splat,
    path: Option<&str>,
    display: bool,
    target: Option<&DocumentIdentity>,
) -> Result<DisplayedEdit, String> {
    let bytes = write_ply(splat).map_err(|error| format!("could not encode the splat: {error}"))?;
    let written = match path {
        Some(path) => Some(
            crate::tools::author::write_splat_file(path, &bytes)?
                .to_string_lossy()
                .to_string(),
        ),
        None => None,
    };
    let mut document = None;
    if display {
        let file_name = written
            .as_deref()
            .and_then(|path| std::path::Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned();
        let encoded = BASE64.encode(&bytes);
        let params = match target {
            Some(target) => {
                replace_ply_params(encoded, &target.document_id, target.revision, Some(false))
            }
            None => load_ply_params(encoded, Some(file_name)),
        };
        // Editing keeps the current camera, so the caller's view is not thrown away.
        let status: ViewerStatus = link.request_typed(Method::ViewerLoadPly, params)?;
        document = DocumentIdentity::of_summary(status.document.as_ref());
    }
    Ok(DisplayedEdit {
        path: written,
        displayed: display,
        document,
    })
}

/// Builds the reply of `edit_splat`.
pub fn edit_reply(
    splat: &Splat,
    steps: Vec<StepReport>,
    path: Option<String>,
    displayed: bool,
    document: Option<DocumentIdentity>,
) -> EditReply {
    EditReply {
        summary: SplatSummary::of(splat),
        document,
        steps,
        import: None,
        path,
        displayed,
    }
}

/// Same reply, reporting what reading a file source did.
pub fn with_import(mut reply: EditReply, import: Option<PlyImportSummary>) -> EditReply {
    reply.import = import;
    reply
}

/// Builds the reply of `splat_info`.
pub fn info_reply(splat: &Splat, source: String, sample: Option<usize>) -> InfoReply {
    info_reply_with_document(splat, source, sample, None)
}

/// Same, with the identity of the document the splat came from.
pub fn info_reply_with_document(
    splat: &Splat,
    source: String,
    sample: Option<usize>,
    document: Option<DocumentIdentity>,
) -> InfoReply {
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
        inspection: Some(inspect_splat(splat)),
        sample,
        source: Some(source),
        document,
        import: None,
    }
}

/// Same reply, reporting what reading a file source did.
pub fn info_reply_with_import(
    reply: InfoReply,
    import: Option<PlyImportSummary>,
) -> InfoReply {
    InfoReply { import, ..reply }
}

/// Bounded inspection of a splat that is already in memory.
pub fn inspect_splat(splat: &Splat) -> InspectionSummary {
    InspectionSummary::from(&splat.inspection(splatmcp_core::ValidationLimits::default()))
}

/// True when `splat_info` can answer with bounded metadata instead of geometry.
///
/// Only the displayed document can be inspected where it lives, and only when no sample
/// was asked for: a sample needs the gaussians themselves, so that call reads the PLY.
pub fn wants_bounded_inspection(source: Option<&str>, points: Option<usize>) -> bool {
    if points.is_some() {
        return false;
    }
    match source.map(str::trim) {
        None | Some("") => true,
        Some(name) => name.eq_ignore_ascii_case(SOURCE_VIEWER),
    }
}

/// What a bounded inspection of the displayed document produced.
#[derive(Debug)]
pub enum InspectOutcome {
    /// The app answered with bounded metadata and the document's identity.
    Summary(Box<InspectResult>),
    /// Nothing is displayed; the message is the actionable one to show the caller.
    NoDocument(String),
    /// The app cannot serve this method, so the caller should read the PLY as before.
    Unavailable,
}

/// Inspects the displayed document without transferring its geometry.
pub fn inspect_displayed(link: &AppLink) -> InspectOutcome {
    match link.request_typed::<InspectResult>(Method::DocumentInspect, Value::Null) {
        Ok(result) => InspectOutcome::Summary(Box::new(result)),
        Err(error) if error.contains("no splat") => InspectOutcome::NoDocument(
            "no splat is displayed in the SplatMCP window; create one with create_splat, \
             load one with load_splat, or pass source as a .ply path"
                .to_owned(),
        ),
        // An older app has no `document.inspect`; the caller falls back to reading the
        // PLY, which is what it did before this method existed.
        Err(_) => InspectOutcome::Unavailable,
    }
}

/// Reply of `splat_info` built from a bounded inspection.
pub fn info_reply_from_inspect(result: InspectResult) -> InfoReply {
    let document = DocumentIdentity::of_summary(Some(&result.document));
    InfoReply {
        summary: SplatSummary::of_inspection(&result.inspection),
        inspection: Some(result.inspection),
        sample: None,
        source: Some(SOURCE_VIEWER.to_owned()),
        document,
        import: None,
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
            color_min: None,
            color_max: None,
            component: None,
            point_ids: None,
            selection_handle: None,
            frame: None,
            sphere: None,
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
        let op: EditOpInput = serde_json::from_str(r#"{"op":"set_opacity","factor":0.5}"#).unwrap();
        assert_eq!(op.factor, Some(Factor::All(0.5)));
        assert_eq!(
            to_step(&op, 0).unwrap().op,
            EditOp::SetOpacity { factor: 0.5 }
        );
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
            Some(Box3::from_corners([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]))
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
        let error = resolve_source(&link, Some("gallery"), PlyImportPolicy::Strict).unwrap_err();
        assert!(error.contains("unknown source 'gallery'"), "{error}");
        assert!(error.contains("viewer"), "{error}");

        let resolved = resolve_source(&link, Some("new"), PlyImportPolicy::Strict).unwrap();
        assert!(resolved.splat.is_empty());
        assert_eq!(resolved.source, "new");
        assert_eq!(resolved.path, None);
        assert!(
            resolved.document.is_none(),
            "a new splat has no document identity"
        );

        let error =
            resolve_source(&link, Some("C:/nowhere/missing.ply"), PlyImportPolicy::Strict).unwrap_err();
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
    fn the_info_reply_carries_the_document_identity_when_there_is_one() {
        let splat = grid(2);
        let reply = info_reply_with_document(
            &splat,
            "viewer".to_owned(),
            None,
            Some(DocumentIdentity {
                document_id: "doc-7".to_owned(),
                revision: 4,
            }),
        );
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(
            encoded.contains("\"document\":{\"document_id\":\"doc-7\",\"revision\":4}"),
            "{encoded}"
        );

        // A reply without identity omits the field entirely, so an older app or a .ply
        // path does not add noise to the reply.
        let plain = info_reply(&splat, "new".to_owned(), None);
        assert!(!serde_json::to_string(&plain).unwrap().contains("document"));
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
        assert!(
            encoded.starts_with("[{\"position\":[0.0,0.0,0.0]"),
            "{encoded}"
        );
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
            None,
        );
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.starts_with("{\"point_count\":3"), "{encoded}");
        assert!(encoded.contains("\"steps\":[{\"op_index\":0,\"affected\":3,\"remaining\":3}]"));
        assert!(encoded.contains("\"displayed\":true"));
        assert!(!encoded.contains("0.80000001"), "{encoded}");
        assert!(
            !encoded.contains("\"document\""),
            "no identity is invented: {encoded}"
        );

        // With an identity it is reported, so a caller can quote the revision back.
        let named = edit_reply(
            &splat,
            Vec::new(),
            None,
            false,
            Some(DocumentIdentity {
                document_id: "doc-5-1".to_owned(),
                revision: 2,
            }),
        );
        let encoded = serde_json::to_string(&named).unwrap();
        assert!(
            encoded.contains("\"document\":{\"document_id\":\"doc-5-1\",\"revision\":2}"),
            "{encoded}"
        );
    }

    #[test]
    fn the_opacity_range_helper_rounds() {
        let splat = CoreSplat::from_points(vec![
            SplatPoint::new(
                [0.0; 3],
                [0.1; 3],
                [0.5; 3],
                0.123_456,
                [1.0, 0.0, 0.0, 0.0],
            ),
            SplatPoint::new(
                [1.0, 0.0, 0.0],
                [0.1; 3],
                [0.5; 3],
                0.987_65,
                [1.0, 0.0, 0.0, 0.0],
            ),
        ]);
        assert_eq!(opacity_range(&splat), [0.123, 0.988]);
    }
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
    fn a_file_source_is_strict_by_default_and_repairable_on_request() {
        let path = std::env::temp_dir().join(format!("splatmcp-import-{}.ply", std::process::id()));
        std::fs::write(&path, ascii_with_zero_quaternion()).unwrap();
        let text = path.to_string_lossy().to_string();

        // Default: refuse, with the index and a way forward.
        let refused = read_splat_file(&text, PlyImportPolicy::Strict)
            .map(|_| ())
            .unwrap_err();
        assert!(refused.contains("point 0 rotation"), "{refused}");
        assert!(refused.contains("repair"), "{refused}");

        // Explicit: load it and report every change, which is what the reply carries.
        let repaired = read_splat_file(&text, PlyImportPolicy::Repair).unwrap();
        assert_eq!(repaired.splat.len(), 2);
        assert_eq!(repaired.report.total_repairs, 1);
        let summary = PlyImportSummary::of(&repaired.report).unwrap();
        assert!(summary.changed[0].contains("point 0 rotation"));
        assert_eq!(summary.policy, "repair");

        // The policy the tools select from an omitted or explicit flag.
        assert_eq!(PlyImportPolicy::from_repair_flag(None), PlyImportPolicy::Strict);
        assert_eq!(PlyImportPolicy::from_repair_flag(Some(true)), PlyImportPolicy::Repair);
        let strict: LoadInput = serde_json::from_str(r#"{"path":"a.ply"}"#).unwrap();
        assert_eq!(strict.repair, None);
        let repairing: LoadInput =
            serde_json::from_str(r#"{"path":"a.ply","repair":true}"#).unwrap();
        assert_eq!(repairing.repair, Some(true));
        let info: InfoInput =
            serde_json::from_str(r#"{"source":"a.ply","repair":true}"#).unwrap();
        assert_eq!(info.repair, Some(true));
        let editing: EditInput = serde_json::from_str(
            r#"{"source":"a.ply","ops":[{"op":"translate","by":[0,1,0]}],"repair":true}"#,
        )
        .unwrap();
        assert_eq!(editing.repair, Some(true));
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn a_bounded_inspection_is_chosen_only_when_it_can_answer() {
        assert!(wants_bounded_inspection(None, None));
        assert!(wants_bounded_inspection(Some("viewer"), None));
        assert!(wants_bounded_inspection(Some("VIEWER"), None));
        // A sample needs the geometry, and a path is not the displayed document.
        assert!(!wants_bounded_inspection(Some("viewer"), Some(10)));
        assert!(!wants_bounded_inspection(Some("C:/tmp/a.ply"), None));
        assert!(!wants_bounded_inspection(Some("new"), None));
    }

    #[test]
    fn an_inspection_reply_keeps_the_summary_shape() {
        let splat = grid(3);
        let result = InspectResult {
            document: splatmcp_bridge::DocumentSummary {
                document_id: "doc-5".to_owned(),
                revision: 2,
                point_count: splat.len(),
                file_name: "a.ply".to_owned(),
                ..splatmcp_bridge::DocumentSummary::default()
            },
            inspection: inspect_splat(&splat),
        };
        let reply = info_reply_from_inspect(result);
        assert_eq!(reply.summary, SplatSummary::of(&splat));
        assert_eq!(reply.source.as_deref(), Some("viewer"));
        assert_eq!(reply.sample, None);
        let document = reply
            .document
            .clone()
            .expect("the identity travels with the summary");
        assert_eq!(document.document_id, "doc-5");
        assert_eq!(document.revision, 2);

        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.starts_with("{\"point_count\":3"), "{encoded}");
        assert!(encoded.contains("\"contract_version\""), "{encoded}");
        assert!(encoded.contains("\"owned_bytes\""), "{encoded}");
        assert!(encoded.len() < 1500, "{} bytes", encoded.len());
    }

    #[test]
    fn an_app_that_cannot_answer_leaves_the_ply_path_to_the_caller() {
        // No app is running here, so a bounded inspection is unavailable rather than
        // fatal: the caller reads the PLY, exactly as it did before this method existed.
        let link = AppLink::new(false);
        assert!(matches!(
            inspect_displayed(&link),
            InspectOutcome::Unavailable
        ));
    }

    #[test]
    fn merged_points_are_checked_before_they_are_clamped() {
        let merge = |scale: Factor| {
            to_step(
                &EditOpInput {
                    op: EditOpKind::Merge,
                    points: Some(vec![crate::tools::author::PointInput {
                        position: [0.0; 3],
                        color: None,
                        opacity: None,
                        scale: Some(scale),
                        rotation: None,
                    }]),
                    ..step(EditOpKind::Merge)
                },
                2,
            )
        };
        let error = merge(Factor::All(-1.0)).unwrap_err();
        assert!(error.contains("op 2 (merge)"), "{error}");
        assert!(error.contains("point 0 scale"), "{error}");
        assert!(error.contains("positive radius"), "{error}");

        let accepted = merge(Factor::All(0.05)).unwrap();
        match accepted.op {
            EditOp::Merge { points } => assert_eq!(points[0].scale, [0.05; 3]),
            other => panic!("expected merge, got {other:?}"),
        }
    }
}
