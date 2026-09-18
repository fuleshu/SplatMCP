//! Shared asynchronous operation jobs: identity, state, progress and receipts.
//!
//! A long import, edit, export or capture must not hold a synchronous tool or bridge request
//! open, and it must not freeze the window. This module is the one job service the process
//! hosts: ordinary SplatMCP operations and the Python executor adapter both submit here, so
//! there is a single queue, a single set of states and a single receipt format.
//!
//! # States
//!
//! ```text
//! queued -> running -> validating -> committing -> committed
//!                 \-> cancel_requested -> cancelled
//!                 \-> failed / conflict
//!                 \-> completed            (a read-only job's success state)
//! ```
//!
//! `cancelled`, `committed`, `completed`, `failed` and `conflict` are terminal. A read-only
//! job never claims `committed`: [`JobState::Completed`] says exactly what happened.
//!
//! # Cancellation
//!
//! Cooperative, and honest about it:
//!
//! - a **queued** job is removed immediately;
//! - a **running** job is asked to stop, and its body checks at the documented boundaries
//!   ([`JobContext::check`], [`JobContext::progress`] and [`JobContext::log`] all do);
//! - before the commit linearization point the check runs once more, so cancellation can
//!   still win there;
//! - once [`JobContext::commit`] has started, **the commit wins**: a cancel that arrives
//!   after that point is recorded on the receipt as arriving too late, never as a lie that
//!   the work was stopped;
//! - a body stuck in an uninterruptible native call simply stays `cancel_requested` until it
//!   returns, which is why a job is never reported `cancelled` while it can still publish.
//!
//! # Reconnects and retention
//!
//! Progress updates are coalesced (the newest value wins) and each log line carries a
//! sequence number, so a client that lost a connection resumes with `log_after` and reads
//! the status it missed instead of restarting anything: a disconnect never cancels and never
//! resubmits an accepted job. Retention is bounded - at most
//! [`JobLimits::max_retained_jobs`] receipts, [`JobLimits::max_log_entries`] log lines per
//! job, and a receipt ttl after which the job reports that its receipt has expired. Jobs are
//! **session-only**: an id from an earlier run of the app is reported as such rather than
//! resolving to a different job, and no unfinished operation is ever rerun automatically.
//!
//! NumPy/PyTorch allocations inside a Python job are *not* hard process-wide memory-limited
//! by this service: it bounds what it queues, retains and logs, and the Python runtime's own
//! budgets are reported separately.

mod service;

pub use service::{JobContext, JobService, JobStats};

use std::fmt;
use std::time::Duration;

use crate::document::ArtifactChecksum;

/// Version of the job contract these types implement.
pub const JOB_CONTRACT_VERSION: u32 = 1;

/// Identity of one job, minted by a [`JobService`] for the session that owns it.
///
/// The rendered form embeds the session stamp, so a job id from an earlier run of the app
/// fails loudly instead of naming whatever job happens to hold that number now.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(String, u64);

impl JobId {
    /// Mints the `index`-th identity of `session`.
    pub fn mint(session: u64, index: u64) -> Self {
        Self(format!("job-{session:x}-{index}"), index)
    }

    /// Reads an identity back from text, or `None` when it was not produced by this format.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("job-")?;
        let (session, index) = rest.split_once('-')?;
        if session.is_empty() || !session.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let index: u64 = index.parse().ok()?;
        Some(Self(text.to_owned(), index))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Sequence number within the session that minted it.
    pub fn serial(&self) -> u64 {
        self.1
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a job does. The kind decides whether its success is a commit or a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Reading a scene from a file or an asset into the document.
    Import,
    /// Applying an edit batch, or a patch to the displayed document.
    Edit,
    /// Writing a document revision to a file.
    Export,
    /// Rendering a frame.
    Capture,
    /// A read-only inspection or measurement.
    Inspect,
    /// A generation or edit performed by a script.
    Generate,
}

impl JobKind {
    pub const ALL: [Self; 6] = [
        Self::Import,
        Self::Edit,
        Self::Export,
        Self::Capture,
        Self::Inspect,
        Self::Generate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Edit => "edit",
            Self::Export => "export",
            Self::Capture => "capture",
            Self::Inspect => "inspect",
            Self::Generate => "generate",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text.trim().to_ascii_lowercase())
    }

    /// True for a job that never mutates the document.
    ///
    /// Such a job completes with [`JobState::Completed`], which is deliberately a different
    /// word from `committed`: nothing was committed, and a receipt must not imply it was.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Export | Self::Capture | Self::Inspect)
    }
}

