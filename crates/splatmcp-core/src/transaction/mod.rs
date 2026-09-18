//! Atomic, previewable, retry-safe and undoable edit batches.
//!
//! Task #13 in one module. The store (`crate::document`) owns identity, revisions and
//! compare-and-swap; this module owns the *transaction boundary* around an edit sequence:
//!
//! ```text
//! request (operation id + canonical hash)
//!   -> replay a recorded receipt, or refuse conflicting content with the same id
//!   -> resolve the source revision (exact snapshot)
//!   -> resolve targets against that snapshot (stable point ids by default)
//!   -> apply the whole operation list to a detached candidate
//!   -> validate the candidate
//!   -> commit once, under compare-and-swap, as exactly one new revision
//!   -> record an undo step and a receipt
//! ```
//!
//! Anything that fails before the commit leaves the authoritative document untouched: the
//! candidate is a copy, and the store is only ever asked to swap in a validated candidate.
//!
//! # Target resolution
//!
//! [`TargetResolution::Stable`] (the default) resolves every step's targets **once, against the
//! source snapshot**, keeps them as stable [`PointId`]s and re-maps them to rows before each
//! step. Deleting or merging points therefore cannot redirect a later operation through shifted
//! row indices - the classic "the delete moved everything left" bug.
//!
//! [`TargetResolution::Sequential`] is the documented opt-in alternative: a step that selects by
//! component, box or attribute re-evaluates that selection against the *candidate as it stands
//! after the previous step*. Explicit point ids always resolve by identity in both modes.
//!
//! # Preview
//!
//! [`TransactionService::preview`] runs the same pipeline and keeps the candidate behind a
//! bounded handle ([`PreviewOutcome`]). Previewing does not touch the displayed document.
//! Committing a preview requires the caller's expectation to still resolve to the exact revision
//! the preview was built from, so a stale preview can never overwrite newer work.
//!
//! # Idempotency
//!
//! `operation_id` plus the canonical request hash identifies one mutation. An identical retry
//! returns the recorded receipt (marked `replayed`); the same id with different content is a
//! conflict; an id whose receipt is no longer retained returns an explicit *unknown outcome*
//! rather than replaying a destructive operation. The ledger lives in memory and is bounded, so
//! after a process restart an old id is unknown: the caller must inspect the document.
//!
//! # Undo and redo
//!
//! History is bounded by entries and by bytes, per document, and evicts the oldest step first.
//! Undo and redo each commit a **new** revision - they never rewind the revision counter - and a
//! new edit clears the redo stack. Previews and selection handles built on a revision that has
//! been replaced are dropped, so they cannot be committed later.
//!
//! # What this module does *not* own
//!
//! Export and viewer presentation are the app's business. A receipt therefore reports commit,
//! export and display as three separate outcomes, and the app fills in the last two: a committed
//! edit whose export or display failed is still a recorded commit.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::components::{
    AuthoringSet, Component, ComponentId, Frame, LocalTransform, PointId, SelectionError,
    SelectionHandle, SelectionHandles, SelectionQuery,
};
use crate::document::{
    DocumentError, DocumentHandle, DocumentId, DocumentStore, Expected, Mutation, fingerprint,
    now_ms,
};
use crate::edit::{self, EditOp, EditStep, Selection};
use crate::{Bounds, Splat};

/// Largest preview candidate retained per process.
pub const MAX_PREVIEWS: usize = 4;
/// Bytes of preview candidates retained per process.
pub const MAX_PREVIEW_BYTES: usize = 256 * 1024 * 1024;
/// Recorded receipts kept for retry detection.
pub const MAX_RECEIPTS: usize = 32;
/// How long a recorded receipt stays authoritative for a retry.
pub const RECEIPT_TTL_MS: u64 = 15 * 60 * 1000;
/// Undo steps kept per document.
pub const MAX_HISTORY_ENTRIES: usize = 8;
/// Geometry bytes of undo history kept per document.
pub const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;

/// Bounded resources of the transaction service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLimits {
    pub max_previews: usize,
    pub max_preview_bytes: usize,
    pub max_receipts: usize,
    pub receipt_ttl_ms: u64,
    pub max_history_entries: usize,
    pub max_history_bytes: usize,
    pub max_selection_handles: usize,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            max_previews: MAX_PREVIEWS,
            max_preview_bytes: MAX_PREVIEW_BYTES,
            max_receipts: MAX_RECEIPTS,
            receipt_ttl_ms: RECEIPT_TTL_MS,
            max_history_entries: MAX_HISTORY_ENTRIES,
            max_history_bytes: MAX_HISTORY_BYTES,
            max_selection_handles: crate::components::MAX_SELECTION_HANDLES,
        }
    }
}

/// How a step's targets are resolved when a batch is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetResolution {
    /// Resolve once against the source snapshot, act through stable point ids. The default.
    #[default]
    Stable,
    /// Re-evaluate selection-style targets against the candidate after each step.
    Sequential,
}

/// What one step acts on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchTargets {
    /// Attribute/box/sphere predicates, with the edit layer's filter semantics.
    pub selection: Selection,
    /// Frame the box predicates are evaluated in.
    pub frame: Frame,
    /// Restrict to one component's members.
    pub component: Option<ComponentId>,
    /// Restrict to these exact gaussians.
    pub point_ids: Vec<PointId>,
    /// Restrict to a saved selection handle. Resolved by the service, never re-evaluated.
    pub selection_handle: Option<u64>,
}

impl BatchTargets {
    /// Targets every gaussian.
    pub fn all() -> Self {
        Self::default()
    }

    /// Targets from an edit-layer selection, as the existing tools produce it.
    pub fn from_selection(selection: Selection) -> Self {
        Self {
            selection,
            ..Self::default()
        }
    }

    /// Targets exactly these gaussians.
    pub fn points(point_ids: Vec<PointId>) -> Self {
        Self {
            point_ids,
            ..Self::default()
        }
    }

    /// Expands the edit-layer selection into a full query.
    pub fn to_query(&self) -> SelectionQuery {
        SelectionQuery {
            component: self.component.clone(),
            point_ids: self.point_ids.clone(),
            within: self.selection.within,
            outside: self.selection.outside,
            sphere: None,
            frame: self.frame,
            color_min: self.selection.color_min,
            color_max: self.selection.color_max,
            opacity_min: self.selection.opacity_min,
            max_radius: self.selection.max_radius,
            first: self.selection.first,
        }
    }
}

/// One operation and what it acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStep {
    pub op: EditOp,
    pub targets: BatchTargets,
}

impl BatchStep {
    /// A step acting on every gaussian.
    pub fn new(op: EditOp) -> Self {
        Self {
            op,
            targets: BatchTargets::all(),
        }
    }

    /// A step acting on one selection.
    pub fn with_targets(op: EditOp, targets: BatchTargets) -> Self {
        Self { op, targets }
    }

    /// A step from an edit-layer step.
    pub fn of(step: &EditStep) -> Self {
        Self {
            op: step.op.clone(),
            targets: BatchTargets::from_selection(step.selection.clone()),
        }
    }
}

/// A complete, atomic edit sequence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditBatch {
    /// Caller-supplied identity for retry detection.
    pub operation_id: Option<String>,
    pub resolution: TargetResolution,
    pub steps: Vec<BatchStep>,
}

impl EditBatch {
    /// A batch of steps with default (stable) resolution.
    pub fn new(steps: Vec<BatchStep>) -> Self {
        Self {
            operation_id: None,
            resolution: TargetResolution::Stable,
            steps,
        }
    }

    /// The same batch, in the sequential-resolution mode.
    pub fn sequential(mut self) -> Self {
        self.resolution = TargetResolution::Sequential;
        self
    }

