//! The desktop's job host: one job service, and the adapters that put real work in it.
//!
//! [`JobHost`] owns the process-wide [`splatmcp_core::JobService`]. Ordinary operations -
//! importing a scene, exporting a revision, inspecting one - are submitted as jobs, so a long
//! import no longer holds a bridge request open and the window stays responsive; the same
//! service answers the window's own status and cancel requests.
//!
//! # What an adapter does
//!
//! Every adapter is a `JobBody`: it reports phases through [`JobContext::progress`], checks
//! cancellation at its boundaries through [`JobContext::check`], and commits or produces its
//! artifact through [`JobContext::commit`] - the documented linearization point. The
//! bookkeeping, states, receipts and retention all belong to the service, so an adapter adds
//! no second registry and no second status model.
//!
//! # Why the work is off the UI thread
//!
//! A body runs on a service worker thread. Nothing here is called from the Tauri command
//! thread, so `document` and asset work never blocks the window; the only thing the UI thread
//! does is submit, read a status and ask for cancellation.

use std::sync::Arc;

use serde_json::{Value, json};
use splatmcp_core::{
    AssetHandle, AssetKind, DocumentHandle, Expected, JobAdmission, JobBody, JobError, JobFailure,
    JobId, JobKind, JobLimits, JobPhase, JobReceipt, JobRequest, JobResult, JobService, JobState,
    JobView, LogLevel, Mutation, PlyImportPolicy, SideEffectState,
};
use tauri::{AppHandle, Manager};

use crate::assets::AssetHost;
use crate::document::{self, AppState};

/// The process-wide job service plus the adapters that use it.
pub struct JobHost {
    service: Arc<JobService>,
}

impl Default for JobHost {
    fn default() -> Self {
        Self::with_limits(JobLimits::default())
    }
}

impl JobHost {
    /// A host whose service enforces `limits`.
    pub fn with_limits(limits: JobLimits) -> Self {
        Self {
            service: Arc::new(JobService::with_limits(limits)),
        }
    }

    /// The service itself, for a caller that wants to submit its own body.
    pub fn service(&self) -> &Arc<JobService> {
        &self.service
    }

    /// Submits an arbitrary body: the seam a Python executor adapter uses instead of keeping
    /// its own registry.
    pub fn submit(&self, request: JobRequest, body: JobBody) -> Result<JobAdmission, JobError> {
        self.service.submit(request, body)
    }

    pub fn view(
        &self,
        job_id: &JobId,
        log_after: u64,
        log_limit: usize,
    ) -> Result<JobView, JobError> {
        self.service.view(job_id, log_after, log_limit)
    }

    pub fn cancel(&self, job_id: &JobId) -> Result<JobReceipt, JobError> {
        self.service.cancel(job_id)
    }

    pub fn recent(&self, limit: usize) -> Vec<JobReceipt> {
        self.service.recent(limit)
    }

    /// Stops the service: queued work is cancelled outright and running work is asked to stop.
    pub fn shutdown(&self) {
        self.service.shutdown();
    }
}

/// Managed state wrapper so the Tauri commands and the bridge share one service.
pub struct JobHostState(pub Arc<JobHost>);

/// Starts an import job: a registered asset is decoded into a new or replaced document.
///
/// The asset is resolved *before* submission, so an unknown id is a request error and the
/// decoded snapshot is held by the job - editing the source file afterwards cannot change what
/// this job imports.
pub struct ImportJob;