/// Lifecycle of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    CancelRequested,
    Validating,
    Committing,
    /// The document changed and the receipt records the new revision.
    Committed,
    /// A read-only job finished successfully.
    Completed,
    /// The job stopped before it could publish anything.
    Cancelled,
    Failed,
    /// The commit lost a revision race: nothing was overwritten.
    Conflict,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Validating => "validating",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Conflict => "conflict",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::CancelRequested,
            Self::Validating,
            Self::Committing,
            Self::Committed,
            Self::Completed,
            Self::Cancelled,
            Self::Failed,
            Self::Conflict,
        ]
        .into_iter()
        .find(|state| state.as_str() == text.trim().to_ascii_lowercase())
    }

    /// True once the job will not change state again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Completed | Self::Cancelled | Self::Failed | Self::Conflict
        )
    }

    /// True while the job is still able to publish a result.
    pub fn can_still_publish(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::CancelRequested | Self::Validating
        )
    }

    /// True for the two states a successful job ends in.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Committed | Self::Completed)
    }
}

/// What part of the work is in progress, for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Admitted,
    Reading,
    Decoding,
    Computing,
    Validating,
    Committing,
    Exporting,
    Publishing,
}

impl JobPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Reading => "reading",
            Self::Decoding => "decoding",
            Self::Computing => "computing",
            Self::Validating => "validating",
            Self::Committing => "committing",
            Self::Exporting => "exporting",
            Self::Publishing => "publishing",
        }
    }
}

/// Coalesced progress: only the newest value is kept, and a reconnect reads it from status.
#[derive(Debug, Clone, PartialEq)]
pub struct JobProgress {
    pub phase: JobPhase,
    /// Units done so far, in whatever unit the phase measures (gaussians, steps, bytes).
    pub done: u64,
    /// Units expected, when they are known before the work starts.
    pub total: Option<u64>,
    /// `done / total`, or `0` when the total is unknown. Never reported as 1.0 before the
    /// work is over.
    pub fraction: f32,
    /// Short human-readable note, e.g. which step is running.
    pub message: Option<String>,
    /// How many updates were superseded by this one, so a client can tell a stale view.
    pub coalesced: u64,
}

impl Default for JobProgress {
    fn default() -> Self {
        Self {
            phase: JobPhase::Admitted,
            done: 0,
            total: None,
            fraction: 0.0,
            message: None,
            coalesced: 0,
        }
    }
}

impl JobProgress {
    /// Single line, bounded, for a reply or the window's status line.
    pub fn describe(&self) -> String {
        match self.total {
            Some(total) => format!(
                "{} {}/{} ({:.0}%)",
                self.phase.as_str(),
                self.done,
                total,
                self.fraction * 100.0
            ),
            None => format!("{} {}", self.phase.as_str(), self.done),
        }
    }
}

/// Severity of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "info" => Some(Self::Info),
            "warning" | "warn" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// One bounded log line, with the sequence number a reconnect resumes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLogEntry {
    pub sequence: u64,
    pub at_ms: u64,
    pub level: LogLevel,
    pub message: String,
}

/// What a caller asks for, recorded on the receipt so a retry is recognisable.
///
/// Every field of the semantic request is part of [`JobRequest::identity_hash`], which is what
/// admission dedup compares. That matters because a caller may reuse one operation id while
/// changing what the job acts on - a different revision of the same document, a different
/// destination file, a different asset - and those are *different* jobs. Treating them as a
/// replay would answer with a receipt for work that did something else.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRequest {
    pub kind: JobKind,
    /// Operation name, e.g. `edit_batch`, `export`, `run_python_splat`.
    pub operation: String,
    /// Caller-supplied identity for retry detection.
    pub operation_id: Option<String>,
    /// Source or target identity, e.g. `doc-4f2a-1@7` or a file path.
    pub target: Option<String>,
    /// Registered asset the job reads, when it reads one.
    pub asset_id: Option<String>,
    /// File the job writes, when it writes one.
    pub path: Option<String>,
    /// Document the job acts on, when it names one.
    pub document_id: Option<String>,
    /// Revision that document must still be at, when the caller stated one.
    pub expected_revision: Option<u64>,
    /// Absolute deadline; an expired job fails with `deadline_exceeded` before it commits.
    pub deadline_ms: Option<u64>,
}

impl JobRequest {
    /// A request with nothing but its kind and operation named.
    pub fn new(kind: JobKind, operation: impl Into<String>) -> Self {
        Self {
            kind,
            operation: operation.into(),
            operation_id: None,
            target: None,
            asset_id: None,
            path: None,
            document_id: None,
            expected_revision: None,
            deadline_ms: None,
        }
    }

