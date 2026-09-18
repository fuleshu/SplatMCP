//! The bridge service the desktop app hosts.
//!
//! The app cannot be reached over stdio - the MCP client owns that - so it publishes a
//! loopback port and a random token in `bridge.json` and serves the methods declared in
//! `splatmcp_bridge`. Viewer methods are forwarded into the webview; document methods are
//! answered here, because the app owns the displayed splat.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use splatmcp_bridge::client::CAPTURE_TIMEOUT;
use splatmcp_bridge::{
    BatchPointParams, BoundsInfo, BridgeDescriptor, BridgeServer, BridgeService, CaptureRequest,
    CommitPreviewRequest, ComponentSummary, ComponentsReply, ComponentsRequest, DocumentPlyReply,
    DocumentReply, DocumentSummary, DocumentTargetRequest, EditBatchReply, EditBatchRequest,
    GetPlyRequest, Handler, HistoryReply, HistoryStepSummary, InspectRequest, InspectResult,
    InspectionSummary, LoadPlyRequest, Method, PlyImportSummary, PreviewSummary,
    PythonCancelRequest, PythonJobQuery,
    PythonRunRequest, ReloadRequest, RetentionSummary, SelectionParams, SelectionSummary,
    SetComponentRequest, SideEffectSummary, StepSummary, TransformSummary, ViewerStatus,
};
use tauri::{AppHandle, Emitter, Manager};

use splatmcp_core::validation::ValidationLimits;
use splatmcp_core::{
    BatchStep, BatchTargets, Box3, ComponentId, EditBatch, EditOp, Expected, Frame, LocalTransform,
    PlyImportPolicy, PointId, SelectionQuery, Sphere, SplatPoint,
};

use crate::document::{self, AppState, Mutation, MutationKind, OutcomeInfo, SplatInfo};
use crate::python::{PythonHost, RevisionPayload};
use crate::viewer::{VIEWER_TIMEOUT, VIEWER_WINDOW, Viewer};

/// Bridge handler that turns requests into webview work, document reads or generation
/// jobs.
pub(crate) struct AppBridge {
    app: AppHandle,
    viewer: Arc<Viewer>,
    /// The one generation service this process hosts; the panel uses the same one.
    python: Arc<PythonHost>,
    started: Instant,
    app_version: String,
}

impl Handler for AppBridge {
    fn app_version(&self) -> String {
        self.app_version.clone()
    }

    fn handle(&self, method: Method, params: Value) -> Result<Value, String> {
        match method {
            Method::Hello => Err("the handshake is answered by the bridge server".to_owned()),
            Method::AppPing => Ok(json!({
                "app_version": self.app_version,
                "pid": std::process::id(),
                "uptime_ms": self.started.elapsed().as_millis() as u64,
            })),
            Method::ViewerStatus => {
                let value = self.viewer.request(method, Value::Null, VIEWER_TIMEOUT)?;
                Ok(value)
            }
            Method::ViewerGetCamera | Method::ViewerSetCamera => {
                self.viewer.request(method, params, VIEWER_TIMEOUT)
            }
            Method::ViewerCapture => {
                // Validate the request here so the viewer only sees well-formed input.
                let request: CaptureRequest = if params.is_null() {
                    CaptureRequest::default()
                } else {
                    serde_json::from_value(params.clone())
                        .map_err(|error| format!("invalid capture request: {error}"))?
                };
                self.viewer.request(
                    method,
                    serde_json::to_value(request).map_err(|error| error.to_string())?,
                    CAPTURE_TIMEOUT,
                )
            }
            Method::ViewerLoadPly => self.load_ply(params),
            Method::DocumentGetPly => self.document_ply(params),
            Method::DocumentInspect => self.document_inspect(params),
            Method::DocumentReload => self.document_reload(params),
            Method::DocumentSetComponent => self.document_set_component(params),
            Method::DocumentEditBatch => self.document_edit_batch(params),
            Method::DocumentCommitPreview => self.document_commit_preview(params),
            Method::DocumentHistory => self.document_history(params),
            Method::DocumentUndo => self.document_undo(params, true),
            Method::DocumentRedo => self.document_undo(params, false),
            Method::DocumentComponents => self.document_components(params),
            Method::PythonRuntimeInfo => Ok(self.python.runtime_info()),
            Method::PythonRunSplat => {
                let request: PythonRunRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid python run request: {error}"))?;
                let receipt = self.python.submit(request)?;
                serde_json::to_value(receipt).map_err(|error| error.to_string())
            }
            Method::PythonJob => {
                let query: PythonJobQuery = if params.is_null() {
                    PythonJobQuery::default()
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| format!("invalid python job query: {error}"))?
                };
                self.python_job(query)
            }
            Method::PythonJobCancel => {
                let request: PythonCancelRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid python cancel request: {error}"))?;
                let view = self.python.cancel(request.job_id)?;
                serde_json::to_value(view).map_err(|error| error.to_string())
            }
        }
    }
}

