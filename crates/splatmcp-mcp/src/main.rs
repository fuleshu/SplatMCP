//! SplatMCP MCP server binary.
//!
//! Speaks MCP over stdio so any MCP client (Claude Desktop, Codex, ...) can drive it.
//! The desktop app launches this binary and bridges camera / screenshot calls to the
//! PlayCanvas viewer window.

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use splatmcp_mcp::SplatMcpServer;

#[tokio::main]
async fn main() -> Result<()> {
    let service = SplatMcpServer::new()
        .serve(stdio())
        .await
        .inspect_err(|error| {
            eprintln!("splatmcp: failed to start MCP service: {error}");
        })?;
    service.waiting().await?;
    Ok(())
}
