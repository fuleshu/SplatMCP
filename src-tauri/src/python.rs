//! The app's Python generation host.
//!
//! This module owns the single [`GenerationService`] the desktop process runs, the
//! [`DocumentTarget`] that lets it commit into the displayed document, and the Tauri
//! commands and bridge methods both call into it. MCP-started jobs and panel-started jobs
//! are therefore the same kind of job on the same document.
//!
//! Publication is revision addressed: a committed job emits a small
//! [`REVISION_EVENT`] with its identity, the frontend fetches the exact revision as binary
//! bytes through `splat_bytes_for_revision`, and it acknowledges the render separately. No
//! geometry travels through MCP, a base64 event, or a per-gaussian Tauri call, and a
//! committed revision is never reported as displayed until the viewer says so.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use splatmcp_python::arrays::{BatchMetadata, BoundsOut, GaussianBatch};
use splatmcp_python::executor::{ExecutorConfig, SourceSnapshot};
use splatmcp_python::runtime::{Limits, PythonRuntime, RuntimeRoots};
use splatmcp_python::script::ScriptSnapshot;
use splatmcp_python::{
    CommitOutcome, CommitRequest, DocumentIdentity, DocumentTarget, GenerationRequest,
    GenerationService, JobReceipt, JobSummary, JobView, PublishOptions, PythonError,
    RuntimeReport, ServiceConfig, TargetSpec,
};
use splatmcp_python::executor::{RunnerInfo, ScriptRunner};
use tauri::{AppHandle, Emitter, Manager};

use splatmcp_core::Mutation;
use splatmcp_core::document::{
    DocumentError, DocumentHandle, DocumentId, DocumentMetadata, Expected,
};

use crate::document::{AppState, ServiceError, SplatInfo};
use crate::viewer::VIEWER_WINDOW;

/// Event the app emits when a revision is committed and should be displayed.
pub const REVISION_EVENT: &str = "splat://revision";

/// Payload of [`REVISION_EVENT`]: identity only, never geometry.
#[derive(Debug, Clone, Serialize)]
pub struct RevisionPayload {
    pub revision: u64,
    pub document_id: String,
    pub file_name: String,
    pub point_count: usize,
    pub component_id: Option<String>,
    /// True when the viewer should re-frame the new revision.
    pub frame: bool,
}

/// The app's Python host: one service, one interpreter, one document.
pub struct PythonHost {
    service: Arc<GenerationService>,
}

impl PythonHost {
    /// Builds the host.
    ///
    /// The runtime is looked up in this order: `SPLATMCP_PYTHON_HOME`, the copy the
    /// installer placed in the application's resources, then one provisioned into the
    /// app's data directory. A missing or broken runtime does not stop the app: the
    /// service is created with a runner that reports why generation is unavailable, and
    /// the viewer, open, save and edit paths keep working.
    pub fn start(app: &AppHandle) -> Self {
        let config = ServiceConfig {
            executor: ExecutorConfig::default(),
        };
        let target: Arc<dyn DocumentTarget> = Arc::new(AppDocumentTarget { app: app.clone() });

        let bundled_root = bundled_runtime_dir(app);
        let roots = RuntimeRoots::from_env(bundled_root, Some(crate::paths::python_runtime_dir()));
        let (runner, report): (Arc<dyn ScriptRunner>, RuntimeReport) =
            match PythonRuntime::discover(&roots) {
                Ok(runtime) => {
                    println!(
                        "splatmcp: python runtime {} ({})",
                        runtime.root().display(),
                        runtime.source().name()
                    );
                    let report = runtime.report(&Limits::of(&config.executor));
                    (
                        Arc::new(splatmcp_python::embedded::PythonRunner::new(runtime)),
                        report,
                    )
                }
                Err(error) => {
                    eprintln!("splatmcp: {error}");
                    (
                        Arc::new(UnavailableRunner {
                            message: error.to_string(),
                        }),
                        RuntimeReport::unavailable(error.to_string(), &Limits::of(&config.executor)),
                    )
                }
            };
        let service = Arc::new(GenerationService::start(runner, config, target, report));
        Self { service }
    }

