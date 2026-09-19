//! SplatMCP MCP server library.
//!
//! The server exposes the SplatMCP tool surface over stdio and drives the desktop app
//! through the loopback bridge (`splatmcp_bridge`). Tools that build or edit a splat use
//! `splatmcp_core`; tools that show or capture one go through [`bridge::AppLink`].
//!
//! Every tool reply is compact JSON: three decimals, no indentation, and no field that
//! merely repeats an argument.

pub mod app_launch;
pub mod bridge;
pub mod contract;
pub mod tools;

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_router,
};
use serde_json::{Value, json};

use bridge::AppLink;
use splatmcp_bridge::Method;
use splatmcp_core::PlyImportPolicy;
use contract::{Envelope, ErrorLayer, Failure, ToolOutput};
use tools::{asset, author, contract as contract_tools, edit, job, publication, python, viewer};

/// Shared state of the MCP service: the link to the desktop app.
#[derive(Clone)]
pub struct SplatMcpServer {
    link: Arc<AppLink>,
}

impl Default for SplatMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl SplatMcpServer {
    /// A server that may start the desktop app when it is not running.
    pub fn new() -> Self {
        Self {
            link: Arc::new(AppLink::default()),
        }
    }

    /// A server that never starts the app, for tests and for embedding.
    pub fn without_launching() -> Self {
        Self {
            link: Arc::new(AppLink::new(false)),
        }
    }

    /// The link to the desktop app.
    pub fn link(&self) -> Arc<AppLink> {
        self.link.clone()
    }
}