    /// The same batch, named by an operation id.
    pub fn with_operation_id(mut self, operation_id: impl Into<String>) -> Self {
        self.operation_id = Some(operation_id.into());
        self
    }

    /// A batch from edit-layer steps, i.e. the existing `edit_splat` shape.
    pub fn of_steps(steps: &[EditStep]) -> Self {
        Self::new(steps.iter().map(BatchStep::of).collect())
    }

    /// Checks the batch describes work that can be run.
    pub fn validate(&self) -> Result<(), TransactionError> {
        if self.steps.is_empty() {
            return Err(TransactionError::Invalid(
                "no operations given; pass at least one step".to_owned(),
            ));
        }
        if let Some(operation_id) = &self.operation_id
            && operation_id.trim().is_empty()
        {
            return Err(TransactionError::Invalid(
                "operation_id must not be blank when it is given".to_owned(),
            ));
        }
        for (index, step) in self.steps.iter().enumerate() {
            if step.targets.frame == Frame::Local && step.targets.component.is_none() {
                return Err(TransactionError::Invalid(format!(
                    "step {index} asks for a local frame without naming a component"
                )));
            }
        }
        Ok(())
    }

    /// Canonical text of the request: two requests with the same text are the same mutation.
    ///
    /// Built field by field rather than from a serialisation crate, so the hash cannot change
    /// when a dependency changes its formatting.
    pub fn canonical(&self) -> String {
        let mut text = String::from(match self.resolution {
            TargetResolution::Stable => "resolution=stable",
            TargetResolution::Sequential => "resolution=sequential",
        });
        for step in &self.steps {
            text.push_str("|op=");
            match &step.op {
                EditOp::Translate { by } => text.push_str(&format!("translate{by:?}")),
                EditOp::Rotate {
                    axis,
                    degrees,
                    center,
                } => text.push_str(&format!("rotate{axis:?}/{degrees:?}/{center:?}")),
                EditOp::Scale { center, factor } => {
                    text.push_str(&format!("scale{center:?}/{factor:?}"))
                }
                EditOp::SetRadius { factor } => text.push_str(&format!("set_radius{factor:?}")),
                EditOp::AdjustColor { delta } => text.push_str(&format!("adjust_color{delta:?}")),
                EditOp::SetColor { color, mix } => {
                    text.push_str(&format!("set_color{color:?}/{mix:?}"))
                }
                EditOp::SetOpacity { factor } => text.push_str(&format!("set_opacity{factor:?}")),
                EditOp::Duplicate { by } => text.push_str(&format!("duplicate{by:?}")),
                EditOp::Remove => text.push_str("remove"),
                EditOp::Merge { points } => {
                    text.push_str(&format!("merge:{}", points.len()));
                    for point in points {
                        text.push_str(&format!(
                            "{:?}{:?}{:?}{:?}{:?}",
                            point.position, point.scale, point.color, point.opacity, point.rotation
                        ));
                    }
                }
            }
            let targets = &step.targets;
            text.push_str(&format!(
                "?within{:?}outside{:?}color_min{:?}color_max{:?}opacity{:?}radius{:?}first{:?}frame{:?}component{:?}points{}",
                targets.selection.within,
                targets.selection.outside,
                targets.selection.color_min,
                targets.selection.color_max,
                targets.selection.opacity_min,
                targets.selection.max_radius,
                targets.selection.first,
                targets.frame,
                targets.component,
                targets.point_ids.len(),
            ));
            for id in &targets.point_ids {
                text.push_str(&format!(",{}", id.serial()));
            }
            if let Some(handle) = targets.selection_handle {
                text.push_str(&format!("handle{handle}"));
            }
        }
        text
    }

    /// Stable hash of [`EditBatch::canonical`].
    pub fn request_hash(&self) -> u64 {
        fingerprint(self.canonical().as_bytes())
    }
}

/// What one step did, once the batch ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchStepReport {
    pub op_index: usize,
    /// Gaussians the step changed, or removed.
    pub affected: usize,
    /// Gaussians in the candidate afterwards.
    pub remaining: usize,
}

/// Dry-run result: what a batch would do, without doing it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewReport {
    /// Revision the preview was built from.
    pub source: DocumentHandle,
    pub steps: Vec<BatchStepReport>,
    pub points_before: usize,
    pub points_after: usize,
    pub bounds_before: Option<Bounds>,
    pub bounds_after: Option<Bounds>,
    /// Validation and addressing warnings a caller should read before committing.
    pub warnings: Vec<String>,
    /// Geometry the candidate holds, and the point identities that go with it.
    pub memory_estimate_bytes: usize,
    pub point_ids: usize,
}

/// A retained preview candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewOutcome {
    pub preview_id: u64,
    pub report: PreviewReport,
}

/// A preview candidate's content, for a renderer or an export.
#[derive(Debug, Clone)]
pub struct PreviewSnapshot {
    pub preview_id: u64,
    pub report: PreviewReport,
    pub splat: Arc<Splat>,
}

/// A candidate committed from a preview, as the receipt reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewCommit {
    pub preview_id: u64,
    /// Revision the preview was built from.
    pub source: u64,
}

/// Outcome of an optional side effect that is *not* part of the document commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    /// The caller did not ask for it.
    NotRequested,
    /// The revision was handed to the renderer, which has not acknowledged it yet.
    ///
    /// Deliberately *not* [`SideEffect::Done`]: an event that left the app successfully says
    /// nothing about whether anything was drawn, so a receipt never claims a revision was
    /// presented when all that happened is that it was announced.
    Published,
    /// It happened and was acknowledged.
    Done,
    /// It failed, and the message says why. The commit, if any, still stands.
    Failed(String),
}

impl SideEffect {
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// True when the caller asked for it and it succeeded.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// True when the revision was announced but not acknowledged yet.
    pub fn is_published(&self) -> bool {
        matches!(self, Self::Published)
    }

    /// True when the caller asked for it at all.
    pub fn is_requested(&self) -> bool {
        !matches!(self, Self::NotRequested)
    }
}

/// The document a receipt describes, recorded at commit time.
///
/// A receipt is replayed long after the fact - possibly after the document has been replaced or
/// the revision evicted - so it carries its own copy of what it produced instead of resolving
/// *current* state when it is read back. That is the difference between "here is what your edit
/// did" and "here is what happens to be displayed now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptDocument {
    pub document_id: DocumentId,
    pub revision: u64,
    pub point_count: usize,
    /// File the document was saved under when the commit happened.
    pub file_name: String,
}

impl ReceiptDocument {
    /// The exact handle this receipt produced.
    pub fn handle(&self) -> DocumentHandle {
        DocumentHandle::new(self.document_id.clone(), self.revision)
    }
}

/// What a committed batch produced: identity, per-step counts and separate side effects.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionReceipt {
    /// Caller-supplied identity, when one was given.
    pub operation_id: Option<String>,
    /// Hash of the canonical request this receipt answers.
    pub request_hash: u64,
    /// What the commit produced, recorded rather than re-resolved on read.
    pub recorded: ReceiptDocument,
    /// The revision the commit produced.
    pub document: DocumentHandle,
    /// Always true for a receipt; kept explicit so a caller never infers it.
    pub committed: bool,
    pub steps: Vec<BatchStepReport>,
    pub point_count: usize,
    /// Set when the commit came from a preview candidate.
    pub preview: Option<PreviewCommit>,
    /// Undo/redo availability after this commit.
    pub undo_available: bool,
    pub redo_available: bool,
    /// File export outcome; the app fills this in.
    pub export: SideEffect,
    /// Viewer presentation outcome; the app fills this in.
    pub display: SideEffect,
    /// True when this receipt was replayed for an identical retry.
    pub replayed: bool,
    /// Warnings the transaction produced, e.g. an authoring layer that had to be rebuilt.
    pub warnings: Vec<String>,
    pub at_ms: u64,
}

