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

mod assets;
mod authoring;
mod bridge;
mod document;
mod jobs;
mod paths;
mod publication;
mod python;
mod settings;
mod viewer;

use std::sync::Arc;

use document::AppState;
use serde_json::{Value, json};
use splatmcp_core::{Expected, PlyImportPolicy};
use tauri::ipc::Response;
use tauri::{Manager, State};
use viewer::Viewer;

/// Number of bytes an export wrote.
fn outcome_bytes(outcome: &document::ExportOutcome) -> usize {
    outcome.bytes
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
    // Opening a file is the strict import path: a file that needs repair is refused with
    // its indexed reason instead of being loaded as a quietly repaired document.
    let imported = state.open_ply(
        &bytes,
        document::open_mutation(&path),
        PlyImportPolicy::Strict,
    )?;
    if !imported.report.is_lossless() {
        println!("splatmcp: import report: {}", imported.report.summary());
    }
    // Component metadata is restored here, from the sidecar that describes exactly these bytes:
    // the note (a restore, or the reason nothing was attached) travels in the reply.
    let note = bridge::restore_note(&state, &path, &imported, &bytes);
    Ok(Some(document::SplatInfo::of_with_note(
        &imported.metadata,
        note,
    )))
}

/// Reads the source file of the displayed document again, as a new revision.
#[tauri::command]
fn reload_splat(state: State<'_, AppState>) -> Result<document::SplatInfo, String> {
    let imported = state.reload(Expected::Any, PlyImportPolicy::Strict)?;
    if !imported.report.is_lossless() {
        println!("splatmcp: import report: {}", imported.report.summary());
    }
    Ok(document::SplatInfo::of(&imported.metadata))
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
    if let Ok((handle, layer)) = state.authoring_snapshot(Expected::Any)
        && !layer.components().is_empty()
    {
        let record = authoring::record(
            handle.document_id.as_str(),
            handle.revision,
            &outcome.checksum,
            outcome_bytes(&outcome),
            &layer,
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

/// Records that the window displayed one exact revision.
///
/// A commit reports `published` until this arrives: the app announcing a revision proves the
/// announcement, not the picture, and this is the acknowledgement that closes that gap. The
/// `token` names the publication request, so a late load of an older revision is refused
/// instead of being recorded as the picture.
#[tauri::command]
fn edit_note_displayed(
    state: State<'_, AppState>,
    publications: State<'_, publication::PublicationHostState>,
    document_id: String,
    revision: u64,
    token: Option<u64>,
) -> Result<Value, String> {
    let handle = document::handle_of(&document_id, revision)?;
    let recorded = state.note_side_effect(
        &handle,
        splatmcp_core::ReceiptSlot::Display,
        splatmcp_core::SideEffect::Done,
    );
    let publications = publications.0.clone();
    let status = match token {
        Some(token) => match publications.acknowledge(&document_id, revision, token) {
            Ok(status) => status,
            Err(error) => {
                // A stale acknowledgement is reported, not swallowed: the display state keeps
                // whatever was actually there, and the caller learns why.
                return Ok(json!({
                    "recorded": false,
                    "receipt": recorded,
                    "error": crate::publication::error_json(&error),
                }));
            }
        },
        // No token: an older caller. The receipt is updated, and the publication status is
        // left as it was - an acknowledgement without a request identity cannot prove which
        // request it answers.
        None => return Ok(json!({"recorded": recorded, "token": Value::Null})),
    };
    Ok(json!({
        "recorded": recorded,
        "receipt": recorded,
        "status": crate::publication::status_json(&status),
    }))
}

/// Records that the window could not display one exact revision.
///
/// The previously displayed model stays on screen: this reports a publication failure without
/// touching what a frame presented.
#[tauri::command]
fn edit_note_display_failed(
    state: State<'_, AppState>,
    publications: State<'_, publication::PublicationHostState>,
    document_id: String,
    revision: u64,
    message: String,
) -> Result<Value, String> {
    let handle = document::handle_of(&document_id, revision)?;
    let recorded = state.note_side_effect(
        &handle,
        splatmcp_core::ReceiptSlot::Display,
        splatmcp_core::SideEffect::Failed(message.clone()),
    );
    let publications = publications.0.clone();
    match publications.fail(&document_id, revision, message) {
        Ok(status) => Ok(json!({
            "recorded": recorded,
            "status": crate::publication::status_json(&status),
        })),
        Err(error) => Ok(json!({
            "recorded": recorded,
            "error": crate::publication::error_json(&error),
        })),
    }
}

/// Bounded markers that show where a selection handle's gaussians are.
///
/// The reply carries a small PLY of bright markers at exactly those positions, in document
/// space, so the window draws the *same* gaussians a tool call selected instead of a second
/// opinion about "the selection".
#[tauri::command]
fn selection_highlight(
    state: State<'_, AppState>,
    handle_id: u64,
    max_markers: Option<usize>,
) -> Result<Value, String> {
    let limit = max_markers.unwrap_or(512).clamp(1, 4096);
    let markers = state
        .selection_markers(handle_id, limit)
        .map_err(|error| error.to_string())?;
    let ply = state
        .marker_ply_bytes(&markers)
        .map_err(|error| error.to_string())?;
    let mut value = serde_json::to_value(&markers).map_err(|error| error.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.insert("marker_count".to_owned(), json!(markers.shown));
        object.insert("truncated".to_owned(), json!(markers.truncated()));
        object.insert("ply_base64".to_owned(), json!(base64_of(&ply)));
    }
    Ok(value)
}

/// Base64 of a byte buffer, for a JSON reply that carries bytes.
fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
            assets::asset_register,
            assets::asset_info,
            assets::asset_release,
            assets::asset_upload,
            publication::publication_status,
            publication::renderer_capabilities,
            publication::splat_bytes_for_handle,
            jobs::job_import,
            jobs::job_export,
            jobs::job_inspect,
            jobs::job_status,
            jobs::job_list,
            jobs::job_cancel,
            jobs::job_stats,
            jobs::job_wait,
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
            edit_redo,
            edit_note_displayed,
            edit_note_display_failed,
            selection_highlight
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
            app.manage(python::PythonHostState(python.clone()));            // One asset registry for the whole process: the bridge and the window register
            // and resolve the same ids, so a payload is never copied to be shared.
            let assets = Arc::new(assets::AssetHost::default());
            app.manage(assets::AssetHostState(assets.clone()));
            // One job service for the whole process: an import started from MCP and one
            // started from the window are the same queue, with the same states and receipts.
            let jobs = Arc::new(jobs::JobHost::default());
            app.manage(jobs::JobHostState(jobs.clone()));
            // One publication tracker: it is the app's single answer to "which revision is the
            // viewer showing?", kept apart from the committed revisions in the store.
            let publications = Arc::new(publication::PublicationHost::default());
            app.manage(publication::PublicationHostState(publications.clone()));
            // A bridge failure must not stop the viewer from working: without a data
            // directory only the MCP half of the app is unavailable.
            match bridge::start(&handle, viewer, python, assets) {
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
            // Queued jobs are cancelled outright and running ones are asked to stop; nothing
            // is rerun on the next start.
            handle.state::<jobs::JobHostState>().0.shutdown();
        }
    });
}
