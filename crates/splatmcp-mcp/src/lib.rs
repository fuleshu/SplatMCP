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
use tools::{author, edit, viewer};

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
        annotations(title = "Server status", read_only_hint = true, open_world_hint = false)
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
        annotations(title = "Create splat", read_only_hint = false, open_world_hint = false)
    )]
    async fn create_splat(
        &self,
        Parameters(input): Parameters<author::CreateInput>,
    ) -> Result<CallToolResult, McpError> {
        let splat = author::build_splat(&input).map_err(tool_error)?;
        let bytes = splatmcp_core::write_ply(&splat).map_err(|error| tool_error(error.to_string()))?;

        let path = match input.path.as_deref() {
            Some(path) => Some(author::write_splat_file(path, &bytes).map_err(tool_error)?),
            None => None,
        };

        let display = input.display.unwrap_or(true);
        let mut displayed = false;
        if display {
            let file_name = path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("splat.ply")
                .to_owned();
            author::display_splat(&self.link, &file_name, &bytes).map_err(tool_error)?;
            displayed = true;
        }

        tool_json(&author::splat_reply(&splat, path.as_deref(), displayed))
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
        let (mut splat, _, _) = edit::resolve_source(&self.link, input.source.as_deref())
            .map_err(tool_error)?;
        let steps = edit::apply_edits(&mut splat, &input.ops).map_err(tool_error)?;
        let (path, displayed) = edit::save_and_display(
            &self.link,
            &splat,
            input.path.as_deref(),
            input.display.unwrap_or(true),
        )
        .map_err(tool_error)?;
        tool_json(&edit::edit_reply(&splat, steps, path, displayed))
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
        let bytes = splatmcp_core::write_ply(&splat).map_err(|error| tool_error(error.to_string()))?;
        let file_name = std::path::Path::new(&input.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned();
        author::display_splat(&self.link, &file_name, &bytes).map_err(tool_error)?;
        tool_json(&author::splat_reply(&splat, Some(std::path::Path::new(&input.path)), true))
    }

    /// Describes a splat: count, bounds, colour and opacity.
    #[tool(
        description = "Describe a gaussian splat: point count, bounds, mean colour, opacity range. \
                       Reads the displayed splat by default, or a .ply path; set points to inspect \
                       the first n gaussians in detail.",
        annotations(title = "Splat info", read_only_hint = true, open_world_hint = false)
    )]
    async fn splat_info(
        &self,
        Parameters(input): Parameters<edit::InfoInput>,
    ) -> Result<CallToolResult, McpError> {
        let (splat, source, _) = edit::resolve_source(&self.link, input.source.as_deref())
            .map_err(tool_error)?;
        tool_json(&edit::info_reply(&splat, source, input.points))
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
    let text = serde_json::to_string(value)
        .map_err(|error| McpError::internal_error(format!("could not encode the reply: {error}"), None))?;
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

    /// Largest tool listing this server may present to a client.
    ///
    /// A listing is sent to the model with every session, so its size is a real cost. The
    /// budget is a guard, not a target: adding a tool or a field is fine, silently
    /// doubling the listing is not.
    const LISTING_BUDGET_BYTES: usize = 16 * 1024;

    #[test]
    fn the_tool_listing_stays_within_its_context_budget() {
        let router = SplatMcpServer::tool_router();
        let tools = router.list_all();
        let listing = serde_json::to_string(&tools).unwrap();

        assert_eq!(tools.len(), 8, "the surface is expected to hold eight tools");
        assert!(
            listing.len() <= LISTING_BUDGET_BYTES,
            "the tool listing is {} bytes, above the {LISTING_BUDGET_BYTES} byte budget; \
             trim descriptions or fields instead of raising the budget",
            listing.len()
        );

        // Every tool needs a description and a schema, and a name short enough to be
        // cheap to mention in a conversation.
        for tool in &tools {
            assert!(
                tool.description.as_ref().is_some_and(|text| !text.is_empty()),
                "{} has no description",
                tool.name
            );
            assert!(
                tool.name.len() <= 24,
                "{} has a long name",
                tool.name
            );
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