impl TransactionReceipt {
    /// Records the export outcome without touching the commit.
    pub fn with_export(mut self, export: SideEffect) -> Self {
        self.export = export;
        self
    }

    /// Records the display outcome without touching the commit.
    pub fn with_display(mut self, display: SideEffect) -> Self {
        self.display = display;
        self
    }

    /// The same receipt, marked as a replay.
    ///
    /// A replay keeps the recorded document, counts and side effects and only changes this flag:
    /// re-running the request changed nothing, so nothing about the outcome may differ.
    pub fn replayed(mut self) -> Self {
        self.replayed = true;
        self
    }

    /// The revision this receipt produced, as a handle.
    pub fn handle(&self) -> DocumentHandle {
        self.recorded.handle()
    }
}

/// One undo step: the state before and after a committed transaction.
#[derive(Debug, Clone)]
struct SnapshotState {
    splat: Arc<Splat>,
    ids: Vec<PointId>,
    authoring: AuthoringSet,
}

impl SnapshotState {
    fn bytes(&self) -> usize {
        self.splat.len() * std::mem::size_of::<crate::SplatPoint>()
            + self.ids.len() * std::mem::size_of::<PointId>()
    }
}

#[derive(Debug, Clone)]
struct HistoryItem {
    id: u64,
    label: String,
    document: DocumentId,
    before: SnapshotState,
    after: SnapshotState,
    /// Revision the `after` state was last recorded at: the step's own revision while it sits
    /// on the undo stack, or the revision an undo/redo produced after a cycle.
    revision: u64,
    at_ms: u64,
}

impl HistoryItem {
    fn bytes(&self) -> usize {
        self.before.bytes() + self.after.bytes()
    }
}

/// One undo step, as a reply describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: u64,
    pub label: String,
    /// Revision the step produced.
    pub revision: u64,
    pub point_count: usize,
    pub at_ms: u64,
}

/// Undo/redo availability and the bounded steps kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryReport {
    pub document: Option<DocumentHandle>,
    /// Newest undoable step.
    pub undo: Option<HistoryEntry>,
    /// Newest redoable step.
    pub redo: Option<HistoryEntry>,
    /// Undo steps, newest first.
    pub entries: Vec<HistoryEntry>,
    pub retained_bytes: usize,
    pub max_bytes: usize,
}

/// Components of one document, as an inspection reply describes them.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentList {
    pub document: DocumentHandle,
    pub components: Vec<Component>,
    /// True when the authoring layer was rebuilt for this revision, so ids are new and no
    /// component membership survived.
    pub rebuilt: bool,
}

/// A metadata change that produced a new revision.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentChange {
    pub document: DocumentHandle,
    pub component_id: ComponentId,
}

/// Everything the transaction service can refuse, with a stable code per case.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionError {
    /// Identity, revision or retention: the document half of the failure.
    Document(DocumentError),
    /// The request does not describe work that can be run.
    Invalid(String),
    /// An operation failed; the document was not changed.
    Edit(String),
    /// A selection could not be resolved; the document was not changed.
    Selection(SelectionError),
    /// The same operation id was reused with different content.
    OperationConflict {
        operation_id: String,
        expected_hash: u64,
        recorded_hash: u64,
        recorded: DocumentHandle,
    },
    /// The operation id is known but its receipt is no longer authoritative.
    UnknownOutcome {
        operation_id: String,
        reason: String,
    },
    /// The preview handle was evicted or never existed.
    PreviewExpired { preview_id: u64 },
    /// The preview was built from a revision that is no longer current.
    PreviewConflict {
        preview_id: u64,
        expected: DocumentHandle,
        current: DocumentHandle,
    },
    /// There is nothing to undo or redo for that document.
    NoHistory {
        document: DocumentHandle,
        direction: &'static str,
    },
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(error) => write!(formatter, "{error}"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Edit(message) => formatter.write_str(message),
            Self::Selection(error) => write!(formatter, "{error}"),
            Self::OperationConflict {
                operation_id,
                recorded,
                ..
            } => write!(
                formatter,
                "operation '{operation_id}' was already used with different content (recorded at {recorded}); \
                 use a new operation id or resend the identical request"
            ),
            Self::UnknownOutcome {
                operation_id,
                reason,
            } => write!(
                formatter,
                "the outcome of operation '{operation_id}' is not known here: {reason}; \
                 inspect the document before retrying"
            ),
            Self::PreviewExpired { preview_id } => write!(
                formatter,
                "preview {preview_id} is no longer retained; build a new preview"
            ),
            Self::PreviewConflict {
                preview_id,
                expected,
                current,
            } => write!(
                formatter,
                "preview {preview_id} was built from {expected} but {current} is current; \
                 a stale preview cannot be committed"
            ),
            Self::NoHistory {
                document,
                direction,
            } => write!(
                formatter,
                "there is nothing to {direction} in {document}: the newest revision was not produced \
                 by this transaction service, or its history has been evicted"
            ),
        }
    }
}

impl std::error::Error for TransactionError {}

impl TransactionError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Document(error) => error.code(),
            Self::Invalid(_) => "invalid_request",
            Self::Edit(_) => "edit_failed",
            Self::Selection(error) => error.code(),
            Self::OperationConflict { .. } => "operation_conflict",
            Self::UnknownOutcome { .. } => "unknown_outcome",
            Self::PreviewExpired { .. } => "preview_expired",
            Self::PreviewConflict { .. } => "preview_conflict",
            Self::NoHistory { .. } => "no_history",
        }
    }

    /// True when the failure is a concurrency outcome to reconcile.
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            Self::Document(error) if error.is_conflict()
        ) || matches!(
            self,
            Self::PreviewConflict { .. } | Self::OperationConflict { .. }
        )
    }

    /// Current identity and revision, when the store or a preview knows them.
    pub fn current(&self) -> Option<&DocumentHandle> {
        match self {
            Self::Document(error) => error.current(),
            Self::PreviewConflict { current, .. } => Some(current),
            _ => None,
        }
    }
}

impl From<DocumentError> for TransactionError {
    fn from(error: DocumentError) -> Self {
        Self::Document(error)
    }
}

impl From<SelectionError> for TransactionError {
    fn from(error: SelectionError) -> Self {
        Self::Selection(error)
    }
}

impl From<crate::SplatError> for TransactionError {
    fn from(error: crate::SplatError) -> Self {
        Self::Edit(error.to_string())
    }
}

/// What a batch produced before it was committed.
struct Candidate {
    splat: Splat,
    ids: Vec<PointId>,
    authoring: AuthoringSet,
    reports: Vec<BatchStepReport>,
    warnings: Vec<String>,
}

/// A retained preview.
#[derive(Clone)]
struct Preview {
    id: u64,
    source: DocumentHandle,
    candidate: Arc<Splat>,
    ids: Vec<PointId>,
    authoring: AuthoringSet,
    report: PreviewReport,
}

impl Preview {
    fn bytes(&self) -> usize {
        self.candidate.len() * std::mem::size_of::<crate::SplatPoint>()
            + self.ids.len() * std::mem::size_of::<PointId>()
    }
}

/// A recorded receipt, for retry detection.
#[derive(Clone)]
struct LedgerEntry {
    operation_id: String,
    request_hash: u64,
    receipt: TransactionReceipt,
    at_ms: u64,
}