    /// Readiness, versions, limits and queue state.
    pub fn runtime_info(&self) -> Value {
        let report = self.service.runtime_report();
        let mut value = serde_json::to_value(&report).unwrap_or(Value::Null);
        // Tauri resolves resource paths through the Windows verbatim form (`\\?\C:\...`).
        // It is correct but unreadable in a reply a person or a model reads, and it is what
        // a caller would have to paste into a bug report.
        if let Some(object) = value.as_object_mut() {
            for key in ["root", "interpreter"] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    let readable = readable_path(text);
                    object.insert(key.to_owned(), json!(readable));
                }
            }
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("busy".to_owned(), json!(self.service.is_busy()));
            object.insert("queued_jobs".to_owned(), json!(self.service.queued_jobs()));
        }
        value
    }

    /// Files a job and returns its receipt.
    pub fn submit(&self, request: splatmcp_bridge::PythonRunRequest) -> Result<JobReceipt, String> {
        request.validate().map_err(|error| error.to_string())?;
        let entry_point = request.entry_point();
        let snapshot = match (&request.code, &request.script_path) {
            (Some(code), None) => {
                ScriptSnapshot::inline(&request.request_id, code, entry_point, request.params.clone(), request.seed)
            }
            (None, Some(path)) => ScriptSnapshot::from_file(
                &request.request_id,
                std::path::Path::new(path),
                entry_point,
                request.params.clone(),
                request.seed,
            ),
            _ => Err(PythonError::Script(
                "pass either code or script_path, not both".to_owned(),
            )),
        }
        .map_err(|error| error.to_string())?;

        // Three shapes of job, told apart by what the caller stated:
        // - a named document and revision: an edit of that exact revision
        // - an expected revision without a document id: an edit of whatever is open, which
        //   must still be at that revision. Callers normally do not know the document id,
        //   so requiring it here would make a stale edit undetectable.
        // - neither: a new document that replaces what is displayed, named by `file_name`.
        let target = match (request.document_id.clone(), request.expected_revision) {
            (Some(document_id), expected_revision) => TargetSpec {
                document_id: Some(document_id),
                component_id: request.component_id.clone(),
                expected_revision,
                file_name: None,
            },
            (None, Some(expected_revision)) => TargetSpec {
                document_id: None,
                component_id: request.component_id.clone(),
                expected_revision: Some(expected_revision),
                file_name: None,
            },
            (None, None) => TargetSpec::new_document(request.file_name.clone()),
        };

        let generation = GenerationRequest {
            snapshot,
            target,
            display: request.display.unwrap_or(true),
            // Default to framing: a first look at a new object is what a caller almost
            // always wants, and preserving the camera is the exception an agent asks for.
            frame: request.frame.unwrap_or(true),
            export_path: request.export_path.as_deref().map(PathBuf::from),
            deadline: request
                .deadline_seconds
                .map(std::time::Duration::from_secs),
        };
        self.service.submit(generation).map_err(|error| error.to_string())
    }

    /// Status of one job.
    pub fn status(&self, job_id: u64, log_after: u64, log_limit: usize) -> Result<JobView, String> {
        self.service
            .status(job_id, log_after, log_limit)
            .map_err(|error| error.to_string())
    }

    /// Job history, newest first.
    pub fn recent(&self, limit: usize) -> Vec<JobSummary> {
        self.service.recent(limit)
    }

    /// Asks a job to stop.
    pub fn cancel(&self, job_id: u64) -> Result<splatmcp_python::CancelView, String> {
        self.service.cancel(job_id).map_err(|error| error.to_string())
    }

    /// Records that the viewer rendered a revision.
    pub fn note_rendered(&self, revision: u64) -> Option<u64> {
        self.service.note_rendered(revision)
    }

    /// Records that loading a revision failed in the viewer.
    pub fn note_display_failed(&self, revision: u64, message: String) -> Option<u64> {
        self.service.note_display_failed(revision, message)
    }

    /// Stops the executor, cancelling anything still queued.
    pub fn shutdown(&self) {
        self.service.shutdown();
    }

}

