//! SplatMCP MCP server library.
//!
//! Milestone 1: minimal `rmcp` service that compiles and answers `tools/list`.
//! The real splat tools (create / edit / camera / screenshot) land in later milestones.

use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_router,
};

/// Shared state for the MCP service. A camera / screenshot bridge to the Tauri
/// desktop app will be added here in a later milestone.
#[derive(Clone, Debug, Default)]
pub struct SplatMcpServer;

impl SplatMcpServer {
    pub fn new() -> Self {
        Self
    }
}

#[tool_router(router = tool_router, vis = "pub")]
impl SplatMcpServer {
    /// Placeholder tool so `tools/list` is non-empty while the splat tools are being built.
    #[tool(description = "Returns the SplatMCP server status.")]
    async fn ping(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "splatmcp ok",
        )]))
    }
}

#[rmcp::tool_handler]
impl ServerHandler for SplatMcpServer {
    fn get_info(&self) -> ServerInfo {
        // TODO: verify — rmcp 3.x ServerInfo is non-exhaustive, so it is built via constructors.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ),
        )
    }
}