enum LedgerLookup {
    Miss,
    Replay(Box<TransactionReceipt>),
    Conflict {
        recorded_hash: u64,
        recorded: DocumentHandle,
    },
    Expired,
}

/// Where a side effect is recorded back onto a receipt.
///
/// Addressed by the *revision the receipt produced*, because that is what the renderer and the
/// file system refer to: a caller that shows or exports a revision can report the outcome
/// without knowing (or guessing) an operation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptSlot {
    Export,
    Display,
}

/// Service state behind one mutex.
struct State {
    /// Authoring layer per document, kept at the newest revision this service produced.
    authoring: HashMap<DocumentId, AuthoringSet>,
    /// Documents whose authoring layer was rebuilt because a revision appeared from outside.
    rebuilt: HashSet<DocumentId>,
    ledger: VecDeque<LedgerEntry>,
    /// Undo steps per document, oldest first.
    history: HashMap<DocumentId, VecDeque<HistoryItem>>,
    /// Redo steps per document, oldest first.
    redo: HashMap<DocumentId, VecDeque<HistoryItem>>,
    /// Newest revision this service produced per document, so undo/redo never crosses a
    /// revision that came from somewhere else.
    latest: HashMap<DocumentId, u64>,
    previews: VecDeque<Preview>,
    selections: SelectionHandles,
    next_preview: u64,
    next_history: u64,
}

impl State {
    fn new(limits: TransactionLimits) -> Self {
        Self {
            authoring: HashMap::new(),
            rebuilt: HashSet::new(),
            ledger: VecDeque::new(),
            history: HashMap::new(),
            redo: HashMap::new(),
            latest: HashMap::new(),
            previews: VecDeque::new(),
            selections: SelectionHandles::new(limits.max_selection_handles),
            next_preview: 1,
            next_history: 1,
        }
    }

    fn ledger_lookup(
        &self,
        operation_id: &str,
        hash: u64,
        at_ms: u64,
        ttl_ms: u64,
    ) -> LedgerLookup {
        let Some(entry) = self
            .ledger
            .iter()
            .find(|entry| entry.operation_id == operation_id)
        else {
            return LedgerLookup::Miss;
        };
        if entry.request_hash != hash {
            return LedgerLookup::Conflict {
                recorded_hash: entry.request_hash,
                recorded: entry.receipt.document.clone(),
            };
        }
        if at_ms.saturating_sub(entry.at_ms) > ttl_ms {
            return LedgerLookup::Expired;
        }
        LedgerLookup::Replay(Box::new(entry.receipt.clone()))
    }

    /// Records an acknowledged side effect back onto the receipt it belongs to.
    ///
    /// Returns `false` when no retained receipt describes that revision, which is the honest
    /// answer once the receipt has been evicted: the caller's report is then simply not stored.
    fn note_side_effect(
        &mut self,
        handle: &DocumentHandle,
        slot: ReceiptSlot,
        side: SideEffect,
    ) -> bool {
        let Some(entry) = self
            .ledger
            .iter_mut()
            .rev()
            .find(|entry| &entry.receipt.document == handle)
        else {
            return false;
        };
        match slot {
            ReceiptSlot::Export => entry.receipt.export = side,
            ReceiptSlot::Display => entry.receipt.display = side,
        }
        true
    }

    /// The receipt recorded for a revision, if it is still retained.
    fn receipt_for(&self, handle: &DocumentHandle) -> Option<&TransactionReceipt> {
        self.ledger
            .iter()
            .rev()
            .find(|entry| &entry.receipt.document == handle)
            .map(|entry| &entry.receipt)
    }

    fn record_receipt(&mut self, receipt: &TransactionReceipt, limits: TransactionLimits) {
        if let Some(operation_id) = &receipt.operation_id {
            self.ledger
                .retain(|entry| &entry.operation_id != operation_id);
            self.ledger.push_back(LedgerEntry {
                operation_id: operation_id.clone(),
                request_hash: receipt.request_hash,
                receipt: receipt.clone(),
                at_ms: receipt.at_ms,
            });
            while self.ledger.len() > limits.max_receipts {
                self.ledger.pop_front();
            }
        }
    }

    /// The authoring layer for an exact revision, minting one when it is not known.
    ///
    /// A revision this service did not produce (a file replace, a Python job) cannot inherit
    /// identities: the layer is rebuilt, `rebuilt` is reported, and every previously saved
    /// selection or component id for that document stops resolving. Silently re-attaching old
    /// ids to new rows is exactly the bug this layer exists to prevent.
    fn authoring_for(&mut self, handle: &DocumentHandle, points: usize) -> AuthoringSet {
        let matches = self
            .authoring
            .get(&handle.document_id)
            .is_some_and(|set| set.revision == handle.revision && set.len() == points);
        if matches {
            self.latest
                .insert(handle.document_id.clone(), handle.revision);
            return self
                .authoring
                .get(&handle.document_id)
                .expect("checked above")
                .clone();
        }
        if self.authoring.contains_key(&handle.document_id) {
            self.rebuilt.insert(handle.document_id.clone());
            self.history.remove(&handle.document_id);
            self.redo.remove(&handle.document_id);
            self.selections.forget_document(&handle.document_id);
            self.previews
                .retain(|preview| preview.source.document_id != handle.document_id);
        }
        let fresh = AuthoringSet::new(Some(handle.document_id.clone()), handle.revision, points);
        self.authoring
            .insert(handle.document_id.clone(), fresh.clone());
        self.latest
            .insert(handle.document_id.clone(), handle.revision);
        fresh
    }

    /// Drops selection handles that a new revision invalidated.
    ///
    /// Previews are deliberately kept: committing a stale preview is refused with an explicit
    /// conflict that names both revisions, which is more useful than "unknown preview", and the
    /// preview table is bounded on its own.
    fn invalidate(&mut self, handle: &DocumentHandle) {
        self.selections
            .drop_stale(&handle.document_id, handle.revision);
    }

    fn push_history(&mut self, item: HistoryItem, limits: TransactionLimits) {
        let stack = self.history.entry(item.document.clone()).or_default();
        stack.push_back(item);
        trim_history(stack, limits);
    }

    fn push_redo(&mut self, item: HistoryItem, limits: TransactionLimits) {
        let stack = self.redo.entry(item.document.clone()).or_default();
        stack.push_back(item);
        trim_history(stack, limits);
    }
}

/// Trims a stack to the entry and byte bounds, oldest first.
fn trim_history(stack: &mut VecDeque<HistoryItem>, limits: TransactionLimits) {
    while stack.len() > limits.max_history_entries {
        stack.pop_front();
    }
    while stack.len() > 1
        && stack.iter().map(HistoryItem::bytes).sum::<usize>() > limits.max_history_bytes
    {
        stack.pop_front();
    }
}

/// Atomic, previewable, retry-safe and undoable edits over one document store.
pub struct TransactionService {
    store: Arc<DocumentStore>,
    limits: TransactionLimits,
    state: Mutex<State>,
}

impl TransactionService {
    /// A service over `store`.
    pub fn new(store: Arc<DocumentStore>, limits: TransactionLimits) -> Self {
        Self {
            store,
            limits,
            state: Mutex::new(State::new(limits)),
        }
    }

    /// The store this service commits into.
    pub fn store(&self) -> &Arc<DocumentStore> {
        &self.store
    }

    pub fn limits(&self) -> TransactionLimits {
        self.limits
    }

