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

mod authoring;
mod bridge;
mod document;
mod paths;
mod python;
mod settings;
mod viewer;

use std::sync::Arc;

use document::AppState;
use serde_json::{Value, json};
use splatmcp_core::Expected;
use tauri::ipc::Response;
use tauri::{Manager, State};
use viewer::Viewer;

/// Number of bytes an export wrote.
fn outcome_bytes(outcome: &document::ExportOutcome) -> usize {
    outcome.bytes
}

/// Reports authoring metadata that exists next to a just-opened file but does not describe it.
///
/// Opening always mints a **new** document identity, so a sidecar written for an earlier session
/// can never match by accident; when one is present the reason is printed rather than silently
/// attaching ids that would mean something else.
fn warn_about_sidecar(path: &std::path::Path, metadata: &document::DocumentMetadata, bytes: &[u8]) {
    let artifact = document::ArtifactChecksum::of(bytes);
    let artifact = format!("{}:{}", artifact.algorithm, artifact.hex());
    match authoring::lookup(
        path,
        metadata.handle.document_id.as_str(),
        metadata.handle.revision,
        &artifact,
        metadata.point_count,
    ) {
        authoring::SidecarLookup::Absent => {}
        authoring::SidecarLookup::Attached(_) => {
            println!(
                "splatmcp: authoring metadata attached to {}",
                path.display()
            );
        }
        authoring::SidecarLookup::Refused(reason) => {
            eprintln!(
                "splatmcp: ignoring the authoring sidecar of {}: {reason}",
                path.display()
            );
            eprintln!("splatmcp: component metadata is not attached by file name");
        }
    }
}

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

    let bytes =
        std::fs::read(&path).map_err(|error| format!("could not read {path:?}: {error}"))?;
    let metadata = state.open_ply(&bytes, document::open_mutation(&path))?;
    warn_about_sidecar(&path, &metadata, &bytes);
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

    // A PLY carries geometry only: the versioned authoring sidecar records the components, their
    // membership and their frames against this exact artifact, and a plain export keeps the
    // documented "geometry only" guarantee. Failing to write either sidecar must not fail a
    // successful save.
    if let Ok((handle, components)) = state.authoring_snapshot(Expected::Any)
        && !components.is_empty()
    {
        let record = authoring::record(
            handle.document_id.as_str(),
            handle.revision,
            &outcome.checksum,
            outcome_bytes(&outcome),
            &components,
        );
        match authoring::write(&path, &record) {
            Ok(sidecar) => println!("splatmcp: wrote {}", sidecar.display()),
            Err(error) => eprintln!("splatmcp: could not write the authoring sidecar: {error}"),
        }
    }

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

/// Lists the components of the displayed document, with their stable ids.
#[tauri::command]
fn component_list(app: tauri::AppHandle) -> Result<Value, String> {
    let request = json!({ "action": "list" });
    bridge::AppBridge::for_commands(&app).document_components(request)
}

/// Runs a component or selection action: create, rename, remove, transform, members,
/// apply_transform or select.
#[tauri::command]
fn component_action(app: tauri::AppHandle, request: Value) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_components(request)
}

/// Dry-runs an edit batch: nothing is committed and a bounded preview handle comes back.
#[tauri::command]
fn edit_preview(app: tauri::AppHandle, request: Value) -> Result<Value, String> {
    let mut request = request;
    if let Some(object) = request.as_object_mut() {
        object.insert("dry_run".to_owned(), Value::Bool(true));
        object.insert("display".to_owned(), Value::Bool(false));
    }
    bridge::AppBridge::for_commands(&app).document_edit_batch(request)
}

/// PLY bytes of a preview candidate, so the viewer can show it without committing anything.
#[tauri::command]
fn preview_splat_bytes(state: State<'_, AppState>, preview_id: u64) -> Result<Response, String> {
    Ok(Response::new(state.preview_ply_bytes(preview_id)?))
}

/// Commits the candidate a dry run retained, if it is still the current revision.
#[tauri::command]
fn commit_preview(app: tauri::AppHandle, request: Value) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_commit_preview(request)
}

/// Applies an edit batch as one atomic transaction.
#[tauri::command]
fn edit_batch(app: tauri::AppHandle, request: Value) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_edit_batch(request)
}

/// Undo/redo availability and the retained steps of the displayed document.
#[tauri::command]
fn edit_history(app: tauri::AppHandle, request: Option<Value>) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_history(request.unwrap_or(Value::Null))
}

/// Undoes the newest step as a new revision.
#[tauri::command]
fn edit_undo(app: tauri::AppHandle, request: Option<Value>) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_undo(request.unwrap_or(Value::Null), true)
}

/// Redoes the newest undone step as a new revision.
#[tauri::command]
fn edit_redo(app: tauri::AppHandle, request: Option<Value>) -> Result<Value, String> {
    bridge::AppBridge::for_commands(&app).document_undo(request.unwrap_or(Value::Null), false)
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
            python::python_write_script,
            component_list,
            component_action,
            edit_batch,
            edit_preview,
            preview_splat_bytes,
            commit_preview,
            edit_history,
            edit_undo,
            edit_redo
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
                    eprintln!("splatmcp: set SPLATMCP_DATA_DIR to a writable folder to enable it");
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