impl ImportJob {
    /// The job body for loading one PLY asset into the document.
    pub fn body(
        app: AppHandle,
        asset: AssetHandle,
        expected: Expected,
        policy: PlyImportPolicy,
        display: bool,
    ) -> JobBody {
        Box::new(move |context| {
            context.log(
                LogLevel::Info,
                format!(
                    "importing {} ({} bytes)",
                    asset.source(),
                    asset.bytes().len()
                ),
            );
            context.progress(JobPhase::Reading, 0, Some(asset.bytes().len() as u64), None);
            let _ = context.check()?;

            context.progress(
                JobPhase::Decoding,
                asset.bytes().len() as u64,
                Some(asset.bytes().len() as u64),
                Some("parsing PLY".to_owned()),
            );
            // Parsing runs here, off the UI thread: the window keeps drawing while a large
            // file is decoded.
            let imported = {
                let state = app.state::<AppState>();
                let imported = if let Some(handle) = expected.handle() {
                    state.replace_ply(
                        Expected::Handle(handle.clone()),
                        asset.bytes(),
                        Mutation::new(splatmcp_core::MutationKind::Import)
                            .operation("import_asset")
                            .file_name(file_name_of(asset.source())),
                        policy,
                    )
                } else {
                    state.open_ply(
                        asset.bytes(),
                        Mutation::import(file_name_of(asset.source())),
                        policy,
                    )
                };
                imported.map_err(import_failure)?
            };
            let _ = context.check()?;
            context.progress(JobPhase::Validating, 1, Some(1), Some("checked".to_owned()));

            // The commit happened inside the store above; the receipt records what it was.
            let result = context.commit(|| {
                Ok(JobResult::Document {
                    document_id: imported.metadata.handle.document_id.to_string(),
                    revision: imported.metadata.handle.revision,
                    point_count: imported.metadata.point_count,
                })
            })?;
            if !imported.report.is_lossless() {
                context.log(LogLevel::Warning, imported.report.summary());
            }
            if display {
                context.progress(
                    JobPhase::Publishing,
                    1,
                    Some(1),
                    Some("announcing".to_owned()),
                );
                let receipt = splatmcp_core::ReceiptDocument {
                    document_id: imported.metadata.handle.document_id.clone(),
                    revision: imported.metadata.handle.revision,
                    point_count: imported.metadata.point_count,
                    file_name: imported.metadata.provenance.file_name.clone(),
                };
                // Through the one publication seam: an identity event with a request token, and
                // bytes the viewer fetches by (document, revision). Announcing is not displaying,
                // so the receipt stays `pending` until the window acknowledges what it drew - and
                // a refusal to announce is reported as an unsuccessful display.
                context.note_side_effect(
                    "display",
                    match crate::publication::announce(&app, &receipt, None, true) {
                        Ok(_) => SideEffectState::Pending,
                        Err(error) if error.code() == "already_displayed" => SideEffectState::Done,
                        Err(error) => SideEffectState::Failed(error.to_string()),
                    },
                );
            }
            Ok(result)
        })
    }
}

/// Starts an export job: a document revision is written to a file.
pub struct ExportJob;

impl ExportJob {
    /// The job body for exporting one revision to `path`.
    pub fn body(app: AppHandle, expected: Expected, path: String) -> JobBody {
        Box::new(move |context| {
            context.log(LogLevel::Info, format!("exporting to {path}"));
            context.progress(JobPhase::Exporting, 0, None, None);
            let _ = context.check()?;
            let outcome = {
                let state = app.state::<AppState>();
                // Serialisation runs outside every document lock: the store hands out an
                // immutable snapshot and the write happens here.
                match state.export(expected.clone(), std::path::Path::new(&path)) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        // The export *was* requested and did fail. Reporting `not_requested`
                        // here would hide a downstream failure behind the job's failure code.
                        context
                            .note_side_effect("export", SideEffectState::Failed(error.to_string()));
                        return Err(export_failure(error));
                    }
                }
            };
            let checksum = outcome.checksum.clone();
            let bytes = outcome.bytes;
            let _ = context.check()?;
            let result = context.commit(|| {
                Ok(JobResult::Artifact {
                    path: path.clone(),
                    checksum,
                    bytes: bytes as u64,
                })
            })?;
            context.note_side_effect("export", SideEffectState::Done);
            Ok(result)
        })
    }
}

/// Starts an inspection job: bounded metadata of one revision, nothing written.
pub struct InspectJob;

impl InspectJob {
    /// The job body for inspecting a revision.
    pub fn body(app: AppHandle, expected: Expected) -> JobBody {
        Box::new(move |context| {
            context.progress(JobPhase::Computing, 0, None, None);
            let _ = context.check()?;
            let summary = {
                let state = app.state::<AppState>();
                let snapshot = state
                    .snapshot(expected.clone())
                    .map_err(|error| JobFailure::new("inspect_failed", error.to_string()))?;
                let inspection = snapshot
                    .splat()
                    .inspection(splatmcp_core::ValidationLimits::default());
                format!(
                    "{}: {} gaussians{}",
                    snapshot.handle(),
                    inspection.point_count,
                    inspection
                        .bounds
                        .map(|bounds| format!(", radius {:.3}", bounds.radius))
                        .unwrap_or_default()
                )
            };
            // A read-only job completes; it never claims to have committed anything.
            Ok(JobResult::Message(summary))
        })
    }
}