#[tool_router(router = tool_router, vis = "pub")]
impl SplatMcpServer {
    /// Server status plus whether the desktop app is reachable.
    #[tool(
        description = "Report SplatMCP status: server version and whether the desktop app is attached.",
        annotations(
            title = "Server status",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn splatmcp_status(&self) -> Result<CallToolResult, McpError> {
        let attached = self.link.attached();
        let (app, note) = match attached {
            Some((pid, version)) => (
                json!({"attached": true, "pid": pid, "version": version}),
                "The desktop app is attached.",
            ),
            None => (
                json!({"attached": false}),
                "No desktop app is attached yet; the next viewer tool call starts one.",
            ),
        };
        Ok(tool_text(json!({
            "server_version": env!("CARGO_PKG_VERSION"),
            "bridge_protocol": splatmcp_bridge::PROTOCOL_VERSION,
            "app": app,
            "note": note,
        })))
    }

    /// Moves the camera of the desktop app.
    #[tool(
        description = "Move the camera in the window and return where it ended up. Give fit=true \
                       to frame the whole splat, a position, or azimuth/elevation/distance to \
                       orbit it.",
        annotations(
            title = "Set camera",
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_camera(
        &self,
        Parameters(camera): Parameters<viewer::CameraInput>,
    ) -> Result<CallToolResult, McpError> {
        let state = viewer::set_camera(&self.link, camera).map_err(tool_error)?;
        tool_json(&state)
    }

    /// Reads the camera of the desktop app.
    #[tool(
        description = "Read the displayed splat's camera: position, look-at target and field of view.",
        annotations(title = "Get camera", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_camera(&self) -> Result<CallToolResult, McpError> {
        let state = viewer::get_camera(&self.link).map_err(tool_error)?;
        tool_json(&state)
    }

    /// Builds a splat from parameters and shows it in the app.
    #[tool(
        description = "Create a gaussian splat from a shape (sphere, cube, plane, line, shell, \
                       ring, grid) or explicit points, in fixed RGB colour. Optionally writes a \
                       .ply and shows it. Returns the point count and bounds.",
        annotations(
            title = "Create splat",
            read_only_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_splat(
        &self,
        Parameters(input): Parameters<author::CreateInput>,
    ) -> Result<CallToolResult, McpError> {
        let splat = author::build_splat(&input).map_err(tool_error)?;
        let bytes =
            splatmcp_core::write_ply(&splat).map_err(|error| tool_error(error.to_string()))?;

        let path = match input.path.as_deref() {
            Some(path) => Some(author::write_splat_file(path, &bytes).map_err(tool_error)?),
            None => None,
        };

        let display = input.display.unwrap_or(true);
        let mut displayed = false;
        let mut status = None;
        if display {
            let file_name = path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("splat.ply")
                .to_owned();
            // A created splat is a new document: nothing was edited, so nothing is replaced.
            status = Some(
                author::display_splat(&self.link, &file_name, &bytes, None, None)
                    .map_err(tool_error)?,
            );
            displayed = true;
        }

        tool_json(&author::splat_reply(
            &splat,
            path.as_deref(),
            displayed,
            status.as_ref(),
        ))
    }

    /// Applies edit steps to a splat.
    #[tool(
        description = "Edit a gaussian splat: translate, rotate, scale, set_radius, adjust_color, \
                       set_color, set_opacity, duplicate, remove, merge or patch, each with an \
                       optional box, sphere, attribute, component, point-id or saved-selection \
                       target. The displayed document is edited through the edit_batch \
                       transaction (stable ids, one revision, components and undo preserved). A \
                       .ply or 'new' source is a detached buffer and refuses document-only \
                       targets.",
        annotations(title = "Edit splat", read_only_hint = false, open_world_hint = false)
    )]
    async fn edit_splat(
        &self,
        Parameters(input): Parameters<edit::EditInput>,
    ) -> Result<CallToolResult, McpError> {
        // The displayed document is edited *in the app*, through the shared transaction service:
        // that is the only place its component membership, point identities, history and
        // receipts live, so an edit routed anywhere else would quietly discard all of them.
        if edit::is_displayed_source(input.source.as_deref()) {
            let request = edit::edit_batch_request(&input).map_err(tool_error)?;
            let params =
                serde_json::to_value(&request).map_err(|error| tool_error(error.to_string()))?;
            let reply = self
                .link
                .request(Method::DocumentEditBatch, params)
                .map_err(tool_error)?;
            return Ok(tool_text(reply));
        }

        // A file source is imported under the policy the caller chose: strict by default,
        // so a damaged file is refused with indexed diagnostics instead of being edited as
        // if it were intact. The report travels back in the reply.
        let policy = PlyImportPolicy::from_repair_flag(input.repair);
        let resolved = edit::resolve_source(&self.link, input.source.as_deref(), policy)
            .map_err(tool_error)?;
        let import = resolved.import.clone();
        let mut splat = resolved.splat;
        let target = resolved.document.clone();
        let steps = edit::apply_edits(&mut splat, &input.ops).map_err(tool_error)?;
        let outcome = edit::save_and_display(
            &self.link,
            &splat,
            input.path.as_deref(),
            input.display.unwrap_or(true),
            target.as_ref(),
        )
        .map_err(tool_error)?;
        let reply = edit::edit_reply(
            &splat,
            steps,
            outcome.path,
            outcome.displayed,
            outcome.document,
        );
        tool_json(&edit::with_import(reply, import))
    }

    /// Shows an existing PLY file, or a registered asset, in the app.
    #[tool(
        description = "Load a .ply gaussian splat into the window and frame it, by path or by a \
                       registered asset_id (the compact form for large files). Strict: a file \
                       that needs repair is refused with indexed diagnostics, unless repair:true \
                       accepts it and the reply reports every change.",
        annotations(
            title = "Load splat",
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn load_splat(
        &self,
        Parameters(input): Parameters<edit::LoadInput>,
    ) -> Result<CallToolResult, McpError> {
        // A registered asset is loaded by the app, which owns the bytes: the reply is the
        // app's own report of the identity, count and import it resolved - nothing is
        // re-encoded here, and no large payload crosses the bridge.
        if let Some(asset_id) = input.asset_id.as_deref() {
            if input.path.is_some() {
                return Err(tool_error(
                    "load_splat takes path or asset_id, not both: a load has one source".to_owned(),
                ));
            }
            let status = edit::display_asset(&self.link, asset_id).map_err(tool_error)?;
            return tool_json(&status);
        }
        let path = input.path.as_deref().ok_or_else(|| {
            tool_error(
                "load_splat needs path (.ply file) or asset_id (registered asset)".to_owned(),
            )
        })?;
        // Strict by default: a file that needs repair is refused with indexed diagnostics,
        // and `repair: true` accepts it and reports every value that was changed.
        let policy = PlyImportPolicy::from_repair_flag(input.repair);
        let file = edit::read_splat_file(path, policy).map_err(tool_error)?;
        let import = splatmcp_bridge::PlyImportSummary::of(&file.report);
        let splat = file.splat;
        let bytes =
            splatmcp_core::write_ply(&splat).map_err(|error| tool_error(error.to_string()))?;
        let source = std::path::Path::new(path);
        let file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned();
        // Loading a file opens a document: the file is provenance, not identity. The *path* goes
        // with the request, because component metadata lives beside that file - the app cannot
        // find it from a display name, and the file need not be anywhere near the app.
        let status = author::display_splat(&self.link, &file_name, &bytes, Some(source), None)
            .map_err(tool_error)?;
        tool_json(
            &author::splat_reply(&splat, Some(source), true, Some(&status)).with_import(import),
        )
    }

    /// Registers a payload as an immutable asset the app holds.
    #[tool(
        description = "Register a payload as an immutable asset and get its asset_id back. Give \
                       an absolute path (snapshotted once, so editing the file afterwards cannot \
                       change queued work) or bytes_base64 for a small inline payload; kind is \
                       ply, splat_buffers or attribute_patch. Use the id in load_splat, a merge \
                       step or a patch. Refusals name what is wrong.",
        annotations(
            title = "Register asset",
            read_only_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn register_asset(
        &self,
        Parameters(input): Parameters<asset::RegisterAssetInput>,
    ) -> Result<CallToolResult, McpError> {
        let reply = asset::register(&self.link, &input).map_err(tool_error)?;
        tool_json(&reply)
    }

    /// Describes, lists or releases registered assets.
    #[tool(
        description = "Describe one registered asset by asset_id (kind, schema, bytes, checksum, \
                       provenance, gaussian count, lifetime), list every live asset when no id \
                       is given, or forget one with release:true. An asset is addressed by id, \
                       never by a path.",
        annotations(
            title = "Asset info",
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn asset_info(
        &self,
        Parameters(input): Parameters<asset::AssetInfoInput>,
    ) -> Result<CallToolResult, McpError> {
        let value = asset::info(&self.link, &input).map_err(tool_error)?;
        Ok(tool_text(value))
    }

    /// Applies an edit batch as one atomic, previewable and retry-safe transaction.
    #[tool(
        description = "Apply several edit steps as ONE transaction: all commit as one new \
                       revision or nothing changes. dry_run reports a preview without committing; \
                       commit it later with preview_id (refused if the document moved on). \
                       operation_id makes a retry safe; undo, redo and history are shared with \
                       the window. background:true runs the same batch as a job and returns a \
                       job_id to poll.",
        annotations(title = "Edit batch", read_only_hint = false, open_world_hint = false)
    )]
    async fn edit_batch(
        &self,
        Parameters(input): Parameters<edit::EditBatchInput>,
    ) -> Result<CallToolResult, McpError> {
        match edit::batch_call(&input).map_err(tool_error)? {
            edit::BatchCall::Batch(request) => {
                let params = serde_json::to_value(&request)
                    .map_err(|error| tool_error(error.to_string()))?;
                let reply = self
                    .link
                    .request(Method::DocumentEditBatch, params)
                    .map_err(tool_error)?;
                Ok(tool_text(reply))
            }
            edit::BatchCall::Background(request) => {
                // The same batch, submitted as a job: the app admits it at once and the caller
                // polls the job, which reports progress, logs and the same receipt. The batch
                // still commits through the one transaction service, so this is not a second
                // edit path - only a second way to wait for it.
                let params = json!({
                    "operation": "edit",
                    "document_id": request.document_id,
                    "expected_revision": request.expected_revision,
                    "operation_id": request.operation_id,
                    "display": request.display,
                    "steps": request.steps,
                });
                let reply = self
                    .link
                    .request(Method::JobSubmit, params)
                    .map_err(tool_error)?;
                let mut reply = reply;
                if let Some(object) = reply.as_object_mut() {
                    object.insert(
                        "note".to_owned(),
                        json!(
                            "the batch is running as a job; poll document_job with action:status \
                             and this job_id for its progress, receipt and logs"
                        ),
                    );
                }
                Ok(tool_text(reply))
            }
            edit::BatchCall::CommitPreview(request) => {
                let params = serde_json::to_value(&request)
                    .map_err(|error| tool_error(error.to_string()))?;
                let reply = self
                    .link
                    .request(Method::DocumentCommitPreview, params)
                    .map_err(tool_error)?;
                Ok(tool_text(reply))
            }
        }
    }

    /// Reports, undoes or redoes the newest edit step of the displayed document.
    #[tool(
        description = "Edit history of the displayed document: action 'status' reports undo and \
                       redo availability plus the retained steps, 'undo' restores the previous \
                       state and 'redo' re-applies it. Each commits a NEW revision.",
        annotations(
            title = "Edit history",
            read_only_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_history(
        &self,
        Parameters(input): Parameters<edit::HistoryInput>,
    ) -> Result<CallToolResult, McpError> {
        let action = input.action.clone().unwrap_or_else(|| "status".to_owned());
        let method = match action.as_str() {
            "status" => Method::DocumentHistory,
            "undo" => Method::DocumentUndo,
            "redo" => Method::DocumentRedo,
            other => {
                return Err(tool_error(format!(
                    "unknown action '{other}'; use status, undo or redo"
                )));
            }
        };
        let params = serde_json::to_value(edit::history_target(&input))
            .map_err(|error| tool_error(error.to_string()))?;
        let reply = self.link.request(method, params).map_err(tool_error)?;
        Ok(tool_text(reply))
    }

    /// Lists, creates, renames, removes or reframes named components, and resolves selections.
    #[tool(
        description = "Named components and stable selections of the displayed document. Actions: \
                       list, create, rename, remove, transform (declares a local frame; \
                       anisotropic gaussians are transformed through their covariance and \
                       singular or reflecting frames are refused), members, apply_transform and \
                       select (count, bounds, bounded sample). These ids are the window's ids.",
        annotations(
            title = "Splat components",
            read_only_hint = false,
            open_world_hint = false
        )
    )]
    async fn splat_components(
        &self,
        Parameters(input): Parameters<edit::ComponentsInput>,
    ) -> Result<CallToolResult, McpError> {
        let request = edit::components_request(&input).map_err(tool_error)?;
        let params =
            serde_json::to_value(&request).map_err(|error| tool_error(error.to_string()))?;
        let reply = self
            .link
            .request(Method::DocumentComponents, params)
            .map_err(tool_error)?;
        Ok(tool_text(reply))
    }

    /// Describes a splat: count, bounds, colour and opacity.
    #[tool(
        description = "Describe a gaussian splat: point count, bounds, mean colour, opacity \
                       range, distributions, contract diagnostics, buffer sizes, and the \
                       displayed document's id and revision. Reads the displayed document as \
                       bounded metadata by default, or a .ply path; set points for the first n \
                       gaussians themselves.",
        annotations(title = "Splat info", read_only_hint = true, open_world_hint = false)
    )]
    async fn splat_info(
        &self,
        Parameters(input): Parameters<edit::InfoInput>,
    ) -> Result<CallToolResult, McpError> {
        // The displayed document is inspected where it lives: no PLY is serialised and no
        // point array crosses the bridge. A sample, or a file path, needs the gaussians
        // and takes the read-the-PLY path as before.
        if edit::wants_bounded_inspection(input.source.as_deref(), input.points) {
            match edit::inspect_displayed(&self.link) {
                edit::InspectOutcome::Summary(result) => {
                    return tool_json(&edit::info_reply_from_inspect(*result));
                }
                edit::InspectOutcome::NoDocument(message) => return Err(tool_error(message)),
                edit::InspectOutcome::Unavailable => {}
            }
        }
        let policy = PlyImportPolicy::from_repair_flag(input.repair);
        let resolved = edit::resolve_source(&self.link, input.source.as_deref(), policy)
            .map_err(tool_error)?;
        let reply = edit::info_reply_with_document(
            &resolved.splat,
            resolved.source,
            input.points,
            resolved.document,
        );
        tool_json(&edit::info_reply_with_import(reply, resolved.import))
    }

