//! The generation job registry: submission, deduplication, validation, atomic commit and
//! display bookkeeping.
//!
//! [`GenerationService`] is the single entry point MCP tools and Tauri commands both use,
//! so an MCP-started job and a UI-started job are the same kind of job on the same
//! document. It owns the job records and the commit rules; [`crate::executor`] owns the
//! interpreter thread.
//!
//! # Job lifecycle
//!
//! ```text
//! queued -> running -> validating -> committing -> committed
//!    |         |            |            |
//!    |         |            |            +-> conflict   (document moved on)
//!    |         |            +-> failed   (invalid arrays or over budget)
//!    |         +-> failed   (script error) / cancel_requested -> cancelled
//!    +-> cancelled         (cancelled before it started)
//! ```
//!
//! Display is deliberately a separate axis. A committed document is not proof that the
//! viewer rendered it, so every job also carries a [`DisplayState`] and the revision the
//! viewer acknowledged. An export or display failure never changes the compute state: it
//! is reported next to it, so a retry cannot silently rerun a generation that already
//! succeeded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use splatmcp_core::Splat;

use crate::arrays::BoundsOut;

use crate::arrays::GaussianBatch;
use crate::executor::{
    CancelReason, CancelToken, ExecutorConfig, ExecutorOutcome, JobLogLine, JobObserver, JobTicket,
    LogLevel, LogSink, PythonExecutor, ProgressSink, RunContext, ScriptRunner, SourceSnapshot,
};
use crate::runtime::{Limits, RuntimeFingerprint, RuntimeReport};
use crate::script::{now_ms, RecipeRecord, ScriptSnapshot};
use crate::{PythonError, Result};

/// Where a job's result should go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSpec {
    /// Document being edited; absent for a new document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Named component to replace; absent to replace the whole document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Revision the caller believes it is editing. Required for a mutation, because a
    /// late job must never overwrite newer work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Name a new document should be saved under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

impl TargetSpec {
    /// A new document, replacing whatever is displayed.
    pub fn new_document(file_name: Option<String>) -> Self {
        Self {
            document_id: None,
            component_id: None,
            expected_revision: None,
            file_name,
        }
    }

    /// Replacement of one named component of an existing document.
    pub fn component(
        document_id: impl Into<String>,
        component_id: impl Into<String>,
        expected_revision: u64,
    ) -> Self {
        Self {
            document_id: Some(document_id.into()),
            component_id: Some(component_id.into()),
            expected_revision: Some(expected_revision),
            file_name: None,
        }
    }

    pub fn is_new_document(&self) -> bool {
        self.document_id.is_none()
    }

    /// True when this job edits content that already exists.
    pub fn edits_existing(&self) -> bool {
        self.document_id.is_some() || self.expected_revision.is_some()
    }

    /// Rejects a mutation that does not say which revision it expects.
    pub fn validate(&self) -> Result<()> {
        if self.document_id.is_some() && self.expected_revision.is_none() {
            return Err(PythonError::DocumentConflict(
                "editing an existing document needs expected_revision, so concurrent changes \
                 are reported instead of overwritten"
                    .to_owned(),
            ));
        }
        if let Some(component) = &self.component_id
            && component.trim().is_empty()
        {
            return Err(PythonError::InvalidBatch(
                "component_id must not be blank".to_owned(),
            ));
        }
        Ok(())
    }

    /// Stable text used in a request's content fingerprint.
    pub fn fingerprint(&self) -> String {
        format!(
            "doc:{}|component:{}|revision:{}",
            self.document_id.as_deref().unwrap_or("<new>"),
            self.component_id.as_deref().unwrap_or("<all>"),
            self.expected_revision
                .map(|revision| revision.to_string())
                .unwrap_or_else(|| "-".to_owned())
        )
    }
}

/// Identity of a document revision a job produced or found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentIdentity {
    pub document_id: String,
    pub revision: u64,
    pub point_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
}

/// A commit request handed to the document owner.
pub struct CommitRequest {
    pub target: TargetSpec,
    pub splat: Splat,
    pub provenance: RecipeRecord,
}

/// Result of a commit attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum CommitOutcome {
    /// The candidate became the new revision.
    Committed { identity: DocumentIdentity },
    /// The document had already moved on: the candidate was discarded.
    Conflict {
        document_id: String,
        expected: u64,
        actual: u64,
    },
}

/// The document owner: the app, or a test double.
///
/// Only these three operations are needed, which keeps the Python service free of any
/// knowledge of Tauri, the viewer or the filesystem layout.
pub trait DocumentTarget: Send + Sync + 'static {
    /// A detached read-only copy of the current content, for an edit job.
    fn snapshot(&self, target: &TargetSpec) -> Result<SourceSnapshot>;

    /// Swaps in a candidate when the target revision still matches.
    fn commit(&self, request: CommitRequest) -> Result<CommitOutcome>;

    /// Asks the viewer to show a committed revision.
    fn publish(&self, target: &TargetSpec, identity: &DocumentIdentity) -> Result<()>;
}

/// Whether the viewer is showing a job's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum DisplayState {
    /// The request asked for no display.
    NotRequested,
    /// Published; the viewer has not acknowledged the revision yet.
    Pending,
    /// The viewer acknowledged this revision.
    Rendered,
    /// Loading the new revision failed; the previous view is preserved.
    Failed { message: String },
}

/// Job states, mirroring the lifecycle design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    CancelRequested,
    Validating,
    Committing,
    Committed,
    Cancelled,
    Failed,
    Conflict,
}

impl JobState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Validating => "validating",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Conflict => "conflict",
        }
    }

    /// True once no further transition is possible.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Cancelled | Self::Failed | Self::Conflict
        )
    }
}

/// Structured failure of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    /// Stable machine readable code, e.g. `invalid_batch`.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceback: Option<String>,
}

impl JobError {
    /// Turns a library error into a job error.
    pub fn of(error: &PythonError) -> Self {
        match error {
            // A traceback is only meaningful for a script failure.
            PythonError::Script(message) => Self {
                code: error.code().to_owned(),
                message: message.clone(),
                traceback: extract_traceback(message),
            },
            other => Self {
                code: other.code().to_owned(),
                message: other.to_string(),
                traceback: None,
            },
        }
    }
}