/// Strips a Windows verbatim prefix so a reported path reads like the one a user sees.
fn readable_path(path: &str) -> String {
    path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
}

/// The runtime directory the installer ships, if this build has one.
///
/// A development build has no resource directory yet, so a failure here is normal and only
/// means the search moves on to the other roots.
fn bundled_runtime_dir(app: &AppHandle) -> Option<PathBuf> {
    let resources = app.path().resource_dir().ok()?;
    let candidate = resources.join("python-runtime");
    candidate.join("runtime-manifest.json").is_file().then_some(candidate)
}

/// The document owner: it reads and commits the app's displayed splat.
struct AppDocumentTarget {
    app: AppHandle,
}

impl DocumentTarget for AppDocumentTarget {
    /// Reads one exact revision of one exact document, as a detached copy.
    ///
    /// The expectation is checked here rather than at commit time, so a job that names a
    /// document which is no longer displayed - or a revision that has moved on - fails at
    /// request receipt instead of computing a candidate that could never be applied.
    fn snapshot(&self, target: &TargetSpec) -> splatmcp_python::Result<SourceSnapshot> {
        let state = self.app.state::<AppState>();
        let expected = expected_for(target)?;
        let snapshot = state
            .snapshot(expected)
            .map_err(|error| PythonError::DocumentConflict(error.to_string()))?;
        Ok(SourceSnapshot {
            document_id: snapshot.handle().document_id.to_string(),
            revision: snapshot.handle().revision,
            component_id: target.component_id.clone(),
            // A detached copy: the script cannot see or tear the live document, and the
            // copy stays readable for the life of the job.
            batch: GaussianBatch::from_splat(snapshot.splat(), BatchMetadata::default()),
        })
    }

    fn commit(&self, request: CommitRequest) -> splatmcp_python::Result<CommitOutcome> {
        // Committing only. The service decides whether the result is shown, and publishing
        // here would make `display: false` mean nothing: the viewer would be told about the
        // revision and the user's model would change under them.
        let state = self.app.state::<AppState>();
        let component = request.target.component_id.clone();
        let operation = match &component {
            Some(component) => format!("job ({component})"),
            None => "job".to_owned(),
        };
        // The producer record travels verbatim as JSON: the store neither parses nor
        // interprets a recipe, and a save can still write it beside the exported file.
        let recipe = serde_json::to_string(&request.provenance).ok();
        let with_component = |mutation: Mutation| match &component {
            Some(component) => mutation.component(component.clone()),
            None => mutation,
        };

        // A job that targets a new document has nothing to read and nothing to compare:
        // its geometry becomes a document of its own, named as the request asked.
        if request.target.is_new_document() {
            let file_name = request
                .target
                .file_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "generated.ply".to_owned());
            let metadata = state
                .open_splat(
                    request.splat,
                    with_component(Mutation::job(&operation, recipe).file_name(file_name)),
                )
                .map_err(|error| PythonError::DocumentConflict(error.to_string()))?;
            return Ok(CommitOutcome::Committed {
                identity: identity_of(&metadata),
            });
        }

        let expected = expected_for(&request.target)?;
        let mutation = with_component(Mutation::job(&operation, recipe));
        match state.commit(expected, request.splat, mutation) {
            Ok(metadata) => Ok(CommitOutcome::Committed {
                identity: identity_of(&metadata),
            }),
            Err(error) => match conflict_of(&error, &request.target) {
                Some(conflict) => Ok(conflict),
                None => Err(PythonError::DocumentConflict(error.to_string())),
            },
        }
    }
    fn publish(
        &self,
        _target: &TargetSpec,
        identity: &DocumentIdentity,
        options: PublishOptions,
    ) -> splatmcp_python::Result<()> {
        self.publish_revision(identity, options.frame)
            .map_err(PythonError::Display)
    }
}

