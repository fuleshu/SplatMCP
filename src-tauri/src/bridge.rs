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
    BatchOpParams, BatchPointParams, BoundsInfo, BridgeDescriptor, BridgeServer, BridgeService,
    CaptureRequest, CommitPreviewRequest, ComponentSummary, ComponentsReply, ComponentsRequest,
    DocumentPlyReply, DocumentReply, DocumentSummary, DocumentTargetRequest, EditBatchReply,
    EditBatchRequest, GetPlyRequest, Handler, HistoryReply, HistoryStepSummary, InspectRequest,
    InspectResult, InspectionSummary, LoadPlyRequest, Method, PlyImportSummary, PreviewSummary,
    PythonCancelRequest, PythonJobQuery, PythonRunRequest, ReloadRequest, RetentionSummary,
    SelectionParams, SelectionSummary, SetComponentRequest, SideEffectSummary, StepSummary,
    TransformSummary, ViewerStatus,
};
use tauri::{AppHandle, Emitter, Manager};

use splatmcp_core::validation::ValidationLimits;
use splatmcp_core::{
    BatchStep, BatchTargets, Box3, ComponentId, EditBatch, EditOp, Expected, Frame, LocalTransform,
    PlyImportPolicy, PointId, PublicationSource, ReceiptSlot, SelectionQuery, SideEffect, Sphere,
    SplatPoint,
};

use crate::assets::AssetHost;
use crate::document::{self, AppState, Mutation, MutationKind, OutcomeInfo, SplatInfo};
use crate::publication::PublicationHostState;
use crate::python::{PythonHost, RevisionPayload};
use crate::viewer::{VIEWER_TIMEOUT, VIEWER_WINDOW, Viewer};

/// Bridge handler that turns requests into webview work, document reads, generation
/// jobs or asset registry work.
pub(crate) struct AppBridge {
    app: AppHandle,
    viewer: Arc<Viewer>,
    /// The one generation service this process hosts; the panel uses the same one.
    python: Arc<PythonHost>,
    /// The one asset registry this process hosts; the window uses the same one.
    assets: Arc<AssetHost>,
    started: Instant,
    app_version: String,
}

impl Handler for AppBridge {
    fn app_version(&self) -> String {
        self.app_version.clone()
    }

