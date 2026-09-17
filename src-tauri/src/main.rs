//! SplatMCP desktop shell.
//!
//! Opens the viewer window, hosts the loopback bridge that MCP tools call, and exposes
//! the PLY import/export of [`splatmcp_core`] to the frontend as
//! `open_splat` / `current_splat_bytes` / `save_splat`.
//!
//! The MCP server itself is a separate process spawned by the MCP client. It finds this
//! app through `bridge.json` in the app data directory; see `docs/design/milestone-4.md`.

mod bridge;
mod document;
mod paths;
mod settings;
mod viewer;

use std::sync::Arc;

use document::AppState;
use serde_json::Value;
use tauri::ipc::Response;
use tauri::{Manager, State};
use viewer::Viewer;

/// Picks a PLY file, imports it and makes it the displayed splat.
/// Returns `None` when the user cancels the dialog.
#[tauri::command]
fn open_splat(state: State<'_, AppState>) -> Result<Option<document::SplatInfo>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Open Gaussian Splat")
        .add_filter("Gaussian splat (PLY)", &["ply"])
        .pick_file()
    else {
        return Ok(None);
    };

    let bytes = std::fs::read(&path).map_err(|error| format!("could not read {path:?}: {error}"))?;
    let splat = document::Document::from_ply_bytes(&bytes, path)?;
    Ok(Some(state.replace(splat)?))
}

/// Raw PLY bytes of the displayed splat, handed to the PlayCanvas viewer.
#[tauri::command]
fn current_splat_bytes(state: State<'_, AppState>) -> Result<Response, String> {
    let Some(bytes) = state.ply_bytes()? else {
        return Err("no splat is loaded".to_owned());
    };
    Ok(Response::new(bytes))
}

/// Exports the displayed splat as PLY. Returns `None` when the user cancels.
#[tauri::command]
fn save_splat(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let (bytes, file_name) = state.with_document(|document| {
        (document.ply_bytes(), document.file_name())
    })?;
    let bytes = bytes?;

    let Some(path) = rfd::FileDialog::new()
        .set_title("Save Gaussian Splat")
        .add_filter("Gaussian splat (PLY)", &["ply"])
        .set_file_name(&file_name)
        .save_file()
    else {
        return Ok(None);
    };

    std::fs::write(&path, bytes).map_err(|error| format!("could not write {path:?}: {error}"))?;
    Ok(Some(path.to_string_lossy().to_string()))
}

/// Answers a bridge request that was forwarded into the webview.
#[tauri::command]
fn bridge_respond(
    id: u64,
    result: Option<Value>,
    error: Option<String>,
    viewer: State<'_, bridge::ViewerState>,
) -> Result<(), String> {
    let outcome = match error {
        Some(message) => Err(message),
        None => Ok(result.unwrap_or(Value::Null)),
    };
    viewer.0.respond(id, outcome)
}

fn main() {
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .manage(bridge::BridgeHostState(std::sync::Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            open_splat,
            current_splat_bytes,
            save_splat,
            bridge_respond
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // The window geometry is restored before anything else can move the window.
            let window_settings = settings::track(&handle);
            app.manage(window_settings);

            let viewer = Arc::new(Viewer::new(handle.clone()));
            app.manage(bridge::ViewerState(viewer.clone()));
            // A bridge failure must not stop the viewer from working: without a data
            // directory only the MCP half of the app is unavailable.
            match bridge::start(&handle, viewer) {
                Ok(host) => {
                    println!("splatmcp: bridge listening on 127.0.0.1:{}", host.port());
                    let state = app.state::<bridge::BridgeHostState>();
                    *state
                        .0
                        .lock()
                        .map_err(|_| std::io::Error::other("bridge state is locked"))? = Some(host);
                }
                Err(error) => {
                    eprintln!("splatmcp: MCP bridge is unavailable: {error}");
                    eprintln!(
                        "splatmcp: set SPLATMCP_DATA_DIR to a writable folder to enable it"
                    );
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build SplatMCP");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Leave no descriptor behind that points at a dead port.
            handle.state::<bridge::BridgeHostState>().shutdown();
            bridge::retire();
        }
    });
}