impl AppBridge {
    /// Stores pushed bytes as a document revision, then shows them in the viewer.
    ///
    /// Two shapes of request, and the difference is identity:
    ///
    /// - no `document_id`: the bytes become a **new document** at revision 1 (opening or
    ///   importing geometry);
    /// - a `document_id` with `expected_revision`: an explicit **replacement** of that
    ///   document, which keeps its identity and advances its revision by one. A stale
    ///   revision is refused, and the displayed document is left exactly as it was.
    ///
    /// Parsing happens before the viewer is asked and before the store is touched, so a
    /// malformed payload is rejected while the window keeps showing what it showed before.
    fn load_ply(&self, params: Value) -> Result<Value, String> {
        let request: LoadPlyRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid load_ply request: {error}"))?;
        let bytes = BASE64
            .decode(request.ply_base64.as_bytes())
            .map_err(|error| format!("ply_base64 is not valid base64: {error}"))?;

        let file_name = request
            .file_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "splat.ply".to_owned());
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        // Repair is opt-in: without it a file that would need repair is refused with indexed
        // diagnostics instead of being loaded as a quietly repaired document.
        let policy = PlyImportPolicy::from_repair_flag(request.repair);

        let state = self.app.state::<AppState>();
        let imported = match expected {
            Expected::Any => state.open_ply(&bytes, Mutation::import(file_name), policy)?,
            target => state.replace_ply(
                target,
                &bytes,
                Mutation::new(MutationKind::Edit)
                    .operation("load_splat")
                    .file_name(file_name),
                policy,
            )?,
        };
        let info = SplatInfo::of(&imported.metadata);