    /// Locks the service, recovering from a poisoned lock.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs a batch as a dry run and retains the candidate behind a bounded handle.
    pub fn preview(
        &self,
        expected: Expected,
        batch: &EditBatch,
    ) -> Result<PreviewOutcome, TransactionError> {
        batch.validate()?;
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let source = Arc::clone(snapshot.splat());
        let mut state = self.lock();
        let mut working_authoring = state.authoring_for(&handle, source.len());
        let candidate = {
            let State { selections, .. } = &mut *state;
            run_batch(&source, &mut working_authoring, selections, batch)?
        };
        let report = PreviewReport {
            source: handle.clone(),
            steps: candidate.reports.clone(),
            points_before: source.len(),
            points_after: candidate.splat.len(),
            bounds_before: source.bounds(),
            bounds_after: candidate.splat.bounds(),
            warnings: candidate.warnings.clone(),
            memory_estimate_bytes: candidate.splat.len() * std::mem::size_of::<crate::SplatPoint>()
                + candidate.ids.len() * std::mem::size_of::<PointId>(),
            point_ids: candidate.ids.len(),
        };
        let State {
            previews,
            next_preview,
            ..
        } = &mut *state;
        let id = *next_preview;
        *next_preview += 1;
        previews.push_back(Preview {
            id,
            source: handle,
            candidate: Arc::new(candidate.splat),
            ids: candidate.ids,
            authoring: candidate.authoring,
            report: report.clone(),
        });
        while previews.len() > self.limits.max_previews
            || (previews.len() > 1
                && previews.iter().map(Preview::bytes).sum::<usize>()
                    > self.limits.max_preview_bytes)
        {
            previews.pop_front();
        }
        Ok(PreviewOutcome {
            preview_id: id,
            report,
        })
    }

    /// The retained candidate of a preview, for rendering or exporting it.
    ///
    /// The displayed document is not changed by reading this.
    pub fn preview_snapshot(&self, preview_id: u64) -> Result<PreviewSnapshot, TransactionError> {
        let state = self.lock();
        let preview = state
            .previews
            .iter()
            .find(|preview| preview.id == preview_id)
            .ok_or(TransactionError::PreviewExpired { preview_id })?;
        Ok(PreviewSnapshot {
            preview_id,
            report: preview.report.clone(),
            splat: Arc::clone(&preview.candidate),
        })
    }

    /// Applies a batch as one atomic commit.
    pub fn commit(
        &self,
        expected: Expected,
        batch: &EditBatch,
        mutation: Mutation,
    ) -> Result<TransactionReceipt, TransactionError> {
        batch.validate()?;
        let hash = batch.request_hash();
        let at_ms = mutation.at_ms.unwrap_or_else(now_ms);

        // The recorded outcome is resolved *first*, before any source snapshot is required.
        // A retry of an operation whose source revision has since been evicted or replaced is
        // still a retry of work that already happened: refusing it with `snapshot_expired`
        // would report the passage of time as a failure of the request.
        {
            let mut state = self.lock();
            if let Some(operation_id) = &batch.operation_id {
                match state.ledger_lookup(operation_id, hash, at_ms, self.limits.receipt_ttl_ms) {
                    LedgerLookup::Miss => {}
                    LedgerLookup::Replay(receipt) => return Ok(receipt.replayed()),
                    LedgerLookup::Conflict {
                        recorded_hash,
                        recorded,
                    } => {
                        return Err(TransactionError::OperationConflict {
                            operation_id: operation_id.clone(),
                            expected_hash: hash,
                            recorded_hash,
                            recorded,
                        });
                    }
                    LedgerLookup::Expired => {
                        return Err(TransactionError::UnknownOutcome {
                            operation_id: operation_id.clone(),
                            reason: "its receipt is no longer retained".to_owned(),
                        });
                    }
                }
            }
        }

        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let source = Arc::clone(snapshot.splat());
        let mut state = self.lock();
        let before = state.authoring_for(&handle, source.len());
        let candidate = {
            let State { selections, .. } = &mut *state;
            let mut working_authoring = before.clone();
            run_batch(&source, &mut working_authoring, selections, batch)?
        };
        let receipt = self.commit_candidate(
            &mut state,
            &handle,
            source,
            candidate,
            before,
            batch.operation_id.clone(),
            hash,
            mutation,
            None,
            at_ms,
            true,
        )?;
        Ok(receipt)
    }

    /// Records an acknowledged export or display outcome onto the receipt of a revision.
    ///
    /// A commit cannot know whether a picture appeared or a file was written - those happen
    /// after it returns - so the caller that *did* the work reports it here. Until then a
    /// display outcome reads [`SideEffect::Published`], never `done`.
    pub fn note_side_effect(
        &self,
        handle: &DocumentHandle,
        slot: ReceiptSlot,
        side: SideEffect,
    ) -> bool {
        self.lock().note_side_effect(handle, slot, side)
    }

    /// The recorded receipt of a revision, when it is still retained.
    pub fn receipt(&self, handle: &DocumentHandle) -> Option<TransactionReceipt> {
        self.lock().receipt_for(handle).cloned()
    }

    /// Installs an authoring layer for one revision, for a restored sidecar.
    ///
    /// The layer must match the revision's geometry: a mismatch is refused rather than attached,
    /// because ids that do not describe these gaussians are worse than no metadata at all.
    pub fn install_authoring(
        &self,
        handle: &DocumentHandle,
        set: AuthoringSet,
    ) -> Result<usize, TransactionError> {
        let snapshot = self.store.resolve(handle)?;
        let points = snapshot.splat().len();
        if set.len() != points {
            return Err(TransactionError::Invalid(format!(
                "authoring metadata describes {} gaussians but {} holds {points}",
                set.len(),
                handle
            )));
        }
        let mut state = self.lock();
        let mut set = set;
        set.document = Some(handle.document_id.clone());
        set.set_rows(handle.revision, set.ids().to_vec());
        let components = set.components().len();
        // A restored layer replaces whatever was inferred for this document, and the revision it
        // describes is not one this service produced, so history and selections for it go.
        state.history.remove(&handle.document_id);
        state.redo.remove(&handle.document_id);
        state.selections.forget_document(&handle.document_id);
        state
            .latest
            .insert(handle.document_id.clone(), handle.revision);
        state.authoring.insert(handle.document_id.clone(), set);
        state.rebuilt.remove(&handle.document_id);
        Ok(components)
    }