/// Classifies an import failure, so a revision conflict lands in the `conflict` state.
///
/// Reporting a stale target as `import_failed` would leave the caller to read the prose for the
/// one outcome it can act on: a conflict is its own state, with its own code and counter.
fn import_failure(error: crate::document::ServiceError) -> JobFailure {
    if error.is_conflict() {
        return JobFailure::conflict(error.to_string());
    }
    JobFailure::new(error.code(), error.to_string())
}

/// Classifies an export failure: a stale revision is a conflict, anything else is a failure.
fn export_failure(error: crate::document::ServiceError) -> JobFailure {
    if error.is_conflict() {
        return JobFailure::conflict(error.to_string());
    }
    JobFailure::new("export_failed", error.to_string())
}

/// The file name of a provenance string, for a document's display name.
fn file_name_of(source: &str) -> String {
    std::path::Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "splat.ply".to_owned())
}

/// Parses a job id from wire text, with the reason when it is not one.
pub fn parse_job_id(text: &str) -> Result<JobId, String> {
    JobId::parse(text).ok_or_else(|| format!("'{text}' is not a job id"))
}

/// Resolves a document target from an optional id and revision.
pub fn target(document_id: Option<&str>, revision: Option<u64>) -> Result<Expected, String> {
    document::expected_target(document_id, revision)
}

impl JobHost {
    /// Submits an import of a registered PLY asset.
    pub fn import(
        &self,
        app: &AppHandle,
        asset: AssetHandle,
        expected: Expected,
        operation_id: Option<String>,
    ) -> Result<JobAdmission, String> {
        if asset.kind() != AssetKind::Ply {
            return Err(format!(
                "asset {} holds {} bytes; an import needs a ply asset",
                asset.id(),
                asset.kind().as_str()
            ));
        }
        // The whole request is the retry identity: the same asset into the same revision is a
        // retry, while the same asset into a *different* revision is different work and must not
        // be answered with the earlier receipt.
        let request = JobRequest::new(JobKind::Import, "import_asset")
            .with_target(format!("{} into {}", asset.id(), expected.describe()))
            .with_asset(asset.id().to_string())
            .with_document(expected.describe(), expected.handle().map(|handle| handle.revision));
        let request = match operation_id {
            Some(operation_id) => request.with_operation_id(operation_id),
            None => request,
        };
        let body = ImportJob::body(
            app.clone(),
            asset.clone(),
            expected,
            PlyImportPolicy::Strict,
            true,
        );
        self.service
            .submit(request, body)
            .map_err(|e| e.to_string())
    }

    /// Submits an export of one revision to `path`.
    pub fn export(
        &self,
        app: &AppHandle,
        expected: Expected,
        path: String,
        operation_id: Option<String>,
    ) -> Result<JobAdmission, String> {
        let request = JobRequest::new(JobKind::Export, "export")
            .with_target(format!("{path} from {}", expected.describe()))
            .with_path(path.clone())
            .with_document(expected.describe(), expected.handle().map(|handle| handle.revision));
        let request = match operation_id {
            Some(operation_id) => request.with_operation_id(operation_id),
            None => request,
        };
        let body = ExportJob::body(app.clone(), expected, path);
        self.service
            .submit(request, body)
            .map_err(|e| e.to_string())
    }

    /// Submits a read-only inspection of one revision.
    pub fn inspect(
        &self,
        app: &AppHandle,
        expected: Expected,
        operation_id: Option<String>,
    ) -> Result<JobAdmission, String> {
        let request = JobRequest::new(JobKind::Inspect, "inspect_document")
            .with_target(expected.describe())
            .with_document(expected.describe(), expected.handle().map(|handle| handle.revision));
        let request = match operation_id {
            Some(operation_id) => request.with_operation_id(operation_id),
            None => request,
        };
        let body = InspectJob::body(app.clone(), expected);
        self.service
            .submit(request, body)
            .map_err(|e| e.to_string())
    }