        let value = self.viewer.request(
            Method::ViewerLoadPly,
            serde_json::to_value(&request).map_err(|error| error.to_string())?,
            CAPTURE_TIMEOUT,
        )?;
        let mut status: ViewerStatus = serde_json::from_value(value)
            .map_err(|error| format!("the viewer returned an unexpected reply: {error}"))?;
        status.point_count = info.point_count;
        status.loaded = true;
        // The reply identifies the revision the caller actually got, and what the import did
        // to the file it came from.
        status.document = Some(DocumentSummary::from(&imported.metadata));
        status.import = PlyImportSummary::of(&imported.report);
        serde_json::to_value(status).map_err(|error| error.to_string())
    }

    /// A job's status, or the job history when no job id was given.
    fn python_job(&self, query: PythonJobQuery) -> Result<Value, String> {
        if query.job_id == 0 {
            let recent = self.python.recent(query.log_limit.unwrap_or(20).min(100));
            return Ok(json!({ "recent": recent }));
        }
        let view = self.python.status(
            query.job_id,
            query.log_after.unwrap_or(0),
            query.log_limit.unwrap_or(200).min(1000),
        )?;
        serde_json::to_value(view).map_err(|error| error.to_string())
    }

    /// Bounded metadata of a document revision.
    ///
    /// This is what keeps an inspection cheap: the app owns the document, so it answers with
    /// counts, bounds and distributions instead of serialising a PLY that the MCP server
    /// would immediately have to parse again. The snapshot is taken under the lock and the
    /// inspection runs outside it, so a 500 000 gaussian document does not block the store.
    fn document_inspect(&self, params: Value) -> Result<Value, String> {
        let request: InspectRequest = if params.is_null() {
            InspectRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid inspect request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = document::expected_target(request.document_id.as_deref(), request.revision)?;
        let snapshot = state.snapshot(expected)?;
        let limits = inspect_limits(&request);
        let inspection = InspectionSummary::from(&snapshot.splat().inspection(limits));
        let result = InspectResult {
            document: DocumentSummary::from(snapshot.metadata()),
            inspection,
        };
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    /// Reads the source of a document again, as a new revision of it.
    fn document_reload(&self, params: Value) -> Result<Value, String> {
        let request: ReloadRequest = if params.is_null() {
            ReloadRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid reload request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = match request.expected_revision {
            Some(revision) => Expected::Revision(revision),
            None => Expected::Any,
        };
        let policy = PlyImportPolicy::from_repair_flag(request.repair);
        let imported = state.reload(expected, policy)?;
        serde_json::to_value(DocumentReply {
            document: DocumentSummary::from(&imported.metadata),
            retention: RetentionSummary::from(state.retention()),
            import: PlyImportSummary::of(&imported.report),
        })
        .map_err(|error| error.to_string())
    }

    /// Changes the named component of the displayed document.
    fn document_set_component(&self, params: Value) -> Result<Value, String> {
        let request: SetComponentRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid set_component request: {error}"))?;
        let state = self.app.state::<AppState>();
        let expected = match request.expected_revision {
            Some(revision) => Expected::Revision(revision),
            None => Expected::Any,
        };
        let operation = request
            .operation
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "set_component".to_owned());
        let metadata = state.set_component(expected, &request.component_id, &operation)?;
        serde_json::to_value(DocumentReply {
            document: DocumentSummary::from(&metadata),
            retention: RetentionSummary::from(state.retention()),
            import: None,
        })
        .map_err(|error| error.to_string())
    }

    /// PLY bytes of one revision of one document, with the identity they are of.
    ///
    /// The bytes are serialised from the snapshot after the store lock is released, and the
    /// reply names the exact revision it resolved, so a caller can tell an expired handle
    /// from a mismatch.
    fn document_ply(&self, params: Value) -> Result<Value, String> {
        let request: GetPlyRequest = if params.is_null() {
            GetPlyRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid get_ply request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = document::expected_target(request.document_id.as_deref(), request.revision)?;
        let (snapshot, bytes) = match expected {
            Expected::Handle(handle) => state.ply_bytes_for(&handle)?,
            target => {
                let snapshot = state.snapshot(target)?;
                let bytes = document::ply_bytes(snapshot.splat())?;
                (snapshot, bytes)
            }
        };
        let reply = DocumentPlyReply {
            ply_base64: BASE64.encode(&bytes),
            file_name: Some(snapshot.provenance().file_name.clone()),
            document: DocumentSummary::from(snapshot.metadata()),
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }
}

/// Event the app emits when an edit transaction committed a revision that should be shown.
///
/// Deliberately separate from the Python job's `splat://revision`: a transaction is not a job,
/// and a viewer acknowledgement of a job's revision must not be confused with an edit.
pub const EDIT_REVISION_EVENT: &str = "splat://edit-revision";

/// Parses a six- or two-number box, the same shape the edit tools accept.
fn parse_box(values: &[f32], name: &str) -> Result<Box3, String> {
    let finite = values.iter().all(|value| value.is_finite());
    match values {
        [min_x, min_y, min_z, max_x, max_y, max_z] if finite => Ok(Box3::from_corners(
            [*min_x, *min_y, *min_z],
            [*max_x, *max_y, *max_z],
        )),
        [min_x, max_x] if finite => Ok(Box3::from_corners([*min_x, 0.0, 0.0], [*max_x, 0.0, 0.0])),
        _ => Err(format!(
            "{name} must be [min_x, min_y, min_z, max_x, max_y, max_z] or [min_x, max_x] of \
             finite numbers"
        )),
    }
}

/// Turns a wire selection into a core query.
pub(crate) fn to_selection(params: &SelectionParams) -> Result<SelectionQuery, String> {
    let frame = match params.frame.as_deref() {
        None | Some("world") => Frame::World,
        Some("local") => Frame::Local,
        Some(other) => {
            return Err(format!("unknown frame '{other}'; use 'local' or 'world'"));
        }
    };
    let component = match params.component.as_deref() {
        Some(text) => Some(
            ComponentId::parse(text).ok_or_else(|| format!("'{text}' is not a component id"))?,
        ),
        None => None,
    };
    let mut point_ids = Vec::with_capacity(params.point_ids.len());
    for text in &params.point_ids {
        point_ids.push(PointId::parse(text).ok_or_else(|| format!("'{text}' is not a point id"))?);
    }
    Ok(SelectionQuery {
        component,
        point_ids,
        within: params
            .within
            .as_deref()
            .map(|values| parse_box(values, "within"))
            .transpose()?,
        outside: params
            .outside
            .as_deref()
            .map(|values| parse_box(values, "outside"))
            .transpose()?,
        sphere: params
            .sphere
            .map(|[x, y, z, radius]| Sphere::new([x, y, z], radius)),
        frame,
        color_min: params.color_min,
        color_max: params.color_max,
        opacity_min: params.opacity_min,
        max_radius: params.max_radius,
        first: params.first,
    })
}

/// Turns a wire selection into batch targets, keeping a saved handle reference.
fn to_targets(params: &SelectionParams) -> Result<BatchTargets, String> {
    let query = to_selection(params)?;
    Ok(BatchTargets {
        selection: splatmcp_core::Selection {
            within: query.within,
            outside: query.outside,
            color_min: query.color_min,
            color_max: query.color_max,
            opacity_min: query.opacity_min,
            max_radius: query.max_radius,
            first: query.first,
        },
        frame: query.frame,
        component: query.component,
        point_ids: query.point_ids,
        selection_handle: params.selection_handle,
    })
}

/// Validates one merged point, so a bad value is reported rather than repaired.
fn to_point(params: &BatchPointParams) -> Result<SplatPoint, String> {
    SplatPoint::try_new(
        params.position,
        params.scale.unwrap_or([0.01; 3]),
        params.color.unwrap_or([0.5; 3]),
        params.opacity.unwrap_or(1.0),
        params.rotation.unwrap_or([1.0, 0.0, 0.0, 0.0]),
    )
    .map_err(|error| error.to_string())
}

fn required<T>(value: Option<T>, index: usize, op: &str, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("step {index} ({op}) needs {field}"))
}

/// Turns a wire batch into a core batch, refusing anything it cannot mean.
pub(crate) fn to_batch(request: &EditBatchRequest) -> Result<EditBatch, String> {
    let mut steps = Vec::with_capacity(request.steps.len());
    for (index, op) in request.steps.iter().enumerate() {
        let targets = match &op.selection {
            Some(params) => to_targets(params)?,
            None => BatchTargets::all(),
        };
        let edit_op = match op.op.as_str() {
            "translate" => EditOp::Translate {
                by: required(op.by, index, "translate", "by")?,
            },
            "rotate" => EditOp::Rotate {
                axis: op.axis.unwrap_or([0.0, 1.0, 0.0]),
                degrees: required(op.degrees, index, "rotate", "degrees")?,
                center: op.center.unwrap_or([0.0; 3]),
            },
            "scale" => EditOp::Scale {
                center: op.center.unwrap_or([0.0; 3]),
                factor: required(op.factor, index, "scale", "factor")?,
            },
            "set_radius" => EditOp::SetRadius {
                factor: required(op.factor, index, "set_radius", "factor")?[0],
            },
            "adjust_color" => EditOp::AdjustColor {
                delta: required(op.delta, index, "adjust_color", "delta")?,
            },
            "set_color" => EditOp::SetColor {
                color: required(op.color, index, "set_color", "color")?,
                mix: op.mix.unwrap_or(1.0),
            },
            "set_opacity" => EditOp::SetOpacity {
                factor: required(op.factor, index, "set_opacity", "factor")?[0],
            },
            "duplicate" => EditOp::Duplicate {
                by: required(op.by, index, "duplicate", "by")?,
            },
            "remove" => EditOp::Remove,
            "merge" => EditOp::Merge {
                points: op
                    .points
                    .iter()
                    .map(to_point)
                    .collect::<Result<Vec<_>, _>>()?,
            },
            other => {
                return Err(format!(
                    "step {index}: unknown operation '{other}'; use translate, rotate, scale, \
                     set_radius, adjust_color, set_color, set_opacity, duplicate, remove or merge"
                ));
            }
        };
        steps.push(BatchStep::with_targets(edit_op, targets));
    }
    let mut batch = EditBatch::new(steps);
    match request.resolution.as_deref() {
        None | Some("stable") => {}
        Some("sequential") => batch = batch.sequential(),
        Some(other) => {
            return Err(format!(
                "unknown resolution '{other}'; use 'stable' or 'sequential'"
            ));
        }
    }
    if let Some(operation_id) = &request.operation_id {
        batch = batch.with_operation_id(operation_id.clone());
    }
    Ok(batch)
}

fn step_summaries(steps: &[document::StepOutcome]) -> Vec<StepSummary> {
    steps
        .iter()
        .map(|step| StepSummary {
            op_index: step.op_index,
            affected: step.affected,
            remaining: step.remaining,
        })
        .collect()
}

fn bounds_info(bounds: Option<document::BoundsInfo>) -> Option<BoundsInfo> {
    bounds.map(|bounds| BoundsInfo {
        min: bounds.min,
        max: bounds.max,
        center: bounds.center,
        radius: bounds.radius,
    })
}

fn side_effect(outcome: &OutcomeInfo) -> SideEffectSummary {
    SideEffectSummary {
        status: outcome.status.to_owned(),
        message: outcome.message.clone(),
    }
}

fn not_requested() -> SideEffectSummary {
    SideEffectSummary {
        status: "not_requested".to_owned(),
        message: None,
    }
}

fn preview_summary(preview: &document::PreviewInfo) -> PreviewSummary {
    PreviewSummary {
        preview_id: preview.preview_id,
        source_revision: preview.source_revision,
        points_before: preview.points_before,
        points_after: preview.points_after,
        steps: step_summaries(&preview.steps),
        warnings: preview.warnings.clone(),
        memory_estimate_bytes: preview.memory_estimate_bytes,
        point_ids: preview.point_ids,
        bounds_before: bounds_info(preview.bounds_before),
        bounds_after: bounds_info(preview.bounds_after),
    }
}

fn component_summaries(components: &[document::ComponentInfo]) -> Vec<ComponentSummary> {
    components
        .iter()
        .map(|component| ComponentSummary {
            component_id: component.component_id.clone(),
            name: component.name.clone(),
            point_count: component.point_count,
            transform: component.transform.map(|transform| TransformSummary {
                translation: transform.translation,
                rotation: transform.rotation,
                scale: transform.scale,
            }),
            metadata: component.metadata.clone(),
        })
        .collect()
}

fn selection_summary(selection: &document::SelectionInfo) -> SelectionSummary {
    SelectionSummary {
        handle_id: selection.handle_id,
        revision: selection.revision,
        count: selection.count,
        sample: selection.sample.clone(),
        truncated: selection.truncated,
        bounds: bounds_info(selection.bounds),
    }
}

/// Turns a rejected transaction into a message a caller can act on.
fn transaction_message(error: splatmcp_core::TransactionError) -> String {
    format!("{} ({})", error, error.code())
}

impl AppBridge {
    /// Tells the viewer to load the revision the store just produced.
    ///
    /// Publication is revision addressed: the frontend fetches the exact revision as binary, so
    /// a display failure can never leave the viewer showing half of an edit, and the commit
    /// stays recorded either way.
    fn publish(&self, handle: &splatmcp_core::DocumentHandle, frame: bool) -> Result<(), String> {
        let state = self.app.state::<AppState>();
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        if &metadata.handle != handle {
            return Err(format!(
                "the displayed document is {} but {} was committed",
                metadata.handle, handle
            ));
        }
        let payload = RevisionPayload {
            revision: handle.revision,
            document_id: handle.document_id.to_string(),
            file_name: metadata.provenance.file_name.clone(),
            point_count: metadata.point_count,
            component_id: metadata.provenance.component_id.clone(),
            frame,
        };
        self.app
            .emit_to(VIEWER_WINDOW, EDIT_REVISION_EVENT, payload)
            .map_err(|error| format!("could not tell the viewer about {handle}: {error}"))
    }

    /// Runs an edit batch, or dry-runs it when `dry_run` is set.
    pub(crate) fn document_edit_batch(&self, params: Value) -> Result<Value, String> {
        let request: EditBatchRequest = if params.is_null() {
            EditBatchRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid edit_batch request: {error}"))?
        };
        let batch = to_batch(&request)?;
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let state = self.app.state::<AppState>();
        if request.dry_run.unwrap_or(false) {
            let preview = state
                .preview_batch(expected, &batch)
                .map_err(transaction_message)?;
            let metadata = state
                .metadata()
                .ok_or_else(|| "no splat is loaded".to_owned())?;
            let reply = EditBatchReply {
                document: DocumentSummary::from(&metadata),
                retention: RetentionSummary::from(state.retention()),
                committed: false,
                replayed: false,
                point_count: preview.points_after,
                steps: step_summaries(&preview.steps),
                warnings: preview.warnings.clone(),
                preview_id: Some(preview.preview_id),
                preview: Some(preview_summary(&preview)),
                undo_available: false,
                redo_available: false,
                export: not_requested(),
                display: not_requested(),
            };
            return serde_json::to_value(reply).map_err(|error| error.to_string());
        }
        let display = request.display.unwrap_or(true);
        let mut outcome = state
            .commit_batch(expected, &batch, "edit_batch")
            .map_err(transaction_message)?;
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        outcome.display = match display {
            true => match self.publish(&metadata.handle, false) {
                Ok(()) => OutcomeInfo {
                    status: "done",
                    message: None,
                },
                Err(message) => OutcomeInfo {
                    status: "failed",
                    message: Some(message),
                },
            },
            false => OutcomeInfo {
                status: "not_requested",
                message: None,
            },
        };
        self.edit_batch_reply(&outcome, None)
    }

    /// Commits the candidate a dry run retained, if it is still current.
    pub(crate) fn document_commit_preview(&self, params: Value) -> Result<Value, String> {
        let request: CommitPreviewRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid commit_preview request: {error}"))?;
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let state = self.app.state::<AppState>();
        let mut outcome = state
            .commit_preview(request.preview_id, expected)
            .map_err(transaction_message)?;
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        if request.display.unwrap_or(true) {
            outcome.display = match self.publish(&metadata.handle, false) {
                Ok(()) => OutcomeInfo {
                    status: "done",
                    message: None,
                },
                Err(message) => OutcomeInfo {
                    status: "failed",
                    message: Some(message),
                },
            };
        }
        self.edit_batch_reply(&outcome, None)
    }

    /// Undo or redo, then show the revision that resulted.
    pub(crate) fn document_undo(&self, params: Value, undo: bool) -> Result<Value, String> {
        let request: DocumentTargetRequest = if params.is_null() {
            DocumentTargetRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid history request: {error}"))?
        };
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let state = self.app.state::<AppState>();
        let mut outcome = if undo {
            state.undo(expected)
        } else {
            state.redo(expected)
        }
        .map_err(transaction_message)?;
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        if request.display.unwrap_or(true) {
            outcome.display = match self.publish(&metadata.handle, false) {
                Ok(()) => OutcomeInfo {
                    status: "done",
                    message: None,
                },
                Err(message) => OutcomeInfo {
                    status: "failed",
                    message: Some(message),
                },
            };
        }
        self.edit_batch_reply(&outcome, None)
    }

    /// Undo/redo availability and the retained steps.
    pub(crate) fn document_history(&self, params: Value) -> Result<Value, String> {
        let request: DocumentTargetRequest = if params.is_null() {
            DocumentTargetRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid history request: {error}"))?
        };
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let state = self.app.state::<AppState>();
        let history = state.edit_history(expected).map_err(transaction_message)?;
        let step = |entry: &document::HistoryStepInfo| HistoryStepSummary {
            id: entry.id,
            label: entry.label.clone(),
            revision: entry.revision,
            point_count: entry.point_count,
            at_ms: entry.at_ms,
        };
        let reply = HistoryReply {
            document: DocumentSummary::from(&state.metadata().ok_or("no splat is loaded")?),
            retention: RetentionSummary::from(state.retention()),
            undo: history.undo.as_ref().map(step),
            redo: history.redo.as_ref().map(step),
            entries: history.entries.iter().map(step).collect(),
            retained_bytes: history.retained_bytes,
            max_bytes: history.max_bytes,
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    /// Components and selections, through one action-shaped method.
    pub(crate) fn document_components(&self, params: Value) -> Result<Value, String> {
        let request: ComponentsRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid components request: {error}"))?;
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let component_id = || -> Result<ComponentId, String> {
            let text = request
                .component_id
                .clone()
                .ok_or_else(|| "this action needs component_id".to_owned())?;
            ComponentId::parse(&text).ok_or_else(|| format!("'{text}' is not a component id"))
        };
        let state = self.app.state::<AppState>();
        let mut selection = None;
        let mut steps = Vec::new();
        let (components, rebuilt, changed) = match request.action.as_str() {
            "list" => {
                let list = state.components(expected).map_err(transaction_message)?;
                (list.components, list.rebuilt, None)
            }
            "create" => {
                let name = request
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| "action 'create' needs name".to_owned())?;
                let change = state
                    .create_component(expected, &name)
                    .map_err(transaction_message)?;
                (change.components, change.rebuilt, Some(change.component_id))
            }
            "rename" => {
                let name = request
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| "action 'rename' needs name".to_owned())?;
                let change = state
                    .rename_component(expected, &component_id()?, &name)
                    .map_err(transaction_message)?;
                (change.components, change.rebuilt, Some(change.component_id))
            }
            "remove" => {
                let change = state
                    .remove_component(expected, &component_id()?)
                    .map_err(transaction_message)?;
                (change.components, change.rebuilt, Some(change.component_id))
            }
            "transform" => {
                let transform = if request.clear_transform.unwrap_or(false) {
                    None
                } else {
                    Some(LocalTransform {
                        translation: request.translation.unwrap_or([0.0; 3]),
                        rotation: request.rotation.unwrap_or([1.0, 0.0, 0.0, 0.0]),
                        scale: request.scale.unwrap_or([1.0; 3]),
                    })
                };
                let change = state
                    .set_component_transform(expected, &component_id()?, transform)
                    .map_err(transaction_message)?;
                (change.components, change.rebuilt, Some(change.component_id))
            }
            "members" => {
                let params = request
                    .selection
                    .as_ref()
                    .ok_or_else(|| "action 'members' needs selection".to_owned())?;
                let query = to_selection(params)?;
                let change = state
                    .set_component_members(expected, &component_id()?, &query)
                    .map_err(transaction_message)?;
                if request.apply_transform.unwrap_or(false) {
                    let handle = document::handle_from_info(&change.document)?;
                    let component = ComponentId::parse(&change.component_id)
                        .ok_or_else(|| "the app reported an unreadable component id".to_owned())?;
                    let outcome = state
                        .apply_component_transform(Expected::Handle(handle), &component)
                        .map_err(transaction_message)?;
                    steps = step_summaries(&outcome.steps);
                }
                let list = state
                    .components(Expected::Any)
                    .map_err(transaction_message)?;
                (list.components, change.rebuilt, Some(change.component_id))
            }
            "apply_transform" => {
                let target = component_id()?;
                let outcome = state
                    .apply_component_transform(expected, &target)
                    .map_err(transaction_message)?;
                steps = step_summaries(&outcome.steps);
                let list = state
                    .components(Expected::Any)
                    .map_err(transaction_message)?;
                (list.components, list.rebuilt, Some(target.to_string()))
            }
            "select" => {
                let params = request
                    .selection
                    .as_ref()
                    .ok_or_else(|| "action 'select' needs selection".to_owned())?;
                let query = to_selection(params)?;
                let resolved = state
                    .select_points(expected, &query)
                    .map_err(transaction_message)?;
                selection = Some(selection_summary(&resolved));
                let list = state
                    .components(Expected::Any)
                    .map_err(transaction_message)?;
                (list.components, list.rebuilt, None)
            }
            other => {
                return Err(format!(
                    "unknown action '{other}'; use list, create, rename, remove, transform, \
                     members, apply_transform or select"
                ));
            }
        };
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        let reply = ComponentsReply {
            document: DocumentSummary::from(&metadata),
            retention: RetentionSummary::from(state.retention()),
            components: component_summaries(&components),
            rebuilt,
            component_id: changed,
            selection,
            steps,
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    /// A bridge handler for a caller that already has an app handle: the Tauri commands and the
    /// loopback bridge then share one implementation of every transaction method.
    ///
    /// The viewer and Python host are managed state, so a command resolves the same instances the
    /// bridge uses rather than a second copy.
    pub(crate) fn for_commands(app: &AppHandle) -> Self {
        let viewer = app.state::<ViewerState>().0.clone();
        let python = app.state::<crate::python::PythonHostState>().0.clone();
        Self {
            app: app.clone(),
            viewer,
            python,
            started: Instant::now(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// The wire shape of a committed batch, with side effects kept separate.
    fn edit_batch_reply(
        &self,
        outcome: &document::BatchOutcome,
        preview: Option<PreviewSummary>,
    ) -> Result<Value, String> {
        let state = self.app.state::<AppState>();
        let metadata = state
            .metadata()
            .ok_or_else(|| "no splat is loaded".to_owned())?;
        let reply = EditBatchReply {
            document: DocumentSummary::from(&metadata),
            retention: RetentionSummary::from(state.retention()),
            committed: outcome.committed,
            replayed: outcome.replayed,
            point_count: outcome.point_count,
            steps: step_summaries(&outcome.steps),
            warnings: outcome.warnings.clone(),
            preview_id: outcome
                .preview_id
                .or(preview.as_ref().map(|p| p.preview_id)),
            preview,
            undo_available: outcome.undo_available,
            redo_available: outcome.redo_available,
            export: side_effect(&outcome.export),
            display: side_effect(&outcome.display),
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }
}

/// The limits an inspect request asks for.
///
/// Absent means the contract's own ceiling, and the limit that was applied is reported
/// back in the summary rather than silently assumed.
pub(crate) fn inspect_limits(request: &InspectRequest) -> ValidationLimits {
    match request.max_points {
        Some(limit) => ValidationLimits::with_max_points(limit),
        None => ValidationLimits::default(),
    }
}

/// A started bridge: the published descriptor plus the service handle.
pub struct BridgeHost {
    pub descriptor: BridgeDescriptor,
    service: BridgeService,
}

impl BridgeHost {
    pub fn port(&self) -> u16 {
        self.descriptor.port
    }

    fn shutdown(self) {
        self.service.shutdown();
    }
}

/// Binds the bridge, publishes `bridge.json` and starts serving.
pub fn start(
    app: &AppHandle,
    viewer: Arc<Viewer>,
    python: Arc<PythonHost>,
) -> Result<BridgeHost, String> {
    let server = BridgeServer::bind().map_err(|error| error.to_string())?;
    let descriptor = server
        .publish(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("could not publish bridge.json: {error}"))?;
    let handler = Arc::new(AppBridge {
        app: app.clone(),
        viewer,
        python,
        started: Instant::now(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    let service = server
        .serve(handler, Duration::from_secs(60))
        .map_err(|error| format!("could not start the bridge: {error}"))?;
    Ok(BridgeHost {
        descriptor,
        service,
    })
}

/// Removes the descriptor this process published, on the way out.
pub fn retire() {
    splatmcp_bridge::BridgeServer::retire(std::process::id());
}

/// Managed state wrapper so the `bridge_respond` command can find the viewer.
pub struct ViewerState(pub Arc<Viewer>);

/// Keeps the bridge host alive for the lifetime of the app.
pub struct BridgeHostState(pub std::sync::Mutex<Option<BridgeHost>>);

impl BridgeHostState {
    pub fn shutdown(&self) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(host) = guard.take()
        {
            host.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_bridge::protocol::ViewerStatus as Status;

    #[test]
    fn an_edit_batch_request_becomes_a_transaction_the_core_can_run() {
        let request: EditBatchRequest = serde_json::from_value(json!({
            "operation_id": "recipe-1",
            "resolution": "stable",
            "steps": [
                { "op": "translate", "by": [0.0, 1.0, 0.0] },
                {
                    "op": "remove",
                    "selection": { "within": [0.5, -1.0, -1.0, 1.5, 1.0, 1.0], "first": 2 }
                }
            ]
        }))
        .unwrap();
        let batch = to_batch(&request).unwrap();
        assert_eq!(batch.operation_id.as_deref(), Some("recipe-1"));
        assert_eq!(batch.steps.len(), 2);
        assert_eq!(batch.steps[0].targets, BatchTargets::all());

        // An unknown operation, an unknown frame and a malformed id are all refused with a
        // message that names what is wrong.
        let unknown_op: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "explode" }]
        }))
        .unwrap();
        assert!(
            to_batch(&unknown_op)
                .unwrap_err()
                .contains("unknown operation")
        );

        let bad_frame: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "remove", "selection": { "frame": "camera" } }]
        }))
        .unwrap();
        assert!(to_batch(&bad_frame).unwrap_err().contains("unknown frame"));

        let bad_id: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "remove", "selection": { "point_ids": ["row-3"] } }]
        }))
        .unwrap();
        assert!(to_batch(&bad_id).unwrap_err().contains("not a point id"));

        // A missing required field says which step and which field.
        let missing: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "rotate", "degrees": 90.0 }]
        }))
        .unwrap();
        let batch = to_batch(&missing).unwrap();
        assert_eq!(
            batch.steps[0].op,
            EditOp::Rotate {
                axis: [0.0, 1.0, 0.0],
                degrees: 90.0,
                center: [0.0; 3]
            }
        );
        let missing_by: EditBatchRequest =
            serde_json::from_value(json!({ "steps": [{ "op": "translate" }] })).unwrap();
        assert!(
            to_batch(&missing_by)
                .unwrap_err()
                .contains("step 0 (translate) needs by")
        );
    }

    #[test]
    fn a_selection_request_keeps_its_composition_and_rejects_nonsense() {
        let params = SelectionParams {
            within: Some(vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0]),
            sphere: Some([0.0, 0.0, 0.0, 2.0]),
            frame: Some("local".to_owned()),
            component: Some(splatmcp_core::ComponentId::mint(1).to_string()),
            selection_handle: Some(7),
            ..SelectionParams::default()
        };
        let query = to_selection(&params).unwrap();
        assert_eq!(query.frame, Frame::Local);
        assert!(query.within.is_some() && query.sphere.is_some());
        let targets = to_targets(&params).unwrap();
        assert_eq!(targets.selection_handle, Some(7));
        assert_eq!(
            targets.component,
            params
                .component
                .map(|text| splatmcp_core::ComponentId::parse(&text).unwrap())
        );
    }

    #[test]
    fn capture_requests_are_validated_before_they_reach_the_viewer() {
        let broken: Result<CaptureRequest, _> = serde_json::from_value(json!({"width": "wide"}));
        assert!(broken.is_err());

        let request: CaptureRequest =
            serde_json::from_value(json!({"width": 800, "format": "png"})).unwrap();
        assert_eq!(request.width, Some(800));
        assert_eq!(request.format.as_deref(), Some("png"));

        let empty: CaptureRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty.width, None);
    }

    #[test]
    fn a_load_reply_carries_the_identity_it_resolved() {
        let status = Status {
            viewer_ready: true,
            loaded: true,
            point_count: 0,
            canvas_width: 1280,
            canvas_height: 720,
            camera: None,
            document: None,
            import: None,
        };
        // The handler overwrites the count and fills in the resolved identity.
        let mut adjusted = status.clone();
        adjusted.point_count = 189;
        adjusted.document = Some(DocumentSummary {
            document_id: "doc-1-2".to_owned(),
            revision: 4,
            point_count: 189,
            file_name: "scene.ply".to_owned(),
            created_at_ms: 10,
            updated_at_ms: 20,
            ..DocumentSummary::default()
        });
        let encoded = serde_json::to_value(&adjusted).unwrap();
        assert_eq!(encoded["point_count"], 189);
        assert_eq!(encoded["document"]["revision"], 4);
        assert_eq!(encoded["document"]["point_count"], 189);

        // A viewer that reports no identity is still readable, so an older frontend works.
        let legacy: Status =
            serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert!(legacy.document.is_none());
    }

    #[test]
    fn an_inspect_request_names_the_limit_it_applies_and_what_it_targets() {
        assert_eq!(
            inspect_limits(&InspectRequest::default()).applied_max_points(),
            Some(splatmcp_core::MAX_POINTS)
        );
        assert_eq!(
            inspect_limits(&InspectRequest {
                max_points: Some(4),
                ..InspectRequest::default()
            })
            .applied_max_points(),
            Some(4)
        );
        // A named revision is an exact target; an absent one means the displayed document.
        assert_eq!(
            document::expected_target(Some("doc-4f2a-1"), Some(2)).unwrap(),
            Expected::Handle(splatmcp_core::DocumentHandle::new(
                splatmcp_core::DocumentId::mint(0x4f2a, 1),
                2
            ))
        );
        assert_eq!(
            document::expected_target(None, None).unwrap(),
            Expected::Any
        );
    }

    #[test]
    fn an_inspection_reports_the_document_it_describes() {
        // The handler's own conversion, exercised without an app: the summary is bounded
        // metadata and the identity travels beside it.
        let splat = splatmcp_core::fixtures::axis_fixture();
        let request = InspectRequest {
            max_points: Some(4),
            ..InspectRequest::default()
        };
        let inspection = InspectionSummary::from(&splat.inspection(inspect_limits(&request)));
        let metadata = splatmcp_core::DocumentStore::with_session(
            splatmcp_core::RetentionLimits::default(),
            0x4f2a,
        )
        .open(splat.clone(), Mutation::import("axis.ply"));
        let result = InspectResult {
            document: DocumentSummary::from(&metadata),
            inspection,
        };
        assert_eq!(result.inspection.point_count, splat.len());
        assert_eq!(result.inspection.point_limit, Some(4));
        assert!(!result.inspection.within_limits);
        assert_eq!(result.document.revision, 1);
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.len() < 1600, "{} bytes", encoded.len());
        assert!(encoded.contains("\"revision\":1"));
        assert!(encoded.contains("axis.ply"));
    }
}