impl AppDocumentTarget {
    /// Emits the revision identity the viewer loads from.
    fn publish_revision(&self, identity: &DocumentIdentity, frame: bool) -> Result<(), String> {
        let state = self.app.state::<AppState>();
        let file_name = state
            .metadata()
            .map(|metadata| metadata.provenance.file_name)
            .unwrap_or_else(|| "splat.ply".to_owned());
        let payload = RevisionPayload {
            revision: identity.revision,
            document_id: identity.document_id.clone(),
            file_name,
            point_count: identity.point_count,
            component_id: identity.component_id.clone(),
            frame,
        };
        self.app
            .emit_to(VIEWER_WINDOW, REVISION_EVENT, payload)
            .map_err(|error| format!("could not tell the viewer about revision {}: {error}", identity.revision))
    }
}

/// The expectation a job's target states, in the store's terms.
///
/// A job that names a document must state its revision: the generation service already
/// enforces that for a Python recipe, and the same rule keeps any other caller from
/// overwriting work it never read.
fn expected_for(target: &TargetSpec) -> splatmcp_python::Result<Expected> {
    match (target.document_id.as_deref(), target.expected_revision) {
        (Some(text), Some(revision)) => {
            let document_id = DocumentId::parse(text).ok_or_else(|| {
                PythonError::DocumentConflict(format!("'{text}' is not a document id"))
            })?;
            Ok(Expected::Handle(DocumentHandle::new(document_id, revision)))
        }
        (Some(text), None) => Err(PythonError::DocumentConflict(format!(
            "document '{text}' needs expected_revision, so concurrent changes are reported \
             instead of overwritten"
        ))),
        (None, Some(revision)) => Ok(Expected::Revision(revision)),
        (None, None) => Ok(Expected::Any),
    }
}

/// Identity of a committed revision, in the generation service's vocabulary.
fn identity_of(metadata: &DocumentMetadata) -> DocumentIdentity {
    DocumentIdentity {
        document_id: metadata.handle.document_id.to_string(),
        revision: metadata.handle.revision,
        point_count: metadata.point_count,
        bounds: metadata.bounds.map(BoundsOut::from),
        component_id: metadata.provenance.component_id.clone(),
    }
}

/// Turns a stale revision into the explicit conflict the service reports.
///
/// Only a revision conflict becomes a `Conflict` outcome; every other failure - a document
/// that is not displayed, an expired snapshot, an invalid id - is an error, because there is
/// nothing to reconcile and the job must not look like it merely arrived late.
fn conflict_of(error: &ServiceError, target: &TargetSpec) -> Option<CommitOutcome> {
    let DocumentError::Conflict { expected, current } = error.document_error()? else {
        return None;
    };
    Some(CommitOutcome::Conflict {
        document_id: target
            .document_id
            .clone()
            .unwrap_or_else(|| current.document_id.to_string()),
        expected: expected.revision,
        actual: current.revision,
    })
}

/// Runner used when no usable runtime was found.
///
/// It exists so the app can start and report the problem instead of failing to launch: the
/// viewer, open, save and edit tools must work without Python.
struct UnavailableRunner {
    message: String,
}

impl ScriptRunner for UnavailableRunner {
    fn describe(&self) -> RunnerInfo {
        RunnerInfo {
            ready: false,
            interpreter: String::new(),
            python_version: None,
            error: Some(self.message.clone()),
            packages: Vec::new(),
        }
    }

    fn run(&self, _context: &Arc<splatmcp_python::RunContext>) -> splatmcp_python::Result<GaussianBatch> {
        Err(PythonError::RuntimeUnavailable(self.message.clone()))
    }
}

/// Tauri command: readiness, versions and limits of the embedded runtime.
#[tauri::command]
pub fn python_runtime_info(host: tauri::State<'_, PythonHostState>) -> Value {
    host.0.runtime_info()
}