    /// Submits an edit batch as a job.
    ///
    /// The batch runs through the app's one transaction service, so an edit submitted this way is
    /// the same atomic, retry-safe, undoable commit as `edit_splat` - it simply does not hold the
    /// request open while it runs, and its progress, logs and receipt appear in the job list.
    pub fn edit(
        &self,
        app: &AppHandle,
        expected: Expected,
        batch: splatmcp_core::EditBatch,
        display: bool,
        operation_id: Option<String>,
    ) -> Result<JobAdmission, String> {
        let steps = batch.steps.len();
        // The batch's own canonical hash is the semantic content of an edit, so it belongs in the
        // retry identity: the same operation id with a *different* batch must be refused rather
        // than replayed.
        let hash = batch.request_hash();
        let request = JobRequest::new(JobKind::Edit, "edit_batch")
            .with_target(format!(
                "{} ({steps} step(s), batch {hash:016x})",
                expected.describe()
            ))
            .with_document(expected.describe(), expected.handle().map(|handle| handle.revision));
        let request = match operation_id.or_else(|| batch.operation_id.clone()) {
            Some(operation_id) => request.with_operation_id(operation_id),
            None => request,
        };
        let body = EditJob::body(app.clone(), expected, batch, display);
        self.service
            .submit(request, body)
            .map_err(|e| e.to_string())
    }
}

/// Starts an edit job: one atomic batch committed through the shared transaction service.
pub struct EditJob;

impl EditJob {
    /// The job body for one edit batch.
    pub fn body(
        app: AppHandle,
        expected: Expected,
        batch: splatmcp_core::EditBatch,
        display: bool,
    ) -> JobBody {
        Box::new(move |context| {
            context.log(
                LogLevel::Info,
                format!(
                    "applying {} step(s) to {}",
                    batch.steps.len(),
                    expected.describe()
                ),
            );
            context.progress(JobPhase::Validating, 0, Some(batch.steps.len() as u64), None);
            let _ = context.check()?;
            context.progress(
                JobPhase::Committing,
                batch.steps.len() as u64,
                Some(batch.steps.len() as u64),
                Some("committing".to_owned()),
            );
            // The commit is the linearization point: cancellation is checked immediately before
            // it, and from here on the transaction wins.
            let receipt = context.commit(|| {
                let state = app.state::<AppState>();
                state
                    .commit_batch(expected.clone(), &batch, "edit_job")
                    .map_err(|error| JobFailure::new(error.code(), error.to_string()))
            })?;
            let recorded = receipt.recorded();
            let result = JobResult::Document {
                document_id: recorded.document_id.to_string(),
                revision: recorded.revision,
                point_count: recorded.point_count,
            };
            // The commit is recorded on the publication tracker even when nobody displays it, so
            // a hidden edit cannot leave the status claiming the old revision is current.
            let publications = app
                .state::<crate::publication::PublicationHostState>()
                .0
                .clone();
            if let Err(error) = publications.committed(&recorded.handle()) {
                context.log(
                    LogLevel::Warning,
                    format!("could not record the commit for display status: {error}"),
                );
            }
            if display {
                context.progress(
                    JobPhase::Publishing,
                    1,
                    Some(1),
                    Some("announcing".to_owned()),
                );
                context.note_side_effect(
                    "display",
                    match crate::publication::announce(&app, &recorded, None, false) {
                        Ok(_) => SideEffectState::Pending,
                        Err(error) if error.code() == "already_displayed" => SideEffectState::Done,
                        Err(error) => SideEffectState::Failed(error.to_string()),
                    },
                );
            }
            Ok(result)
        })
    }
}

/// Resolves an asset for an import job, so an unknown id fails before anything is queued.
pub fn resolve_asset(assets: &AssetHost, asset_id: &str) -> Result<AssetHandle, String> {
    // The host hands back `Arc<Asset>`, which is exactly the handle type a job body holds.
    assets.resolve(asset_id).map(|handle| handle)
}