    /// Commits the candidate a preview retained.
    ///
    /// The caller's expectation must still resolve to the exact revision the preview was built
    /// from: a preview that has been overtaken cannot overwrite newer work.
    ///
    /// `operation_id` makes the commit retry-safe. Without one, a second identical commit is
    /// still refused with `preview_expired` - the candidate is consumed by the first commit and
    /// there is nothing that identifies the two requests as the same one - so a caller that
    /// wants retry safety supplies an id, exactly as it does for a batch.
    pub fn commit_preview(
        &self,
        preview_id: u64,
        expected: Expected,
        operation_id: Option<String>,
    ) -> Result<TransactionReceipt, TransactionError> {
        // The request identity is the candidate plus the target the caller named. It is
        // computed from the *request*, never from current state: a retry after the first commit
        // advanced the revision is the same request, and must reach the recorded outcome rather
        // than looking like different content.
        let hash =
            fingerprint(format!("preview-commit:{preview_id}:{}", expected.describe()).as_bytes());
        let at_ms = now_ms();
        {
            let mut state = self.lock();
            if let Some(operation_id) = &operation_id {
                match state.ledger_lookup(operation_id, hash, at_ms, self.limits.receipt_ttl_ms) {
                    LedgerLookup::Miss => {}
                    LedgerLookup::Replay(receipt) => return Ok(receipt.replayed()),
                    LedgerLookup::Conflict {
                        recorded_hash,
                        recorded,
                    } => {
                        return Err(TransactionError::OperationConflict {
                            operation_id: operation_id.clone(),
                            expected_hash: hash,
                            recorded_hash,
                            recorded,
                        });
                    }
                    LedgerLookup::Expired => {
                        return Err(TransactionError::UnknownOutcome {
                            operation_id: operation_id.clone(),
                            reason: "its receipt is no longer retained".to_owned(),
                        });
                    }
                }
            }
        }
        let snapshot = self.store.snapshot(expected)?;
        let current = snapshot.handle().clone();
        let mut state = self.lock();
        let preview = state
            .previews
            .iter()
            .find(|preview| preview.id == preview_id)
            .cloned()
            .ok_or(TransactionError::PreviewExpired { preview_id })?;
        if preview.source != current {
            return Err(TransactionError::PreviewConflict {
                preview_id,
                expected: preview.source.clone(),
                current,
            });
        }
        let before = state.authoring_for(&current, snapshot.splat().len());
        let candidate = Candidate {
            splat: (*preview.candidate).clone(),
            ids: preview.ids.clone(),
            authoring: preview.authoring.clone(),
            reports: preview.report.steps.clone(),
            warnings: preview.report.warnings.clone(),
        };
        let receipt = self.commit_candidate(
            &mut state,
            &current,
            Arc::clone(snapshot.splat()),
            candidate,
            before,
            operation_id,
            hash,
            Mutation::edit("edit_splat"),
            Some(PreviewCommit {
                preview_id,
                source: preview.source.revision,
            }),
            at_ms,
            true,
        )?;
        state.previews.retain(|preview| preview.id != preview_id);
        Ok(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_candidate(
        &self,
        state: &mut State,
        handle: &DocumentHandle,
        source: Arc<Splat>,
        candidate: Candidate,
        before: AuthoringSet,
        operation_id: Option<String>,
        request_hash: u64,
        mutation: Mutation,
        preview: Option<PreviewCommit>,
        at_ms: u64,
        clear_redo: bool,
    ) -> Result<TransactionReceipt, TransactionError> {
        let Candidate {
            splat,
            ids,
            mut authoring,
            reports,
            warnings,
        } = candidate;
        splat.validate().map_err(TransactionError::from)?;
        let label = mutation
            .operation
            .clone()
            .unwrap_or_else(|| mutation.kind.name().to_owned());
        let candidate_arc = Arc::new(splat);
        let stored = (*candidate_arc).clone();
        let metadata = self
            .store
            .commit(Expected::Handle(handle.clone()), stored, mutation)?;
        let produced = metadata.handle.clone();
        // The name the document is saved under, recorded with the receipt so a replay reports
        // the same file without asking the store what is displayed now.
        let file_name = metadata.provenance.file_name.clone();

        // Identity bookkeeping: surviving ids stay, retired ids go, new ids were minted while
        // the batch ran. Membership is cleaned so no component points at a retired row.
        let live: HashSet<PointId> = ids.iter().copied().collect();
        let retired: Vec<PointId> = authoring
            .ids()
            .iter()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        authoring.set_rows(produced.revision, ids.clone());
        if !retired.is_empty() {
            authoring.remove_rows(&retired);
        }
        state
            .authoring
            .insert(produced.document_id.clone(), authoring.clone());
        state
            .latest
            .insert(produced.document_id.clone(), produced.revision);

        let item = HistoryItem {
            id: state.next_history,
            label,
            document: produced.document_id.clone(),
            before: SnapshotState {
                splat: Arc::clone(&source),
                ids: before.ids().to_vec(),
                authoring: before,
            },
            after: SnapshotState {
                splat: Arc::clone(&candidate_arc),
                ids: ids.clone(),
                authoring: authoring.clone(),
            },
            revision: produced.revision,
            at_ms,
        };
        state.next_history += 1;
        state.push_history(item, self.limits);
        if clear_redo {
            // A new edit makes the undone steps unredoable; undo/redo themselves do not.
            state.redo.remove(&produced.document_id);
        }
        state.invalidate(&produced);

        let undo_available = state
            .history
            .get(&produced.document_id)
            .is_some_and(|stack| !stack.is_empty());
        let redo_available = state
            .redo
            .get(&produced.document_id)
            .is_some_and(|stack| !stack.is_empty());
        let receipt = TransactionReceipt {
            operation_id,
            request_hash,
            recorded: ReceiptDocument {
                document_id: produced.document_id.clone(),
                revision: produced.revision,
                point_count: ids.len(),
                file_name,
            },
            document: produced.clone(),
            committed: true,
            steps: reports,
            point_count: ids.len(),
            preview,
            undo_available,
            redo_available,
            export: SideEffect::NotRequested,
            display: SideEffect::NotRequested,
            replayed: false,
            warnings,
            at_ms,
        };
        state.record_receipt(&receipt, self.limits);
        Ok(receipt)
    }

    /// Undoes the newest step of a document as a new revision.
    pub fn undo(&self, expected: Expected) -> Result<TransactionReceipt, TransactionError> {
        self.step_history(expected, "undo")
    }

    /// Redoes the newest undone step as a new revision.
    pub fn redo(&self, expected: Expected) -> Result<TransactionReceipt, TransactionError> {
        self.step_history(expected, "redo")
    }

    fn step_history(
        &self,
        expected: Expected,
        direction: &'static str,
    ) -> Result<TransactionReceipt, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let current = snapshot.handle().clone();
        let mut state = self.lock();
        let current_authoring = state.authoring_for(&current, snapshot.splat().len());
        let current_state = SnapshotState {
            splat: Arc::clone(snapshot.splat()),
            ids: current_authoring.ids().to_vec(),
            authoring: current_authoring,
        };

        // Undo and redo only cross revisions this service produced: a change made outside it
        // (a file replace, a Python job) is not something a stack of edit steps can step over.
        if state.latest.get(&current.document_id) != Some(&current.revision) {
            return Err(TransactionError::NoHistory {
                document: current.clone(),
                direction,
            });
        }
        let document = current.document_id.clone();
        let mut item = {
            let stack = match direction {
                "undo" => state.history.get_mut(&document),
                _ => state.redo.get_mut(&document),
            };
            stack
                .and_then(|stack| stack.pop_back())
                .ok_or_else(|| TransactionError::NoHistory {
                    document: current.clone(),
                    direction,
                })?
        };

        let target = match direction {
            "undo" => item.before.clone(),
            _ => item.after.clone(),
        };
        let candidate = Candidate {
            splat: (*target.splat).clone(),
            ids: target.ids.clone(),
            authoring: target.authoring.clone(),
            reports: Vec::new(),
            warnings: Vec::new(),
        };
        let mut receipt = self.commit_candidate(
            &mut state,
            &current,
            Arc::clone(snapshot.splat()),
            candidate,
            current_state.authoring.clone(),
            None,
            fingerprint(format!("{direction}:{}", current.revision).as_bytes()),
            Mutation::edit(direction),
            None,
            now_ms(),
            false,
        )?;

        // The step moves to the other stack, so undo and redo keep working in cycles. Its
        // states are unchanged: only the revision it produced is refreshed.
        item.revision = receipt.document.revision;
        item.label = direction.to_owned();
        match direction {
            "undo" => state.push_redo(item, self.limits),
            _ => state.push_history(item, self.limits),
        }
        receipt.undo_available = state
            .history
            .get(&receipt.document.document_id)
            .is_some_and(|stack| !stack.is_empty());
        receipt.redo_available = state
            .redo
            .get(&receipt.document.document_id)
            .is_some_and(|stack| !stack.is_empty());
        Ok(receipt)
    }

    /// Undo/redo availability and the retained steps.
    pub fn history(&self, expected: Expected) -> Result<HistoryReport, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let state = self.lock();
        let entries = state
            .history
            .get(&handle.document_id)
            .map(|stack| {
                stack
                    .iter()
                    .rev()
                    .map(|item| HistoryEntry {
                        id: item.id,
                        label: item.label.clone(),
                        revision: item.revision,
                        point_count: item.after.splat.len(),
                        at_ms: item.at_ms,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let redo = state
            .redo
            .get(&handle.document_id)
            .and_then(|stack| stack.back())
            .map(|item| HistoryEntry {
                id: item.id,
                label: item.label.clone(),
                revision: item.revision,
                point_count: item.after.splat.len(),
                at_ms: item.at_ms,
            });
        Ok(HistoryReport {
            document: Some(handle.clone()),
            undo: entries.first().cloned(),
            redo,
            entries,
            retained_bytes: state
                .history
                .get(&handle.document_id)
                .map(|stack| stack.iter().map(HistoryItem::bytes).sum())
                .unwrap_or_default(),
            max_bytes: self.limits.max_history_bytes,
        })
    }

    /// Resolves a query and retains it as a revision-bound selection handle.
    pub fn select(
        &self,
        expected: Expected,
        query: &SelectionQuery,
    ) -> Result<SelectionHandle, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let authoring = state.authoring_for(&handle, snapshot.splat().len());
        let captured = {
            let State { selections, .. } = &mut *state;
            selections.capture(snapshot.splat(), &authoring, query)?
        };
        Ok(captured)
    }

    /// A retained selection handle.
    pub fn selection(&self, id: u64) -> Option<SelectionHandle> {
        self.lock().selections.get(id).cloned()
    }

    /// The gaussians a selection resolved to, bounded, in ascending row order.
    ///
    /// Read through the identities the handle recorded, so this is exactly the set the handle
    /// promised: a viewer highlight and a tool reply therefore describe the same gaussians.
    pub fn selected_points(
        &self,
        id: u64,
        max: usize,
    ) -> Result<Vec<crate::SplatPoint>, TransactionError> {
        // Lock 1: the promise itself. The store is untouched while the lock is held.
        let (handle, ids) = {
            let state = self.lock();
            let selection = state.selections.get(id).ok_or_else(|| {
                TransactionError::Selection(SelectionError::Invalid(format!(
                    "selection handle {id} is no longer retained; select again"
                )))
            })?;
            let document = selection
                .document
                .clone()
                .ok_or_else(|| TransactionError::Document(DocumentError::NoDocument))?;
            (
                DocumentHandle::new(document, selection.revision),
                selection.ids().to_vec(),
            )
        };

        // Outside the lock: resolve the revision and read the points it names.
        let snapshot = self.store.resolve(&handle)?;
        let splat = Arc::clone(snapshot.splat());

        // Lock 2: the identity mapping of that revision, for the rows the ids occupy.
        let rows = {
            let mut state = self.lock();
            let set = state.authoring_for(&handle, splat.len());
            ids.iter()
                .filter_map(|id| set.row_of(*id))
                .take(max)
                .collect::<Vec<usize>>()
        };
        Ok(rows
            .into_iter()
            .filter_map(|row| splat.points.get(row).copied())
            .collect())
    }

    /// The authoring layer of an exact revision: ids, components and membership.
    ///
    /// A reply reports [`ComponentList`]; this is what a versioned sidecar records, because a
    /// restore needs the rows its members occupy, not only their identities.
    pub fn authoring_layer(
        &self,
        expected: Expected,
    ) -> Result<(DocumentHandle, AuthoringSet), TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let set = state.authoring_for(&handle, snapshot.splat().len());
        Ok((handle, set))
    }

    /// The authoring layer of an exact revision, with ids and components.
    pub fn components(&self, expected: Expected) -> Result<ComponentList, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let authoring = state.authoring_for(&handle, snapshot.splat().len());
        // One-shot notice: the layer was rebuilt because a revision arrived from outside.
        let rebuilt = state.rebuilt.remove(&handle.document_id);
        Ok(ComponentList {
            document: handle,
            components: authoring.components().to_vec(),
            rebuilt,
        })
    }

    /// Creates a component and records a component revision.
    pub fn create_component(
        &self,
        expected: Expected,
        name: &str,
    ) -> Result<ComponentChange, TransactionError> {
        if name.trim().is_empty() {
            return Err(TransactionError::Invalid(
                "a component name must not be blank".to_owned(),
            ));
        }
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let mut authoring = state.authoring_for(&handle, snapshot.splat().len());
        let component_id = authoring.mint_component(name);
        let metadata = self.store.set_component(
            Expected::Handle(handle),
            component_id.as_str(),
            "component_create",
        )?;
        authoring.revision = metadata.handle.revision;
        state
            .authoring
            .insert(metadata.handle.document_id.clone(), authoring);
        state.invalidate(&metadata.handle);
        Ok(ComponentChange {
            document: metadata.handle,
            component_id,
        })
    }

    /// Renames a component, keeping its identity.
    pub fn rename_component(
        &self,
        expected: Expected,
        component: &ComponentId,
        name: &str,
    ) -> Result<ComponentChange, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let mut authoring = state.authoring_for(&handle, snapshot.splat().len());
        authoring.rename_component(component, name)?;
        let metadata = self.store.set_component(
            Expected::Handle(handle),
            component.as_str(),
            "component_rename",
        )?;
        authoring.revision = metadata.handle.revision;
        state
            .authoring
            .insert(metadata.handle.document_id.clone(), authoring);
        state.invalidate(&metadata.handle);
        Ok(ComponentChange {
            document: metadata.handle,
            component_id: component.clone(),
        })
    }

