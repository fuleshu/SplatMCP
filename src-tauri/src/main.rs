//! SplatMCP desktop shell.
//!
//! Opens the viewer window, starts the `splatmcp-mcp` MCP server process alongside
//! the app, and exposes the PLY import/export of [`splatmcp_core`] to the frontend
//! as `open_splat` / `current_splat_bytes` / `save_splat`.

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;

use serde::Serialize;
use splatmcp_core::{Splat, read_ply, write_ply};
use tauri::ipc::Response;
use tauri::{Manager, State};

const MCP_SERVER_BINARY: &str = "splatmcp-mcp";

#[derive(Default)]
struct AppState {
    /// Child handle for the MCP server; killed when the app state is dropped.
    mcp_server: Mutex<Option<McpServerProcess>>,
    /// The splat currently shown by the viewer, kept in file-independent form so
    /// that saving re-serialises through `splatmcp_core` instead of echoing bytes.
    document: Mutex<Option<Document>>,
}

struct Document {
    path: PathBuf,
    splat: Splat,
}

/// Holds the MCP server child process and terminates it on drop.
///
/// The child's `stdin` handle is deliberately kept open: a stdio MCP server treats
/// end-of-input as "client gone" and exits immediately, so redirecting `stdin` from
/// `NUL` made the server die seconds after launch. Nobody sends requests over this
/// pipe yet - wiring the server to the window (camera / screenshot) is still an open
/// design point - but holding it open keeps the process alive and idle.
struct McpServerProcess {
    child: Child,
    _stdin: ChildStdin,
}

impl Drop for McpServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Summary of a loaded splat, returned to the frontend after a successful open.
#[derive(Serialize)]
struct SplatInfo {
    path: String,
    file_name: String,
    point_count: usize,
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
    let mut child = Command::new(mcp_server_path())
        .stdin(Stdio::piped())
        // The server never writes without a request, so discarding its output
        // cannot fill a pipe that nobody is reading.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    Some(McpServerProcess {
        child,
        _stdin: stdin,
    })
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("splat.ply")
        .to_owned()
}

fn document_info(document: &Document) -> SplatInfo {
    SplatInfo {
        path: document.path.to_string_lossy().to_string(),
        file_name: file_name_of(&document.path),
        point_count: document.splat.len(),
    }
}

/// Picks a PLY file, imports it and makes it the displayed splat.
/// Returns `None` when the user cancels the dialog.
#[tauri::command]
fn open_splat(state: State<'_, AppState>) -> Result<Option<SplatInfo>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Open Gaussian Splat")
        .add_filter("Gaussian splat (PLY)", &["ply"])
        .pick_file()
    else {
        return Ok(None);
    };

    let bytes = std::fs::read(&path).map_err(|error| format!("could not read {path:?}: {error}"))?;
    let splat = read_ply(&bytes).map_err(|error| error.to_string())?;
    splat.validate().map_err(|error| error.to_string())?;

    let document = Document { path, splat };
    let info = document_info(&document);
    *state
        .document
        .lock()
        .map_err(|_| "splat state is locked".to_owned())? = Some(document);
    Ok(Some(info))
}

/// Raw PLY bytes of the displayed splat, handed to the PlayCanvas viewer.
#[tauri::command]
fn current_splat_bytes(state: State<'_, AppState>) -> Result<Response, String> {
    let guard = state
        .document
        .lock()
        .map_err(|_| "splat state is locked".to_owned())?;
    let document = guard.as_ref().ok_or("no splat is loaded".to_owned())?;
    let bytes = write_ply(&document.splat).map_err(|error| error.to_string())?;
    Ok(Response::new(bytes))
}

/// Exports the displayed splat as PLY. Returns `None` when the user cancels.
#[tauri::command]
fn save_splat(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state
        .document
        .lock()
        .map_err(|_| "splat state is locked".to_owned())?;
    let document = guard.as_ref().ok_or("no splat is loaded".to_owned())?;

    let Some(path) = rfd::FileDialog::new()
        .set_title("Save Gaussian Splat")
        .add_filter("Gaussian splat (PLY)", &["ply"])
        .set_file_name(file_name_of(&document.path))
        .save_file()
    else {
        return Ok(None);
    };

    let bytes = write_ply(&document.splat).map_err(|error| error.to_string())?;
    std::fs::write(&path, bytes).map_err(|error| format!("could not write {path:?}: {error}"))?;
    Ok(Some(path.to_string_lossy().to_string()))
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
        .invoke_handler(tauri::generate_handler![
            open_splat,
            current_splat_bytes,
            save_splat
        ])
        .run(tauri::generate_context!())
        .expect("failed to run SplatMCP");
}
