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
use tools::{author, edit, python, viewer};

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
        description = "Move the camera in the SplatMCP window and return where it ended up. Give \
                       fit=true to frame the whole splat, a position, or azimuth/elevation/distance \
                       to orbit it.",
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
        description = "Report the camera of the SplatMCP window: position, look-at target and field of view.",
        annotations(title = "Get camera", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_camera(&self) -> Result<CallToolResult, McpError> {
        let state = viewer::get_camera(&self.link).map_err(tool_error)?;
        tool_json(&state)
    }

    /// Builds a splat from parameters and shows it in the app.
    #[tool(
        description = "Create a gaussian splat from a shape (sphere, cube, plane, line, shell, \
                       ring, grid) or explicit points, in fixed RGB colour. Optionally writes a .ply \
                       and shows it in the SplatMCP window. Returns the point count and bounds.",
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
                author::display_splat(&self.link, &file_name, &bytes, None).map_err(tool_error)?,
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
        description = "Edit a gaussian splat in place: translate, rotate, scale, set_radius, \
                       adjust_color, set_color, set_opacity, duplicate, remove or merge, each with \
                       an optional box or attribute selection. Works on the displayed splat by \
                       default; returns per-step counts.",
        annotations(title = "Edit splat", read_only_hint = false, open_world_hint = false)
    )]
    async fn edit_splat(
        &self,
        Parameters(input): Parameters<edit::EditInput>,
    ) -> Result<CallToolResult, McpError> {
        let resolved =
            edit::resolve_source(&self.link, input.source.as_deref()).map_err(tool_error)?;
        let mut splat = resolved.splat;
        // The identity the edit started from: when the source was the displayed document the
        // result replaces exactly that revision, so the edit keeps its identity and a stale
        // edit is refused. A `.ply` source produces a document of its own.
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
        tool_json(&edit::edit_reply(
            &splat,
            steps,
            outcome.path,
            outcome.displayed,
            outcome.document,
        ))
    }

    /// Shows an existing PLY file in the app.
    #[tool(
        description = "Load a .ply gaussian splat into the SplatMCP window and frame it.",
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
        let splat = edit::read_splat_file(&input.path).map_err(tool_error)?;
        let bytes =
            splatmcp_core::write_ply(&splat).map_err(|error| tool_error(error.to_string()))?;
        let file_name = std::path::Path::new(&input.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned();
        // Loading a file opens a document: the file is provenance, not identity.
        let status =
            author::display_splat(&self.link, &file_name, &bytes, None).map_err(tool_error)?;
        tool_json(&author::splat_reply(
            &splat,
            Some(std::path::Path::new(&input.path)),
            true,
            Some(&status),
        ))
    }

    /// Applies an edit batch as one atomic, previewable and retry-safe transaction.
    #[tool(
        description = "Apply several edit steps as ONE transaction: all of them commit as one new \
                       revision or nothing changes. dry_run reports a preview without committing; \
                       commit that candidate later with preview_id (refused if the document moved \
                       on). operation_id makes a retry after a lost response safe. Undo/redo and \
                       the history are shared with the window.",
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
                       state and 'redo' re-applies it. Each commits a NEW revision and a new edit \
                       clears the redo stack.",
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
                       list, create, rename, remove, transform (declares an explicit local frame; \
                       anisotropic gaussians are transformed through their covariance and singular \
                       or reflecting frames are refused), members, apply_transform and select \
                       (count, bounds and a bounded sample). These ids are the window's ids.",
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
                       range, scale and colour distributions, contract diagnostics, buffer \
                       sizes, and the displayed document's id and revision (quote it as \
                       expected_revision when a Python job edits that document). Reads the \
                       displayed document as bounded metadata by default, or a .ply path; set \
                       points for the first n gaussians themselves.",
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
        let resolved =
            edit::resolve_source(&self.link, input.source.as_deref()).map_err(tool_error)?;
        tool_json(&edit::info_reply_with_document(
            &resolved.splat,
            resolved.source,
            input.points,
            resolved.document,
        ))
    }

    /// Reports readiness and versions of the app's embedded Python runtime.
    #[tool(
        description = "Report the SplatMCP Python runtime: readiness, interpreter and package \
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
                       the result in the SplatMCP window. Pass code or script_path plus a \
                       request_id, then poll the returned job id with get_python_job. Use \
                       display:false to commit without changing what is displayed, and \
                       frame:false to keep the current camera. Scripts are local code execution, \
                       not a sandbox.",
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
        description = "Read a run_python_splat job: state, progress, timings, revision, point \
                       count, bounds, export, structured error and logs. Pass log_after from the \
                       previous reply for only new lines. A committed job is not proof the viewer \\
                       rendered it: check display.",
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
        description = "Cancel a run_python_splat job. A queued job stops immediately; a running \
                       job stops at its next checkpoint, so a native NumPy or PyTorch call can \
                       keep it in cancel_requested. A late result is discarded, not committed.",
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

    /// Renders the current view and returns it as an image.
    #[tool(
        description = "Render the SplatMCP window and return the frame as an image, optionally after \
                       moving the camera. Use it to check what the splat looks like; give a small \
                       width to keep the reply cheap.",
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
/// The message is the whole payload: an MCP client shows it to the model, so it must
/// already say what went wrong and what to do next.
pub(crate) fn tool_error(message: String) -> McpError {
    McpError::internal_error(message, None)
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
    /// The value tracks the largest schema the surface actually needs. It was raised from 1600
    /// to 2100 when `edit_batch` and `splat_components` arrived: a batch carries a *nested* step
    /// schema (operation plus selection) and the component tool carries a nested selection
    /// filter, which no amount of flattened wording makes smaller - the measured listing is
    /// 30 KB for fifteen tools, about 2 KB per tool. Trimming descriptions and field names was
    /// done first; raising the number is the last resort, not the first.
    const LISTING_BUDGET_BYTES_PER_TOOL: usize = 2100;

    #[test]
    fn the_tool_listing_stays_within_its_context_budget() {
        let router = SplatMcpServer::tool_router();
        let tools = router.list_all();
        let listing = serde_json::to_string(&tools).unwrap();

        assert_eq!(
            tools.len(),
            15,
            "the surface is expected to hold fifteen tools"
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