/// Tauri command: submit a generation job.
#[tauri::command]
pub fn python_submit(
    request: splatmcp_bridge::PythonRunRequest,
    host: tauri::State<'_, PythonHostState>,
) -> Result<JobReceipt, String> {
    host.0.submit(request)
}

/// Tauri command: read a job, or the job history when `job_id` is zero.
#[tauri::command]
pub fn python_job(query: splatmcp_bridge::PythonJobQuery, host: tauri::State<'_, PythonHostState>) -> Result<Value, String> {
    if query.job_id == 0 {
        let recent = host.0.recent(query.log_limit.unwrap_or(20).min(100));
        return Ok(json!({ "recent": recent }));
    }
    let view = host.0.status(
        query.job_id,
        query.log_after.unwrap_or(0),
        query.log_limit.unwrap_or(200).min(1000),
    )?;
    serde_json::to_value(view).map_err(|error| error.to_string())
}

/// Tauri command: cancel a job.
#[tauri::command]
pub fn python_job_cancel(
    request: splatmcp_bridge::PythonCancelRequest,
    host: tauri::State<'_, PythonHostState>,
) -> Result<splatmcp_python::CancelView, String> {
    host.0.cancel(request.job_id)
}

/// Tauri command: the viewer rendered a revision.
#[tauri::command]
pub fn python_note_rendered(revision: u64, host: tauri::State<'_, PythonHostState>) -> Option<u64> {
    host.0.note_rendered(revision)
}

/// Tauri command: the viewer failed to load a revision.
#[tauri::command]
pub fn python_note_display_failed(
    revision: u64,
    message: String,
    host: tauri::State<'_, PythonHostState>,
) -> Option<u64> {
    host.0.note_display_failed(revision, message)
}

/// Tauri command: the exact PLY bytes of one revision of the displayed document.
///
/// The viewer asks for the revision it was told about, so a mismatch or an evicted revision
/// is reported instead of silently displaying newer geometry.
#[tauri::command]
pub fn splat_bytes_for_revision(
    revision: u64,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let handle = state
        .active_handle()
        .ok_or_else(|| "no splat is loaded".to_owned())?;
    let (_, bytes) = state.ply_bytes_for(&DocumentHandle::new(handle.document_id, revision))?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Tauri command: the displayed document's identity, revision and provenance.
#[tauri::command]
pub fn document_info(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    match state.metadata() {
        Some(metadata) => {
            let info = SplatInfo::of(&metadata);
            let mut value = serde_json::to_value(&info).map_err(|error| error.to_string())?;
            if let Some(object) = value.as_object_mut() {
                let retention = state.retention();
                object.insert(
                    "retained_revisions".to_owned(),
                    json!(metadata.retained_revisions),
                );
                object.insert(
                    "retention".to_owned(),
                    json!({
                        "documents": retention.documents,
                        "revisions": retention.revisions,
                        "bytes": retention.bytes,
                        "pins": retention.pins,
                        "over_budget": retention.over_budget(),
                    }),
                );
            }
            Ok(value)
        }
        None => Ok(json!({ "loaded": false })),
    }
}

/// Tauri command: read a script file for the panel's editor.
#[tauri::command]
pub fn python_read_script(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|error| format!("could not read {path}: {error}"))
}

/// Tauri command: write the panel's editor content back to a script file.
#[tauri::command]
pub fn python_write_script(path: String, text: String) -> Result<String, String> {
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {path}: {error}"))?;
    Ok(path)
}

/// Managed wrapper so the bridge handler and the commands share one host.
pub struct PythonHostState(pub Arc<PythonHost>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_runner_reports_why_and_refuses_jobs() {
        let runner = UnavailableRunner {
            message: "python_runtime_unavailable: no interpreter".to_owned(),
        };
        let info = runner.describe();
        assert!(!info.ready);
        assert!(info.error.unwrap().contains("no interpreter"));
    }

}
