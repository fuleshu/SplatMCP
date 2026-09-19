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
use contract::{Envelope, ErrorCode, ErrorLayer, Failure, ToolOutput};
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
    async fn splatmcp_status(&self) -> Result<CallToolResult, McpError> { run_tool(|| {
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
        tool_text(json!({
            "server_version": env!("CARGO_PKG_VERSION"),
            "bridge_protocol": splatmcp_bridge::PROTOCOL_VERSION,
            "app": app,
            "note": note,
        }))
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let state = viewer::set_camera(&self.link, camera).map_err(tool_error)?;
        tool_json(&state)
    })
    }

    /// Reads the camera of the desktop app.
    #[tool(
        description = "Read the displayed splat's camera: position, look-at target and field of view.",
        annotations(title = "Get camera", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_camera(&self) -> Result<CallToolResult, McpError> { run_tool(|| {
        let state = viewer::get_camera(&self.link).map_err(tool_error)?;
        tool_json(&state)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
            return tool_text(reply);
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
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let reply = asset::register(&self.link, &input).map_err(tool_error)?;
        tool_json(&reply)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let value = asset::info(&self.link, &input).map_err(tool_error)?;
        tool_text(value)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        match edit::batch_call(&input).map_err(tool_error)? {
            edit::BatchCall::Batch(request) => {
                let params = serde_json::to_value(&request)
                    .map_err(|error| tool_error(error.to_string()))?;
                let reply = self
                    .link
                    .request(Method::DocumentEditBatch, params)
                    .map_err(tool_error)?;
                tool_text(reply)
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
                tool_text(reply)
            }
            edit::BatchCall::CommitPreview(request) => {
                let params = serde_json::to_value(&request)
                    .map_err(|error| tool_error(error.to_string()))?;
                let reply = self
                    .link
                    .request(Method::DocumentCommitPreview, params)
                    .map_err(tool_error)?;
                tool_text(reply)
            }
        }
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
        tool_text(reply)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let request = edit::components_request(&input).map_err(tool_error)?;
        let params =
            serde_json::to_value(&request).map_err(|error| tool_error(error.to_string()))?;
        let reply = self
            .link
            .request(Method::DocumentComponents, params)
            .map_err(tool_error)?;
        tool_text(reply)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
    })
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
    async fn python_runtime_info(&self) -> Result<CallToolResult, McpError> { run_tool(|| {
        let info = python::runtime_info(&self.link).map_err(tool_error)?;
        tool_json(&info)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let receipt = python::run(&self.link, &input).map_err(tool_error)?;
        tool_json(&receipt)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let job = python::job(&self.link, &input).map_err(tool_error)?;
        tool_json(&job)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let cancelled = python::cancel(&self.link, &input).map_err(tool_error)?;
        tool_json(&cancelled)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let value = job::run(&self.link, &input).map_err(tool_error)?;
        tool_text(value)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let value = publication::run(&self.link, &input).map_err(tool_error)?;
        tool_text(value)
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        match contract_tools::capabilities(&self.link, &input, &served_tool_names()) {
            Ok(payload) => tool_envelope(contract_tools::envelope(payload), None),
            Err(failure) => Err(failure),
        }
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
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
            Err(failure) => Err(failure),
        }
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        match contract_tools::capture_views(&self.link, &input) {
            Ok(payload) => tool_envelope(contract_tools::envelope(payload), None),
            Err(failure) => Err(failure),
        }
    })
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
    ) -> Result<CallToolResult, McpError> { run_tool(|| {
        let capture = viewer::screenshot(&self.link, &input).map_err(tool_error)?;
        let payload = serde_json::to_value(viewer::CaptureSummary::from(&capture))
            .map_err(|error| tool_error(error.to_string()))?;
        // The frame stays native image content, and the summary travels as structured content with
        // the same text it always carried.
        let envelope = Envelope::ok(payload);
        let text = envelope.to_text();
        let mut result = CallToolResult::structured(envelope.to_value());
        result.content = vec![ContentBlock::image(
            BASE64.encode(&capture.bytes),
            capture.mime_type.clone(),
        )];
        result.content.push(ContentBlock::text(text));
        Ok(result)
    })
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

/// The names this server serves, for capability discovery.
///
/// Read from the router itself rather than from a list kept beside it, so a tool that is added or
/// removed cannot leave discovery claiming something that does not exist.
pub(crate) fn served_tool_names() -> Vec<String> {
    SplatMcpServer::tool_router()
        .list_all()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect()
}

/// A tool's own outcome: a reply, or a classified failure.
///
/// Every body produces this, so a refusal reaches the caller as a *tool execution error* - the
/// call was received and routed, and the tool refused - instead of escaping as a protocol error.
/// MCP draws that line deliberately, and `-32602` for a stale revision told a client nothing it
/// could act on.
pub(crate) type ToolResult = std::result::Result<CallToolResult, Failure>;

/// Runs a tool body and turns its refusal into an error tool result.
pub(crate) fn run_tool(body: impl FnOnce() -> ToolResult) -> Result<CallToolResult, McpError> {
    match body() {
        Ok(result) => Ok(result),
        Err(failure) => tool_failure(failure),
    }
}

/// Wraps a reply in the result envelope: structured content a client reads, and the same JSON as
/// text so a model reads exactly what it read before.
///
/// The text is byte-for-byte what this server returned before the envelope existed - the payload,
/// compact - so an existing caller is unaffected, while a new one can follow `status`,
/// `correlation` and `failure` by field.
pub(crate) fn envelope_result(payload: Value, text: String) -> CallToolResult {
    let envelope = Envelope::ok(payload);
    let mut result = CallToolResult::structured(envelope.to_value());
    result.content = vec![ContentBlock::text(text)];
    result
}

/// Wraps JSON as the text block a tool returns, and as structured content.
pub(crate) fn tool_text(value: Value) -> ToolResult {
    let text = compact_json(&value);
    Ok(envelope_result(value, text))
}

/// Serialises a typed reply and wraps it as the text block a tool returns, with structured
/// content built from the same value.
///
/// Passing a struct instead of a `Value` keeps `f32` fields at their shortest form in the text:
/// widening through `serde_json::Value` would turn `0.01` into `0.009999999776482582`.
pub(crate) fn tool_json<T: serde::Serialize>(value: &T) -> ToolResult {
    let text = serde_json::to_string(value)
        .map_err(|error| Failure::new(ErrorCode::InternalError, ErrorLayer::Mcp, format!("could not encode the reply: {error}")))?;
    // The payload is parsed back from the text rather than converted from the struct: going
    // through `Value` directly would widen every `f32` into its `f64` neighbour and print
    // `0.699999988079071` where the text says `0.7`. One bounded parse keeps the two forms
    // identical, which is the whole point of reporting them together.
    let payload: Value = serde_json::from_str(&text).map_err(|error| {
        Failure::new(
            ErrorCode::InternalError,
            ErrorLayer::Mcp,
            format!("could not encode the reply: {error}"),
        )
    })?;
    Ok(envelope_result(payload, text))
}

/// Classifies a layer's failure message into a typed failure.
///
/// The message is the whole payload a caller reads, and it is deliberately **not** wrapped as an
/// internal error: a stale revision, an unsupported mode or an exhausted budget is not an internal
/// failure, and a client has to be able to tell them apart.
pub(crate) fn tool_error(message: String) -> Failure {
    Failure::inferred(ErrorLayer::App, message)
}

/// Runs a tool body that already produces a typed tool result.
pub(crate) fn tool_envelope(envelope: Envelope, extra: Option<Vec<ContentBlock>>) -> ToolResult {
    let mut blocks = extra.unwrap_or_default();
    blocks.push(ContentBlock::text(envelope.to_text()));
    Ok(CallToolResult::structured(envelope.to_value()).with_content(blocks))
}

/// A tool reply that failed: an error result with structured content, not a protocol error.
pub(crate) fn tool_failure(failure: Failure) -> Result<CallToolResult, McpError> {
    let output = ToolOutput::failed(failure);
    let text = output.envelope.to_text();
    let structured = output.envelope.to_value();
    let mut result = CallToolResult::structured_error(structured);
    result.content = vec![ContentBlock::text(text)];
    Ok(result)
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

/// Compact JSON: no indentation, so a tool reply stays small.
pub(crate) fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of a tool result, concatenated, as a client would read it.
    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|text| text.text.clone())
            .collect::<String>()
    }

    #[test]
    fn tool_replies_are_compact_json() {
        let result = tool_text(json!({ "point_count": 12, "max_opacity": 0.5 })).unwrap();
        let text = text_of(&result);
        // The text is byte-for-byte what this server returned before the envelope existed.
        assert_eq!(text, "{\"max_opacity\":0.5,\"point_count\":12}");
        assert!(!text.contains('\n'));
        assert!(!text.contains("  "));
    }

    #[test]
    fn every_reply_carries_structured_content_beside_its_text() {
        // The point of the envelope: a client reads the payload by field, and a model reads the
        // same JSON as text. Both come from one value, so they cannot disagree.
        let result = tool_text(json!({ "point_count": 12 })).unwrap();
        let structured = result
            .structured_content
            .as_ref()
            .expect("a tool reply carries structured content");
        assert_eq!(structured["status"], "ok");
        assert_eq!(structured["payload"]["point_count"], 12);
        assert_eq!(result.is_error, Some(false));
        assert_eq!(text_of(&result), "{\"point_count\":12}");

        // A correlated reply names the document and revision it landed on.
        let correlated = tool_text(json!({
            "document": { "document_id": "doc-1-2", "revision": 4 }
        }))
        .unwrap();
        let structured = correlated.structured_content.unwrap();
        assert_eq!(structured["correlation"]["document_id"], "doc-1-2");
        assert_eq!(structured["correlation"]["revision"], 4);
    }

    #[test]
    fn a_typed_reply_keeps_its_field_order_in_the_text() {
        // A struct serialises in declaration order; going through the envelope must not sort the
        // fields or widen the floats a model reads.
        let point = splatmcp_core::SplatPoint::new(
            [0.1, 0.2, 0.3],
            [0.01, 0.02, 0.03],
            [0.4, 0.5, 0.6],
            0.7,
            [1.0, 0.0, 0.0, 0.0],
        );
        let result = tool_json(&tools::PointOut::of(&point)).unwrap();
        assert_eq!(
            text_of(&result),
            "{\"position\":[0.1,0.2,0.3],\"scale\":[0.01,0.02,0.03],\"color\":[0.4,0.5,0.6],\
             \"opacity\":0.7,\"rotation\":[1.0,0.0,0.0,0.0]}"
        );
        let structured = result.structured_content.unwrap();
        // The structured form carries the same digits the text does: an f32 widened to f64 would
        // read 0.699999988079071 and a client comparing the two would find a difference that is
        // not there.
        let structured_text = serde_json::to_string(&structured).unwrap();
        assert!(structured_text.contains("0.7"), "{structured_text}");
        assert!(!structured_text.contains("0.699"), "{structured_text}");
    }

    #[test]
    fn a_refusal_is_a_tool_error_result_not_a_protocol_error() {
        // A stale revision or a missing file is an expected outcome of a call that was received and
        // routed. Escaping as a protocol error hid the type, the retryability and the commit state.
        let result = run_tool(|| Err(tool_error("document_conflict: expected revision 3, current 5".to_owned())))
            .expect("a refusal is a result, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().expect("structured failure");
        assert_eq!(structured["status"], "error");
        assert_eq!(structured["failure"]["code"], "stale_revision");
        assert_eq!(structured["failure"]["layer"], "app");
        // A commit is not implied: a failure that did not travel further stays unknown.
        assert_eq!(structured["failure"]["outcome"], "unknown");
        assert!(text_of(&result).contains("stale_revision"));
    }

    #[test]
    fn a_budget_refusal_is_classified_as_one() {
        // The two limit checks a caller meets first: an oversized frame and too many views. Both
        // name a limit, so both are budget exhaustion rather than a malformed field.
        let result = run_tool(|| {
            Err(tool_error(
                "9 views were requested; this build captures at most 8 per call".to_owned(),
            ))
        })
        .unwrap();
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["failure"]["code"], "budget_exhausted");
        assert_eq!(structured["failure"]["category"], "budget");
    }

    #[test]
    fn a_legacy_tool_refusal_also_returns_an_error_result() {
        // Through the real tool path: with no app attached, describing a file cannot succeed, and
        // the caller has to receive a typed failure rather than a protocol error.
        let server = SplatMcpServer::without_launching();

        // A missing file named by load_splat, and a missing document named by splat_info: both are
        // expected outcomes of a call that arrived, so both answer with a typed failure.
        let load: edit::LoadInput =
            serde_json::from_value(json!({ "path": "C:/nowhere/missing.ply" })).unwrap();
        let loaded = futures_lite_block_on(server.load_splat(Parameters(load)))
            .expect("a missing file is a tool result, not a protocol error");
        assert_eq!(loaded.is_error, Some(true));

        let info: edit::InfoInput =
            serde_json::from_value(json!({ "source": "C:/nowhere/missing.ply" })).unwrap();
        let result = futures_lite_block_on(server.splat_info(Parameters(info)))
            .expect("the call reached the tool, so it answers with a result");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["status"], "error");
        assert!(
            structured["failure"]["code"].as_str().is_some_and(|code| code != "internal_error"),
            "an expected operational failure must be classified: {structured}"
        );
    }

    /// Runs one tool future to completion without pulling in an async runtime for the tests.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        // The tool handlers never await anything real - they call the blocking bridge - so a
        // minimal executor is enough and keeps the tests fast.
        use std::pin::Pin;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: std::sync::Arc<Self>) {}
        }
        let waker = Waker::from(std::sync::Arc::new(Noop));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Pin::as_mut(&mut future).poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
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