    /// Removes a component. Its gaussians and their identities survive.
    pub fn remove_component(
        &self,
        expected: Expected,
        component: &ComponentId,
    ) -> Result<ComponentChange, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let mut authoring = state.authoring_for(&handle, snapshot.splat().len());
        let removed = authoring.remove_component(component).ok_or_else(|| {
            TransactionError::Selection(SelectionError::UnknownComponent(component.to_string()))
        })?;
        let metadata = self.store.set_component(
            Expected::Handle(handle),
            removed.id.as_str(),
            "component_remove",
        )?;
        authoring.revision = metadata.handle.revision;
        state
            .authoring
            .insert(metadata.handle.document_id.clone(), authoring);
        state.invalidate(&metadata.handle);
        Ok(ComponentChange {
            document: metadata.handle,
            component_id: removed.id,
        })
    }

    /// Sets (or clears) a component's explicit local frame.
    pub fn set_component_transform(
        &self,
        expected: Expected,
        component: &ComponentId,
        transform: Option<LocalTransform>,
    ) -> Result<ComponentChange, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let mut authoring = state.authoring_for(&handle, snapshot.splat().len());
        authoring.set_component_transform(component, transform)?;
        let metadata = self.store.set_component(
            Expected::Handle(handle),
            component.as_str(),
            "component_transform",
        )?;
        authoring.revision = metadata.handle.revision;
        state
            .authoring
            .insert(metadata.handle.document_id.clone(), authoring);
        state.invalidate(&metadata.handle);
        Ok(ComponentChange {
            document: metadata.handle,
            component_id: component.clone(),
        })
    }

    /// Transforms a component's members through its own local frame, as one committed edit.
    ///
    /// This is the "targeted transform" path: the frame is declared once on the component and
    /// applied through the same transaction boundary as every other edit, so a rotated
    /// anisotropic gaussian is transformed by its covariance rather than by scaling radii.
    pub fn apply_component_transform(
        &self,
        expected: Expected,
        component: &ComponentId,
    ) -> Result<TransactionReceipt, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let source = Arc::clone(snapshot.splat());
        let mut state = self.lock();
        let before = state.authoring_for(&handle, source.len());
        let target = before
            .component(component)
            .cloned()
            .ok_or_else(|| SelectionError::UnknownComponent(component.to_string()))?;
        let Some(transform) = target.transform else {
            return Err(TransactionError::Invalid(format!(
                "component {component} has no local transform to apply"
            )));
        };
        let transform = transform.validate()?;
        let mut splat = (*source).clone();
        let mut affected = 0;
        for id in &target.point_ids {
            let Some(row) = before.row_of(*id) else {
                continue;
            };
            if row >= splat.len() {
                continue;
            }
            splat.points[row] = transform.apply_point(&splat.points[row])?;
            affected += 1;
        }
        if affected == 0 {
            return Err(TransactionError::Invalid(format!(
                "component {component} has no members to transform"
            )));
        }
        let remaining = splat.len();
        let candidate = Candidate {
            splat,
            ids: before.ids().to_vec(),
            authoring: before.clone(),
            reports: vec![BatchStepReport {
                op_index: 0,
                affected,
                remaining,
            }],
            warnings: Vec::new(),
        };
        let hash =
            fingerprint(format!("component_transform:{component}:{}", handle.revision).as_bytes());
        self.commit_candidate(
            &mut state,
            &handle,
            source,
            candidate,
            before,
            None,
            hash,
            Mutation::edit("component_transform"),
            None,
            now_ms(),
            true,
        )
    }

    /// Binds a component to exactly the gaussians a query resolves to.
    ///
    /// Membership is authoring metadata, so this advances the component revision and moves no
    /// geometry: replacing a component's *geometry* is an edit batch that removes the old
    /// members and merges the new ones.
    pub fn set_component_members(
        &self,
        expected: Expected,
        component: &ComponentId,
        query: &SelectionQuery,
    ) -> Result<ComponentChange, TransactionError> {
        let snapshot = self.store.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let mut state = self.lock();
        let authoring = state.authoring_for(&handle, snapshot.splat().len());
        let rows = query.resolve(snapshot.splat(), &authoring)?;
        let members: Vec<PointId> = rows
            .iter()
            .filter_map(|row| authoring.id_of(*row))
            .collect();
        let mut authoring = authoring;
        authoring.set_membership(component, &members)?;
        let metadata = self.store.set_component(
            Expected::Handle(handle),
            component.as_str(),
            "component_members",
        )?;
        authoring.revision = metadata.handle.revision;
        state
            .authoring
            .insert(metadata.handle.document_id.clone(), authoring);
        state.invalidate(&metadata.handle);
        Ok(ComponentChange {
            document: metadata.handle,
            component_id: component.clone(),
        })
    }
}