/// Splits the traceback a Python exception carries off its message.
fn extract_traceback(message: &str) -> Option<String> {
    let index = message.find("Traceback (most recent call last)")?;
    Some(message[index..].to_owned())
}

/// Wall-clock timings of a job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timings {
    pub queued_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Time spent waiting for the executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ms: Option<u64>,
    /// Time the script itself ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<u64>,
}

/// Outcome of writing an exported PLY, kept separate from the compute state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportOutcome {
    pub path: String,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<String>,
    /// Set when the geometry was computed but the file could not be written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a caller asks for.
pub struct GenerationRequest {
    /// Frozen script, parameters, seed and request identity.
    pub snapshot: ScriptSnapshot,
    /// Where the result goes.
    pub target: TargetSpec,
    /// Show the committed revision in the viewer when it is ready.
    pub display: bool,
    /// Optional `.ply` export of the candidate.
    pub export_path: Option<PathBuf>,
    /// Cooperative deadline; the configured default applies when absent.
    pub deadline: Option<Duration>,
}

/// Receipt of an accepted submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobReceipt {
    pub job_id: u64,
    pub request_id: String,
    pub state: JobState,
    pub queued_at_ms: u64,
    /// True when this request id had already been accepted with the same content.
    pub deduplicated: bool,
    pub content_hash: String,
    pub script_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayState>,
}

/// A job's status, as reported by `get_python_job`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub job_id: u64,
    pub request_id: String,
    pub state: JobState,
    pub target: TargetSpec,
    pub seed: u64,
    pub entry_point: String,
    pub script_hash: String,
    pub content_hash: String,
    pub progress: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_message: Option<String>,
    pub timings: Timings,
    pub display: DisplayState,
    /// Revision the viewer acknowledged, once it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
    #[serde(default)]
    pub logs: Vec<JobLogLine>,
    /// Cursor to pass as `log_after` on the next poll.
    pub log_cursor: u64,
    /// True when older lines were dropped by the log bound.
    pub log_truncated: bool,
    /// Runtime the job ran in, so a result can be traced to its environment.
    pub runtime: RuntimeFingerprint,
}

/// Short job entry for the UI's job list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: u64,
    pub request_id: String,
    pub state: JobState,
    pub display: DisplayState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    pub queued_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Answer to `cancel_python_job`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelView {
    pub job_id: u64,
    pub state: JobState,
    /// True while the interpreter is still unwinding inside a native call.
    pub still_unwinding: bool,
    pub message: String,
}

/// Budgets and defaults of the whole service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceConfig {
    pub executor: ExecutorConfig,
}

/// The single generation service the app hosts.
pub struct GenerationService {
    registry: Arc<Registry>,
    executor: PythonExecutor,
}

struct Registry {
    jobs: Mutex<HashMap<u64, JobRecord>>,
    receipts: Mutex<HashMap<String, ReceiptEntry>>,
    next_job_id: AtomicU64,
    target: Arc<dyn DocumentTarget>,
    config: ServiceConfig,
    base_report: RuntimeReport,
}

#[derive(Clone)]
struct ReceiptEntry {
    content_hash: String,
    job_id: u64,
}

#[derive(Clone)]
struct JobRecord {
    job_id: u64,
    snapshot: ScriptSnapshot,
    target: TargetSpec,
    content_hash: String,
    state: JobState,
    display_requested: bool,
    display: DisplayState,
    displayed_revision: Option<u64>,
    progress: ProgressSink,
    logs: LogSink,
    cancel: CancelToken,
    timings: Timings,
    document_id: Option<String>,
    revision: Option<u64>,
    component_id: Option<String>,
    point_count: Option<usize>,
    bounds: Option<BoundsOut>,
    export: Option<ExportOutcome>,
    export_path: Option<PathBuf>,
    error: Option<JobError>,
    runtime: RuntimeFingerprint,
}

impl Registry {
    fn record(&self, job_id: u64) -> Result<JobRecord> {
        self.jobs
            .lock()
            .map_err(|_| PythonError::Script("the job table is locked".to_owned()))?
            .get(&job_id)
            .cloned()
            .ok_or_else(|| PythonError::JobNotFound(format!("no job {job_id}")))
    }

    /// Applies a transition to a job record, ignoring an unknown job.
    fn update(&self, job_id: u64, change: impl FnOnce(&mut JobRecord)) {
        if let Ok(mut jobs) = self.jobs.lock()
            && let Some(record) = jobs.get_mut(&job_id)
        {
            change(record);
        }
    }

    fn view(&self, job_id: u64, log_after: u64, log_limit: usize) -> Result<JobView> {
        let record = self.record(job_id)?;
        let (mut logs, truncated) = record.logs.tail(log_after);
        if logs.len() > log_limit {
            logs.truncate(log_limit);
        }
        let log_cursor = record.next_log_cursor(log_after, &logs);
        Ok(JobView {
            job_id,
            request_id: record.snapshot.request_id.clone(),
            state: record.state,
            target: record.target.clone(),
            seed: record.snapshot.seed,
            entry_point: record.snapshot.entry_point.clone(),
            script_hash: record.snapshot.script_hash.clone(),
            content_hash: record.content_hash.clone(),
            progress: record.progress.fraction(),
            progress_message: record.progress.message(),
            timings: record.timings.clone(),
            display: record.display.clone(),
            displayed_revision: record.displayed_revision,
            document_id: record.document_id.clone(),
            revision: record.revision,
            component_id: record.component_id.clone(),
            point_count: record.point_count,
            bounds: record.bounds,
            export: record.export.clone(),
            error: record.error.clone(),
            logs,
            log_cursor,
            log_truncated: truncated || record.logs.dropped() > 0,
            runtime: record.runtime.clone(),
        })
    }
}

impl JobRecord {
    /// Cursor a caller passes back to receive only newer log lines.
    fn next_log_cursor(&self, after: u64, returned: &[JobLogLine]) -> u64 {
        returned
            .last()
            .map(|line| line.seq)
            .or_else(|| self.logs.tail(0).0.last().map(|line| line.seq))
            .unwrap_or(after)
    }
}