/// A job receipt as the window and the bridge read it.
pub fn receipt_json(receipt: &JobReceipt) -> Value {
    json!({
        "job_id": receipt.job_id.to_string(),
        "kind": receipt.kind.as_str(),
        "state": receipt.state.as_str(),
        "terminal": receipt.state.is_terminal(),
        "success": receipt.state.is_success(),
        "operation": receipt.operation,
        "operation_id": receipt.operation_id,
        "target": receipt.target,
        "admitted_at_ms": receipt.admitted_at_ms,
        "started_at_ms": receipt.started_at_ms,
        "finished_at_ms": receipt.finished_at_ms,
        "duration_ms": match (receipt.started_at_ms, receipt.finished_at_ms) {
            (Some(start), Some(end)) => Value::from(end.saturating_sub(start)),
            _ => Value::Null,
        },
        "progress": {
            "phase": receipt.progress.phase.as_str(),
            "done": receipt.progress.done,
            "total": receipt.progress.total,
            "percent": (receipt.progress.fraction * 100.0).round(),
            "message": receipt.progress.message,
            "coalesced": receipt.progress.coalesced,
        },
        "log_count": receipt.log_count,
        "next_log_sequence": receipt.next_log_sequence,
        "result": receipt.result.describe(),
        "failure": receipt.failure.as_ref().map(|failure| json!({
            "code": failure.code,
            "message": failure.message,
        })),
        "export": receipt.export.as_str(),
        "display": receipt.display.as_str(),
        "notes": receipt.notes,
        "replayed": receipt.replayed,
    })
}

/// A job view (receipt plus the log lines asked for) as JSON.
pub fn view_json(view: &JobView) -> Value {
    let logs: Vec<Value> = view
        .logs
        .iter()
        .map(|line| {
            json!({
                "sequence": line.sequence,
                "at_ms": line.at_ms,
                "level": line.level.as_str(),
                "message": line.message,
            })
        })
        .collect();
    json!({ "receipt": receipt_json(&view.receipt), "logs": logs })
}

/// A job error as a structured reply, never a silent failure.
pub fn error_json(error: &JobError) -> Value {
    json!({ "code": error.code(), "message": error.to_string() })
}

/// The limits and counts in force, for a capabilities reply.
pub fn stats_json(host: &JobHost) -> Value {
    let stats = host.service().stats();
    json!({
        "queued": stats.counts.queued,
        "running": stats.counts.running,
        "retained": stats.counts.retained,
        "committed": stats.counts.committed,
        "completed": stats.counts.completed,
        "cancelled": stats.counts.cancelled,
        "failed": stats.counts.failed,
        "conflicted": stats.counts.conflicted,
        "evicted": stats.counts.evicted,
        "limits": stats.limits,
        "shutting_down": stats.shutting_down,
        "memory_note": stats.memory_note,
    })
}

/// A submitted job's admission as the wire shape both callers read.
pub fn admission_json(admission: &JobAdmission) -> Value {
    json!({
        "job_id": admission.job_id.to_string(),
        "state": admission.state.as_str(),
        "replayed": admission.replayed,
    })
}

/// The document handle a receipt names, when it names one.
pub fn receipt_handle(receipt: &JobReceipt) -> Option<DocumentHandle> {
    match &receipt.result {
        JobResult::Document {
            document_id,
            revision,
            ..
        } => splatmcp_core::DocumentId::parse(document_id)
            .map(|id| splatmcp_core::DocumentHandle::new(id, *revision)),
        _ => None,
    }
}

/// One line describing how a job ended, for the window and for a caller's log.
pub fn completion_summary(receipt: &JobReceipt) -> String {
    match receipt.state {
        splatmcp_core::JobState::Committed => format!("committed {}", receipt.result.describe()),
        splatmcp_core::JobState::Completed => format!("completed {}", receipt.result.describe()),
        splatmcp_core::JobState::Cancelled => "cancelled".to_owned(),
        splatmcp_core::JobState::Conflict => format!(
            "refused: {}",
            receipt
                .failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "revision conflict".to_owned())
        ),
        splatmcp_core::JobState::Failed => format!(
            "failed: {}",
            receipt
                .failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "unknown failure".to_owned())
        ),
        other => other.as_str().to_owned(),
    }
}

/// True when a failure means the caller should look for a different job rather than retry.
pub fn is_unknown_job(error: &JobError) -> bool {
    matches!(
        error,
        JobError::UnknownJob { .. } | JobError::ReceiptExpired { .. }
    )
}

