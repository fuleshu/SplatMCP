//! SplatMCP desktop shell.
//!
//! Milestone 1: open the viewer window and start the `splatmcp-mcp` MCP server
//! process alongside the app. Window-state persistence, open/save dialogs, the
//! PlayCanvas canvas and the real MCP tools are added in later milestones.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use tauri::Manager;

const MCP_SERVER_BINARY: &str = "splatmcp-mcp";

#[derive(Default)]
struct AppState {
    /// Child handle for the MCP server; killed when the app state is dropped.
    mcp_server: Mutex<Option<McpServerProcess>>,
}

/// Holds the MCP server child process and terminates it on drop.
struct McpServerProcess(Child);

impl Drop for McpServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Best-effort location of the MCP server binary, which cargo builds next to the
/// desktop executable.
fn mcp_server_path() -> PathBuf {
    let name = if cfg!(windows) {
        format!("{MCP_SERVER_BINARY}.exe")
    } else {
        MCP_SERVER_BINARY.to_owned()
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn start_mcp_server() -> Option<McpServerProcess> {
    let child = Command::new(mcp_server_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(McpServerProcess(child))
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            let state = app.state::<AppState>();
            *state
                .mcp_server
                .lock()
                .map_err(|_| std::io::Error::other("mcp server state is locked"))? =
                start_mcp_server();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run SplatMCP");
}