    pub fn with_operation_id(mut self, operation_id: impl Into<String>) -> Self {
        self.operation_id = Some(operation_id.into());
        self
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_asset(mut self, asset_id: impl Into<String>) -> Self {
        self.asset_id = Some(asset_id.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Names the document and the revision the job acts on.
    pub fn with_document(mut self, document_id: impl Into<String>, revision: Option<u64>) -> Self {
        self.document_id = Some(document_id.into());
        self.expected_revision = revision;
        self
    }

    pub fn with_deadline_ms(mut self, deadline_ms: u64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    /// Canonical text of the whole semantic request.
    ///
    /// Built field by field rather than through a serialisation crate, so the hash cannot change
    /// when a dependency changes its formatting.
    pub fn canonical(&self) -> String {
        format!(
            "kind={}|operation={}|target={:?}|asset={:?}|path={:?}|document={:?}|revision={:?}",
            self.kind.as_str(),
            self.operation,
            self.target,
            self.asset_id,
            self.path,
            self.document_id,
            self.expected_revision
        )
    }

    /// Stable hash of the whole request: the identity admission dedup compares.
    ///
    /// A changed target, revision, asset or destination produces a different hash, so a reused
    /// operation id with different work is refused as a conflict instead of replaying a receipt
    /// for something else. The operation id itself is deliberately *not* part of the hash - it is
    /// the key a caller chooses, and the hash is what makes a retry recognisable.
    pub fn identity_hash(&self) -> u64 {
        crate::document::fingerprint(self.canonical().as_bytes())
    }
}

/// What a job produced, kept bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    /// Nothing to report beyond the state.
    None,
    /// The document this job produced or read.
    Document {
        document_id: String,
        revision: u64,
        point_count: usize,
    },
    /// An artifact a job wrote, identified by its checksum rather than its path.
    Artifact {
        path: String,
        checksum: String,
        bytes: u64,
    },
    /// A short message, for a read-only job.
    Message(String),
}

impl JobResult {
    /// One bounded line; never the payload.
    pub fn describe(&self) -> String {
        match self {
            Self::None => "no result".to_owned(),
            Self::Document {
                document_id,
                revision,
                point_count,
            } => format!("{document_id}@{revision} with {point_count} gaussians"),
            Self::Artifact {
                path,
                checksum,
                bytes,
            } => format!("{path} ({bytes} bytes, {checksum})"),
            Self::Message(message) => message.clone(),
        }
    }
}

/// Why a job stopped, in the shape a receipt reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    /// Stable machine-readable code, e.g. `document_conflict`.
    pub code: String,
    pub message: String,
}

impl JobFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The failure a body returns when its own check found the job cancelled.
    pub fn cancelled() -> Self {
        Self::new("cancelled", "the job was cancelled before it published anything")
    }

    /// The failure a body returns when it ran past its deadline.
    pub fn deadline_exceeded() -> Self {
        Self::new("deadline_exceeded", "the job ran past its deadline")
    }

    /// A commit that lost a revision race.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("document_conflict", message)
    }
}

impl fmt::Display for JobFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

/// How a downstream side effect (export, display) ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffectState {
    NotRequested,
    /// Announced, not yet acknowledged: the same distinction the transaction receipts keep.
    Pending,
    Done,
    Failed(String),
}

impl SideEffectState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed(_) => "failed",
        }
    }
}

/// The recorded outcome of one job: identity, timings, progress, logs and result.
#[derive(Debug, Clone, PartialEq)]
pub struct JobReceipt {
    pub contract_version: u32,
    pub job_id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub operation: String,
    pub operation_id: Option<String>,
    pub request_hash: u64,
    pub target: Option<String>,
    pub admitted_at_ms: u64,
    /// When a worker started the body.
    pub started_at_ms: Option<u64>,
    /// When the job reached a terminal state.
    pub finished_at_ms: Option<u64>,
    pub progress: JobProgress,
    /// Log lines retained for this job.
    pub log_count: usize,
    /// Sequence number to pass as `log_after` to continue a dropped connection.
    pub next_log_sequence: u64,
    pub result: JobResult,
    pub failure: Option<JobFailure>,
    /// Separately recorded side effects: the commit is not the export, and neither is the
    /// display. A partial downstream failure is visible here instead of being folded into
    /// one success flag.
    pub export: SideEffectState,
    pub display: SideEffectState,
    /// Bounded notes, e.g. that a cancel arrived after the commit point.
    pub notes: Vec<String>,
    /// True when this receipt answers an identical retry instead of a new job.
    pub replayed: bool,
}

impl JobReceipt {
    /// One bounded line for a status reply or a log.
    pub fn summary(&self) -> String {
        let timed = match (self.started_at_ms, self.finished_at_ms) {
            (Some(start), Some(end)) => format!(" in {} ms", end.saturating_sub(start)),
            (Some(_), None) => " (running)".to_owned(),
            _ => String::new(),
        };
        format!(
            "{} {} {}{timed} ({})",
            self.job_id,
            self.kind.as_str(),
            self.state.as_str(),
            self.progress.describe()
        )
    }