/// Tauri command: submit an import of a registered PLY asset.
#[tauri::command]
pub fn job_import(app: AppHandle, request: Value) -> Result<Value, String> {
    let assets = app.state::<crate::assets::AssetHostState>().0.clone();
    let jobs = app.state::<JobHostState>().0.clone();
    let asset_id = request
        .get("asset_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "job_import needs asset_id (a registered ply asset)".to_owned())?;
    let expected = target(
        request.get("document_id").and_then(Value::as_str),
        request.get("expected_revision").and_then(Value::as_u64),
    )?;
    let asset = resolve_asset(&assets, asset_id)?;
    let admission = jobs.import(
        &app,
        asset,
        expected,
        request
            .get("operation_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )?;
    Ok(admission_json(&admission))
}

/// Tauri command: submit an export of one revision.
#[tauri::command]
pub fn job_export(app: AppHandle, request: Value) -> Result<Value, String> {
    let jobs = app.state::<JobHostState>().0.clone();
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "job_export needs path".to_owned())?
        .to_owned();
    let expected = target(
        request.get("document_id").and_then(Value::as_str),
        request.get("expected_revision").and_then(Value::as_u64),
    )?;
    let admission = jobs.export(
        &app,
        expected,
        path,
        request
            .get("operation_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )?;
    Ok(admission_json(&admission))
}

/// Tauri command: submit a read-only inspection of one revision.
#[tauri::command]
pub fn job_inspect(app: AppHandle, request: Value) -> Result<Value, String> {
    let jobs = app.state::<JobHostState>().0.clone();
    let expected = target(
        request.get("document_id").and_then(Value::as_str),
        request.get("expected_revision").and_then(Value::as_u64),
    )?;
    let expected = match expected {
        Expected::Any => Expected::Any,
        other => other,
    };
    let admission = jobs.inspect(&app, expected, None)?;
    Ok(admission_json(&admission))
}

/// Tauri command: one job's status and the log lines after `log_after`.
#[tauri::command]
pub fn job_status(host: tauri::State<'_, JobHostState>, request: Value) -> Result<Value, String> {
    let job_id = request
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "job_status needs job_id".to_owned())
        .and_then(parse_job_id)?;
    let log_after = request
        .get("log_after")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let log_limit = request
        .get("log_limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .min(500) as usize;
    match host.0.view(&job_id, log_after, log_limit) {
        Ok(view) => Ok(view_json(&view)),
        Err(error) => Ok(json!({ "error": error_json(&error) })),
    }
}

/// Tauri command: the newest jobs, newest first, for the window's job list.
#[tauri::command]
pub fn job_list(host: tauri::State<'_, JobHostState>, limit: Option<usize>) -> Value {
    let limit = limit.unwrap_or(10).min(64);
    let jobs: Vec<Value> = host.0.recent(limit).iter().map(receipt_json).collect();
    json!({ "jobs": jobs, "stats": stats_json(&host.0) })
}

/// Tauri command: ask a job to stop, and report what actually happened.
#[tauri::command]
pub fn job_cancel(host: tauri::State<'_, JobHostState>, job_id: String) -> Result<Value, String> {
    let job_id = parse_job_id(&job_id)?;
    match host.0.cancel(&job_id) {
        Ok(receipt) => Ok(receipt_json(&receipt)),
        Err(error) => Ok(json!({ "error": error_json(&error) })),
    }
}

/// Tauri command: the service's counts and limits, for a capabilities view.
#[tauri::command]
pub fn job_stats(host: tauri::State<'_, JobHostState>) -> Value {
    stats_json(&host.0)
}

/// Tauri command: wait for a job for a bounded time, so the window can offer a "wait" button
/// without holding its own thread open forever.
#[tauri::command]
pub fn job_wait(
    host: tauri::State<'_, JobHostState>,
    job_id: String,
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    let job_id = parse_job_id(&job_id)?;
    let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(2000).min(30_000));
    match host.0.service().wait(&job_id, timeout) {
        Ok(receipt) => Ok(receipt_json(&receipt)),
        Err(error) => Ok(json!({ "error": error_json(&error) })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_admission_reports_what_was_accepted() {
        let admission = JobAdmission {
            job_id: JobId::mint(0x4f2a, 2),
            state: JobState::Queued,
            replayed: false,
        };
        let encoded = admission_json(&admission);
        assert_eq!(encoded["job_id"], "job-4f2a-2");
        assert_eq!(encoded["state"], "queued");
        assert_eq!(encoded["replayed"], false);
    }

    #[test]
    fn a_job_id_must_be_a_job_id() {
        assert!(parse_job_id("job-4f2a-1").is_ok());
        assert!(
            parse_job_id("asset-4f2a-1")
                .unwrap_err()
                .contains("not a job id")
        );
        assert!(parse_job_id("job-1").is_err());
    }

    #[test]
    fn a_receipt_reads_as_bounded_json() {
        let service = JobService::with_session(0x4f2a, JobLimits::default());
        let admission = service
            .submit(
                JobRequest::new(JobKind::Export, "export").with_target("C:/out/scene.ply"),
                Box::new(|context| {
                    context.progress(JobPhase::Exporting, 5, Some(10), Some("writing".to_owned()));
                    Ok(JobResult::Artifact {
                        path: "C:/out/scene.ply".to_owned(),
                        checksum: "fnv1a64:00000000000000ff".to_owned(),
                        bytes: 4096,
                    })
                }),
            )
            .unwrap();
        let receipt = service
            .wait(&admission.job_id, std::time::Duration::from_secs(5))
            .unwrap();
        let encoded = receipt_json(&receipt);
        assert_eq!(encoded["kind"], "export");
        assert_eq!(encoded["state"], "completed", "a read-only job completes");
        assert_eq!(encoded["success"], true);
        assert_eq!(encoded["terminal"], true);
        assert!(encoded["result"].as_str().unwrap().contains("4096 bytes"));
        assert!(encoded.get("export").is_some() && encoded.get("display").is_some());
        assert!(encoded.to_string().len() < 900, "{encoded}");
        assert_eq!(receipt_handle(&receipt), None);
        assert!(completion_summary(&receipt).starts_with("completed"));
    }

    #[test]
    fn a_view_carries_the_logs_a_reconnect_asked_for() {
        let service = JobService::with_session(7, JobLimits::default());
        let admission = service
            .submit(
                JobRequest::new(JobKind::Import, "import_asset"),
                Box::new(|context| {
                    context.log(LogLevel::Info, "reading scene.ply");
                    context.log(LogLevel::Warning, "repaired 2 radii");
                    Ok(JobResult::Document {
                        document_id: "doc-7-1".to_owned(),
                        revision: 1,
                        point_count: 12,
                    })
                }),
            )
            .unwrap();
        service
            .wait(&admission.job_id, std::time::Duration::from_secs(5))
            .unwrap();
        let view = service.view(&admission.job_id, 1, 10).unwrap();
        let encoded = view_json(&view);
        assert_eq!(encoded["logs"].as_array().unwrap().len(), 1);
        assert_eq!(encoded["logs"][0]["level"], "warning");
        assert_eq!(encoded["receipt"]["state"], "committed");
        let handle = receipt_handle(&view.receipt).unwrap();
        assert_eq!(handle.to_string(), "doc-7-1@1");
    }

    #[test]
    fn a_job_error_is_a_structured_reply() {
        let error = JobError::UnknownJob {
            job_id: "job-99-1".to_owned(),
            hint: "it was minted by an earlier run of the app".to_owned(),
        };
        let encoded = error_json(&error);
        assert_eq!(encoded["code"], "unknown_job");
        assert!(encoded["message"].as_str().unwrap().contains("earlier run"));
        assert!(is_unknown_job(&error));
        assert!(!is_unknown_job(&JobError::QueueFull { limit: 2 }));
    }

    #[test]
    fn stats_expose_the_limits_and_the_memory_disclosure() {
        let host = JobHost::with_limits(JobLimits {
            max_queued: 3,
            ..JobLimits::default()
        });
        let encoded = stats_json(&host);
        assert_eq!(encoded["queued"], 0);
        assert!(encoded["limits"].as_str().unwrap().contains("queued<=3"));
        assert!(
            encoded["memory_note"]
                .as_str()
                .unwrap()
                .contains("not hard-limited")
        );
    }
}