impl GenerationService {
    /// Builds the service and starts its execution thread.
    ///
    /// `base_report` describes the resolved runtime; live package versions replace it once
    /// the interpreter has answered.
    pub fn start(
        runner: Arc<dyn ScriptRunner>,
        config: ServiceConfig,
        target: Arc<dyn DocumentTarget>,
        base_report: RuntimeReport,
    ) -> Self {
        let registry = Arc::new(Registry {
            jobs: Mutex::new(HashMap::new()),
            receipts: Mutex::new(HashMap::new()),
            next_job_id: AtomicU64::new(1),
            target,
            config: config.clone(),
            base_report,
        });
        let observer: Arc<dyn JobObserver> = Arc::new(Observer {
            registry: registry.clone(),
        });
        let executor = PythonExecutor::start(runner, config.executor, observer);
        Self { registry, executor }
    }

    /// Accepts a job, or explains why it was refused.
    pub fn submit(&self, request: GenerationRequest) -> Result<JobReceipt> {
        request.target.validate()?;
        let content_hash = request.snapshot.content_hash(&request.target.fingerprint());
        let request_id = request.snapshot.request_id.clone();
        let job_id = self.registry.next_job_id.fetch_add(1, Ordering::SeqCst);
        let queued_at_ms = now_ms();

        // Reservation and record insertion happen under one lock so a concurrent retry of
        // the same request_id always finds a job record to report.
        {
            let mut receipts = self
                .registry
                .receipts
                .lock()
                .map_err(|_| PythonError::Script("the request table is locked".to_owned()))?;
            if let Some(existing) = receipts.get(&request_id) {
                if existing.content_hash == content_hash {
                    let existing_record = self.registry.record(existing.job_id)?;
                    return Ok(JobReceipt {
                        job_id: existing.job_id,
                        request_id,
                        state: existing_record.state,
                        queued_at_ms: existing_record.timings.queued_at_ms,
                        deduplicated: true,
                        content_hash,
                        script_hash: request.snapshot.script_hash.clone(),
                        display: Some(existing_record.display.clone()),
                    });
                }
                return Err(PythonError::RequestConflict(format!(
                    "request_id {request_id} was already used with different content; use a new \
                     request id for a different job"
                )));
            }
            receipts.insert(
                request_id.clone(),
                ReceiptEntry {
                    content_hash: content_hash.clone(),
                    job_id,
                },
            );
            let mut jobs = self
                .registry
                .jobs
                .lock()
                .map_err(|_| PythonError::Script("the job table is locked".to_owned()))?;
            jobs.insert(
                job_id,
                JobRecord {
                    job_id,
                    snapshot: request.snapshot.clone(),
                    target: request.target.clone(),
                    content_hash: content_hash.clone(),
                    state: JobState::Queued,
                    display_requested: request.display,
                    display: if request.display {
                        DisplayState::Pending
                    } else {
                        DisplayState::NotRequested
                    },
                    displayed_revision: None,
                    progress: ProgressSink::default(),
                    logs: LogSink::new(
                        self.registry.config.executor.max_log_lines,
                        self.registry.config.executor.max_log_bytes,
                    ),
                    cancel: CancelToken::new(),
                    timings: Timings {
                        queued_at_ms,
                        ..Timings::default()
                    },
                    document_id: request.target.document_id.clone(),
                    revision: None,
                    component_id: request.target.component_id.clone(),
                    point_count: None,
                    bounds: None,
                    export: None,
                    export_path: request.export_path.clone(),
                    error: None,
                    runtime: RuntimeFingerprint::from_report(&self.registry.base_report),
                },
            );
        }

        let outcome = self.enqueue(job_id, request, content_hash);
        if outcome.is_err() {
            self.forget(job_id, &request_id);
        }
        outcome
    }

