//! Embedded CPython generation for SplatMCP.
//!
//! The desktop application hosts one CPython interpreter ([`executor`]) and one generation
//! service ([`service`]). A script builds a Gaussian batch with NumPy, the batch crosses
//! into Rust-owned data through the checked boundary in [`arrays`], and the result is
//! validated against the document's expected revision before it is committed as a new
//! revision.
//!
//! Design rules this crate follows:
//! - Python is optional to *build*: without the `embedded` feature the array contract, job
//!   store and geometry helpers still compile and test, so the rest of the app is never
//!   blocked by a missing interpreter.
//! - Python is optional to *run*: a missing runtime is reported as
//!   `python_runtime_unavailable` and the viewer, open, save and edit tools keep working.
//! - No document or viewer lock is held while Python runs or while a batch is converted.
//! - Cancellation is cooperative and reported honestly; a native call may delay the stop.
//!
//! Modules:
//! - [`arrays`] - the Gaussian array contract and its validation
//! - [`conventions`] - coordinate conventions shared by Python, Rust and the viewer
//! - [`geometry`] - parametric surface and curve sampling helpers
//! - [`runtime`] - where the application private interpreter lives and what it can import
//! - [`script`] - script snapshots, seeds and recipe provenance
//! - [`executor`] - the dedicated execution thread, bounded queue and cancellation
//! - [`service`] - job submission, deduplication, validation and atomic commit
//! - [`embedded`] - the PyO3 interpreter binding (feature `embedded`)

pub mod arrays;
pub mod conventions;
pub mod executor;
pub mod geometry;
pub mod runtime;
pub mod script;
pub mod service;

#[cfg(feature = "embedded")]
pub mod embedded;

pub use arrays::{BatchMetadata, GaussianBatch, MAX_BATCH_POINTS};
pub use conventions::{AuthoringSpace, ConventionReport};
pub use executor::{
    CancelReason, CancelToken, ExecutorConfig, JobLogLine, JobObserver, JobTicket, LogLevel,
    LogSink, ProgressSink, PythonExecutor, RunContext, RunnerInfo, ScriptRunner, SourceSnapshot,
};
pub use geometry::{CurveKind, CurveSpec, SurfaceKind, SurfaceSpec};
pub use runtime::{
    Limits, PackageVersion, PythonRuntime, RuntimeFingerprint, RuntimeReport, RuntimeRoots,
};
pub use script::{RecipeRecord, ScriptSnapshot, SourceOrigin};
pub use service::{
    CancelView, CommitOutcome, CommitRequest, DisplayState, DocumentIdentity, DocumentTarget,
    ExportOutcome, GenerationRequest, GenerationService, JobError, JobReceipt, JobState,
    JobSummary, JobView, PublishOptions, ServiceConfig, TargetSpec, Timings,
};

use thiserror::Error;

/// Everything that can go wrong while running Python or handling its result.
///
/// The variants are the ones a caller has to tell apart: a missing interpreter is a
/// packaging problem, a Python error is a script problem, an invalid batch is an array
/// contract problem, and a conflict means the document moved on while the job ran.
#[derive(Debug, Error)]
pub enum PythonError {
    #[error("python_runtime_unavailable: {0}")]
    RuntimeUnavailable(String),
    #[error("python_script_error: {0}")]
    Script(String),
    #[error("invalid_batch: {0}")]
    InvalidBatch(String),
    #[error("budget_exceeded: {0}")]
    BudgetExceeded(String),
    #[error("job_not_found: {0}")]
    JobNotFound(String),
    #[error("request_conflict: {0}")]
    RequestConflict(String),
    #[error("document_conflict: {0}")]
    DocumentConflict(String),
    #[error("unknown_document: {0}")]
    UnknownDocument(String),
    #[error("no_document: {0}")]
    NoDocument(String),
    #[error("snapshot_expired: {0}")]
    SnapshotExpired(String),
    #[error("display_failed: {0}")]
    Display(String),
    #[error("queue_full: {0}")]
    QueueFull(String),
    #[error("job_cancelled: {0}")]
    Cancelled(String),
}

pub type Result<T> = std::result::Result<T, PythonError>;