    fn handle(&self, method: Method, params: Value) -> Result<Value, String> {
        // Asset methods are answered by the asset host, so this handler stays about the
        // document and the viewer.
        if let Some(result) = self.assets.handle(method, params.clone()) {
            return result;
        }
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
            // Answered by the asset host before this dispatch runs: stated explicitly so a
            // future method added to the protocol cannot fall through to the wrong handler.
            Method::AssetRegister
            | Method::AssetInfo
            | Method::AssetRelease
            | Method::AssetUploadBegin
            | Method::AssetUploadChunk
            | Method::AssetUploadStatus
            | Method::AssetUploadFinalize
            | Method::AssetUploadCancel => {
                Err("this method is answered by the asset host".to_owned())
            }
            Method::PythonRuntimeInfo => Ok(self.python.runtime_info()),
            Method::JobSubmit => self.job_submit(params),
            Method::JobStatus => self.job_status(params),
            Method::JobList => self.job_list(params),
            Method::JobCancel => self.job_cancel(params),
            Method::PublicationStatus => self.publication_status(params),
            Method::PublicationCapabilities => self.publication_capabilities(params),
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
    /// The bytes come from a registered asset when `asset_id` is set, and from the inline
    /// field otherwise. An asset is the compact form: the app already holds a snapshot of
    /// those bytes, so nothing large travels in the request and a later edit of the source
    /// file cannot change what is loaded.
    ///
    /// Parsing happens before the viewer is asked and before the store is touched, so a
    /// malformed payload is rejected while the window keeps showing what it showed before.
    fn load_ply(&self, params: Value) -> Result<Value, String> {
        let request: LoadPlyRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid load_ply request: {error}"))?;
        // Provenance of an asset-backed load: the file the snapshot was read from, when it
        // came from one.
        let mut asset_file: Option<String> = None;
        let bytes = match &request.asset_id {
            Some(asset_id) => {
                let asset = self.assets.resolve(asset_id)?;
                if asset.kind() != splatmcp_core::AssetKind::Ply {
                    return Err(format!(
                        "asset {asset_id} holds {} bytes; loading a document needs a ply asset",
                        asset.kind().as_str()
                    ));
                }
                // A file asset names the file it was read from, so authoring metadata beside
                // that file can still be associated by content - but the bytes loaded are the
                // snapshot, never a second read of the file.
                asset_file = Some(asset.source().to_owned());
                asset.bytes().to_vec()
            }
            None => BASE64
                .decode(request.ply_base64.as_bytes())
                .map_err(|error| format!("ply_base64 is not valid base64: {error}"))?,
        };

        let file_name = request
            .file_name
            .clone()
            .or_else(|| {
                asset_file
                    .as_deref()
                    .map(|path| {
                        std::path::Path::new(path)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    })
                    .filter(|name| !name.is_empty())
            })
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "splat.ply".to_owned());
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        // Repair is opt-in: without it a file that would need repair is refused with indexed
        // diagnostics instead of being loaded as a quietly repaired document.
        let policy = PlyImportPolicy::from_repair_flag(request.repair);
        let source_path = authoring_source(&request, asset_file.as_deref());

        let state = self.app.state::<AppState>();
        let opening = matches!(expected, Expected::Any);
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
        // A *file* load is where component metadata is restored: association is by content, and
        // anything else is reported as a note on this reply rather than attached. A refusal is
        // reported even for a replacement, because "I did not attach your metadata" is something
        // the caller needs to hear either way.
        if let Some(note) = source_path
            .as_deref()
            .and_then(|path| restore_note(&state, std::path::Path::new(path), &imported, &bytes))
        {
            if opening || note.status == "refused" {
                state.set_authoring_note(&imported.metadata.handle, note);
            }
        }

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
        status.document = Some(summary_of(&state, &imported.metadata));
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

impl AppBridge {
    /// Submits a long operation as a job and returns as soon as it is admitted.
    ///
    /// The reply is identity and state only: an import's progress, result and failure are read
    /// afterwards through `job.status`, so a slow operation never holds a bridge request open
    /// and a dropped connection neither cancels nor resubmits the accepted work.
    fn job_submit(&self, params: Value) -> Result<Value, String> {
        let request: splatmcp_bridge::JobSubmitRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid job.submit request: {error}"))?;
        let jobs = self.app.state::<crate::jobs::JobHostState>().0.clone();
        let expected =
            document::expected_target(request.document_id.as_deref(), request.expected_revision)?;
        let admission = match request.operation.as_str() {
            "import" => {
                let asset_id = request.asset_id.as_deref().ok_or_else(|| {
                    "job.submit 'import' needs asset_id (a registered ply asset)".to_owned()
                })?;
                let assets = self.app.state::<crate::assets::AssetHostState>().0.clone();
                let asset = crate::jobs::resolve_asset(&assets, asset_id)?;
                jobs.import(&self.app, asset, expected, request.operation_id)?
            }
            "export" => {
                let path = request.path.clone().ok_or_else(|| {
                    "job.submit 'export' needs path (the file to write)".to_owned()
                })?;
                if std::path::Path::new(&path).is_relative() {
                    return Err(format!(
                        "'{path}' is not an absolute path; pass the full path to write"
                    ));
                }
                jobs.export(&self.app, expected, path, request.operation_id)?
            }
            "inspect" => jobs.inspect(&self.app, expected, request.operation_id)?,
            other => {
                return Err(format!(
                    "unknown job operation '{other}'; use import, export or inspect"
                ));
            }
        };
        serde_json::to_value(splatmcp_bridge::JobAdmissionReply {
            job_id: admission.job_id.to_string(),
            state: admission.state.as_str().to_owned(),
            replayed: admission.replayed,
            limits: jobs.service().limits().describe(),
        })
        .map_err(|error| error.to_string())
    }

    /// One job's status and the log lines after a cursor.
    fn job_status(&self, params: Value) -> Result<Value, String> {
        let request: splatmcp_bridge::JobStatusRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid job.status request: {error}"))?;
        let jobs = self.app.state::<crate::jobs::JobHostState>().0.clone();
        let job_id = crate::jobs::parse_job_id(&request.job_id)?;
        let view = jobs
            .view(
                &job_id,
                request.log_after.unwrap_or(0),
                request.log_limit.unwrap_or(200).min(500),
            )
            .map_err(|error| format!("{} ({})", error, error.code()))?;
        serde_json::to_value(splatmcp_bridge::JobStatusReply::from(&view))
            .map_err(|error| error.to_string())
    }

    /// The newest jobs with the service's counts and limits.
    fn job_list(&self, params: Value) -> Result<Value, String> {
        let request: splatmcp_bridge::JobListRequest = if params.is_null() {
            splatmcp_bridge::JobListRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid job.list request: {error}"))?
        };
        let jobs = self.app.state::<crate::jobs::JobHostState>().0.clone();
        let stats = jobs.service().stats();
        let reply = splatmcp_bridge::JobListReply {
            jobs: jobs
                .recent(request.limit.unwrap_or(10).min(64))
                .iter()
                .map(splatmcp_bridge::JobSummary::from)
                .collect(),
            statistics: splatmcp_bridge::JobStatsSummary::from(&stats),
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    /// Asks a job to stop and reports what actually happened.
    fn job_cancel(&self, params: Value) -> Result<Value, String> {
        let request: splatmcp_bridge::JobCancelRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid job.cancel request: {error}"))?;
        let jobs = self.app.state::<crate::jobs::JobHostState>().0.clone();
        let job_id = crate::jobs::parse_job_id(&request.job_id)?;
        let receipt = jobs
            .cancel(&job_id)
            .map_err(|error| format!("{} ({})", error, error.code()))?;
        serde_json::to_value(splatmcp_bridge::JobSummary::from(&receipt))
            .map_err(|error| error.to_string())
    }

    /// Which revision the viewer is showing, and which publication is in flight.
    ///
    /// Deliberately answers from the publication tracker rather than from the viewer: the app
    /// knows what it announced and what was acknowledged, and a status query must not depend on
    /// the renderer being able to answer right now.
    fn publication_status(&self, params: Value) -> Result<Value, String> {
        let request: splatmcp_bridge::PublicationStatusRequest = if params.is_null() {
            splatmcp_bridge::PublicationStatusRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid publication.status request: {error}"))?
        };
        let publications = self
            .app
            .state::<crate::publication::PublicationHostState>()
            .0
            .clone();
        let document_id = match request.document_id {
            Some(document_id) => document_id,
            // Absent means "the displayed document", which the app resolves once, here, and
            // reports back - rather than leaving the caller to guess.
            None => {
                let state = self.app.state::<AppState>();
                match state.active_handle() {
                    Some(handle) => handle.document_id.to_string(),
                    None => {
                        return Err(
                            "no splat is loaded, so no publication can be described".to_owned()
                        );
                    }
                }
            }
        };
        let status = publications
            .status(&document_id)
            .map_err(|error| format!("{} ({})", error, error.code()))?
            .ok_or_else(|| {
                format!("no publication has been started for {document_id} in this session")
            })?;
        serde_json::to_value(splatmcp_bridge::PublicationStatusReply::from(&status))
            .map_err(|error| error.to_string())
    }

    /// What the renderer can do, with the viewer's own report of what it is showing.
    fn publication_capabilities(&self, params: Value) -> Result<Value, String> {
        let _ = params;
        let publications = self
            .app
            .state::<crate::publication::PublicationHostState>()
            .0
            .clone();
        // The viewer's own status, so the capabilities describe the real renderer rather than
        // an assumption about it. A viewer that cannot answer is reported as not ready.
        let reported = self
            .viewer
            .request(Method::ViewerStatus, Value::Null, VIEWER_TIMEOUT)
            .ok()
            .and_then(|value| serde_json::from_value::<ViewerStatus>(value).ok());
        let (displayed_revision, point_count, viewer_ready) = match reported {
            Some(status) => (
                status.document.as_ref().map(|document| document.revision),
                status.point_count,
                status.viewer_ready,
            ),
            None => (None, 0, false),
        };
        let capabilities = publications.capabilities(displayed_revision, point_count);
        let mut reply = splatmcp_bridge::PublicationCapabilitiesReply::from(&capabilities);
        reply.viewer_ready = viewer_ready;
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }
}

/// Event the app emits when an edit transaction committed a revision that should be shown.///
/// Deliberately separate from the Python job's `splat://revision`: a transaction is not a job,
/// and a viewer acknowledgement of a job's revision must not be confused with an edit.
pub const EDIT_REVISION_EVENT: &str = "splat://edit-revision";

/// Event the app emits when a selection is resolved, so the window can highlight it.
///
/// A selection made over MCP is the same selection the window shows: the payload carries the
/// handle the app retained plus the revision it was resolved against, and the viewer draws the
/// gaussians that handle names.
pub const SELECTION_EVENT: &str = "splat://selection";

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
///
/// `assets` resolves `merge.asset_id` (points from a registered payload) and `patch`
/// (decoded binary values). Both are resolved and validated here, before the transaction
/// starts, so a bad payload is a request error rather than a failed commit.
pub(crate) fn to_batch(
    request: &EditBatchRequest,
    assets: &AssetHost,
) -> Result<EditBatch, String> {
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
                points: merge_points(op, index, assets)?,
            },
            "patch" => EditOp::Patch {
                patch: std::sync::Arc::new(plan_patch(op, index, assets)?),
            },
            other => {
                return Err(format!(
                    "step {index}: unknown operation '{other}'; use translate, rotate, scale, \
                     set_radius, adjust_color, set_color, set_opacity, duplicate, remove, merge \
                     or patch"
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

/// The gaussians a `merge` step appends: from a registered asset, or from inline points.
///
/// The two forms are mutually exclusive on purpose: sending both would leave the caller
/// unsure which one the merge used.
fn merge_points(
    op: &BatchOpParams,
    index: usize,
    assets: &AssetHost,
) -> Result<Vec<SplatPoint>, String> {
    match (&op.asset_id, op.points.is_empty()) {
        (Some(asset_id), true) => Ok(assets.merge_points(asset_id)?.points),
        (Some(_), false) => Err(format!(
            "step {index} (merge): give asset_id or points, not both"
        )),
        (None, _) => op
            .points
            .iter()
            .map(to_point)
            .collect::<Result<Vec<_>, _>>(),
    }
}

/// The typed binary patch a `patch` step applies, planned before the transaction starts.
///
/// The row count is taken from the step's own selection when that selection is a simple
/// prefix (`first`), which is the case a caller can state without knowing the document. Any
/// other selection is resolvable only when the step runs, so the payload's own row count is
/// used and a mismatch is reported by the transaction, before anything is committed.
fn plan_patch(
    op: &BatchOpParams,
    index: usize,
    assets: &AssetHost,
) -> Result<splatmcp_core::AttributePatch, String> {
    let params = op
        .patch
        .as_ref()
        .ok_or_else(|| format!("step {index} (patch) needs patch"))?;
    let rows = op
        .selection
        .as_ref()
        .and_then(|selection| selection.first)
        .filter(|_| {
            // Only a prefix selection has a row count that does not depend on the document.
            op.selection.as_ref().is_some_and(|selection| {
                selection.within.is_none()
                    && selection.outside.is_none()
                    && selection.sphere.is_none()
                    && selection.component.is_none()
                    && selection.point_ids.is_empty()
                    && selection.selection_handle.is_none()
                    && selection.color_min.is_none()
                    && selection.color_max.is_none()
                    && selection.opacity_min.is_none()
                    && selection.max_radius.is_none()
            })
        });
    assets.plan_patch(params, rows)
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

/// Where a load's authoring metadata may live, if anywhere.
///
/// The caller's explicit source path comes first: it is the file the bytes were read from, which
/// need not be anywhere near the app's working directory - a basename is a label, not a location.
/// A registered asset names the file it was snapshotted from. A bare display name is accepted only
/// when it happens to be a real file, which is the legacy request shape; it is never a guess that
/// some file with that name is the right neighbour.
pub(crate) fn authoring_source(
    request: &LoadPlyRequest,
    asset_file: Option<&str>,
) -> Option<String> {
    request
        .source_path
        .clone()
        .filter(|path| !path.trim().is_empty())
        .or_else(|| asset_file.map(str::to_owned))
        .or_else(|| {
            request
                .file_name
                .clone()
                .filter(|name| std::path::Path::new(name).is_file())
        })
        .filter(|path| std::path::Path::new(path).is_file())
}

/// A document summary carrying the one-shot authoring note the app has waiting for it.
///
/// The note is *taken*, not read: a restore (or a refusal) is reported on the reply that opens
/// the document, and a later reply does not repeat a warning about something already known.
fn summary_of(state: &AppState, metadata: &document::DocumentMetadata) -> DocumentSummary {
    let note = state.take_authoring_note(&metadata.handle);
    DocumentSummary::from(metadata).with_authoring(note)
}

/// Restores the authoring metadata that describes exactly these bytes, or reports why not.
///
/// One implementation for every open path, so a load from MCP restores the same way the native
/// dialog does: association is by content (the artifact checksum and the gaussian count), and
/// the outcome is a note the reply carries.
pub(crate) fn restore_note(
    state: &AppState,
    path: &std::path::Path,
    imported: &document::ImportedDocument,
    bytes: &[u8],
) -> Option<splatmcp_bridge::AuthoringNote> {
    let checksum = document::ArtifactChecksum::of(bytes);
    let artifact = format!("{}:{}", checksum.algorithm, checksum.hex());
    match crate::authoring::lookup_for_content(path, &artifact, imported.metadata.point_count) {
        crate::authoring::SidecarLookup::Absent => None,
        crate::authoring::SidecarLookup::Refused(reason) => {
            eprintln!(
                "splatmcp: ignoring the authoring sidecar of {}: {reason}",
                path.display()
            );
            Some(splatmcp_bridge::AuthoringNote {
                status: "refused".to_owned(),
                message: format!("authoring metadata was not attached: {reason}"),
                components: 0,
                members: 0,
            })
        }
        crate::authoring::SidecarLookup::Attached(record) => {
            let mut set = splatmcp_core::components::AuthoringSet::new(
                Some(imported.metadata.handle.document_id.clone()),
                imported.metadata.handle.revision,
                imported.metadata.point_count,
            );
            let summary = crate::authoring::restore(&record, &mut set);
            match state.install_authoring(&imported.metadata.handle, set) {
                Ok(components) => Some(splatmcp_bridge::AuthoringNote {
                    status: "restored".to_owned(),
                    message: format!(
                        "{} (from {} of document {} revision {})",
                        summary.describe(),
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        record.document_id,
                        record.revision
                    ),
                    components,
                    members: summary.members,
                }),
                Err(error) => Some(splatmcp_bridge::AuthoringNote {
                    status: "refused".to_owned(),
                    message: format!("authoring metadata was not attached: {error}"),
                    components: 0,
                    members: 0,
                }),
            }
        }
    }
}

impl AppBridge {
    /// Tells the viewer to load one exact revision, and reports what that proved.
    ///
    /// The event names the document, the revision **and the publication request token**, and
    /// each publication is recorded on the tracker *before* it is emitted. A successful
    /// emission proves only that the announcement left the app: it is reported as
    /// [`SideEffect::Published`], and only the window's acknowledgement of that token turns it
    /// into `done`. That is why this returns an outcome instead of claiming a render.
    fn publish(
        &self,
        document: &splatmcp_core::ReceiptDocument,
        component_id: Option<String>,
        frame: bool,
    ) -> SideEffect {
        let handle = document.handle();
        let publications = self.app.state::<PublicationHostState>().0.clone();
        // A newer publication supersedes anything still in flight: the superseded request is
        // recorded as skipped, so it can never later be acknowledged as displayed.
        let request = match publications.begin(&handle, PublicationSource::Committed, frame) {
            Ok(request) => request,
            // Re-publishing the revision already on screen is not a failure: there is simply
            // nothing to announce.
            Err(error) if error.code() == "already_displayed" => {
                return SideEffect::Done;
            }
            Err(error) => return SideEffect::Failed(error.to_string()),
        };
        let payload = RevisionPayload {
            revision: document.revision,
            document_id: document.document_id.to_string(),
            token: request.token,
            source: request.source.as_str().to_owned(),
            file_name: document.file_name.clone(),
            point_count: document.point_count,
            component_id,
            frame,
        };
        match self
            .app
            .emit_to(VIEWER_WINDOW, EDIT_REVISION_EVENT, payload)
        {
            Ok(()) => SideEffect::Published,
            Err(error) => {
                let _ = publications.fail(
                    request.document_id.as_str(),
                    request.revision,
                    format!("could not tell the viewer: {error}"),
                );
                SideEffect::Failed(format!(
                    "could not tell the viewer about {}@{}: {error}",
                    document.document_id, document.revision
                ))
            }
        }
    }

    /// Records the outcome of publication on the revision's receipt.
    fn note_display(&self, handle: &splatmcp_core::DocumentHandle, side: &SideEffect) {
        if side.is_requested() {
            let state = self.app.state::<AppState>();
            state.note_side_effect(handle, ReceiptSlot::Display, side.clone());
        }
    }

    /// Exports the revision the receipt produced, when the caller asked for a file.
    ///
    /// Export happens after the commit and is recorded separately, so a failed write is reported
    /// as a failed export rather than turning a committed edit into a retryable one.
    fn record_export(
        &self,
        handle: &splatmcp_core::DocumentHandle,
        path: Option<&str>,
    ) -> Option<OutcomeInfo> {
        let path = path?.trim();
        if path.is_empty() {
            return None;
        }
        let state = self.app.state::<AppState>();
        let outcome = state
            .export_revision(handle, std::path::Path::new(path))
            .unwrap_or_else(|error| SideEffect::Failed(error.to_string()));
        Some(OutcomeInfo::of(&outcome))
    }

    /// Runs an edit batch, or dry-runs it when `dry_run` is set.
    pub(crate) fn document_edit_batch(&self, params: Value) -> Result<Value, String> {
        let request: EditBatchRequest = if params.is_null() {
            EditBatchRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid edit_batch request: {error}"))?
        };
        let batch = to_batch(&request, &self.assets)?;
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
        let handle = outcome.handle().ok_or_else(|| {
            "the app recorded an unreadable document id for this commit".to_owned()
        })?;
        if !outcome.replayed {
            // A replay already did all of this: re-exporting or re-announcing it would act on
            // state the original request never produced.
            if let Some(export) = self.record_export(&handle, request.export_path.as_deref()) {
                outcome.export = export;
            }
            let side = match display {
                true => {
                    let component = state
                        .metadata()
                        .filter(|metadata| metadata.handle == handle)
                        .and_then(|metadata| metadata.provenance.component_id.clone());
                    self.publish(&outcome.recorded(), component, false)
                }
                false => SideEffect::NotRequested,
            };
            self.note_display(&handle, &side);
            outcome.display = OutcomeInfo::of(&side);
        }
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
            .commit_preview(request.preview_id, expected, request.operation_id.clone())
            .map_err(transaction_message)?;
        if !outcome.replayed {
            let handle = outcome.handle().ok_or_else(|| {
                "the app recorded an unreadable document id for this commit".to_owned()
            })?;
            let side = match request.display.unwrap_or(true) {
                true => self.publish(&outcome.recorded(), None, false),
                false => SideEffect::NotRequested,
            };
            self.note_display(&handle, &side);
            outcome.display = OutcomeInfo::of(&side);
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
        if let Some(handle) = outcome.handle() {
            let side = match request.display.unwrap_or(true) {
                true => self.publish(&outcome.recorded(), None, false),
                false => SideEffect::NotRequested,
            };
            self.note_display(&handle, &side);
            outcome.display = OutcomeInfo::of(&side);
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
                // Tell the window which gaussians were selected, so a selection made over MCP is
                // visible there too: the ids and the revision are the same ones this reply carries.
                let _ = self.app.emit_to(
                    VIEWER_WINDOW,
                    SELECTION_EVENT,
                    json!({
                        "handle_id": resolved.handle_id,
                        "revision": resolved.revision,
                        "document_id": resolved.document.document_id,
                        "count": resolved.count,
                    }),
                );
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
        let assets = app.state::<crate::assets::AssetHostState>().0.clone();
        Self {
            app: app.clone(),
            viewer,
            python,
            assets,
            started: Instant::now(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// The wire shape of a committed batch, with side effects kept separate.
    ///
    /// Every field comes from the receipt, and the document summary is the *recorded* one: a
    /// replay must never be dressed up with whatever the app happens to display now.
    fn edit_batch_reply(
        &self,
        outcome: &document::BatchOutcome,
        preview: Option<PreviewSummary>,
    ) -> Result<Value, String> {
        let state = self.app.state::<AppState>();
        let reply = EditBatchReply {
            document: outcome.summary(),
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
    assets: Arc<AssetHost>,
) -> Result<BridgeHost, String> {
    let server = BridgeServer::bind().map_err(|error| error.to_string())?;
    let descriptor = server
        .publish(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("could not publish bridge.json: {error}"))?;
    let handler = Arc::new(AppBridge {
        app: app.clone(),
        viewer,
        python,
        assets,
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
        let assets = AssetHost::default();
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
        let batch = to_batch(&request, &assets).unwrap();
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
            to_batch(&unknown_op, &assets)
                .unwrap_err()
                .contains("unknown operation")
        );

        let bad_frame: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "remove", "selection": { "frame": "camera" } }]
        }))
        .unwrap();
        assert!(
            to_batch(&bad_frame, &assets)
                .unwrap_err()
                .contains("unknown frame")
        );

        let bad_id: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "remove", "selection": { "point_ids": ["row-3"] } }]
        }))
        .unwrap();
        assert!(
            to_batch(&bad_id, &assets)
                .unwrap_err()
                .contains("not a point id")
        );

        // A missing required field says which step and which field.
        let missing: EditBatchRequest = serde_json::from_value(json!({
            "steps": [{ "op": "rotate", "degrees": 90.0 }]
        }))
        .unwrap();
        let batch = to_batch(&missing, &assets).unwrap();
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
            to_batch(&missing_by, &assets)
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

    /// Saves a document with one component beside its PLY, the way `save_splat` does.
    fn saved_scene(
        directory: &std::path::Path,
    ) -> (std::path::PathBuf, AppState, splatmcp_core::ComponentId) {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join("scene.ply");
        let state = AppState::default();
        let splat = splatmcp_core::fixtures::axis_fixture();
        let bytes = document::ply_bytes(&splat).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        state
            .open_ply(
                &bytes,
                Mutation::open(path.to_string_lossy().to_string()),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        let created = state.create_component(Expected::Any, "hair").unwrap();
        let component = ComponentId::parse(&created.component_id).unwrap();
        state
            .set_component_members(
                Expected::Any,
                &component,
                &SelectionQuery {
                    within: Some(Box3::from_corners([-1.0, -0.2, -1.0], [1.0, 0.25, 1.0])),
                    ..SelectionQuery::all()
                },
            )
            .unwrap();
        // Export writes the PLY the sidecar must describe; then the sidecar itself.
        let outcome = state.export(Expected::Any, &path).unwrap();
        let (handle, layer) = state.authoring_snapshot(Expected::Any).unwrap();
        let record = crate::authoring::record(
            handle.document_id.as_str(),
            handle.revision,
            &outcome.checksum,
            layer.len(),
            &layer,
        );
        crate::authoring::write(&path, &record).unwrap();
        (path, state, component)
    }

    #[test]
    fn reopening_a_saved_file_restores_its_components_with_new_identities() {
        let directory =
            std::env::temp_dir().join(format!("splatmcp-reopen-{}", std::process::id()));
        let (path, saved, component) = saved_scene(&directory);
        let members = saved
            .components(Expected::Any)
            .unwrap()
            .components
            .iter()
            .find(|entry| entry.component_id == component.to_string())
            .unwrap()
            .point_count;
        assert!(members > 0);

        // A fresh app opens the same bytes: it has no components until the sidecar is restored.
        let fresh = AppState::default();
        let bytes = std::fs::read(&path).unwrap();
        let imported = fresh
            .open_ply(
                &bytes,
                Mutation::open(path.to_string_lossy().to_string()),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        assert!(
            fresh
                .components(Expected::Any)
                .unwrap()
                .components
                .is_empty()
        );

        let note = restore_note(&fresh, &path, &imported, &bytes).expect("a note");
        assert_eq!(note.status, "restored", "{note:?}");
        assert_eq!(note.components, 1);
        assert_eq!(note.members, members);
        assert!(note.message.contains("ids re-minted"), "{}", note.message);

        let restored = fresh.components(Expected::Any).unwrap();
        assert_eq!(restored.components.len(), 1);
        assert_eq!(restored.components[0].name, "hair");
        assert_eq!(restored.components[0].point_count, members);
        assert_ne!(
            restored.components[0].component_id,
            component.to_string(),
            "identities are re-minted for the new document"
        );
        // The note is reported once, on the reply that describes the document it belongs to:
        // a registered note is taken by the first summary, and not repeated afterwards.
        fresh.set_authoring_note(&imported.metadata.handle, note.clone());
        let summary = summary_of(&fresh, &imported.metadata);
        assert_eq!(
            summary.authoring.as_ref().map(|n| n.status.as_str()),
            Some("restored")
        );
        assert!(summary.authoring.unwrap().message.contains("hair") == false);
        assert!(summary_of(&fresh, &imported.metadata).authoring.is_none());

        std::fs::remove_dir_all(&directory).ok();
    }

    /// The fixtures the review produced, read the way the loader reads them.
    ///
    /// They live outside the repository, so the test reports that it could not check them rather
    /// than passing silently when they are absent. The pair is exactly the defect: one sidecar
    /// records the exported revision's 6 gaussians, the other the 864-byte file *size*, and the
    /// same 6-point PLY must restore from the first and be refused by the second.
    #[test]
    fn the_reviewed_fixtures_restore_or_are_refused_by_their_count() {
        let directory = std::path::Path::new(r"F:\_splat");
        // Both PLY files hold the same six gaussians; only what the sidecar beside each one
        // recorded differs. `authoring::read` derives the sidecar from the PLY path it is given.
        let ply = directory.join("tasks13-14-fixes-1789741426569-correct-count.ply");
        let native_ply = directory.join("tasks13-14-fixes-1789741426569-native-save-count.ply");
        if !ply.is_file() || !native_ply.is_file() {
            println!(
                "reviewed fixtures are not present at {}; skipping",
                directory.display()
            );
            return;
        }

        let bytes = std::fs::read(&ply).unwrap();
        let checksum = document::ArtifactChecksum::of(&bytes);
        let artifact = format!("{}:{}", checksum.algorithm, checksum.hex());
        let fresh = AppState::default();
        let imported = fresh
            .open_ply(
                &bytes,
                Mutation::open(ply.to_string_lossy().to_string()),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        assert_eq!(
            imported.metadata.point_count, 6,
            "the PLY holds six gaussians"
        );
        assert_ne!(
            imported.metadata.point_count,
            bytes.len(),
            "and that is not the file size, which is {}",
            bytes.len()
        );

        // The corrected record describes these bytes: it restores three components of two.
        let record = crate::authoring::read(&ply).unwrap().unwrap();
        assert!(record.content_association(&artifact, 6).is_none());
        let note =
            restore_note(&fresh, &ply, &imported, &bytes).expect("a matching sidecar attaches");
        assert_eq!(note.status, "restored", "{note:?}");
        assert_eq!(note.components, 3);
        assert_eq!(note.members, 6);
        let names: Vec<String> = fresh
            .components(Expected::Any)
            .unwrap()
            .components
            .iter()
            .map(|component| component.name.clone())
            .collect();
        assert_eq!(names, vec!["face", "hair", "sweater"]);

        // The record a pre-fix Save wrote claims the file size: refused, with that as the reason.
        let stale = crate::authoring::read(&native_ply).unwrap().unwrap();
        let reason = stale
            .content_association(&artifact, 6)
            .expect("a count that is a file size does not describe six gaussians");
        assert_eq!(
            reason,
            format!(
                "authoring metadata describes {} gaussians but this file holds 6",
                stale.point_count
            )
        );
        assert_eq!(stale.point_count, bytes.len());
    }

    #[test]
    fn a_sidecar_that_does_not_describe_these_bytes_is_reported_not_attached() {
        let directory = std::env::temp_dir().join(format!("splatmcp-stale-{}", std::process::id()));
        let (path, _saved, _component) = saved_scene(&directory);

        // The file changes after the metadata was written: same name, different content.
        let other = splatmcp_core::fixtures::rotated_fixture();
        let bytes = document::ply_bytes(&other).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let fresh = AppState::default();
        let imported = fresh
            .open_ply(
                &bytes,
                Mutation::open(path.to_string_lossy().to_string()),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        let note = restore_note(&fresh, &path, &imported, &bytes).expect("a note");
        assert_eq!(note.status, "refused");
        assert_eq!(note.components, 0);
        assert!(
            note.message.contains("artifact"),
            "the reason names what did not match: {}",
            note.message
        );
        assert!(
            fresh
                .components(Expected::Any)
                .unwrap()
                .components
                .is_empty(),
            "nothing is attached by file name"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_load_keeps_the_source_path_so_metadata_beside_the_file_is_found() {
        let directory =
            std::env::temp_dir().join(format!("splatmcp-source-path-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let ply = directory.join("scene.ply");
        std::fs::write(&ply, b"ply bytes").unwrap();

        // A caller that read a file says where it read it: the path survives, and the display
        // name is only a label beside it.
        let request = LoadPlyRequest {
            ply_base64: String::new(),
            asset_id: None,
            file_name: Some("scene.ply".to_owned()),
            source_path: Some(ply.to_string_lossy().to_string()),
            frame: None,
            document_id: None,
            expected_revision: None,
            repair: None,
        };
        assert_eq!(
            authoring_source(&request, None).as_deref(),
            Some(ply.to_string_lossy().as_ref())
        );

        // A bare display name that is not a file here is not treated as one.
        let mut bare = request.clone();
        bare.source_path = None;
        bare.file_name = Some("elsewhere.ply".to_owned());
        assert_eq!(authoring_source(&bare, None), None);

        // The legacy shape - a name that happens to be a real file - still works.
        let mut named = plus_file(&directory);
        named.source_path = None;
        assert!(authoring_source(&named, None).is_some());

        // A registered asset names the file it was snapshotted from.
        assert_eq!(
            authoring_source(
                &LoadPlyRequest::default(),
                Some(ply.to_string_lossy().as_ref())
            )
            .as_deref(),
            Some(ply.to_string_lossy().as_ref())
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// A request whose display name is a real file, for the legacy shape.
    fn plus_file(directory: &std::path::Path) -> LoadPlyRequest {
        let ply = directory.join("scene.ply");
        LoadPlyRequest {
            file_name: Some(ply.to_string_lossy().to_string()),
            ..LoadPlyRequest::default()
        }
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