/// Runs every step of a batch against a detached candidate.
fn run_batch(
    source: &Splat,
    authoring: &mut AuthoringSet,
    selections: &SelectionHandles,
    batch: &EditBatch,
) -> Result<Candidate, TransactionError> {
    let source_ids: Vec<PointId> = authoring.ids().to_vec();
    let mut working = source.clone();
    let mut ids: Vec<PointId> = source_ids.clone();
    let mut reports = Vec::with_capacity(batch.steps.len());
    let mut warnings: Vec<String> = Vec::new();

    // Every operation and every selection is checked before anything is applied, so a batch
    // that cannot run is refused as a whole and never leaves a half-applied candidate behind.
    for step in &batch.steps {
        edit::validate_op(&step.op)?;
        edit::validate_selection(&step.targets.selection)?;
    }

    // Stable resolution: fix every step's targets against the source snapshot, as point ids.
    let mut plans: Vec<Option<Vec<PointId>>> = Vec::with_capacity(batch.steps.len());
    for step in &batch.steps {
        if batch.resolution == TargetResolution::Stable && !matches!(step.op, EditOp::Merge { .. })
        {
            let rows = resolve_rows(&step.targets, source, authoring, selections)?;
            let planned: Vec<PointId> = rows
                .iter()
                .filter_map(|row| source_ids.get(*row).copied())
                .collect();
            plans.push(Some(planned));
        } else {
            plans.push(None);
        }
    }

    for (index, step) in batch.steps.iter().enumerate() {
        match &step.op {
            EditOp::Merge { points } => {
                if points.is_empty() {
                    return Err(TransactionError::Edit(format!(
                        "step {index}: merge needs at least one point"
                    )));
                }
                let minted = authoring.mint_points(points.len());
                working.points.extend(points.iter().copied());
                ids.extend(minted);
                reports.push(BatchStepReport {
                    op_index: index,
                    affected: points.len(),
                    remaining: working.len(),
                });
            }
            op => {
                let rows: Vec<usize> = match &plans[index] {
                    Some(planned) => {
                        let index_of: HashMap<PointId, usize> =
                            ids.iter().enumerate().map(|(row, id)| (*id, row)).collect();
                        planned
                            .iter()
                            .filter_map(|id| index_of.get(id).copied())
                            .collect()
                    }
                    None => {
                        // Sequential mode: the targets are evaluated against the candidate as
                        // it stands now, with the current identity mapping.
                        let mut view = authoring.clone();
                        view.set_rows(authoring.revision, ids.clone());
                        resolve_rows(&step.targets, &working, &view, selections)?
                    }
                };
                if rows.is_empty() {
                    return Err(TransactionError::Edit(format!(
                        "step {index}: no gaussian matched the selection; nothing was changed"
                    )));
                }
                edit::apply_to_indices(&mut working, &rows, op)?;
                match op {
                    EditOp::Remove => {
                        for row in rows.iter().rev() {
                            ids.remove(*row);
                        }
                    }
                    EditOp::Duplicate { .. } => {
                        let minted = authoring.mint_points(rows.len());
                        ids.extend(minted);
                    }
                    _ => {}
                }
                reports.push(BatchStepReport {
                    op_index: index,
                    affected: rows.len(),
                    remaining: working.len(),
                });
            }
        }
    }

    if working.is_empty() {
        warnings.push(
            "the batch would leave no gaussians, which the contract refuses: keep at least one"
                .to_owned(),
        );
    }
    working.validate().map_err(TransactionError::from)?;

    Ok(Candidate {
        splat: working,
        ids,
        authoring: authoring.clone(),
        reports,
        warnings,
    })
}

/// Resolves a step's targets against one splat and identity mapping.
fn resolve_rows(
    targets: &BatchTargets,
    splat: &Splat,
    authoring: &AuthoringSet,
    selections: &SelectionHandles,
) -> Result<Vec<usize>, TransactionError> {
    let mut query = targets.to_query();
    if let Some(handle_id) = targets.selection_handle {
        let handle = selections.get(handle_id).ok_or_else(|| {
            TransactionError::Selection(SelectionError::Invalid(format!(
                "selection handle {handle_id} is no longer retained; select again"
            )))
        })?;
        if handle.document.is_some() && handle.document != authoring.document {
            return Err(TransactionError::Selection(SelectionError::Invalid(
                format!("selection handle {handle_id} belongs to a different document"),
            )));
        }
        if handle.revision != authoring.revision {
            return Err(TransactionError::Selection(SelectionError::Invalid(
                format!(
                    "selection handle {handle_id} was resolved at revision {} but the document is at {}",
                    handle.revision, authoring.revision
                ),
            )));
        }
        query.point_ids.extend(handle.ids().iter().copied());
    }
    query
        .resolve(splat, authoring)
        .map_err(TransactionError::from)
}

#[cfg(test)]
mod tests;