impl PythonError {
    /// Stable machine readable code, used by the MCP tool replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::RuntimeUnavailable(_) => "python_runtime_unavailable",
            Self::Script(_) => "python_script_error",
            Self::InvalidBatch(_) => "invalid_batch",
            Self::BudgetExceeded(_) => "budget_exceeded",
            Self::JobNotFound(_) => "job_not_found",
            Self::RequestConflict(_) => "request_conflict",
            Self::DocumentConflict(_) => "document_conflict",
            Self::UnknownDocument(_) => "unknown_document",
            Self::NoDocument(_) => "no_document",
            Self::SnapshotExpired(_) => "snapshot_expired",
            Self::Display(_) => "display_failed",
            Self::QueueFull(_) => "queue_full",
            Self::Cancelled(_) => "job_cancelled",
        }
    }

    /// The message without the stable code prefix.
    ///
    /// Useful when the detail is stored and re-wrapped, so a failure reported through
    /// several layers does not accumulate repeated codes.
    pub fn detail(&self) -> String {
        match self {
            Self::RuntimeUnavailable(message)
            | Self::Script(message)
            | Self::InvalidBatch(message)
            | Self::BudgetExceeded(message)
            | Self::JobNotFound(message)
            | Self::RequestConflict(message)
            | Self::DocumentConflict(message)
            | Self::UnknownDocument(message)
            | Self::NoDocument(message)
            | Self::SnapshotExpired(message)
            | Self::Display(message)
            | Self::QueueFull(message)
            | Self::Cancelled(message) => message.clone(),
        }
    }

    /// Maps a document failure onto the code that names it.
    ///
    /// A caller has to be able to tell the cases apart: a document that is not available
    /// (something else was opened in the meantime), a revision that raced (a conflict to
    /// reconcile) and a revision that retention has already evicted (expired, so the
    /// request has to start again) are three different problems with three different
    /// answers. Collapsing them into one code hides which one happened.
    pub fn document(error: &splatmcp_core::DocumentError) -> Self {
        let message = error.to_string();
        match error {
            splatmcp_core::DocumentError::Conflict { .. } => Self::DocumentConflict(message),
            splatmcp_core::DocumentError::UnknownDocument { .. } => Self::UnknownDocument(message),
            splatmcp_core::DocumentError::NoDocument => Self::NoDocument(message),
            splatmcp_core::DocumentError::SnapshotExpired { .. } => Self::SnapshotExpired(message),
        }
    }

    /// True when the failure means the caller should change its request rather than retry
    /// it unchanged.
    pub fn is_caller_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidBatch(_)
                | Self::BudgetExceeded(_)
                | Self::RequestConflict(_)
                | Self::DocumentConflict(_)
                | Self::UnknownDocument(_)
                | Self::NoDocument(_)
                | Self::SnapshotExpired(_)
                | Self::JobNotFound(_)
                | Self::QueueFull(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{DocumentError, DocumentHandle, DocumentId};

    /// Every document failure keeps its own code, so a caller can act on the difference
    /// instead of reading a revision race into every problem.
    #[test]
    fn document_failures_keep_their_own_codes() {
        let handle = DocumentHandle::new(DocumentId::mint(1, 1), 3);
        let cases = [
            (DocumentError::NoDocument, "no_document"),
            (
                DocumentError::UnknownDocument {
                    document_id: DocumentId::mint(1, 9),
                    active: Some(DocumentId::mint(1, 1)),
                },
                "unknown_document",
            ),
            (
                DocumentError::Conflict {
                    expected: DocumentHandle::new(handle.document_id.clone(), 2),
                    current: handle.clone(),
                },
                "document_conflict",
            ),
            (
                DocumentError::SnapshotExpired {
                    handle: handle.clone(),
                },
                "snapshot_expired",
            ),
        ];
        for (error, code) in cases {
            let mapped = PythonError::document(&error);
            assert_eq!(mapped.code(), code, "{error}");
            assert!(mapped.is_caller_error());
            // The message keeps the detail the document layer reported.
            assert!(!mapped.detail().is_empty());
        }

        // Opening a different document is not a revision race, and is not reported as one.
        assert_eq!(
            PythonError::document(&DocumentError::UnknownDocument {
                document_id: DocumentId::mint(1, 9),
                active: None,
            })
            .code(),
            "unknown_document"
        );
    }

    #[test]
    fn every_error_carries_a_stable_code() {
        let errors = [
            PythonError::RuntimeUnavailable("none".to_owned()),
            PythonError::Script("boom".to_owned()),
            PythonError::InvalidBatch("shape".to_owned()),
            PythonError::BudgetExceeded("too many".to_owned()),
            PythonError::JobNotFound("no job 4".to_owned()),
            PythonError::RequestConflict("reused".to_owned()),
            PythonError::DocumentConflict("stale".to_owned()),
            PythonError::UnknownDocument("gone".to_owned()),
            PythonError::NoDocument("empty".to_owned()),
            PythonError::SnapshotExpired("evicted".to_owned()),
            PythonError::Display("viewer".to_owned()),
            PythonError::QueueFull("busy".to_owned()),
            PythonError::Cancelled("stopped".to_owned()),
        ];
        for error in errors {
            assert!(!error.code().is_empty());
            assert!(error.to_string().starts_with(error.code()));
        }
    }

    #[test]
    fn the_detail_carries_the_message_without_the_code() {
        let error = PythonError::InvalidBatch("scales must be positive".to_owned());
        assert_eq!(error.detail(), "scales must be positive");
        assert_eq!(error.to_string(), "invalid_batch: scales must be positive");
        assert!(!error.detail().contains("invalid_batch"));
    }

    #[test]
    fn caller_errors_are_told_apart_from_environment_errors() {
        assert!(PythonError::InvalidBatch("x".to_owned()).is_caller_error());
        assert!(!PythonError::RuntimeUnavailable("x".to_owned()).is_caller_error());
        assert!(!PythonError::Script("x".to_owned()).is_caller_error());
    }
}