    /// Builds the run context and hands the job to the executor.
    fn enqueue(
        &self,
        job_id: u64,
        request: GenerationRequest,
        content_hash: String,
    ) -> Result<JobReceipt> {
        let record = self.registry.record(job_id)?;

        // An edit job reads its source through the document owner, which takes the
        // revision-addressed snapshot before the queue so a late job cannot tear it. A job
        // that states an expected revision edits what is open, so it gets a snapshot even
        // without a document id; a job that states neither is creating a new document and
        // has nothing to read.
        let edits_existing =
            request.target.document_id.is_some() || request.target.expected_revision.is_some();
        let source_snapshot = if edits_existing {
            match self.registry.target.snapshot(&request.target) {
                Ok(snapshot) => Some(Arc::new(snapshot)),
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        let deadline = self.registry.config.executor.deadline(request.deadline);
        record.cancel.set_deadline(deadline);

        let context = Arc::new(RunContext {
            job_id,
            source: request.snapshot.source.clone(),
            entry_point: request.snapshot.entry_point.clone(),
            params: request.snapshot.params.clone(),
            seed: request.snapshot.seed,
            max_points: self.registry.config.executor.max_points,
            cancel: record.cancel.clone(),
            logs: record.logs.clone(),
            progress: record.progress.clone(),
            source_snapshot,
        });
        self.executor.submit(JobTicket { job_id, context })?;

        Ok(JobReceipt {
            job_id,
            request_id: request.snapshot.request_id.clone(),
            state: JobState::Queued,
            queued_at_ms: record.timings.queued_at_ms,
            deduplicated: false,
            content_hash,
            script_hash: request.snapshot.script_hash.clone(),
            display: Some(record.display.clone()),
        })
    }

    /// Status of one job, with log lines newer than `log_after`.
    pub fn status(&self, job_id: u64, log_after: u64, log_limit: usize) -> Result<JobView> {
        self.registry.view(job_id, log_after, log_limit)
    }

    /// Requests cancellation and reports what actually happened.
    ///
    /// A queued job is cancelled immediately: it never entered the interpreter, so there is
    /// nothing to unwind. A running job becomes `cancel_requested` and stops at its next
    /// checkpoint, which a native NumPy or PyTorch call can delay - the reply says so
    /// rather than implying the interpreter is already free.
    pub fn cancel(&self, job_id: u64) -> Result<CancelView> {
        let record = self.registry.record(job_id)?;
        if record.state.is_terminal() {
            return Ok(CancelView {
                job_id,
                state: record.state,
                still_unwinding: false,
                message: format!("the job already finished as {}", record.state.name()),
            });
        }
        if record.state == JobState::Queued {
            // The executor still holds the ticket; cancelling the token makes the worker
            // discard it without ever starting the script.
            self.executor.cancel(job_id);
            self.registry.update(job_id, |record| {
                record.state = JobState::Cancelled;
                record.timings.finished_at_ms = Some(now_ms());
                record.error = Some(JobError {
                    code: "job_cancelled".to_owned(),
                    message: "the job was cancelled while it was still waiting and never ran"
                        .to_owned(),
                    traceback: None,
                });
                record.display = DisplayState::NotRequested;
            });
            return Ok(CancelView {
                job_id,
                state: JobState::Cancelled,
                still_unwinding: false,
                message: "cancelled before it started; no script was executed".to_owned(),
            });
        }

        self.executor.cancel(job_id);
        self.registry.update(job_id, |record| {
            if record.state == JobState::Running {
                record.state = JobState::CancelRequested;
            }
        });
        Ok(CancelView {
            job_id,
            state: JobState::CancelRequested,
            still_unwinding: true,
            message: "cancellation requested; a native NumPy or PyTorch call can delay the stop"
                .to_owned(),
        })
    }

    /// Records that the viewer rendered a revision. Returns the job it belongs to.
    pub fn note_rendered(&self, revision: u64) -> Option<u64> {
        self.note_display(revision, None)
    }

    /// Records that loading a revision failed in the viewer.
    pub fn note_display_failed(&self, revision: u64, message: impl Into<String>) -> Option<u64> {
        self.note_display(revision, Some(message.into()))
    }

    fn note_display(&self, revision: u64, failure: Option<String>) -> Option<u64> {
        let mut jobs = self.registry.jobs.lock().ok()?;
        let job_id = jobs
            .iter()
            .filter(|(_, record)| record.revision == Some(revision))
            .max_by_key(|(job_id, _)| **job_id)
            .map(|(job_id, _)| *job_id)?;
        if let Some(record) = jobs.get_mut(&job_id) {
            record.display = match failure {
                Some(message) => DisplayState::Failed { message },
                None => DisplayState::Rendered,
            };
            record.displayed_revision = Some(revision);
        }
        Some(job_id)
    }

    /// Most recent jobs, newest first, for the UI's panel.
    pub fn recent(&self, limit: usize) -> Vec<JobSummary> {
        let mut summaries: Vec<JobSummary> = self
            .registry
            .jobs
            .lock()
            .map(|jobs| {
                jobs.values()
                    .map(|record| JobSummary {
                        job_id: record.job_id,
                        request_id: record.snapshot.request_id.clone(),
                        state: record.state,
                        display: record.display.clone(),
                        component_id: record.component_id.clone(),
                        revision: record.revision,
                        point_count: record.point_count,
                        queued_at_ms: record.timings.queued_at_ms,
                        error: record.error.as_ref().map(|error| error.message.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.job_id));
        summaries.truncate(limit);
        summaries
    }

    /// Readiness, versions and limits, as `python_runtime_info` reports them.
    pub fn runtime_report(&self) -> RuntimeReport {
        let info = self.executor.runner().describe();
        let mut report = self.registry.base_report.clone();
        if !info.packages.is_empty() {
            report.packages = info.packages;
        }
        if info.python_version.is_some() {
            report.python_version = info.python_version;
        }
        if !info.interpreter.is_empty() {
            report.interpreter = info.interpreter;
        }
        if info.error.is_some() {
            report.error = info.error;
        }
        report.ready = info.ready && report.packages_ready();
        report
    }

    /// Limits a caller can see before submitting.
    pub fn limits(&self) -> Limits {
        Limits::of(&self.registry.config.executor)
    }

    /// Interpreter busy state, for `python_runtime_info`.
    pub fn is_busy(&self) -> bool {
        self.executor.is_busy()
    }

    /// Waiting jobs, excluding the running one.
    pub fn queued_jobs(&self) -> usize {
        self.executor.queued()
    }

    /// Cancels waiting jobs and stops the execution thread.
    ///
    /// A script that never returns cannot be stopped safely, so this can wait for it; it
    /// is meant for the app's exit sequence, not for a request handler.
    pub fn shutdown(&self) {
        let running: Vec<u64> = self
            .registry
            .jobs
            .lock()
            .map(|jobs| {
                jobs.values()
                    .filter(|record| !record.state.is_terminal())
                    .map(|record| record.job_id)
                    .collect()
            })
            .unwrap_or_default();
        for job_id in running {
            self.executor.cancel(job_id);
        }
        self.executor.shutdown();
    }

    fn forget(&self, job_id: u64, request_id: &str) {
        if let Ok(mut jobs) = self.registry.jobs.lock() {
            jobs.remove(&job_id);
        }
        if let Ok(mut receipts) = self.registry.receipts.lock()
            && receipts
                .get(request_id)
                .is_some_and(|entry| entry.job_id == job_id)
        {
            receipts.remove(request_id);
        }
    }
}

/// Bridges executor callbacks into registry transitions.
struct Observer {
    registry: Arc<Registry>,
}

impl JobObserver for Observer {
    fn on_started(&self, job_id: u64) {
        let now = now_ms();
        self.registry.update(job_id, |record| {
            record.state = JobState::Running;
            record.timings.started_at_ms = Some(now);
            record.timings.waiting_ms = Some(now.saturating_sub(record.timings.queued_at_ms));
        });
    }

    fn on_finished(&self, outcome: ExecutorOutcome) {
        let finished_at = now_ms();
        // A job cancelled while it was queued was already reported as cancelled by
        // `cancel`; the executor's later callback must not reopen it.
        match self.registry.record(outcome.job_id) {
            Ok(record) if record.state.is_terminal() => return,
            Ok(_) => {}
            Err(_) => return,
        }
        if outcome.cancelled {
            self.registry.update(outcome.job_id, |record| {
                record.state = JobState::Cancelled;
                record.timings.finished_at_ms = Some(finished_at);
                record.timings.execution_ms = Some(outcome.duration.as_millis() as u64);
                record.progress.report(record.progress.fraction(), None);
                let reason = outcome
                    .cancel_reason
                    .map(CancelReason::name)
                    .unwrap_or("cancel_requested");
                record.error = Some(JobError {
                    code: "job_cancelled".to_owned(),
                    message: format!("the job was stopped ({reason}) and its candidate discarded"),
                    traceback: None,
                });
                if record.display == DisplayState::Pending {
                    record.display = DisplayState::NotRequested;
                }
            });
            return;
        }

        match outcome.result {
            Err(error) => {
                let job_error = JobError::of(&error);
                self.registry.update(outcome.job_id, |record| {
                    record.state = JobState::Failed;
                    record.timings.finished_at_ms = Some(finished_at);
                    record.timings.execution_ms = Some(outcome.duration.as_millis() as u64);
                    record.error = Some(job_error);
                    if record.display == DisplayState::Pending {
                        record.display = DisplayState::NotRequested;
                    }
                });
            }
            Ok(batch) => self.commit_candidate(outcome.job_id, batch, finished_at),
        }
    }
}

impl Observer {
    /// Validates a candidate, commits it atomically and reports display and export
    /// separately from the compute result.
    fn commit_candidate(&self, job_id: u64, batch: GaussianBatch, finished_at: u64) {
        let registry = &self.registry;
        let Ok(record) = registry.record(job_id) else {
            return;
        };
        let max_points = registry.config.executor.max_points;

        registry.update(job_id, |record| {
            record.state = JobState::Validating;
            record.timings.finished_at_ms = Some(finished_at);
        });

        if let Err(error) = batch.validate(max_points) {
            let job_error = JobError::of(&error);
            registry.update(job_id, |record| {
                record.state = JobState::Failed;
                record.error = Some(job_error);
            });
            return;
        }

        let bounds = batch.bounds().map(BoundsOut::from);
        let point_count = batch.len();
        let batch_component = batch.metadata.component_id.clone();
        let splat = match batch.to_splat(max_points) {
            Ok(splat) => splat,
            Err(error) => {
                let job_error = JobError::of(&error);
                registry.update(job_id, |record| {
                    record.state = JobState::Failed;
                    record.error = Some(job_error);
                });
                return;
            }
        };

        let runtime = RuntimeFingerprint::from_report(&registry.base_report);
        let provenance = RecipeRecord::new(&record.snapshot, &record.target.fingerprint(), runtime);

        registry.update(job_id, |record| {
            record.state = JobState::Committing;
        });

        let outcome = registry.target.commit(CommitRequest {
            target: record.target.clone(),
            splat,
            provenance: provenance.clone(),
        });

        match outcome {
            Ok(CommitOutcome::Committed { identity }) => {
                let export = record
                    .export_path
                    .as_ref()
                    .map(|path| export_candidate(path, &batch, &provenance, max_points));
                let display_ok = if record.display_requested {
                    match registry.target.publish(&record.target, &identity) {
                        Ok(()) => Ok(DisplayState::Pending),
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    Ok(DisplayState::NotRequested)
                };
                let display_error = display_ok.as_ref().err().cloned();

                registry.update(job_id, |record| {
                    record.state = JobState::Committed;
                    record.document_id = Some(identity.document_id.clone());
                    record.revision = Some(identity.revision);
                    record.component_id = identity
                        .component_id
                        .clone()
                        .or_else(|| record.target.component_id.clone())
                        .or(batch_component.clone());
                    record.point_count = Some(point_count);
                    record.bounds = bounds;
                    record.export = export;
                    record.display = display_ok.unwrap_or(DisplayState::Pending);
                    record.displayed_revision = None;
                    if let Some(message) = display_error {
                        record.display = DisplayState::Failed { message };
                    }
                });
            }
            Ok(CommitOutcome::Conflict {
                document_id,
                expected,
                actual,
            }) => {
                let message = format!(
                    "the document moved from revision {expected} to {actual} while the job ran; \
                     the candidate was discarded, and nothing was overwritten"
                );
                registry.update(job_id, |record| {
                    record.state = JobState::Conflict;
                    record.document_id = Some(document_id);
                    record.point_count = Some(point_count);
                    record.error = Some(JobError {
                        code: "document_conflict".to_owned(),
                        message,
                        traceback: None,
                    });
                    if record.display == DisplayState::Pending {
                        record.display = DisplayState::NotRequested;
                    }
                });
            }
            Err(error) => {
                let job_error = JobError::of(&error);
                registry.update(job_id, |record| {
                    record.state = JobState::Failed;
                    record.error = Some(job_error);
                    if record.display == DisplayState::Pending {
                        record.display = DisplayState::NotRequested;
                    }
                });
            }
        }
    }
}

/// Writes an exported PLY plus its recipe sidecar.
///
/// The export never changes the compute state: a full disk is reported in the job's
/// `export` field, and the geometry that was computed stays committed and displayed.
fn export_candidate(
    path: &PathBuf,
    batch: &GaussianBatch,
    provenance: &RecipeRecord,
    max_points: usize,
) -> ExportOutcome {
    let outcome = (|| -> Result<(usize, Option<String>)> {
        let splat = batch.to_splat(max_points)?;
        let bytes = splatmcp_core::write_ply(&splat)
            .map_err(|error| PythonError::Script(format!("could not encode the PLY: {error}")))?;
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                std::fs::create_dir_all(parent).map_err(|error| {
                    PythonError::Script(format!("could not create {}: {error}", parent.display()))
                })?;
            }
            _ => {}
        }
        std::fs::write(path, &bytes).map_err(|error| {
            PythonError::Script(format!("could not write {}: {error}", path.display()))
        })?;
        let sidecar = RecipeRecord::write_sidecar(path, provenance)
            .ok()
            .map(|path| path.to_string_lossy().to_string());
        Ok((bytes.len(), sidecar))
    })();

    match outcome {
        Ok((bytes, sidecar)) => ExportOutcome {
            path: path.to_string_lossy().to_string(),
            bytes,
            sidecar,
            error: None,
        },
        Err(error) => ExportOutcome {
            path: path.to_string_lossy().to_string(),
            bytes: 0,
            sidecar: None,
            error: Some(error.to_string()),
        },
    }
}

/// Logs one line from a running script, for the embedded binding.
pub fn log_line(context: &RunContext, level: LogLevel, text: impl Into<String>) {
    context.logs.push(level, text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrays::BatchMetadata;
    use crate::executor::RunnerInfo;
    use crate::runtime::{Limits, PackageVersion};
    use std::sync::atomic::AtomicBool;

    /// Runner that produces a chosen batch or error, so the service can be tested without
    /// an interpreter.
    struct StubRunner {
        batch_points: usize,
        fail: Option<PythonError>,
        calls: Mutex<usize>,
    }

    impl StubRunner {
        fn producing(points: usize) -> Arc<Self> {
            Arc::new(Self {
                batch_points: points,
                fail: None,
                calls: Mutex::new(0),
            })
        }

        fn failing(message: &str) -> Arc<Self> {
            Arc::new(Self {
                batch_points: 0,
                fail: Some(PythonError::Script(message.to_owned())),
                calls: Mutex::new(0),
            })
        }
    }

    impl ScriptRunner for StubRunner {
        fn describe(&self) -> RunnerInfo {
            RunnerInfo {
                ready: true,
                interpreter: "stub".to_owned(),
                python_version: Some("3.13.2".to_owned()),
                error: None,
                packages: vec![PackageVersion {
                    name: "numpy".to_owned(),
                    version: Some("2.3.3".to_owned()),
                    available: true,
                    required: true,
                    detail: None,
                }],
            }
        }

        fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch> {
            *self.calls.lock().unwrap() += 1;
            context.logs.push(LogLevel::Info, "stub run");
            if let Some(error) = &self.fail {
                return Err(match error {
                    PythonError::Script(message) => PythonError::Script(message.clone()),
                    other => PythonError::Script(other.to_string()),
                });
            }
            let mut batch = GaussianBatch::with_capacity(self.batch_points);
            for index in 0..self.batch_points {
                batch.push(
                    [index as f32 * 0.01, 0.0, 0.0],
                    [0.05; 3],
                    [1.0, 0.0, 0.0, 0.0],
                    [0.5, 0.5, 0.5],
                    1.0,
                );
            }
            if batch.is_empty() {
                batch.push([0.0; 3], [0.05; 3], [1.0, 0.0, 0.0, 0.0], [0.5; 3], 1.0);
            }
            Ok(batch)
        }
    }

    /// Document owner that keeps one revision counter and records every commit.
    struct StubTarget {
        revision: Mutex<u64>,
        commits: Mutex<Vec<String>>,
        published: Mutex<Vec<u64>>,
        publish_fails: bool,
    }

    impl StubTarget {
        fn new(revision: u64) -> Arc<Self> {
            Arc::new(Self {
                revision: Mutex::new(revision),
                commits: Mutex::new(Vec::new()),
                published: Mutex::new(Vec::new()),
                publish_fails: false,
            })
        }

        fn failing_publish() -> Arc<Self> {
            Arc::new(Self {
                revision: Mutex::new(3),
                commits: Mutex::new(Vec::new()),
                published: Mutex::new(Vec::new()),
                publish_fails: true,
            })
        }
    }

    impl DocumentTarget for StubTarget {
        fn snapshot(&self, target: &TargetSpec) -> Result<SourceSnapshot> {
            if target.document_id.is_none() {
                return Err(PythonError::DocumentConflict(
                    "a new document has no source snapshot".to_owned(),
                ));
            }
            let revision = *self.revision.lock().unwrap();
            let mut batch = GaussianBatch::with_capacity(1);
            batch.push([0.0; 3], [0.1; 3], [1.0, 0.0, 0.0, 0.0], [0.5; 3], 1.0);
            batch.metadata = BatchMetadata::default();
            Ok(SourceSnapshot {
                document_id: target.document_id.clone().unwrap_or_default(),
                revision,
                component_id: target.component_id.clone(),
                batch,
            })
        }

        fn commit(&self, request: CommitRequest) -> Result<CommitOutcome> {
            let mut revision = self.revision.lock().unwrap();
            if let Some(expected) = request.target.expected_revision
                && expected != *revision
            {
                let actual = *revision;
                return Ok(CommitOutcome::Conflict {
                    document_id: request.target.document_id.clone().unwrap_or_default(),
                    expected,
                    actual,
                });
            }
            *revision += 1;
            let identity = DocumentIdentity {
                document_id: request
                    .target
                    .document_id
                    .clone()
                    .unwrap_or_else(|| "doc-new".to_owned()),
                revision: *revision,
                point_count: request.splat.len(),
                bounds: request.splat.bounds().map(BoundsOut::from),
                component_id: request.target.component_id.clone(),
            };
            self.commits
                .lock()
                .unwrap()
                .push(request.provenance.content_hash.clone());
            Ok(CommitOutcome::Committed { identity })
        }

        fn publish(&self, _target: &TargetSpec, identity: &DocumentIdentity) -> Result<()> {
            if self.publish_fails {
                return Err(PythonError::Display("the viewer rejected the revision".to_owned()));
            }
            self.published.lock().unwrap().push(identity.revision);
            Ok(())
        }
    }

    fn service(runner: Arc<dyn ScriptRunner>, target: Arc<dyn DocumentTarget>) -> GenerationService {
        let config = ServiceConfig {
            executor: ExecutorConfig {
                max_points: 1000,
                ..ExecutorConfig::default()
            },
        };
        let report = RuntimeReport {
            ready: true,
            root: "C:/runtime".to_owned(),
            interpreter: "C:/runtime/python.exe".to_owned(),
            source: "application".to_owned(),
            python_version: Some("3.13.2".to_owned()),
            module_paths: Vec::new(),
            packages: Vec::new(),
            standard_library_paths: Vec::new(),
            limits: Limits::of(&config.executor),
            error: None,
        };
        GenerationService::start(runner, config, target, report)
    }

    fn request(request_id: &str, target: TargetSpec, display: bool) -> GenerationRequest {
        GenerationRequest {
            snapshot: ScriptSnapshot::inline(
                request_id,
                "def generate(ctx): ...",
                "generate",
                serde_json::json!({"count": 2}),
                11,
            )
            .unwrap(),
            target,
            display,
            export_path: None,
            deadline: None,
        }
    }

    /// Polls until the job reaches a terminal state.
    fn wait_for(service: &GenerationService, job_id: u64) -> JobView {
        for _ in 0..600 {
            let view = service.status(job_id, 0, 100).unwrap();
            if view.state.is_terminal() {
                return view;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("job {job_id} did not finish");
    }

    #[test]
    fn a_new_document_job_commits_publishes_and_renders() {
        let target = StubTarget::new(0);
        let service = service(StubRunner::producing(5), target.clone());
        let receipt = service
            .submit(request("req-1", TargetSpec::new_document(None), true))
            .unwrap();
        assert_eq!(receipt.state, JobState::Queued);
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed);
        assert_eq!(view.point_count, Some(5));
        assert!(view.bounds.is_some());
        assert_eq!(view.revision, Some(1));
        assert_eq!(view.display, DisplayState::Pending);
        assert_eq!(view.logs.len(), 1);
        assert!(view.log_cursor >= 1);
        assert_eq!(view.runtime.python_version, "3.13.2");

        // The viewer acknowledges the revision, which is tracked separately.
        assert_eq!(service.note_rendered(1), Some(receipt.job_id));
        let view = service.status(receipt.job_id, view.log_cursor, 100).unwrap();
        assert_eq!(view.display, DisplayState::Rendered);
        assert_eq!(view.displayed_revision, Some(1));
        assert!(view.logs.is_empty(), "the log cursor returned no repeats");
        assert_eq!(target.published.lock().unwrap().len(), 1);
        service.shutdown();
    }

    #[test]
    fn a_reused_request_id_with_the_same_content_returns_the_original_job() {
        let target = StubTarget::new(0);
        let runner = StubRunner::producing(3);
        let service = service(runner.clone(), target);
        let first = service
            .submit(request("req-dup", TargetSpec::new_document(None), false))
            .unwrap();
        wait_for(&service, first.job_id);
        let second = service
            .submit(request("req-dup", TargetSpec::new_document(None), false))
            .unwrap();
        assert!(second.deduplicated);
        assert_eq!(second.job_id, first.job_id);
        assert_eq!(*runner.calls.lock().unwrap(), 1, "the script ran once");
        service.shutdown();
    }

    #[test]
    fn a_reused_request_id_with_different_content_is_rejected() {
        let service = service(
            StubRunner::producing(3),
            StubTarget::new(0),
        );
        service
            .submit(request("req-x", TargetSpec::new_document(None), false))
            .unwrap();
        let mut changed = request("req-x", TargetSpec::new_document(None), false);
        changed.snapshot.params = serde_json::json!({"count": 9});
        let error = service.submit(changed).unwrap_err();
        assert_eq!(error.code(), "request_conflict");
        service.shutdown();
    }

    #[test]
    fn a_stale_expected_revision_becomes_a_conflict_without_overwriting() {
        let target = StubTarget::new(4);
        let service = service(StubRunner::producing(2), target.clone());
        // The job expects revision 3, but the document is already at 4.
        let receipt = service
            .submit(request(
                "req-conflict",
                TargetSpec::component("doc-1", "spire", 3),
                false,
            ))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Conflict);
        assert_eq!(view.error.as_ref().unwrap().code, "document_conflict");
        assert_eq!(*target.revision.lock().unwrap(), 4, "nothing was committed");
        assert!(target.commits.lock().unwrap().is_empty());
        service.shutdown();
    }

    #[test]
    fn a_component_edit_commits_at_the_expected_revision() {
        let target = StubTarget::new(2);
        let service = service(StubRunner::producing(4), target.clone());
        let receipt = service
            .submit(request(
                "req-edit",
                TargetSpec::component("doc-1", "spire", 2),
                true,
            ))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed);
        assert_eq!(view.revision, Some(3));
        assert_eq!(view.component_id.as_deref(), Some("spire"));
        assert_eq!(target.commits.lock().unwrap().len(), 1);
        service.shutdown();
    }

    #[test]
    fn a_mutation_without_expected_revision_is_refused_before_queueing() {
        let service = service(StubRunner::producing(1), StubTarget::new(1));
        let mut broken = request("req-bad", TargetSpec::component("doc-1", "spire", 1), false);
        broken.target.expected_revision = None;
        let error = service.submit(broken).unwrap_err();
        assert_eq!(error.code(), "document_conflict");
        assert!(service.recent(10).is_empty(), "nothing was registered");
        service.shutdown();
    }

    #[test]
    fn a_script_error_fails_the_job_with_its_code() {
        let service = service(StubRunner::failing("ZeroDivisionError: division by zero"), StubTarget::new(0));
        let receipt = service
            .submit(request("req-fail", TargetSpec::new_document(None), false))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Failed);
        let error = view.error.unwrap();
        assert_eq!(error.code, "python_script_error");
        assert!(error.message.contains("ZeroDivisionError"));
        service.shutdown();
    }

    #[test]
    fn a_display_failure_does_not_change_the_committed_state() {
        let service = service(StubRunner::producing(2), StubTarget::failing_publish());
        let receipt = service
            .submit(request("req-display", TargetSpec::new_document(None), true))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed, "the compute result stands");
        assert_eq!(view.revision, Some(4));
        match view.display {
            DisplayState::Failed { message } => assert!(message.contains("viewer rejected")),
            other => panic!("expected a display failure, got {other:?}"),
        }
        service.shutdown();
    }

    #[test]
    fn an_export_is_reported_next_to_the_commit() {
        let dir = std::env::temp_dir().join(format!("splatmcp-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("generated.ply");
        let service = service(StubRunner::producing(3), StubTarget::new(0));
        let mut request = request("req-export", TargetSpec::new_document(None), false);
        request.export_path = Some(path.clone());
        let receipt = service.submit(request).unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed);
        let export = view.export.unwrap();
        assert_eq!(export.bytes, std::fs::metadata(&path).unwrap().len() as usize);
        assert!(export.sidecar.is_some(), "the recipe sidecar was written");
        assert!(RecipeRecord::read_sidecar(&path).unwrap().is_some());
        assert!(view.error.is_none());
        std::fs::remove_dir_all(&dir).ok();
        service.shutdown();
    }

    #[test]
    fn an_output_budget_overflow_fails_the_validation_step() {
        let service = service(StubRunner::producing(5000), StubTarget::new(0));
        let receipt = service
            .submit(request("req-big", TargetSpec::new_document(None), false))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Failed);
        assert_eq!(view.error.unwrap().code, "budget_exceeded");
        service.shutdown();
    }

    #[test]
    fn cancelling_a_queued_job_reports_the_actual_state() {
        // A blocking runner keeps job 1 busy so job 2 is still queued when cancelled.
        struct Blocking {
            release: Arc<AtomicBool>,
        }
        impl ScriptRunner for Blocking {
            fn describe(&self) -> RunnerInfo {
                RunnerInfo {
                    ready: true,
                    interpreter: "blocking".to_owned(),
                    ..RunnerInfo::default()
                }
            }
            fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch> {
                while !self.release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                    context.check_cancelled()?;
                }
                let mut batch = GaussianBatch::with_capacity(1);
                batch.push([0.0; 3], [0.1; 3], [1.0, 0.0, 0.0, 0.0], [0.5; 3], 1.0);
                Ok(batch)
            }
        }
        let release = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn ScriptRunner> = Arc::new(Blocking {
            release: release.clone(),
        });
        let service = service(runner, StubTarget::new(0));
        let first = service
            .submit(request("req-a", TargetSpec::new_document(None), false))
            .unwrap();
        for _ in 0..400 {
            if service.is_busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = service
            .submit(request("req-b", TargetSpec::new_document(None), false))
            .unwrap();
        let cancelled = service.cancel(second.job_id).unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(!cancelled.still_unwinding);
        assert!(cancelled.message.contains("before it started"));

        // Cancelling the queued job was immediate, even though the first job is still
        // holding the interpreter.
        let view = service.status(second.job_id, 0, 10).unwrap();
        assert_eq!(view.state, JobState::Cancelled);
        assert_eq!(view.error.unwrap().code, "job_cancelled");

        release.store(true, Ordering::SeqCst);
        let first_view = wait_for(&service, first.job_id);
        assert_eq!(first_view.state, JobState::Committed);
        // The executor's late callback for job 2 did not reopen it.
        assert_eq!(
            service.status(second.job_id, 0, 10).unwrap().state,
            JobState::Cancelled
        );
        service.shutdown();
    }

    #[test]
    fn cancelling_a_finished_job_says_so() {
        let service = service(StubRunner::producing(1), StubTarget::new(0));
        let receipt = service
            .submit(request("req-done", TargetSpec::new_document(None), false))
            .unwrap();
        wait_for(&service, receipt.job_id);
        let cancelled = service.cancel(receipt.job_id).unwrap();
        assert_eq!(cancelled.state, JobState::Committed);
        assert!(!cancelled.still_unwinding);
        service.shutdown();
    }

    #[test]
    fn an_unknown_job_is_reported_as_missing() {
        let service = service(StubRunner::producing(1), StubTarget::new(0));
        let error = service.status(999, 0, 10).unwrap_err();
        assert_eq!(error.code(), "job_not_found");
        assert_eq!(service.cancel(999).unwrap_err().code(), "job_not_found");
        service.shutdown();
    }

    #[test]
    fn runtime_info_merges_the_manifest_with_the_live_interpreter() {
        let service = service(StubRunner::producing(1), StubTarget::new(0));
        let report = service.runtime_report();
        assert!(report.ready);
        assert_eq!(report.python_version.as_deref(), Some("3.13.2"));
        assert_eq!(report.packages[0].name, "numpy");
        assert_eq!(service.limits().max_points, 1000);
        assert!(!service.is_busy());
        service.shutdown();
    }

    #[test]
    fn recent_jobs_are_newest_first() {
        let service = service(StubRunner::producing(1), StubTarget::new(0));
        let first = service
            .submit(request("req-1", TargetSpec::new_document(None), false))
            .unwrap();
        wait_for(&service, first.job_id);
        let second = service
            .submit(request("req-2", TargetSpec::new_document(None), false))
            .unwrap();
        wait_for(&service, second.job_id);
        let recent = service.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].job_id, second.job_id);
        assert_eq!(recent[0].state, JobState::Committed);
        service.shutdown();
    }