    /// Reports readiness and versions of the app's embedded Python runtime.
    #[tool(
        description = "Read the SplatMCP Python runtime: readiness, interpreter and package \
                       versions, and the budgets jobs are held to.",
        annotations(
            title = "Python runtime info",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn python_runtime_info(&self) -> Result<CallToolResult, McpError> {
        let info = python::runtime_info(&self.link).map_err(tool_error)?;
        tool_json(&info)
    }

    /// Runs a Python recipe that generates or edits Gaussians in the app.
    #[tool(
        description = "Run an embedded-Python recipe that builds Gaussians with NumPy and shows \
                       the result in the window. Pass code or script_path plus a request_id: the \
                       reply is a job id (the app's shared job id, also in document_job's list), \
                       which get_python_job polls. display:false commits without changing what is \
                       shown; frame:false keeps the camera. Scripts are local code execution, not \
                       a sandbox.",
        annotations(
            title = "Run Python splat",
            read_only_hint = false,
            open_world_hint = true
        )
    )]
    async fn run_python_splat(
        &self,
        Parameters(input): Parameters<python::RunPythonInput>,
    ) -> Result<CallToolResult, McpError> {
        let receipt = python::run(&self.link, &input).map_err(tool_error)?;
        tool_json(&receipt)
    }

    /// Reads the state, logs and result identity of a Python job.
    #[tool(
        description = "Read a run_python_splat job from the app's shared job service: state, \
                       phase, percent, result, export, display, structured error and logs. Pass \
                       log_after (the previous reply's next_log_sequence) for only new lines. A \
                       committed job is not proof the viewer displayed it: check display.",
        annotations(
            title = "Get Python job",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_python_job(
        &self,
        Parameters(input): Parameters<python::JobQueryInput>,
    ) -> Result<CallToolResult, McpError> {
        let job = python::job(&self.link, &input).map_err(tool_error)?;
        tool_json(&job)
    }

    /// Asks a Python job to stop and reports the honest state.
    #[tool(
        description = "Ask a run_python_splat job to stop. A queued job stops immediately; a \
                       running one stops at its next checkpoint, so a native call can keep it \
                       unwinding - still_unwinding says so, and a job that had already committed \
                       stays committed.",
        annotations(
            title = "Cancel Python job",
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn cancel_python_job(
        &self,
        Parameters(input): Parameters<python::CancelJobInput>,
    ) -> Result<CallToolResult, McpError> {
        let cancelled = python::cancel(&self.link, &input).map_err(tool_error)?;
        tool_json(&cancelled)
    }

    /// Submits, reads, lists or cancels a long desktop operation as a job.
    #[tool(
        description = "Run a long operation as a background job. action:submit takes operation \
                       (import a registered asset_id, export to an absolute path, or inspect) and \
                       returns a job_id at once; status returns state, phase, percent, result and \
                       new log lines (pass log_after back); list returns recent jobs and the real \
                       limits; cancel is cooperative and honest. A dropped connection never \
                       cancels or resubmits the work.",
        annotations(
            title = "Document job",
            read_only_hint = false,
            open_world_hint = false
        )
    )]
    async fn document_job(
        &self,
        Parameters(input): Parameters<job::JobInput>,
    ) -> Result<CallToolResult, McpError> {
        let value = job::run(&self.link, &input).map_err(tool_error)?;
        Ok(tool_text(value))
    }

    /// Reports which revision the viewer is showing, or what the renderer can do.
    #[tool(
        description = "Which revision the window is actually showing. action:status returns \
                       committed_revision and displayed_revision as separate values, whether \
                       display is lagging behind the document, the publication still in flight \
                       with its request token, and which revisions were superseded; \
                       action:capabilities returns the renderer's transport, whether it is \
                       revision-addressed, and its acknowledgement timeout. A commit is not a \
                       display: this is how to tell them apart.",
        annotations(title = "Publication", read_only_hint = true, open_world_hint = false)
    )]
    async fn splat_display(
        &self,
        Parameters(input): Parameters<publication::PublicationInput>,
    ) -> Result<CallToolResult, McpError> {
        let value = publication::run(&self.link, &input).map_err(tool_error)?;
        Ok(tool_text(value))
    }

    /// Reports what this build supports and the limits it enforces.
    #[tool(
        description = "What this build supports and the limits you are held to: capture, Gaussian \
                       and retention budgets, presets, projections, formats, passes, workflow and \
                       unfilled gaps. Limits come from the components enforcing them.",
        annotations(title = "Capabilities", read_only_hint = true, open_world_hint = false)
    )]
    async fn splatmcp_capabilities(
        &self,
        Parameters(input): Parameters<contract_tools::CapabilitiesInput>,
    ) -> Result<CallToolResult, McpError> {
        match contract_tools::capabilities(&self.link, &input) {
            Ok(payload) => tool_envelope(contract_tools::envelope(payload), None),
            Err(failure) => tool_failure(failure),
        }
    }

    /// Captures one frame of one pinned revision, atomically.
    #[tool(
        description = "Capture ONE frame of one exact revision: pose, document and image belong to \
                       one operation. Give camera (pose/orbit/preset/fit), viewport, format and \
                       expected_revision; get the frame id, applied pose, matrices, checksum and \
                       restore outcome. Concurrent captures are refused by name.",
        annotations(
            title = "Capture view",
            read_only_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn capture_view(
        &self,
        Parameters(input): Parameters<contract_tools::CaptureViewInput>,
    ) -> Result<CallToolResult, McpError> {
        match contract_tools::capture_view(&self.link, &input) {
            Ok(outcome) => {
                let correlation = contract_tools::capture_correlation(&outcome.frame);
                let envelope = contract_tools::envelope(json!({
                    "capture": outcome.frame,
                    "note": "the frame is attached as image content when it was returned; \
                             metadata_only requests identity without the image",
                }))
                .with_correlation(correlation);
                let mut blocks = Vec::new();
                if !outcome.data_base64.is_empty() {
                    blocks.push(ContentBlock::image(
                        outcome.data_base64.clone(),
                        outcome.mime_type.clone(),
                    ));
                }
                blocks.push(ContentBlock::text(envelope.to_text()));
                Ok(CallToolResult::structured(envelope.to_value()).with_content(blocks))
            }
            Err(failure) => tool_failure(failure),
        }
    }

    /// Captures a marked set of views from one pinned revision.
    #[tool(
        description = "Capture a SET of labelled views (front, sides, rear) from ONE pinned \
                       revision: each reports a frame, checksum and camera, a failed view is marked \
                       failed rather than replaced, and an optional contact sheet composes what \
                       exists.",
        annotations(
            title = "Capture views",
            read_only_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn capture_views(
        &self,
        Parameters(input): Parameters<contract_tools::CaptureViewsInput>,
    ) -> Result<CallToolResult, McpError> {
        match contract_tools::capture_views(&self.link, &input) {
            Ok(payload) => tool_envelope(contract_tools::envelope(payload), None),
            Err(failure) => tool_failure(failure),
        }
    }

    /// Renders the current view and returns it as an image.
    #[tool(
        description = "Render the SplatMCP window and return the frame as an image, optionally \
                       after moving the camera. Give a small width to keep the reply cheap.",
        annotations(
            title = "Screenshot",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_screenshot(
        &self,
        Parameters(input): Parameters<viewer::CaptureInput>,
    ) -> Result<CallToolResult, McpError> {
        let capture = viewer::screenshot(&self.link, &input).map_err(tool_error)?;
        let summary = tool_json(&viewer::CaptureSummary::from(&capture))?;
        let mut blocks = vec![ContentBlock::image(
            BASE64.encode(&capture.bytes),
            capture.mime_type.clone(),
        )];
        if let Some(text) = summary.content.into_iter().next() {
            blocks.push(text);
        }
        Ok(CallToolResult::success(blocks))
    }
}

#[rmcp::tool_handler]
impl ServerHandler for SplatMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }
}

