//! SplatMCP desktop shell.
//!
//! Opens the viewer window, hosts the loopback bridge that MCP tools call, and exposes the
//! document service of [`splatmcp_core`] to the frontend as `open_splat`,
//! `current_splat_bytes`, `reload_splat` and `save_splat`.
//!
//! The document itself lives in [`document::AppState`], which owns the store of identities,
//! revisions and snapshots; every command here resolves a target and reports the identity it
//! used. The MCP server is a separate process spawned by the MCP client; it finds this app
//! through `bridge.json` in the app data directory; see `docs/design/milestone-4.md`.

mod bridge;
mod document;
mod paths;
mod python;
mod settings;
mod viewer;

use std::sync::Arc;

use document::AppState;
use serde_json::Value;
use splatmcp_core::Expected;
use tauri::ipc::Response;
use tauri::{Manager, State};
use viewer::Viewer;

/// Picks a PLY file, imports it and makes it the displayed document.
///
/// Opening always creates a **new** document identity; the path is recorded as provenance.
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
    let metadata = state.open_ply(&bytes, document::open_mutation(&path))?;
    Ok(Some(document::SplatInfo::of(&metadata)))
}

/// Reads the source file of the displayed document again, as a new revision.
#[tauri::command]
fn reload_splat(state: State<'_, AppState>) -> Result<document::SplatInfo, String> {
    let metadata = state.reload(Expected::Any)?;
    Ok(document::SplatInfo::of(&metadata))
}

/// Raw PLY bytes of the displayed revision, handed to the PlayCanvas viewer.
///
/// The bytes are serialised from a snapshot, so the document store is not held while a large
/// document is written out.
#[tauri::command]
fn current_splat_bytes(state: State<'_, AppState>) -> Result<Response, String> {
    let (_, bytes) = state.active_ply_bytes()?;
    Ok(Response::new(bytes))
}

/// Exports the displayed revision as PLY. Returns `None` when the user cancels.
///
/// The reply names the exact identity that was written and the checksum of those bytes: a hash
/// identifies the artifact, never the document.
#[tauri::command]
fn save_splat(state: State<'_, AppState>) -> Result<Option<document::ExportOutcome>, String> {
    let file_name = match state.metadata() {
        Some(metadata) => metadata.provenance.file_name,
        None => return Err("no splat is loaded".to_owned()),
    };

    let Some(path) = rfd::FileDialog::new()
        .set_title("Save Gaussian Splat")
        .add_filter("Gaussian splat (PLY)", &["ply"])
        .set_file_name(&file_name)
        .save_file()
    else {
        return Ok(None);
    };

    let outcome = state.export(Expected::Any, &path)?;

    // A PLY cannot hold recipe or component metadata, so a generated document's provenance is
    // written next to it. Losing it must not fail a successful save.
    if let Some(recipe) = state.recipe()
        && let Ok(record) = serde_json::from_str::<splatmcp_python::script::RecipeRecord>(&recipe)
    {
        match splatmcp_python::script::RecipeRecord::write_sidecar(&path, &record) {
            Ok(sidecar) => println!("splatmcp: wrote {}", sidecar.display()),
            Err(error) => eprintln!("splatmcp: could not write the recipe sidecar: {error}"),
        }
    }
    Ok(Some(outcome))
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
            reload_splat,
            current_splat_bytes,
            save_splat,
            bridge_respond,
            python::python_runtime_info,
            python::python_submit,
            python::python_job,
            python::python_job_cancel,
            python::python_note_rendered,
            python::python_note_display_failed,
            python::splat_bytes_for_revision,
            python::document_info,
            python::python_read_script,
            python::python_write_script
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // The window geometry is restored before anything else can move the window.
            let window_settings = settings::track(&handle);
            app.manage(window_settings);

            let viewer = Arc::new(Viewer::new(handle.clone()));
            app.manage(bridge::ViewerState(viewer.clone()));

            // The generation host starts before the bridge so an MCP-started job and a
            // panel-started job share one interpreter and one job registry. A missing
            // Python runtime only disables generation; the viewer keeps working.
            let python = Arc::new(python::PythonHost::start(&handle));
            app.manage(python::PythonHostState(python.clone()));
            // A bridge failure must not stop the viewer from working: without a data
            // directory only the MCP half of the app is unavailable.
            match bridge::start(&handle, viewer, python) {
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
            // Stop the interpreter after the window is gone: a script that ignores
            // cancellation can only be waited for, not killed safely.
            handle.state::<python::PythonHostState>().0.shutdown();
        }
    });
}
