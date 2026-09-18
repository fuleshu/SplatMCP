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
use splatmcp_python::executor::{RunnerInfo, ScriptRunner};
use splatmcp_python::runtime::{Limits, PythonRuntime, RuntimeRoots};
use splatmcp_python::script::ScriptSnapshot;
use splatmcp_python::{
    CommitOutcome, CommitRequest, DocumentIdentity, DocumentTarget, GenerationRequest,
    GenerationService, PublishOptions, PythonError, RuntimeReport, ServiceConfig, TargetSpec,
};
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
///
/// The `token` is what makes the acknowledgement unambiguous. The viewer fetches the bytes for
/// this exact `(document, revision)` and then reports that it displayed *this request*, so a
/// load that finishes after a newer one was already shown cannot claim the screen.
#[derive(Debug, Clone, Serialize)]
pub struct RevisionPayload {
    pub revision: u64,
    pub document_id: String,
    /// Publication request this event belongs to, minted by the app before the event is sent.
    pub token: u64,
    /// `committed` for a document revision, `preview` for a retained candidate.
    pub source: String,
    pub file_name: String,
    pub point_count: usize,
    pub component_id: Option<String>,
    /// True when the viewer should re-frame the new revision.
    pub frame: bool,
}

/// Drives one script job on the shared job service, with the interpreter as the executor.
///
/// The engine still owns everything about running a script - the one interpreter, its queue, its
/// validation and its commit - and this adapter owns the *job record*: phases, progress, logs,
/// cancellation and the receipt all come from the shared service, so a script job is described
/// exactly like an import or an export.
///
/// Cancellation is delegated rather than assumed. When the caller asks to stop (or the deadline
/// passes), the engine is asked to cancel and the adapter keeps polling until the engine settles:
/// a script that had already committed is reported as committed, never as cancelled, because
/// "cancelled" must not be claimed while a result can still be published.
fn run_python_job(
    context: &splatmcp_core::JobContext,
    engine: Arc<GenerationService>,
    index: Arc<std::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
    generation: GenerationRequest,
) -> Result<splatmcp_core::JobResult, splatmcp_core::JobFailure> {
    let _ = context.check()?;
    context.progress(splatmcp_core::JobPhase::Computing, 0, Some(100), Some("starting the interpreter".to_owned()));
    let receipt = engine
        .submit(generation)
        .map_err(|error| python_failure(&error))?;
    let engine_id = receipt.job_id;
    if let Ok(mut ids) = index.lock() {
        ids.insert(context.job_id().to_string(), engine_id);
    }
    context.log(
        splatmcp_core::LogLevel::Info,
        format!(
            "engine job {engine_id} accepted for request '{}' ({})",
            receipt.request_id,
            receipt.state.name()
        ),
    );

    let mut log_after = 0u64;
    let mut cancel_forwarded = false;
    loop {
        let view = engine
            .status(engine_id, log_after, 100)
            .map_err(|error| python_failure(&error))?;
        for line in &view.logs {
            log_after = log_after.max(line.seq);
            context.log(log_level(line.level), line.text.clone());
        }
        let done = (view.progress.clamp(0.0, 1.0) * 100.0).round() as u64;
        context.progress(
            python_phase(view.state),
            done,
            Some(100),
            view.progress_message.clone(),
        );
        if view.state.is_terminal() {
            return python_outcome(&view);
        }
        // Cooperative: ask the engine to stop, then keep reading until it settles.
        let stop_requested = context.is_cancelled() || context.check().is_err();
        if stop_requested && !cancel_forwarded {
            context.log(
                splatmcp_core::LogLevel::Warning,
                "cancellation requested; asking the interpreter to stop at its next checkpoint",
            );
            let _ = engine.cancel(engine_id);
            cancel_forwarded = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

/// Maps the engine's terminal state onto the shared receipt's result or failure.
fn python_outcome(
    view: &splatmcp_python::JobView,
) -> Result<splatmcp_core::JobResult, splatmcp_core::JobFailure> {
    match view.state {
        splatmcp_python::JobState::Committed => {
            let revision = view.revision.unwrap_or_default();
            let document_id = view
                .document_id
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            Ok(splatmcp_core::JobResult::Document {
                document_id,
                revision,
                point_count: view.point_count.unwrap_or_default(),
            })
        }
        splatmcp_python::JobState::Cancelled => Err(splatmcp_core::JobFailure::cancelled()),
        splatmcp_python::JobState::Conflict => Err(splatmcp_core::JobFailure::conflict(
            view.error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "the document moved on before the script committed".to_owned()),
        )),
        splatmcp_python::JobState::Failed => Err(view
            .error
            .as_ref()
            .map(|error| splatmcp_core::JobFailure::new(error.code.clone(), error.message.clone()))
            .unwrap_or_else(|| {
                splatmcp_core::JobFailure::new("script_failed", "the script did not complete")
            })),
        other => Err(splatmcp_core::JobFailure::new(
            "script_incomplete",
            format!("the script job ended in {}", other.name()),
        )),
    }
}

/// Names the document a script job targets, the way a receipt and a conflict message quote it.
fn describe_target(target: &TargetSpec) -> String {
    match (&target.document_id, target.expected_revision) {
        (Some(document_id), Some(revision)) => format!("{document_id}@{revision}"),
        (Some(document_id), None) => document_id.clone(),
        (None, Some(revision)) => format!("the displayed document at revision {revision}"),
        (None, None) => "a new document".to_owned(),
    }
}

/// A python error as a job failure, keeping the engine's own stable code.
fn python_failure(error: &PythonError) -> splatmcp_core::JobFailure {
    splatmcp_core::JobFailure::new(error.code(), error.to_string())
}

/// The shared phase that matches an engine state.
fn python_phase(state: splatmcp_python::JobState) -> splatmcp_core::JobPhase {
    use splatmcp_core::JobPhase;
    match state {
        splatmcp_python::JobState::Queued => JobPhase::Admitted,
        splatmcp_python::JobState::Running => JobPhase::Computing,
        splatmcp_python::JobState::CancelRequested => JobPhase::Computing,
        splatmcp_python::JobState::Validating => JobPhase::Validating,
        splatmcp_python::JobState::Committing => JobPhase::Committing,
        // Terminal states keep the phase they reached; the receipt's state carries the outcome.
        _ => JobPhase::Computing,
    }
}

/// The shared log level that matches an engine log line.
fn log_level(level: splatmcp_python::LogLevel) -> splatmcp_core::LogLevel {
    match level {
        // A debug line from a script is informational for the job receipt.
        splatmcp_python::LogLevel::Debug | splatmcp_python::LogLevel::Info => {
            splatmcp_core::LogLevel::Info
        }
        splatmcp_python::LogLevel::Warning => splatmcp_core::LogLevel::Warning,
        splatmcp_python::LogLevel::Error => splatmcp_core::LogLevel::Error,
    }
}

/// The app's Python host: one engine, one interpreter, one document.
///
/// The engine owns the interpreter and its scheduling (task #10's contract). It does **not** own
/// the app's job records: a script job is admitted by the shared [`crate::jobs::JobHost`], so it
/// appears in the same list, with the same states, progress and receipts, as an import or an
/// export. What this host keeps is a small index from the shared job id to the engine's own job
/// id, which is an adapter mapping - not a second status model.
pub struct PythonHost {
    service: Arc<GenerationService>,
    /// Shared job id -> engine job id, for the jobs this process started.
    engine_ids: Arc<std::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
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
                        RuntimeReport::unavailable(
                            error.to_string(),
                            &Limits::of(&config.executor),
                        ),
                    )
                }
            };
        let service = Arc::new(GenerationService::start(runner, config, target, report));
        Self {
            service,
            engine_ids: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        }
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

    /// Files a script job on the shared job service and returns its admission.
    ///
    /// Submission returns as soon as the job is admitted, exactly like an import or an export:
    /// the caller polls the returned job id, whose receipt is the shared one. Retry identity is
    /// the request id plus the whole semantic request, so a *changed* script under the same
    /// request id is refused rather than answered with the earlier receipt.
    pub fn submit(
        &self,
        jobs: &Arc<crate::jobs::JobHost>,
        request: splatmcp_bridge::PythonRunRequest,
    ) -> Result<splatmcp_core::JobAdmission, String> {
        let generation = self.generation_request(&request)?;
        let job_request = splatmcp_core::JobRequest::new(splatmcp_core::JobKind::Generate, "run_python_splat")
            .with_operation_id(request.request_id.clone())
            .with_target(format!(
                "{} for {}",
                request
                    .script_path
                    .clone()
                    .unwrap_or_else(|| format!("inline script {}", request.request_id)),
                describe_target(&generation.target)
            ))
            .with_document(
                generation.target.document_id.clone().unwrap_or_default(),
                generation.target.expected_revision,
            );
        let engine = Arc::clone(&self.service);
        let index = Arc::clone(&self.engine_ids);
        jobs.submit(
            job_request,
            Box::new(move |context| run_python_job(context, engine, index, generation)),
        )
        .map_err(|error| error.to_string())
    }

    /// Validates a run request and turns it into the engine's own generation request.
    fn generation_request(
        &self,
        request: &splatmcp_bridge::PythonRunRequest,
    ) -> Result<GenerationRequest, String> {
        request.validate().map_err(|error| error.to_string())?;
        let entry_point = request.entry_point();
        let snapshot = match (&request.code, &request.script_path) {
            (Some(code), None) => ScriptSnapshot::inline(
                &request.request_id,
                code,
                entry_point,
                request.params.clone(),
                request.seed,
            ),
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

        Ok(GenerationRequest {
            snapshot,
            target,
            display: request.display.unwrap_or(true),
            // Default to framing: a first look at a new object is what a caller almost
            // always wants, and preserving the camera is the exception an agent asks for.
            frame: request.frame.unwrap_or(true),
            export_path: request.export_path.as_deref().map(PathBuf::from),
            deadline: request.deadline_seconds.map(std::time::Duration::from_secs),
        })
    }

    /// The engine job id behind a shared job id, when this process started it.
    pub fn engine_id(&self, job_id: &str) -> Option<u64> {
        self.engine_ids.lock().ok()?.get(job_id).copied()
    }

    /// Asks the engine to stop a job, so the shared cancellation reaches the interpreter.
    pub fn cancel_engine(&self, job_id: &str) -> Option<splatmcp_python::CancelView> {
        let engine_id = self.engine_id(job_id)?;
        self.service.cancel(engine_id).ok()
    }

    /// The engine's own detail for a job, when the engine still knows it.
    ///
    /// This is the python-specific part of a status reply - script hashes, timings, the revision
    /// the viewer acknowledged - which the shared receipt does not carry.
    pub fn engine_detail(&self, job_id: &str) -> Option<Value> {
        let engine_id = self.engine_id(job_id)?;
        let view = self.service.status(engine_id, 0, 200).ok()?;
        Some(json!({
            "engine_job_id": view.job_id,
            "request_id": view.request_id,
            "entry_point": view.entry_point,
            "script_hash": view.script_hash,
            "content_hash": view.content_hash,
            "progress_message": view.progress_message,
            "timings": view.timings,
            "displayed_revision": view.displayed_revision,
        }))
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
    candidate
        .join("runtime-manifest.json")
        .is_file()
        .then_some(candidate)
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
            .map_err(|error| python_error(&error))?;
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
                .map_err(|error| python_error(&error))?;
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
                None => Err(python_error(&error)),
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
    ///
    /// The publication request is recorded first, so the token this event carries is the one
    /// the viewer's acknowledgement must quote back. A Python job's revision therefore goes
    /// through exactly the same publication seam as an edit batch: one token, one
    /// acknowledgement, one displayed revision.
    fn publish_revision(&self, identity: &DocumentIdentity, frame: bool) -> Result<(), String> {
        let state = self.app.state::<AppState>();
        let file_name = state
            .metadata()
            .map(|metadata| metadata.provenance.file_name)
            .unwrap_or_else(|| "splat.ply".to_owned());
        let handle = match splatmcp_core::DocumentId::parse(&identity.document_id) {
            Some(document_id) => splatmcp_core::DocumentHandle::new(document_id, identity.revision),
            None => return Err(format!("'{}' is not a document id", identity.document_id)),
        };
        let publications = self
            .app
            .state::<crate::publication::PublicationHostState>()
            .0
            .clone();
        let request = publications
            .begin(&handle, splatmcp_core::PublicationSource::Committed, frame)
            .map_err(|error| format!("{} ({})", error, error.code()))?;
        let payload = RevisionPayload {
            revision: identity.revision,
            document_id: identity.document_id.clone(),
            token: request.token,
            source: request.source.as_str().to_owned(),
            file_name,
            point_count: identity.point_count,
            component_id: identity.component_id.clone(),
            frame,
        };
        self.app
            .emit_to(VIEWER_WINDOW, REVISION_EVENT, payload)
            .map_err(|error| {
                format!(
                    "could not tell the viewer about revision {}: {error}",
                    identity.revision
                )
            })
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
                PythonError::UnknownDocument(format!("'{text}' is not a document id"))
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

/// Maps a document failure onto the code that names it.
///
/// The three cases stay distinguishable, because the answer differs: a document that is no
/// longer available means the request named something that is gone, a revision conflict
/// means "re-read the revision and try again", and an expired snapshot means the revision
/// is past retention. Only an invalid *request* keeps the pre-existing `document_conflict`
/// shape, because that is the code the service already documents for a malformed target.
fn python_error(error: &ServiceError) -> PythonError {
    match error.document_error() {
        Some(document) => PythonError::document(document),
        None => PythonError::DocumentConflict(error.to_string()),
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

    fn run(
        &self,
        _context: &Arc<splatmcp_python::RunContext>,
    ) -> splatmcp_python::Result<GaussianBatch> {
        Err(PythonError::RuntimeUnavailable(self.message.clone()))
    }
}

/// Tauri command: readiness, versions and limits of the embedded runtime.
#[tauri::command]
pub fn python_runtime_info(host: tauri::State<'_, PythonHostState>) -> Value {
    host.0.runtime_info()
}

/// Tauri command: submit a generation job through the shared job service.
///
/// The reply is an admission, not a receipt: the job is running on the app's job service with the
/// interpreter as its executor, and its status is read with the same command the generic job list
/// uses.
#[tauri::command]
pub fn python_submit(
    request: splatmcp_bridge::PythonRunRequest,
    host: tauri::State<'_, PythonHostState>,
    jobs: tauri::State<'_, crate::jobs::JobHostState>,
) -> Result<Value, String> {
    let admission = host.0.submit(&jobs.0, request)?;
    serde_json::to_value(splatmcp_bridge::JobAdmissionReply {
        job_id: admission.job_id.to_string(),
        state: admission.state.as_str().to_owned(),
        replayed: admission.replayed,
        limits: jobs.0.service().limits().describe(),
    })
    .map_err(|error| error.to_string())
}

/// Tauri command: read a script job from the shared service, with the engine's detail beside it.
#[tauri::command]
pub fn python_job(
    query: splatmcp_bridge::PythonJobQuery,
    host: tauri::State<'_, PythonHostState>,
    jobs: tauri::State<'_, crate::jobs::JobHostState>,
) -> Result<Value, String> {
    if query.job_id.trim().is_empty() {
        let recent: Vec<Value> = jobs
            .0
            .recent(query.log_limit.unwrap_or(20).min(100))
            .iter()
            .map(|receipt| {
                serde_json::to_value(splatmcp_bridge::JobSummary::from(receipt))
                    .unwrap_or(Value::Null)
            })
            .collect();
        return Ok(json!({ "recent": recent }));
    }
    let job_id = crate::jobs::parse_job_id(&query.job_id)?;
    let view = jobs
        .0
        .view(
            &job_id,
            query.log_after.unwrap_or(0),
            query.log_limit.unwrap_or(200).min(500),
        )
        .map_err(|error| format!("{} ({})", error, error.code()))?;
    let mut reply = serde_json::to_value(splatmcp_bridge::JobStatusReply::from(&view))
        .map_err(|error| error.to_string())?;
    if let Some(detail) = host.0.engine_detail(&query.job_id) {
        if let Some(object) = reply.as_object_mut() {
            object.insert("python".to_owned(), detail);
        }
    }
    Ok(reply)
}

/// Tauri command: ask a script job to stop, and report what actually happened.
#[tauri::command]
pub fn python_job_cancel(
    request: splatmcp_bridge::PythonCancelRequest,
    host: tauri::State<'_, PythonHostState>,
    jobs: tauri::State<'_, crate::jobs::JobHostState>,
) -> Result<Value, String> {
    let job_id = crate::jobs::parse_job_id(&request.job_id)?;
    let engine = host.0.cancel_engine(&request.job_id);
    let receipt = jobs
        .0
        .cancel(&job_id)
        .map_err(|error| format!("{} ({})", error, error.code()))?;
    let mut reply = serde_json::to_value(splatmcp_bridge::JobSummary::from(&receipt))
        .map_err(|error| error.to_string())?;
    if let Some(object) = reply.as_object_mut() {
        object.insert(
            "engine".to_owned(),
            match engine {
                Some(view) => json!({
                    "job_id": view.job_id,
                    "state": view.state.name(),
                    "still_unwinding": view.still_unwinding,
                }),
                None => Value::Null,
            },
        );
    }
    Ok(reply)
}

/// Tauri command: the viewer rendered a revision.
///
/// The same acknowledgement also closes the shared publication request, so "what is on screen"
/// has one answer for a Python job and for an edit batch alike. A token that does not match the
/// request in flight is reported rather than recorded as the picture.
#[tauri::command]
pub fn python_note_rendered(
    app: tauri::AppHandle,
    revision: u64,
    document_id: Option<String>,
    token: Option<u64>,
    host: tauri::State<'_, PythonHostState>,
) -> Result<Value, String> {
    let job = host.0.note_rendered(revision);
    let publications = app
        .state::<crate::publication::PublicationHostState>()
        .0
        .clone();
    let acknowledged = match (document_id, token) {
        (Some(document_id), Some(token)) => {
            match publications.acknowledge(&document_id, revision, token) {
                Ok(status) => json!({ "status": crate::publication::status_json(&status) }),
                Err(error) => json!({ "error": crate::publication::error_json(&error) }),
            }
        }
        // No request identity: the job acknowledgement stands, and the publication status is
        // left as it was, because an acknowledgement cannot name a request without its token.
        _ => json!({ "token": Value::Null }),
    };
    Ok(json!({ "job": job, "publication": acknowledged }))
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

/// Tauri command: the exact PLY bytes of one revision of one named document.
///
/// The viewer asks for the revision it was told about, by identity: a revision that belongs to
/// another document, or one that has been evicted, is reported instead of quietly returning
/// whatever is displayed now.
#[tauri::command]
pub fn splat_bytes_for_revision(
    document_id: String,
    revision: u64,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let handle = crate::document::handle_of(&document_id, revision)?;
    let (_, bytes) = state.ply_bytes_for(&handle)?;
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
    std::fs::write(&path, text).map_err(|error| format!("could not write {path}: {error}"))?;
    Ok(path)
}

/// Managed wrapper so the bridge handler and the commands share one host.
pub struct PythonHostState(pub Arc<PythonHost>);

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::DocumentId;

    #[test]
    fn an_unavailable_runner_reports_why_and_refuses_jobs() {
        let runner = UnavailableRunner {
            message: "python_runtime_unavailable: no interpreter".to_owned(),
        };
        let info = runner.describe();
        assert!(!info.ready);
        assert!(info.error.unwrap().contains("no interpreter"));
    }

    #[test]
    fn a_job_keeps_the_document_failure_it_hit() {
        let handle = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 3);
        let cases = [
            (
                ServiceError::Document(DocumentError::NoDocument),
                "no_document",
            ),
            (
                ServiceError::Document(DocumentError::UnknownDocument {
                    document_id: DocumentId::mint(0x4f2a, 9),
                    active: Some(DocumentId::mint(0x4f2a, 1)),
                }),
                "unknown_document",
            ),
            (
                ServiceError::Document(DocumentError::SnapshotExpired {
                    handle: handle.clone(),
                }),
                "snapshot_expired",
            ),
            (
                ServiceError::Document(DocumentError::Conflict {
                    expected: DocumentHandle::new(handle.document_id.clone(), 2),
                    current: handle.clone(),
                }),
                "document_conflict",
            ),
            // A malformed request keeps the code the service documents for a bad target.
            (
                ServiceError::Invalid("component_id must not be blank".to_owned()),
                "document_conflict",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(python_error(&error).code(), code, "{error}");
        }
    }

    #[test]
    fn a_target_that_is_not_a_document_id_is_an_unknown_document() {
        let target = TargetSpec {
            document_id: Some("C:/tmp/scene.ply".to_owned()),
            component_id: None,
            expected_revision: Some(1),
            file_name: None,
        };
        assert_eq!(
            expected_for(&target).unwrap_err().code(),
            "unknown_document"
        );
    }

    #[test]
    fn only_a_revision_race_becomes_a_conflict_outcome() {
        let handle = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 3);
        let target = TargetSpec::component(handle.document_id.to_string(), "roof", 2);
        let stale = ServiceError::Document(DocumentError::Conflict {
            expected: DocumentHandle::new(handle.document_id.clone(), 2),
            current: handle.clone(),
        });
        let outcome = conflict_of(&stale, &target).expect("a stale revision is a conflict");
        assert!(matches!(
            outcome,
            CommitOutcome::Conflict {
                expected: 2,
                actual: 3,
                ..
            }
        ));

        // A document that is gone is an error, not a conflict: the job did not merely arrive
        // late, so a caller must not be told to retry against the revision it can see.
        let gone = ServiceError::Document(DocumentError::UnknownDocument {
            document_id: DocumentId::mint(0x4f2a, 9),
            active: Some(handle.document_id.clone()),
        });
        assert!(conflict_of(&gone, &target).is_none());
    }
}