/// Wraps JSON as the single text block a tool returns.
pub(crate) fn tool_text(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(compact_json(&value))])
}

/// Serialises a typed reply and wraps it as the single text block a tool returns.
///
/// Passing a struct instead of a `Value` keeps `f32` fields at their shortest form:
/// widening through `serde_json::Value` would turn `0.01` into `0.009999999776482582`.
pub(crate) fn tool_json<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string(value).map_err(|error| {
        McpError::internal_error(format!("could not encode the reply: {error}"), None)
    })?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// Turns a failure into a tool error the model can act on.
///
/// The message is classified: it is deliberately **not** wrapped as `internal_error`, because a
/// stale revision, an unsupported mode or a timeout is not an internal failure and a client must be
/// able to tell them apart. The classified form rides in the error data, so a caller sees both the
/// code and the sentence that explains it.
pub(crate) fn tool_error(message: String) -> McpError {
    let failure = Failure::inferred(ErrorLayer::App, message);
    McpError::invalid_params(failure.to_text(), Some(failure.to_value()))
}

/// A tool reply that succeeded, carrying structured content and readable text.
pub(crate) fn tool_envelope(
    envelope: Envelope,
    extra: Option<Vec<ContentBlock>>,
) -> Result<CallToolResult, McpError> {
    let mut blocks = extra.unwrap_or_default();
    blocks.push(ContentBlock::text(envelope.to_text()));
    Ok(CallToolResult::structured(envelope.to_value()).with_content(blocks))
}