    #[test]
    fn a_splat_can_be_rebuilt_from_a_batch_with_its_metadata() {
        let mut batch = GaussianBatch::with_capacity(1);
        batch.push([1.0, 0.0, 0.0], [0.1; 3], [1.0, 0.0, 0.0, 0.0], [1.0; 3], 0.5);
        batch.metadata = BatchMetadata {
            component_id: Some("spire".to_owned()),
            ..BatchMetadata::default()
        };
        let splat = batch.to_splat(10).unwrap();
        assert_eq!(splat.points.len(), 1);
        assert_eq!(splat.points[0].position, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn queue_admission_refuses_work_beyond_the_bound() {
        let release = Arc::new(AtomicBool::new(false));
        struct Blocking(Arc<AtomicBool>);
        impl ScriptRunner for Blocking {
            fn describe(&self) -> RunnerInfo {
                RunnerInfo {
                    ready: true,
                    interpreter: "blocking".to_owned(),
                    ..RunnerInfo::default()
                }
            }
            fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch> {
                while !self.0.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                    context.check_cancelled()?;
                }
                Ok(GaussianBatch::default())
            }
        }
        let config = ServiceConfig {
            executor: ExecutorConfig {
                queue_depth: 1,
                max_points: 100,
                ..ExecutorConfig::default()
            },
        };
        let report = RuntimeReport::unavailable("test", &Limits::of(&config.executor));
        let service = GenerationService::start(
            Arc::new(Blocking(release.clone())),
            config,
            StubTarget::new(0),
            report,
        );
        service
            .submit(request("req-1", TargetSpec::new_document(None), false))
            .unwrap();
        service
            .submit(request("req-2", TargetSpec::new_document(None), false))
            .unwrap();
        let error = service
            .submit(request("req-3", TargetSpec::new_document(None), false))
            .unwrap_err();
        assert_eq!(error.code(), "queue_full");
        // The refused job left no trace, so its request id is free again.
        assert!(service.recent(10).iter().all(|job| job.request_id != "req-3"));
        release.store(true, Ordering::SeqCst);
        service.shutdown();
    }

    #[test]
    fn a_pending_display_is_cleared_when_the_job_never_commits() {
        let service = service(StubRunner::failing("RuntimeError: boom"), StubTarget::new(0));
        let receipt = service
            .submit(request("req-nodisplay", TargetSpec::new_document(None), true))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Failed);
        assert_eq!(view.display, DisplayState::NotRequested);
        service.shutdown();
    }

    #[test]
    fn the_log_helper_marks_its_level() {
        let logs = LogSink::new(5, 4096);
        let context = Arc::new(RunContext {
            job_id: 1,
            source: String::new(),
            entry_point: "generate".to_owned(),
            params: serde_json::Value::Null,
            seed: 0,
            max_points: 10,
            cancel: CancelToken::new(),
            logs: logs.clone(),
            progress: ProgressSink::default(),
            source_snapshot: None,
        });
        log_line(&context, LogLevel::Warning, "careful");
        let (lines, _) = logs.tail(0);
        assert_eq!(lines[0].level, LogLevel::Warning);
        assert_eq!(lines[0].text, "careful");
    }
}