    /// True when the job is finished, however it finished.
    pub fn is_finished(&self) -> bool {
        self.state.is_terminal()
    }
}

/// A job's status plus the log lines a caller asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub receipt: JobReceipt,
    pub logs: Vec<JobLogEntry>,
}

/// Everything that can go wrong when submitting or reading a job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    /// Admission was refused: the queue is at its bound.
    QueueFull { limit: usize },
    /// No job has that id in this session.
    UnknownJob { job_id: String, hint: String },
    /// The job happened, but its receipt is no longer retained.
    ReceiptExpired { job_id: String },
    /// The submission names an operation id that was used for a different request.
    OperationConflict {
        operation_id: String,
        /// Hash of the request that was submitted now.
        expected_hash: u64,
        /// Hash of the request the recorded job answered.
        recorded_hash: u64,
        /// The recorded job, so the caller can see what it really acted on.
        recorded: Box<JobReceipt>,
    },
    /// The service is shutting down and accepts no new work.
    ShuttingDown,
    /// The service's lock is poisoned.
    Unavailable { reason: String },
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull { limit } => write!(
                formatter,
                "the job queue holds its {limit} queued job(s); wait for one to finish and retry"
            ),
            Self::UnknownJob { job_id, hint } => {
                write!(formatter, "job {job_id} is not available: {hint}")
            }
            Self::ReceiptExpired { job_id } => write!(
                formatter,
                "job {job_id} finished, but its receipt is no longer retained; submit a new job"
            ),
            Self::OperationConflict {
                operation_id,
                expected_hash: _,
                recorded_hash: _,
                recorded,
            } => write!(
                formatter,
                "operation_id '{operation_id}' was already used for a different request: the \
                 recorded job acted on '{}', while this request names a different target, asset, \
                 destination or revision",
                recorded.target.as_deref().unwrap_or("the displayed document")
            ),
            Self::ShuttingDown => {
                write!(formatter, "the app is shutting down and accepts no new jobs")
            }
            Self::Unavailable { reason } => {
                write!(formatter, "the job service is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for JobError {}

impl JobError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::QueueFull { .. } => "queue_full",
            Self::UnknownJob { .. } => "unknown_job",
            Self::ReceiptExpired { .. } => "job_receipt_expired",
            Self::OperationConflict { .. } => "operation_conflict",
            Self::ShuttingDown => "shutting_down",
            Self::Unavailable { .. } => "job_service_unavailable",
        }
    }
}

/// Bounds the service enforces, reported verbatim in capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Most jobs that may wait at once.
    pub max_queued: usize,
    /// Most jobs that may run at once.
    pub max_running: usize,
    /// Most receipts kept, oldest evicted first.
    pub max_retained_jobs: usize,
    /// Most log lines kept per job.
    pub max_log_entries: usize,
    /// Longest single log line kept.
    pub max_log_chars: usize,
    /// How long a finished job's receipt stays readable.
    pub receipt_ttl_ms: u64,
    /// How long a job may run before it is failed at its next boundary.
    pub max_run_ms: u64,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            max_queued: 32,
            max_running: 4,
            max_retained_jobs: 64,
            max_log_entries: 500,
            max_log_chars: 500,
            receipt_ttl_ms: 15 * 60 * 1000,
            max_run_ms: 10 * 60 * 1000,
        }
    }
}

impl JobLimits {
    /// The exact numbers, for a capabilities reply.
    pub fn describe(&self) -> String {
        format!(
            "queued<={}, running<={}, retained_jobs<={}, log_entries<={}, log_chars<={}, \
             receipt_ttl_ms<={}, max_run_ms<={}",
            self.max_queued,
            self.max_running,
            self.max_retained_jobs,
            self.max_log_entries,
            self.max_log_chars,
            self.receipt_ttl_ms,
            self.max_run_ms
        )
    }
}

/// What the service is holding right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JobCounts {
    pub queued: usize,
    pub running: usize,
    pub retained: usize,
    pub committed: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub conflicted: u64,
    /// Jobs dropped because they finished and retention moved on.
    pub evicted: u64,
}

/// A submitted job, as admission returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct JobAdmission {
    pub job_id: JobId,
    pub state: JobState,
    /// True when this submission replayed an identical earlier request instead of queueing
    /// new work: the caller either already has the receipt, or will the moment it resolves.
    pub replayed: bool,
}

/// Default patience for a blocking wait on a job.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(5);

/// A checksum as a job receipt names it, so an artifact reference is comparable.
pub fn checksum_hex(checksum: &ArtifactChecksum) -> String {
    format!("{}:{}", checksum.algorithm, checksum.hex())
}

/// The body of an admitted job: the work itself, run on a service worker.
pub type JobBody = Box<dyn FnOnce(&JobContext) -> Result<JobResult, JobFailure> + Send>;