/// `CallToolResult::structured` already carries the JSON as text; a caller that adds blocks wants
/// them beside it, not instead of it.
trait WithContent {
    fn with_content(self, blocks: Vec<ContentBlock>) -> Self;
}

impl WithContent for CallToolResult {
    fn with_content(self, blocks: Vec<ContentBlock>) -> Self {
        let mut result = self;
        result.content.extend(blocks);
        result
    }
}

/// A tool reply that failed: an error result with structured content, not a protocol error.
///
/// MCP separates the two: a protocol error means the call never reached the tool, while a tool
/// execution error means it ran and refused. A refusal has to stay a tool execution error so the
/// client keeps the conversation, the correlation and the actionable fields.
pub(crate) fn tool_failure(failure: Failure) -> Result<CallToolResult, McpError> {
    let output = ToolOutput::failed(failure);
    let text = output.envelope.to_text();
    let structured = output.envelope.to_value();
    // An error *tool result*, not a protocol error: the call reached the tool and was refused, and
    // the client keeps the correlation and the actionable fields.
    let mut result = CallToolResult::structured_error(structured);
    result.content = vec![ContentBlock::text(text)];
    Ok(result)
}

/// Compact JSON: no indentation, so a tool reply stays small.
pub(crate) fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_replies_are_compact_json() {
        let result = tool_text(json!({ "point_count": 12, "max_opacity": 0.5 }));
        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|text| text.text.clone())
            .collect::<String>();
        assert_eq!(text, "{\"max_opacity\":0.5,\"point_count\":12}");
        assert!(!text.contains('\n'));
        assert!(!text.contains("  "));
    }

    #[test]
    fn the_server_reports_its_version_and_protocol() {
        let server = SplatMcpServer::without_launching();
        assert!(server.link().attached().is_none());
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
    }

    /// Largest listing budget per tool.
    ///
    /// A listing is sent to the model with every session, so its size is a real cost. The
    /// budget is per tool rather than absolute, so adding a tool does not need a new magic
    /// number, while a tool that is an order of magnitude larger than its peers - a schema
    /// that grew a point array, say - still fails the test.
    ///
    /// The value tracks the largest schema the surface actually needs, and was raised twice, both
    /// times after trimming: to 2100 when `edit_batch` and `splat_components` arrived (a nested
    /// step schema and a nested selection filter cannot be flattened away), and to 2200 when
    /// `edit_splat` gained the same targeting and retry-safety fields as `edit_batch` - it now
    /// accepts an `operation_id` and the full target vocabulary, which is exactly what stopped it
    /// from silently editing the wrong gaussians. The compact asset and job tools arrived with an
    /// id-shaped surface, so the per-tool budget is unchanged.
    const LISTING_BUDGET_BYTES_PER_TOOL: usize = 2200;

    #[test]
    fn the_tool_listing_stays_within_its_context_budget() {
        let router = SplatMcpServer::tool_router();
        let tools = router.list_all();
        let listing = serde_json::to_string(&tools).unwrap();

        assert_eq!(
            tools.len(),
            22,
            "the surface is expected to hold twenty-two tools"
        );
        let budget = tools.len() * LISTING_BUDGET_BYTES_PER_TOOL;
        assert!(
            listing.len() <= budget,
            "the tool listing is {} bytes, above the {budget} byte budget \
             ({LISTING_BUDGET_BYTES_PER_TOOL} bytes per tool); trim descriptions or fields \
             instead of raising the budget",
            listing.len()
        );

        // Every tool needs a description and a schema, and a name short enough to be
        // cheap to mention in a conversation.
        for tool in &tools {
            assert!(
                tool.description
                    .as_ref()
                    .is_some_and(|text| !text.is_empty()),
                "{} has no description",
                tool.name
            );
            assert!(tool.name.len() <= 24, "{} has a long name", tool.name);
            let schema = serde_json::to_string(&tool.input_schema).unwrap();
            assert!(!schema.is_empty(), "{} has no schema", tool.name);
        }

        // An optional field must be optional: a required field would force the caller to
        // invent values for the fields it does not care about.
        let edit = tools
            .iter()
            .find(|tool| tool.name == "edit_splat")
            .expect("edit_splat exists");
        let required: Vec<String> = edit
            .input_schema
            .get("required")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(required, vec!["ops".to_owned()]);
    }

    #[test]
    fn tool_replies_do_not_carry_widened_floats() {
        // A schema is read by a model, so a default printed as 0.8500000238418579 (an
        // f32 widened to f64) is both confusing and wasted context.
        let router = SplatMcpServer::tool_router();
        let listing = serde_json::to_string(&router.list_all()).unwrap();
        assert!(
            !listing.contains("0.8500000238418579"),
            "a widened float leaked into the tool schema"
        );
        assert!(
            !listing.contains("0.8999999761581421"),
            "a widened float leaked into the tool schema"
        );
    }
}
