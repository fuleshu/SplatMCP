//! The document service the desktop app owns.
//!
//! This module is a thin adapter: identity, revisions, compare-and-swap, snapshots and
//! retention live in `splatmcp_core::document`, so the same behaviour is unit tested without
//! Tauri, and the app adds only what is genuinely app-side - reading and writing files,
//! parsing and serialising PLY, and the flat reply shape the frontend and the tool surface
//! consume.
//!
//! Two rules are enforced here rather than trusted to callers:
//!
//! - **Parsing and serialising happen outside the store's lock.** A snapshot hands back an
//!   `Arc<Splat>`, so a 500 000 gaussian document is read, written or inspected while the
//!   store is free to serve the next request.
//! - **A mutation states what it expects.** An anonymous change resolves to what is displayed
//!   at request receipt and reports the identity it resolved to; a change that names a
//!   document must name its revision too.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use splatmcp_core::components::AuthoringSet;
use splatmcp_core::document::{DocumentStore, now_ms};
use splatmcp_core::{
    ComponentId, ComponentList, EditBatch, HistoryReport, LocalTransform, PlyImportPolicy,
    PlyReport, PreviewReport, ReceiptSlot, SelectionQuery, SideEffect, Splat, SplatError,
    TransactionError, TransactionLimits, TransactionReceipt, TransactionService,
    read_ply_with_policy, write_ply,
};

pub use splatmcp_core::document::{
    ArtifactChecksum, DocumentError, DocumentHandle, DocumentId, DocumentMetadata, Expected,
    Mutation, MutationKind, RetentionStats, Snapshot,
};
pub use splatmcp_core::{Bounds, Component, PointId, SelectionHandle};

/// What writing an authoring sidecar produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidecarOutcome {
    /// Path the record was written to, or would have been.
    pub path: String,
    pub components: usize,
    pub members: usize,
    /// Gaussians the record describes, which is the exported revision's count.
    pub point_count: usize,
    /// Why the record could not be written, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SidecarOutcome {
    /// True when the record reached the disk.
    pub fn written(&self) -> bool {
        self.error.is_none()
    }
}

/// Colour of a selection marker: unmistakably not scene geometry.
pub const HIGHLIGHT_COLOR: [f32; 3] = [1.0, 0.05, 0.6];
/// Opacity of a selection marker: fully opaque, so it is visible inside a dense cloud.
pub const HIGHLIGHT_OPACITY: f32 = 1.0;

/// Everything the document service can fail with.
///
/// The distinction matters to a caller: a [`DocumentError`] is about *identity* - a stale
/// revision, an expired snapshot, a document that is not displayed - and carries the code
/// and the current handle a caller needs to reconcile. An invalid request is about the data
/// or the environment and carries only a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    Document(DocumentError),
    Invalid(String),
}

impl ServiceError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Document(error) => error.code(),
            Self::Invalid(_) => "invalid_request",
        }
    }

    /// True when the failure is a concurrency outcome to reconcile.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Document(error) if error.is_conflict())
    }

    /// Current identity and revision, when the store knows them.
    pub fn current(&self) -> Option<&DocumentHandle> {
        match self {
            Self::Document(error) => error.current(),
            Self::Invalid(_) => None,
        }
    }

    /// The document error behind this failure, when there is one.
    pub fn document_error(&self) -> Option<&DocumentError> {
        match self {
            Self::Document(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(error) => write!(formatter, "{error}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<DocumentError> for ServiceError {
    fn from(error: DocumentError) -> Self {
        Self::Document(error)
    }
}

impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl From<SplatError> for ServiceError {
    fn from(error: SplatError) -> Self {
        Self::Invalid(error.to_string())
    }
}

impl From<ServiceError> for String {
    fn from(error: ServiceError) -> Self {
        error.to_string()
    }
}

impl From<TransactionError> for ServiceError {
    /// Keeps an *identity* failure an identity failure.
    ///
    /// A transaction can fail for reasons the service also has a code for - a stale revision, an
    /// evicted snapshot, an unknown document - and collapsing those into "invalid request" would
    /// lose the current handle a caller needs to reconcile.
    fn from(error: TransactionError) -> Self {
        match error {
            TransactionError::Document(document) => Self::Document(document),
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// What reading PLY bytes produced: the geometry plus what the import reported.
pub struct Imported {
    pub splat: Splat,
    pub report: PlyReport,
}

/// What an import committed: the new revision plus the report of the file it came from.
pub struct ImportedDocument {
    pub metadata: DocumentMetadata,
    pub report: PlyReport,
}

/// Parses PLY bytes under `policy` and validates the result, so an unusable document never
/// enters state.
///
/// The policy is the caller's explicit decision: [`PlyImportPolicy::Strict`] refuses a file
/// that would need repair, with indexed diagnostics, and [`PlyImportPolicy::Repair`]
/// accepts it and reports what was changed. Either way the report travels back with the
/// geometry, so an import never silently changes a caller's data.
pub fn parse_ply(bytes: &[u8], policy: PlyImportPolicy) -> Result<Imported, String> {
    let (splat, report) = read_ply_with_policy(bytes, policy).map_err(|error| error.to_string())?;
    splat.validate().map_err(|error| error.to_string())?;
    Ok(Imported { splat, report })
}

/// Canonical PLY bytes of a splat.
///
/// Takes the snapshot's content rather than the store, so the store's lock is not held while a
/// large document is serialised.
pub fn ply_bytes(splat: &Splat) -> Result<Vec<u8>, String> {
    write_ply(splat).map_err(|error| error.to_string())
}

/// Flat description of a document revision, for the frontend and the panel.
///
/// The field names the window already reads (`file_name`, `point_count`) stay where they were;
/// the identity fields are additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SplatInfo {
    pub document_id: String,
    pub revision: u64,
    pub point_count: usize,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_operation: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// True when the revision carries a producer record, which a save writes beside the file.
    pub has_recipe: bool,
    /// What happened to authoring metadata that sits beside this document's file, reported on
    /// the reply that opened it: a restore, or the reason nothing was attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authoring: Option<splatmcp_bridge::AuthoringNote>,
}

impl SplatInfo {
    /// Projects bounded metadata onto the flat reply shape.
    pub fn of(metadata: &DocumentMetadata) -> Self {
        Self::of_with_note(metadata, None)
    }

    /// Same, with the note about this document's authoring metadata.
    pub fn of_with_note(
        metadata: &DocumentMetadata,
        authoring: Option<splatmcp_bridge::AuthoringNote>,
    ) -> Self {
        Self {
            authoring,
            document_id: metadata.handle.document_id.to_string(),
            revision: metadata.handle.revision,
            point_count: metadata.point_count,
            file_name: metadata.provenance.file_name.clone(),
            source_path: metadata.provenance.source_path.clone(),
            component_id: metadata.provenance.component_id.clone(),
            last_operation: metadata.provenance.last_operation.clone(),
            created_at_ms: metadata.provenance.created_at_ms,
            updated_at_ms: metadata.provenance.updated_at_ms,
            has_recipe: metadata.provenance.has_recipe(),
        }
    }
}

/// What an export produced: the file, and the exact identity that was written to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportOutcome {
    pub path: String,
    pub document_id: String,
    pub revision: u64,
    /// `algorithm:hex` of the written bytes, so a caller can tell one encoding from another.
    pub checksum: String,
    /// Size of the written file, in bytes.
    pub bytes: usize,
    /// Gaussians the exported revision holds.
    ///
    /// Reported separately from [`ExportOutcome::bytes`] on purpose: a sidecar describes geometry,
    /// and confusing a file size for a gaussian count silently makes every saved record
    /// unloadable.
    pub point_count: usize,
}

/// Application state: the one authoritative document store this process owns, plus the one
/// transaction service every caller shares.
///
/// The service is deliberately singular: the native window, the Python panel and every stdio
/// MCP connection talk to the same ledger, the same undo history and the same previews, so a
/// retry from a second connection cannot apply the same edit twice and an undo in the UI is
/// visible to MCP.
pub struct AppState {
    store: Arc<DocumentStore>,
    transactions: TransactionService,
    /// A note about authoring metadata that the *next* reply describing this revision must
    /// carry: what was restored from a sidecar, or why one was refused. Held once, so a warning
    /// reaches a caller instead of only a console nobody is watching.
    authoring_note: Mutex<Option<(DocumentHandle, splatmcp_bridge::AuthoringNote)>>,
}

impl Default for AppState {
    fn default() -> Self {
        let store = Arc::new(DocumentStore::default());
        let transactions =
            TransactionService::new(Arc::clone(&store), TransactionLimits::default());
        Self {
            store,
            transactions,
            authoring_note: Mutex::new(None),
        }
    }
}

impl AppState {
    /// Makes imported bytes a **new document**, at revision 1.
    ///
    /// Opening a file and importing a buffer both land here: two opens of the same path are
    /// two documents, because a path is provenance and not identity.
    pub fn open_ply(
        &self,
        bytes: &[u8],
        mutation: Mutation,
        policy: PlyImportPolicy,
    ) -> Result<ImportedDocument, ServiceError> {
        let imported = parse_ply(bytes, policy)?;
        Ok(ImportedDocument {
            metadata: self.store.open(imported.splat, mutation),
            report: imported.report,
        })
    }

    /// Makes already parsed geometry a **new document**, at revision 1.
    ///
    /// This is the shape a generation job uses when it creates a document: the geometry
    /// never travelled as a file, so there is nothing to parse and nothing to compare
    /// against.
    pub fn open_splat(
        &self,
        splat: Splat,
        mutation: Mutation,
    ) -> Result<DocumentMetadata, ServiceError> {
        splat.validate()?;
        Ok(self.store.open(splat, mutation))
    }

    /// Applies a content change under a compare-and-swap check.
    pub fn commit(
        &self,
        expected: Expected,
        splat: Splat,
        mutation: Mutation,
    ) -> Result<DocumentMetadata, ServiceError> {
        splat.validate()?;
        Ok(self.store.commit(expected, splat, mutation)?)
    }

    /// Parses and commits imported bytes as a new revision of the expected document.
    pub fn replace_ply(
        &self,
        expected: Expected,
        bytes: &[u8],
        mutation: Mutation,
        policy: PlyImportPolicy,
    ) -> Result<ImportedDocument, ServiceError> {
        let imported = parse_ply(bytes, policy)?;
        Ok(ImportedDocument {
            metadata: self.store.commit(expected, imported.splat, mutation)?,
            report: imported.report,
        })
    }

    /// Changes the named component of the displayed document.
    pub fn set_component(
        &self,
        expected: Expected,
        component_id: &str,
        operation: &str,
    ) -> Result<DocumentMetadata, ServiceError> {
        if component_id.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "component_id must not be blank".to_owned(),
            ));
        }
        Ok(self
            .store
            .set_component(expected, component_id, operation)?)
    }

    /// Reads a document's source file again, as a new revision of the same document.
    ///
    /// `expected` selects the document and the revision to re-read; the file is read and parsed
    /// **before** the commit, so a slow disk does not hold the store, and the commit is made
    /// against the exact revision that was read - a change that landed meanwhile conflicts
    /// instead of being lost.
    pub fn reload(
        &self,
        expected: Expected,
        policy: PlyImportPolicy,
    ) -> Result<ImportedDocument, ServiceError> {
        let snapshot = self.snapshot(expected)?;
        let handle = snapshot.handle().clone();
        let Some(source) = snapshot.provenance().source_path.clone() else {
            return Err(ServiceError::Invalid(
                "this document has no source file to reload; open a .ply or import one first"
                    .to_owned(),
            ));
        };
        let bytes =
            std::fs::read(&source).map_err(|error| format!("could not read {source}: {error}"))?;
        drop(snapshot);
        let imported = parse_ply(&bytes, policy)?;
        Ok(ImportedDocument {
            metadata: self.store.commit(
                Expected::Handle(handle),
                imported.splat,
                Mutation::reload("reload").source(source),
            )?,
            report: imported.report,
        })
    }

    /// The displayed revision, as an immutable snapshot.
    pub fn active(&self) -> Result<Snapshot, ServiceError> {
        Ok(self.store.snapshot(Expected::Any)?)
    }

    /// An exact revision, resolved by handle.
    pub fn snapshot(&self, expected: Expected) -> Result<Snapshot, ServiceError> {
        Ok(self.store.snapshot(expected)?)
    }

    /// Canonical PLY bytes of the displayed revision.
    ///
    /// The bytes are produced from the snapshot, after the store lock has been released.
    pub fn active_ply_bytes(&self) -> Result<(Snapshot, Vec<u8>), ServiceError> {
        let snapshot = self.active()?;
        let bytes = ply_bytes(snapshot.splat())?;
        Ok((snapshot, bytes))
    }

    /// Canonical PLY bytes of an exact revision.
    pub fn ply_bytes_for(
        &self,
        handle: &DocumentHandle,
    ) -> Result<(Snapshot, Vec<u8>), ServiceError> {
        let snapshot = self.store.resolve(handle)?;
        let bytes = ply_bytes(snapshot.splat())?;
        Ok((snapshot, bytes))
    }

    /// Writes the expected revision to `path` and records the export.
    ///
    /// The revision is pinned for the duration, so retention cannot drop it between taking the
    /// snapshot and recording what was written. The export does not advance the revision: the
    /// geometry and the component metadata are the ones that were already there, and only the
    /// artifact checksum is new.
    ///
    /// The pin is held by a guard, so every failure path below - a serializer error, a
    /// directory where a file should be, a refused export record - releases it before
    /// returning. A leaked pin would keep a whole scene alive past the retention budget.
    pub fn export(&self, expected: Expected, path: &Path) -> Result<ExportOutcome, ServiceError> {
        let snapshot = self.snapshot(expected)?;
        let _pin = self.store.pin_guarded(snapshot.handle())?;
        let bytes = ply_bytes(snapshot.splat())?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        std::fs::write(path, &bytes)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        let checksum = ArtifactChecksum::of(&bytes);
        let metadata = self.store.record_export(
            snapshot.handle(),
            path.to_string_lossy().to_string(),
            checksum,
            now_ms(),
        )?;
        Ok(ExportOutcome {
            path: path.to_string_lossy().to_string(),
            document_id: metadata.handle.document_id.to_string(),
            revision: metadata.handle.revision,
            checksum: format!("{}:{}", checksum.algorithm, checksum.hex()),
            bytes: checksum.bytes,
            point_count: snapshot.len(),
        })
    }

    /// Bounded metadata of the displayed document.
    pub fn metadata(&self) -> Option<DocumentMetadata> {
        self.store.active_metadata()
    }

    /// What retention currently holds.
    pub fn retention(&self) -> RetentionStats {
        self.store.stats()
    }

    /// Producer record of the displayed revision, when it has one.
    ///
    /// Stored verbatim as JSON, so a save can write it beside the file without the store
    /// knowing anything about recipes.
    pub fn recipe(&self) -> Option<String> {
        self.store
            .active_metadata()
            .and_then(|metadata| metadata.provenance.recipe)
    }

    /// Identity of the displayed document, as a handle.
    pub fn active_handle(&self) -> Option<DocumentHandle> {
        self.store.active_handle()
    }
}

/// What one step of a batch did, as a reply reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StepOutcome {
    pub op_index: usize,
    pub affected: usize,
    pub remaining: usize,
}

/// Axis-aligned bounds of a candidate, as a reply reports them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BoundsInfo {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    pub radius: f32,
}

impl From<Bounds> for BoundsInfo {
    fn from(bounds: Bounds) -> Self {
        Self {
            min: bounds.min,
            max: bounds.max,
            center: bounds.center,
            radius: bounds.radius,
        }
    }
}

/// Outcome of an optional side effect, kept separate from the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutcomeInfo {
    /// `not_requested`, `done` or `failed`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl OutcomeInfo {
    /// Projects a core side-effect outcome.
    ///
    /// `published` means the revision was announced to the window and is waiting for its
    /// acknowledgement: it is reported in those words, because an announcement is not a render.
    pub fn of(side: &SideEffect) -> Self {
        match side {
            SideEffect::NotRequested => Self {
                status: "not_requested",
                message: None,
            },
            SideEffect::Published => Self {
                status: "published",
                message: None,
            },
            SideEffect::Done => Self {
                status: "done",
                message: None,
            },
            SideEffect::Failed(message) => Self {
                status: "failed",
                message: Some(message.clone()),
            },
        }
    }
}

/// Dry-run result of an edit batch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreviewInfo {
    pub preview_id: u64,
    pub source_revision: u64,
    pub points_before: usize,
    pub points_after: usize,
    pub steps: Vec<StepOutcome>,
    pub warnings: Vec<String>,
    pub memory_estimate_bytes: usize,
    pub point_ids: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds_before: Option<BoundsInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds_after: Option<BoundsInfo>,
}

impl PreviewInfo {
    fn of(preview_id: u64, report: &PreviewReport) -> Self {
        Self {
            preview_id,
            source_revision: report.source.revision,
            points_before: report.points_before,
            points_after: report.points_after,
            steps: report
                .steps
                .iter()
                .map(|step| StepOutcome {
                    op_index: step.op_index,
                    affected: step.affected,
                    remaining: step.remaining,
                })
                .collect(),
            warnings: report.warnings.clone(),
            memory_estimate_bytes: report.memory_estimate_bytes,
            point_ids: report.point_ids,
            bounds_before: report.bounds_before.map(BoundsInfo::from),
            bounds_after: report.bounds_after.map(BoundsInfo::from),
        }
    }
}

/// What a committed batch produced, with side effects reported separately.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchOutcome {
    pub document: SplatInfo,
    /// Always true in a reply that carries a receipt; retries report `replayed`.
    pub committed: bool,
    pub replayed: bool,
    pub point_count: usize,
    pub steps: Vec<StepOutcome>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_id: Option<u64>,
    pub undo_available: bool,
    pub redo_available: bool,
    pub export: OutcomeInfo,
    pub display: OutcomeInfo,
}

impl BatchOutcome {
    /// The recorded document, for publication: a replay announces the revision it produced.
    pub fn recorded(&self) -> splatmcp_core::ReceiptDocument {
        splatmcp_core::ReceiptDocument {
            document_id: DocumentId::parse(&self.document.document_id)
                .unwrap_or_else(|| DocumentId::mint(0, 0)),
            revision: self.document.revision,
            point_count: self.document.point_count,
            file_name: self.document.file_name.clone(),
        }
    }

    /// The recorded document as a wire summary, so a replay reports what it produced.
    pub fn summary(&self) -> splatmcp_bridge::DocumentSummary {
        splatmcp_bridge::DocumentSummary::recorded(
            &self.document.document_id,
            self.document.revision,
            self.document.point_count,
            &self.document.file_name,
        )
    }

    /// The exact revision this outcome records.
    ///
    /// `None` only when the recorded identity is not a document id at all, which cannot happen
    /// for a receipt the service produced; a caller then reports the outcome without acting on
    /// it rather than inventing an identity.
    pub fn handle(&self) -> Option<DocumentHandle> {
        let document_id = DocumentId::parse(&self.document.document_id)?;
        Some(DocumentHandle::new(document_id, self.document.revision))
    }

    /// Builds the reply shape from the receipt itself.
    ///
    /// Deliberately *not* from the store: a replayed receipt reports the document, revision and
    /// point count its request produced, even when the displayed document has moved on or that
    /// revision is no longer retained.
    fn of(receipt: &TransactionReceipt) -> Self {
        Self {
            document: SplatInfo {
                document_id: receipt.recorded.document_id.to_string(),
                revision: receipt.recorded.revision,
                point_count: receipt.recorded.point_count,
                file_name: receipt.recorded.file_name.clone(),
                source_path: None,
                component_id: None,
                last_operation: receipt.operation_id.clone(),
                created_at_ms: receipt.at_ms,
                updated_at_ms: receipt.at_ms,
                has_recipe: false,
                authoring: None,
            },
            committed: receipt.committed,
            replayed: receipt.replayed,
            point_count: receipt.point_count,
            steps: receipt
                .steps
                .iter()
                .map(|step| StepOutcome {
                    op_index: step.op_index,
                    affected: step.affected,
                    remaining: step.remaining,
                })
                .collect(),
            warnings: receipt.warnings.clone(),
            preview_id: receipt.preview.map(|preview| preview.preview_id),
            undo_available: receipt.undo_available,
            redo_available: receipt.redo_available,
            export: OutcomeInfo::of(&receipt.export),
            display: OutcomeInfo::of(&receipt.display),
        }
    }
}

/// One undoable step, bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryStepInfo {
    pub id: u64,
    pub label: String,
    pub revision: u64,
    pub point_count: usize,
    pub at_ms: u64,
}

/// Undo/redo availability and the retained steps of one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryInfo {
    pub document: SplatInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo: Option<HistoryStepInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redo: Option<HistoryStepInfo>,
    pub entries: Vec<HistoryStepInfo>,
    pub retained_bytes: usize,
    pub max_bytes: usize,
}

/// Explicit local frame of a component, as a reply reports it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TransformInfo {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl From<LocalTransform> for TransformInfo {
    fn from(transform: LocalTransform) -> Self {
        Self {
            translation: transform.translation,
            rotation: transform.rotation,
            scale: transform.scale,
        }
    }
}

/// One component, as a reply reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentInfo {
    pub component_id: String,
    pub name: String,
    pub point_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

impl ComponentInfo {
    fn of(component: &Component) -> Self {
        Self {
            component_id: component.id.to_string(),
            name: component.name.clone(),
            point_count: component.len(),
            transform: component.transform.map(TransformInfo::from),
            metadata: component.metadata.clone(),
        }
    }
}

/// Components of one document revision.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentListInfo {
    pub document: SplatInfo,
    pub components: Vec<ComponentInfo>,
    /// True when the authoring layer was rebuilt for this revision, so every id is new and no
    /// component membership survived. Reported once, so a caller learns why its ids changed.
    pub rebuilt: bool,
}

/// A component metadata change and the components that resulted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentChangeInfo {
    pub document: SplatInfo,
    pub component_id: String,
    pub components: Vec<ComponentInfo>,
    pub rebuilt: bool,
}

/// Bounded positions of a resolved selection, for a viewer highlight.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelectionMarkers {
    pub handle_id: u64,
    /// Revision the selection was resolved against.
    pub revision: u64,
    /// Gaussians the selection covers.
    pub count: usize,
    /// Markers actually returned, which is `count` unless the bound cut it short.
    pub shown: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsInfo>,
    /// World-space positions of the selected gaussians, in document space.
    pub positions: Vec<[f32; 3]>,
    /// Gaussians in the document the selection belongs to, so a caller can see a mismatch.
    pub document_points: usize,
}

impl SelectionMarkers {
    /// True when only part of the selection is described.
    pub fn truncated(&self) -> bool {
        self.shown < self.count
    }
}

/// A resolved, revision-bound selection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelectionInfo {
    pub handle_id: u64,
    pub document: SplatInfo,
    pub revision: u64,
    pub count: usize,
    /// Bounded sample of the resolved identities, oldest row first.
    pub sample: Vec<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsInfo>,
}

impl SelectionInfo {
    fn of(handle: &SelectionHandle, metadata: &DocumentMetadata) -> Self {
        Self {
            handle_id: handle.id,
            document: SplatInfo::of(metadata),
            revision: handle.revision,
            count: handle.count,
            sample: handle.sample.iter().map(PointId::to_string).collect(),
            truncated: handle.truncated,
            bounds: handle.bounds.map(BoundsInfo::from),
        }
    }
}

impl AppState {
    /// Runs an edit batch as a dry run, retaining the candidate behind a bounded handle.
    ///
    /// Previewing never touches the displayed document; the caller may commit the preview with
    /// [`AppState::commit_preview`] while it is still the current revision.
    pub fn preview_batch(
        &self,
        expected: Expected,
        batch: &EditBatch,
    ) -> Result<PreviewInfo, TransactionError> {
        let outcome = self.transactions.preview(expected, batch)?;
        Ok(PreviewInfo::of(outcome.preview_id, &outcome.report))
    }

    /// Applies an edit batch atomically and returns the receipt.
    ///
    /// The reply is built from the receipt, never from "what is displayed now": a retry reports
    /// the outcome of the request it repeats.
    pub fn commit_batch(
        &self,
        expected: Expected,
        batch: &EditBatch,
        operation: &str,
    ) -> Result<BatchOutcome, TransactionError> {
        let receipt = self.transactions.commit(
            expected,
            batch,
            Mutation::edit(operation).file_name(self.active_file_name()),
        )?;
        Ok(BatchOutcome::of(&receipt))
    }

    /// Commits the candidate a preview retained, if it is still the current revision.
    pub fn commit_preview(
        &self,
        preview_id: u64,
        expected: Expected,
        operation_id: Option<String>,
    ) -> Result<BatchOutcome, TransactionError> {
        let receipt = self
            .transactions
            .commit_preview(preview_id, expected, operation_id)?;
        Ok(BatchOutcome::of(&receipt))
    }

    /// Records an acknowledged export or display outcome onto a revision's receipt.
    pub fn note_side_effect(
        &self,
        handle: &DocumentHandle,
        slot: ReceiptSlot,
        side: SideEffect,
    ) -> bool {
        self.transactions.note_side_effect(handle, slot, side)
    }

    /// Exports the displayed revision **and** writes its authoring sidecar, from one place.
    ///
    /// This is what the native Save action calls, so the two files are always produced from the
    /// same snapshot: the record's gaussian count, checksum and membership all come from the
    /// revision that was actually written. Taking those as arguments - as an earlier version did -
    /// is how a file *size* ended up recorded where a point count belonged.
    ///
    /// A sidecar is written only when the revision has components; a plain export keeps its
    /// documented "geometry only" guarantee. A failure to write the sidecar is reported but does
    /// not undo the export.
    pub fn export_with_authoring(
        &self,
        path: &Path,
    ) -> Result<(ExportOutcome, Option<SidecarOutcome>), ServiceError> {
        let outcome = self.export(Expected::Any, path)?;
        let handle =
            handle_of(&outcome.document_id, outcome.revision).map_err(ServiceError::Invalid)?;
        let (_, layer) = self
            .transactions
            .authoring_layer(Expected::Handle(handle))
            .map_err(ServiceError::from)?;
        if layer.components().is_empty() {
            return Ok((outcome, None));
        }
        let record = crate::authoring::record(
            &outcome.document_id,
            outcome.revision,
            &outcome.checksum,
            // The exported revision's own gaussian count, never the file size.
            outcome.point_count,
            &layer,
        );
        let sidecar = match crate::authoring::write(path, &record) {
            Ok(path) => SidecarOutcome {
                path: path.to_string_lossy().to_string(),
                components: record.components.len(),
                members: record
                    .components
                    .iter()
                    .map(|component| component.point_rows.len())
                    .sum(),
                point_count: record.point_count,
                error: None,
            },
            Err(error) => SidecarOutcome {
                path: crate::authoring::sidecar_path(path)
                    .to_string_lossy()
                    .to_string(),
                components: record.components.len(),
                members: record
                    .components
                    .iter()
                    .map(|component| component.point_rows.len())
                    .sum(),
                point_count: record.point_count,
                error: Some(error),
            },
        };
        Ok((outcome, Some(sidecar)))
    }

    /// Writes a revision to `path` and records the export against its receipt.
    ///
    /// The revision is exported by handle, so a replay of the original request re-exports that
    /// request's revision rather than whatever is displayed.
    pub fn export_revision(
        &self,
        handle: &DocumentHandle,
        path: &Path,
    ) -> Result<SideEffect, ServiceError> {
        match self.export(Expected::Handle(handle.clone()), path) {
            Ok(_) => {
                self.note_side_effect(handle, ReceiptSlot::Export, SideEffect::Done);
                Ok(SideEffect::Done)
            }
            Err(error) => {
                let message = error.to_string();
                self.note_side_effect(
                    handle,
                    ReceiptSlot::Export,
                    SideEffect::Failed(message.clone()),
                );
                Ok(SideEffect::Failed(message))
            }
        }
    }

    /// PLY bytes of a preview candidate, so it can be rendered or exported without replacing
    /// the displayed document.
    pub fn preview_ply_bytes(&self, preview_id: u64) -> Result<Vec<u8>, String> {
        let candidate = self
            .transactions
            .preview_snapshot(preview_id)
            .map_err(|error| error.to_string())?;
        ply_bytes(&candidate.splat)
    }

    /// Undoes the newest step of the displayed document as a new revision.
    pub fn undo(&self, expected: Expected) -> Result<BatchOutcome, TransactionError> {
        let receipt = self.transactions.undo(expected)?;
        Ok(BatchOutcome::of(&receipt))
    }

    /// Redoes the newest undone step as a new revision.
    pub fn redo(&self, expected: Expected) -> Result<BatchOutcome, TransactionError> {
        let receipt = self.transactions.redo(expected)?;
        Ok(BatchOutcome::of(&receipt))
    }

    /// Undo/redo availability and the retained steps.
    pub fn edit_history(&self, expected: Expected) -> Result<HistoryInfo, TransactionError> {
        let report = self.transactions.history(expected)?;
        let handle = report
            .document
            .clone()
            .ok_or(TransactionError::Document(DocumentError::NoDocument))?;
        let metadata = self.store.metadata_for(&handle)?;
        Ok(history_info(&report, &metadata))
    }

    /// Sets the one-shot authoring note the next reply about this revision must carry.
    pub fn set_authoring_note(
        &self,
        handle: &DocumentHandle,
        note: splatmcp_bridge::AuthoringNote,
    ) {
        if let Ok(mut guard) = self.authoring_note.lock() {
            *guard = Some((handle.clone(), note));
        }
    }

    /// Takes the authoring note for a revision, if one is waiting for it.
    pub fn take_authoring_note(
        &self,
        handle: &DocumentHandle,
    ) -> Option<splatmcp_bridge::AuthoringNote> {
        let mut guard = self.authoring_note.lock().ok()?;
        match guard.as_ref() {
            Some((pending, _)) if pending == handle => guard.take().map(|(_, note)| note),
            _ => None,
        }
    }

    /// Installs a restored authoring layer for a revision.
    ///
    /// The layer is remapped by the caller (its ids belong to the file it was saved with, not to
    /// this freshly opened document) and must describe exactly this revision's gaussians.
    pub fn install_authoring(
        &self,
        handle: &DocumentHandle,
        set: AuthoringSet,
    ) -> Result<usize, TransactionError> {
        self.transactions.install_authoring(handle, set)
    }

    /// PLY bytes of bright markers at a selection's positions, for the viewer highlight.
    ///
    /// Authored in document space, exactly like the document's own PLY, so the viewer applies
    /// the same space change and the markers land on their gaussians. The marker radius scales
    /// with the selection so a highlight is visible on a small component and on a large one.
    pub fn marker_ply_bytes(&self, markers: &SelectionMarkers) -> Result<Vec<u8>, ServiceError> {
        if markers.positions.is_empty() {
            return Ok(Vec::new());
        }
        let scale = markers
            .bounds
            .map(|bounds| bounds.radius.max(1e-4))
            .unwrap_or(1.0);
        let radius = (scale * 0.02).max(1e-4);
        let splat = Splat::from_points(
            markers
                .positions
                .iter()
                .map(|position| {
                    splatmcp_core::SplatPoint::new(
                        *position,
                        [radius; 3],
                        HIGHLIGHT_COLOR,
                        HIGHLIGHT_OPACITY,
                        splatmcp_core::contract::IDENTITY_QUATERNION,
                    )
                })
                .collect(),
        );
        ply_bytes(&splat).map_err(ServiceError::Invalid)
    }

    /// World positions of the gaussians a selection handle resolved to, bounded.
    ///
    /// This is what a viewer highlight is drawn from, so a window and a tool call show the same
    /// gaussians instead of two opinions about "the selection".
    pub fn selection_markers(
        &self,
        handle_id: u64,
        max: usize,
    ) -> Result<SelectionMarkers, TransactionError> {
        let selection =
            self.transactions
                .selection(handle_id)
                .ok_or(TransactionError::Selection(
                    splatmcp_core::SelectionError::Invalid(format!(
                        "selection handle {handle_id} is no longer retained; select again"
                    )),
                ))?;
        let document = selection
            .document
            .clone()
            .ok_or_else(|| TransactionError::Document(DocumentError::NoDocument))?;
        let revision = DocumentHandle::new(document, selection.revision);
        let snapshot = self.store.resolve(&revision)?;
        let points = self
            .transactions
            .selected_points(handle_id, max)?
            .into_iter()
            .map(|point| point.position)
            .collect::<Vec<_>>();
        Ok(SelectionMarkers {
            handle_id,
            revision: selection.revision,
            count: selection.count,
            shown: points.len(),
            bounds: selection.bounds.map(BoundsInfo::from),
            positions: points,
            document_points: snapshot.splat().len(),
        })
    }

    /// The full authoring layer of a revision: components with their membership and frames.
    ///
    /// This is what a versioned sidecar records; a reply to a tool call reports counts instead,
    /// so a component with 200 000 members never crosses the bridge by accident.
    pub fn authoring_snapshot(
        &self,
        expected: Expected,
    ) -> Result<(DocumentHandle, AuthoringSet), TransactionError> {
        self.transactions.authoring_layer(expected)
    }

    /// Reads the component membership and the components of a revision.
    pub fn components(&self, expected: Expected) -> Result<ComponentListInfo, TransactionError> {
        let list = self.transactions.components(expected)?;
        let metadata = self.store.metadata_for(&list.document)?;
        Ok(component_list_info(&list, &metadata))
    }

    /// Creates a component and records a component revision.
    pub fn create_component(
        &self,
        expected: Expected,
        name: &str,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let change = self.transactions.create_component(expected, name)?;
        self.component_change_info(change.document, change.component_id)
    }

    /// Renames a component, keeping its identity.
    pub fn rename_component(
        &self,
        expected: Expected,
        component: &ComponentId,
        name: &str,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let change = self
            .transactions
            .rename_component(expected, component, name)?;
        self.component_change_info(change.document, change.component_id)
    }

    /// Removes a component; its gaussians and their identities survive.
    pub fn remove_component(
        &self,
        expected: Expected,
        component: &ComponentId,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let change = self.transactions.remove_component(expected, component)?;
        self.component_change_info(change.document, change.component_id)
    }

    /// Sets or clears a component's explicit local frame.
    pub fn set_component_transform(
        &self,
        expected: Expected,
        component: &ComponentId,
        transform: Option<LocalTransform>,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let change = self
            .transactions
            .set_component_transform(expected, component, transform)?;
        self.component_change_info(change.document, change.component_id)
    }

    /// Binds a component to exactly the gaussians a query resolves to.
    pub fn set_component_members(
        &self,
        expected: Expected,
        component: &ComponentId,
        query: &SelectionQuery,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let change = self
            .transactions
            .set_component_members(expected, component, query)?;
        self.component_change_info(change.document, change.component_id)
    }

    /// Transforms a component's members through its own local frame, as one committed edit.
    pub fn apply_component_transform(
        &self,
        expected: Expected,
        component: &ComponentId,
    ) -> Result<BatchOutcome, TransactionError> {
        let receipt = self
            .transactions
            .apply_component_transform(expected, component)?;
        Ok(BatchOutcome::of(&receipt))
    }

    /// Resolves a selection query and retains it as a revision-bound handle.
    pub fn select_points(
        &self,
        expected: Expected,
        query: &SelectionQuery,
    ) -> Result<SelectionInfo, TransactionError> {
        let handle = self.transactions.select(expected, query)?;
        let document = handle
            .document
            .clone()
            .ok_or(TransactionError::Document(DocumentError::NoDocument))?;
        let metadata = self
            .store
            .metadata_for(&DocumentHandle::new(document, handle.revision))?;
        Ok(SelectionInfo::of(&handle, &metadata))
    }

    fn component_change_info(
        &self,
        document: DocumentHandle,
        component_id: ComponentId,
    ) -> Result<ComponentChangeInfo, TransactionError> {
        let list = self
            .transactions
            .components(Expected::Handle(document.clone()))?;
        let metadata = self.store.metadata_for(&document)?;
        Ok(ComponentChangeInfo {
            document: SplatInfo::of(&metadata),
            component_id: component_id.to_string(),
            components: list.components.iter().map(ComponentInfo::of).collect(),
            rebuilt: list.rebuilt,
        })
    }

    /// The file name the document is saved under, for provenance of an edit.
    fn active_file_name(&self) -> String {
        self.store
            .active_metadata()
            .map(|metadata| metadata.provenance.file_name)
            .unwrap_or_else(|| "splat.ply".to_owned())
    }
}

fn history_info(report: &HistoryReport, metadata: &DocumentMetadata) -> HistoryInfo {
    let step = |entry: &splatmcp_core::HistoryEntry| HistoryStepInfo {
        id: entry.id,
        label: entry.label.clone(),
        revision: entry.revision,
        point_count: entry.point_count,
        at_ms: entry.at_ms,
    };
    HistoryInfo {
        document: SplatInfo::of(metadata),
        undo: report.undo.as_ref().map(step),
        redo: report.redo.as_ref().map(step),
        entries: report.entries.iter().map(step).collect(),
        retained_bytes: report.retained_bytes,
        max_bytes: report.max_bytes,
    }
}

fn component_list_info(list: &ComponentList, metadata: &DocumentMetadata) -> ComponentListInfo {
    ComponentListInfo {
        document: SplatInfo::of(metadata),
        components: list.components.iter().map(ComponentInfo::of).collect(),
        rebuilt: list.rebuilt,
    }
}

/// Parses a document id and revision into a handle, or explains why not.
pub fn handle_of(document_id: &str, revision: u64) -> Result<DocumentHandle, String> {
    let document_id = DocumentId::parse(document_id)
        .ok_or_else(|| format!("'{document_id}' is not a document id"))?;
    Ok(DocumentHandle::new(document_id, revision))
}

/// The exact handle a flat reply describes, for a follow-up call that must name it.
///
/// A reply carries identity and revision; rebuilding the handle is how a caller quotes it back
/// instead of asking for "whatever is displayed now".
pub fn handle_from_info(info: &SplatInfo) -> Result<DocumentHandle, String> {
    let document_id = DocumentId::parse(&info.document_id)
        .ok_or_else(|| format!("'{}' is not a document id", info.document_id))?;
    Ok(DocumentHandle::new(document_id, info.revision))
}

/// Reads a `.ply` file and turns it into a mutation that opens it as a new document.
pub fn open_mutation(path: &Path) -> Mutation {
    Mutation::open(path.to_string_lossy().to_string())
}

/// Turns a bridge/tool target into the expectation the store checks.
///
/// A named document must also name the revision it expects: without one, the request could
/// silently overwrite work that landed in between.
pub fn expected_target(
    document_id: Option<&str>,
    expected_revision: Option<u64>,
) -> Result<Expected, String> {
    match (document_id, expected_revision) {
        (Some(text), Some(revision)) => {
            let document_id =
                DocumentId::parse(text).ok_or_else(|| format!("'{text}' is not a document id"))?;
            Ok(Expected::Handle(DocumentHandle::new(document_id, revision)))
        }
        (Some(text), None) => Err(format!(
            "a change to document '{text}' also needs expected_revision, so concurrent edits \
             are reported instead of overwritten"
        )),
        (None, Some(revision)) => Ok(Expected::Revision(revision)),
        (None, None) => Ok(Expected::Any),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use splatmcp_core::SplatPoint;

    /// Flat description, as the commands report it.
    fn info(state: &AppState) -> Option<SplatInfo> {
        state.metadata().map(|metadata| SplatInfo::of(&metadata))
    }

    fn ply_of(points: usize) -> Vec<u8> {
        let splat = Splat::from_points(
            (0..points)
                .map(|index| {
                    SplatPoint::new(
                        [index as f32, 0.0, 0.0],
                        [0.1; 3],
                        [0.5; 3],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        );
        write_ply(&splat).unwrap()
    }

    #[test]
    fn garbage_never_becomes_the_displayed_document() {
        let state = AppState::default();
        assert!(
            state
                .open_ply(
                    b"not a ply",
                    Mutation::import("x.ply"),
                    PlyImportPolicy::Strict
                )
                .is_err()
        );
        assert!(state.active().is_err());
        assert!(info(&state).is_none());
        assert_eq!(state.retention().documents, 0);
    }

    #[test]
    fn opening_twice_is_two_documents_and_a_named_replacement_keeps_one() {
        let state = AppState::default();
        let first = SplatInfo::of(
            &state
                .open_ply(
                    &ply_of(3),
                    Mutation::open("C:/tmp/house.ply"),
                    PlyImportPolicy::Strict,
                )
                .unwrap()
                .metadata,
        );
        let second = SplatInfo::of(
            &state
                .open_ply(
                    &ply_of(3),
                    Mutation::open("C:/tmp/house.ply"),
                    PlyImportPolicy::Strict,
                )
                .unwrap()
                .metadata,
        );

        assert_ne!(
            first.document_id, second.document_id,
            "a path is not identity"
        );
        assert_eq!(second.revision, 1);
        assert_eq!(second.file_name, "house.ply");
        assert_eq!(second.source_path.as_deref(), Some("C:/tmp/house.ply"));

        // An explicit replacement keeps the identity and moves one revision.
        let replaced = SplatInfo::of(
            &state
                .replace_ply(
                    Expected::Handle(DocumentHandle::new(
                        DocumentId::parse(&second.document_id).unwrap(),
                        second.revision,
                    )),
                    &ply_of(5),
                    Mutation::edit("edit_splat"),
                    PlyImportPolicy::Strict,
                )
                .unwrap()
                .metadata,
        );
        assert_eq!(replaced.document_id, second.document_id);
        assert_eq!(replaced.revision, 2);
        assert_eq!(replaced.point_count, 5);
        assert_eq!(replaced.last_operation.as_deref(), Some("edit_splat"));
    }

    /// A two-point ASCII PLY whose first quaternion is all zero.
    fn ascii_with_zero_quaternion() -> Vec<u8> {
        const PROPERTIES: [&str; 14] = [
            "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
            "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
        ];
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 2\n");
        for name in PROPERTIES {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str("end_header\n");
        header.push_str("0 0 0 0 0 0 0 -8 -8 -8 0 0 0 0\n");
        header.push_str("1 0 0 0 0 0 0 -8 -8 -8 1 0 0 0\n");
        header.into_bytes()
    }

    #[test]
    fn a_strict_import_refuses_a_damaged_file_and_repair_reports_what_it_changed() {
        let state = AppState::default();
        let damaged = ascii_with_zero_quaternion();

        // The default: refuse, with the index and the reason, and change nothing.
        let error = state
            .open_ply(
                &damaged,
                Mutation::import("damaged.ply"),
                PlyImportPolicy::Strict,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
            .unwrap_err();
        assert!(error.contains("point 0 rotation"), "{error}");
        assert!(error.contains("[0, 0, 0, 0]"), "{error}");
        assert!(
            error.contains("repair"),
            "the refusal says what to do: {error}"
        );
        assert!(
            state.metadata().is_none(),
            "a refused import leaves nothing behind"
        );

        // The explicit opt-in: load it, and report every change.
        let imported = state
            .open_ply(
                &damaged,
                Mutation::import("damaged.ply"),
                PlyImportPolicy::Repair,
            )
            .unwrap();
        assert_eq!(imported.metadata.point_count, 2);
        assert_eq!(imported.report.total_repairs, 1);
        assert_eq!(imported.report.repairs[0].point, 0, "repairs are indexed");
        assert!(imported.report.policy.repairs());

        // What a reply carries: the summary names the repair, so it is never silent.
        let summary = splatmcp_bridge::PlyImportSummary::of(&imported.report).unwrap();
        assert_eq!(summary.policy, "repair");
        assert!(!summary.lossless);
        assert!(
            summary
                .changed
                .iter()
                .any(|entry| entry.contains("point 0 rotation"))
        );
        assert_eq!(summary.changed_count, 1);
    }

    #[test]
    fn a_failed_export_releases_its_pin() {
        let state = AppState::default();
        state
            .open_ply(
                &ply_of(4),
                Mutation::import("scene.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        let directory =
            std::env::temp_dir().join(format!("splatmcp-export-dir-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();

        // A directory cannot be written as a file, so the export fails *after* the pin was
        // taken. The pin must be gone again, or repeated failures would keep whole scenes
        // alive past the retention budget.
        assert_eq!(state.retention().pins, 0);
        let error = state
            .export(Expected::Any, &directory)
            .unwrap_err()
            .to_string();
        assert!(!error.is_empty());
        assert_eq!(
            state.retention().pins,
            0,
            "a failed export must not leak a pin"
        );

        // The successful path takes and releases one too.
        let path = directory.join("out.ply");
        let outcome = state.export(Expected::Any, &path).unwrap();
        assert_eq!(state.retention().pins, 0);
        assert!(outcome.checksum.starts_with("fnv1a64:"));
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_stale_replacement_is_refused_and_leaves_the_document_alone() {
        let state = AppState::default();
        let opened = state
            .open_ply(
                &ply_of(3),
                Mutation::import("scene.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        let handle = opened.handle.clone();
        state
            .replace_ply(
                Expected::Handle(handle.clone()),
                &ply_of(4),
                Mutation::edit("edit"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;

        let error = state
            .replace_ply(
                Expected::Handle(handle),
                &ply_of(9),
                Mutation::edit("edit"),
                PlyImportPolicy::Strict,
            )
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains("revision conflict"), "{error}");
        let current = info(&state).unwrap();
        assert_eq!(current.revision, 2);
        assert_eq!(current.point_count, 4);
    }

    #[test]
    fn a_named_document_without_a_revision_is_refused_before_anything_is_read() {
        let error = expected_target(Some("doc-4f2a-1"), None).unwrap_err();
        assert!(error.contains("expected_revision"), "{error}");
        let error = expected_target(Some("C:/tmp/scene.ply"), Some(1)).unwrap_err();
        assert!(error.contains("not a document id"), "{error}");

        assert_eq!(expected_target(None, None).unwrap(), Expected::Any);
        assert_eq!(
            expected_target(None, Some(3)).unwrap(),
            Expected::Revision(3)
        );
        let handle = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 7);
        assert_eq!(
            expected_target(Some("doc-4f2a-1"), Some(7)).unwrap(),
            Expected::Handle(handle)
        );
    }

    #[test]
    fn an_export_records_its_artifact_without_moving_the_revision() {
        let state = AppState::default();
        let opened = state
            .open_ply(
                &ply_of(4),
                Mutation::import("scene.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        let directory =
            std::env::temp_dir().join(format!("splatmcp-export-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("scene-out.ply");

        let outcome = state.export(Expected::Any, &path).unwrap();
        assert_eq!(outcome.document_id, opened.handle.document_id.to_string());
        assert_eq!(
            outcome.revision, opened.handle.revision,
            "an export is not a new revision"
        );
        assert!(outcome.checksum.starts_with("fnv1a64:"));
        assert!(outcome.bytes > 0);
        assert!(path.is_file());
        // The file is a readable splat with the same gaussians.
        let reloaded = parse_ply(&std::fs::read(&path).unwrap(), PlyImportPolicy::Strict)
            .unwrap()
            .splat;
        assert_eq!(reloaded.len(), 4);

        // The export is visible as provenance, newest first.
        let metadata = state.metadata().unwrap();
        let export = metadata.last_export().expect("recorded");
        assert_eq!(export.path, path.to_string_lossy());
        assert_eq!(export.revision, opened.handle.revision);

        // A later change still moves exactly one revision.
        let handle = state.active_handle().unwrap();
        let advanced = state
            .replace_ply(
                Expected::Handle(handle),
                &ply_of(6),
                Mutation::edit("edit"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        assert_eq!(advanced.handle.revision, opened.handle.revision + 1);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn reload_reads_the_source_again_as_a_new_revision() {
        let state = AppState::default();
        let directory =
            std::env::temp_dir().join(format!("splatmcp-reload-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("scene.ply");
        std::fs::write(&path, ply_of(3)).unwrap();
        let opened = state
            .open_ply(
                &std::fs::read(&path).unwrap(),
                Mutation::open(path.to_string_lossy()),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;

        // The file changes on disk; reload brings it in without changing identity.
        std::fs::write(&path, ply_of(6)).unwrap();
        let reloaded = SplatInfo::of(
            &state
                .reload(Expected::Any, PlyImportPolicy::Strict)
                .unwrap()
                .metadata,
        );
        assert_eq!(reloaded.document_id, opened.handle.document_id.to_string());
        assert_eq!(reloaded.revision, opened.handle.revision + 1);
        assert_eq!(reloaded.point_count, 6);
        assert_eq!(reloaded.last_operation.as_deref(), Some("reload"));

        // A document with no source cannot be reloaded, and says why.
        let state = AppState::default();
        state
            .open_ply(
                &ply_of(2),
                Mutation::import("generated.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        let error = state
            .reload(Expected::Any, PlyImportPolicy::Strict)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no source file"), "{error}");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_component_change_moves_the_revision_and_keeps_the_geometry() {
        let state = AppState::default();
        let opened = state
            .open_ply(
                &ply_of(4),
                Mutation::import("scene.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        let geometry = state.active().unwrap();
        let before = Arc::as_ptr(geometry.splat());

        let info = SplatInfo::of(
            &state
                .set_component(Expected::Any, "roof", "set_component")
                .unwrap(),
        );
        assert_eq!(info.revision, opened.handle.revision + 1);
        assert_eq!(info.point_count, 4);
        assert_eq!(info.component_id.as_deref(), Some("roof"));

        // The content was shared, not copied: nothing moved in memory.
        let after = state.active().unwrap();
        assert_eq!(before, Arc::as_ptr(after.splat()));

        let error = state
            .set_component(Expected::Any, "   ", "set_component")
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not be blank"), "{error}");

        // An explicit expectation is checked before anything changes.
        let stale = state
            .set_component(
                Expected::Revision(opened.handle.revision),
                "roof",
                "set_component",
            )
            .unwrap_err();
        assert!(stale.is_conflict());
        assert!(stale.to_string().contains("revision conflict"), "{stale}");
    }

    #[test]
    fn a_replayed_batch_reports_the_recorded_outcome_not_the_current_document() {
        let state = scene_state();
        let batch = splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::new(
            splatmcp_core::EditOp::Duplicate {
                by: [0.0, 0.0, 1.0],
            },
        )])
        .with_operation_id("recipe-replay");
        let first = state
            .commit_batch(Expected::Any, &batch, "edit_splat")
            .unwrap();
        assert_eq!(first.point_count, 8);
        assert_eq!(first.document.point_count, 8);
        assert_eq!(first.document.revision, 2);

        // The document moves on: a later edit and an undo/redo cycle.
        state
            .commit_batch(
                Expected::Any,
                &splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::with_targets(
                    splatmcp_core::EditOp::Remove,
                    splatmcp_core::BatchTargets::from_selection(splatmcp_core::Selection {
                        first: Some(3),
                        ..splatmcp_core::Selection::default()
                    }),
                )]),
                "edit_splat",
            )
            .unwrap();
        assert_eq!(state.metadata().unwrap().point_count, 5);

        let retry = state
            .commit_batch(Expected::Any, &batch, "edit_splat")
            .unwrap();
        assert!(retry.replayed);
        // The reply repeats the original outcome: revision 2 and eight gaussians, not the
        // revision and count that happen to be displayed now.
        assert_eq!(retry.document.revision, 2);
        assert_eq!(retry.document.point_count, 8);
        assert_eq!(retry.point_count, first.point_count);
        assert_eq!(retry.steps, first.steps);
        let summary = retry.summary();
        assert_eq!(summary.revision, 2);
        assert_eq!(summary.point_count, 8);
        assert_eq!(state.metadata().unwrap().handle.revision, 3);
    }

    #[test]
    fn a_recorded_display_outcome_is_replayed_and_published_is_not_done() {
        let state = scene_state();
        let batch = splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::new(
            splatmcp_core::EditOp::Translate {
                by: [1.0, 0.0, 0.0],
            },
        )])
        .with_operation_id("recipe-display");
        let first = state
            .commit_batch(Expected::Any, &batch, "edit_splat")
            .unwrap();
        let handle = first.handle().unwrap();
        // The window has not acknowledged anything yet.
        assert_eq!(first.display.status, "not_requested");
        state.note_side_effect(
            &handle,
            splatmcp_core::ReceiptSlot::Display,
            splatmcp_core::SideEffect::Published,
        );
        let retry = state
            .commit_batch(Expected::Any, &batch, "edit_splat")
            .unwrap();
        assert!(retry.replayed);
        assert_eq!(retry.display.status, "published");
        assert!(
            !retry.display.status.eq("done"),
            "an announcement is not a finished render"
        );

        // The window's acknowledgement is what turns it into done, and the retry then says so.
        state.note_side_effect(
            &handle,
            splatmcp_core::ReceiptSlot::Display,
            splatmcp_core::SideEffect::Done,
        );
        let retry = state
            .commit_batch(Expected::Any, &batch, "edit_splat")
            .unwrap();
        assert_eq!(retry.display.status, "done");
    }

    #[test]
    fn a_preview_commit_with_an_operation_id_can_be_retried() {
        let state = scene_state();
        let batch = splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::with_targets(
            splatmcp_core::EditOp::Remove,
            splatmcp_core::BatchTargets::from_selection(splatmcp_core::Selection {
                first: Some(1),
                ..splatmcp_core::Selection::default()
            }),
        )]);
        let preview = state.preview_batch(Expected::Any, &batch).unwrap();
        let first = state
            .commit_preview(
                preview.preview_id,
                Expected::Any,
                Some("commit-1".to_owned()),
            )
            .unwrap();
        assert_eq!(first.point_count, 3);
        assert_eq!(first.document.revision, 2);

        let retry = state
            .commit_preview(
                preview.preview_id,
                Expected::Any,
                Some("commit-1".to_owned()),
            )
            .unwrap();
        assert!(retry.replayed);
        assert_eq!(retry.document.revision, 2);
        assert_eq!(retry.point_count, 3);
        assert_eq!(
            state.metadata().unwrap().handle.revision,
            2,
            "a replayed commit does not commit again"
        );
    }

    #[test]
    fn save_records_the_exported_revisions_gaussian_count_and_can_be_reopened() {
        // This is the Save path the window uses, not a hand-built record: the point count, the
        // checksum and the membership all come from the revision that was written.
        let directory =
            std::env::temp_dir().join(format!("splatmcp-save-count-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("scene.ply");

        let state = scene_state();
        let created = state.create_component(Expected::Any, "hair").unwrap();
        let component = splatmcp_core::ComponentId::parse(&created.component_id).unwrap();
        state
            .set_component_members(
                Expected::Any,
                &component,
                &splatmcp_core::SelectionQuery {
                    first: Some(2),
                    ..splatmcp_core::SelectionQuery::all()
                },
            )
            .unwrap();

        let (export, sidecar) = state.export_with_authoring(&path).unwrap();
        assert_eq!(export.point_count, 4, "four gaussians were exported");
        assert_ne!(
            export.point_count, export.bytes,
            "a file size is not a gaussian count"
        );
        let sidecar = sidecar.expect("a revision with components writes one");
        assert!(sidecar.written(), "{sidecar:?}");
        assert_eq!(sidecar.point_count, export.point_count);
        assert_eq!(sidecar.components, 1);
        assert_eq!(sidecar.members, 2);

        // The record on disk agrees, and describes these bytes.
        let record = crate::authoring::read(&path).unwrap().unwrap();
        assert_eq!(record.point_count, 4);
        assert_eq!(record.artifact, export.checksum);
        assert_eq!(record.components[0].point_rows, vec![0, 1]);

        // Reopening the file the Save action wrote restores the components: the count a record
        // carries is the one a reopen validates against, so getting it wrong here would make
        // every native save unloadable.
        let bytes = std::fs::read(&path).unwrap();
        let fresh = AppState::default();
        let imported = fresh
            .open_ply(
                &bytes,
                Mutation::open(path.to_string_lossy().to_string()),
                PlyImportPolicy::Strict,
            )
            .unwrap();
        let note = crate::bridge::restore_note(&fresh, &path, &imported, &bytes)
            .expect("a matching sidecar attaches");
        assert_eq!(note.status, "restored", "{note:?}");
        assert_eq!(note.components, 1);
        assert_eq!(note.members, 2);
        assert_eq!(
            fresh.components(Expected::Any).unwrap().components[0].point_count,
            2
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_revision_without_components_writes_no_sidecar() {
        let directory =
            std::env::temp_dir().join(format!("splatmcp-save-plain-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("plain.ply");
        let state = scene_state();
        let (export, sidecar) = state.export_with_authoring(&path).unwrap();
        assert_eq!(export.point_count, 4);
        assert!(sidecar.is_none(), "a plain export stays geometry only");
        assert!(crate::authoring::read(&path).unwrap().is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_selection_highlight_describes_the_same_gaussians_a_tool_call_selected() {
        let state = scene_state();
        let selection = state
            .select_points(
                Expected::Any,
                &splatmcp_core::SelectionQuery {
                    first: Some(2),
                    ..splatmcp_core::SelectionQuery::all()
                },
            )
            .unwrap();
        let markers = state.selection_markers(selection.handle_id, 8).unwrap();
        assert_eq!(markers.handle_id, selection.handle_id);
        assert_eq!(markers.count, 2);
        assert_eq!(markers.shown, 2);
        assert!(!markers.truncated());
        assert_eq!(markers.revision, selection.revision);
        assert_eq!(markers.positions.len(), 2);
        assert_eq!(markers.positions[0], [0.0, 0.0, 0.0]);

        // The markers are ordinary gaussians in a PLY the viewer already knows how to draw.
        let bytes = state.marker_ply_bytes(&markers).unwrap();
        let splat =
            splatmcp_core::read_ply_with_policy(&bytes, splatmcp_core::PlyImportPolicy::Strict)
                .unwrap()
                .0;
        assert_eq!(splat.len(), 2);
        // The PLY round trip is 8-bit per channel, so the marker colour comes back within one
        // step of the constant; what matters is that it is unmistakably the highlight colour.
        let [r, g, b] = splat.points[0].color;
        assert!((r - super::HIGHLIGHT_COLOR[0]).abs() < 0.01);
        assert!((g - super::HIGHLIGHT_COLOR[1]).abs() < 0.01);
        assert!((b - super::HIGHLIGHT_COLOR[2]).abs() < 0.01);
        assert!(splat.points.iter().all(|point| point.opacity > 0.99));

        // A bounded highlight says so instead of pretending it drew everything.
        let markers = state.selection_markers(selection.handle_id, 1).unwrap();
        assert!(markers.truncated());
    }

    fn scene_state() -> AppState {
        let state = AppState::default();
        state
            .open_ply(
                &ply_of(4),
                Mutation::import("scene.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        state
    }

    #[test]
    fn a_batch_commits_once_previews_inertly_and_undo_restores_the_geometry() {
        let state = scene_state();
        let batch = splatmcp_core::EditBatch::new(vec![
            splatmcp_core::BatchStep::new(splatmcp_core::EditOp::Translate {
                by: [0.0, 3.0, 0.0],
            }),
            splatmcp_core::BatchStep::with_targets(
                splatmcp_core::EditOp::Remove,
                splatmcp_core::BatchTargets::from_selection(splatmcp_core::Selection {
                    first: Some(1),
                    ..splatmcp_core::Selection::default()
                }),
            ),
        ]);
        let preview = state.preview_batch(Expected::Any, &batch).unwrap();
        assert_eq!(preview.points_before, 4);
        assert_eq!(preview.points_after, 3);
        assert_eq!(
            state.metadata().unwrap().point_count,
            4,
            "a preview is inert"
        );
        assert!(state.preview_ply_bytes(preview.preview_id).unwrap().len() > 0);

        let receipt = state
            .commit_batch(Expected::Any, &batch, "edit_batch")
            .unwrap();
        assert!(receipt.committed);
        assert_eq!(receipt.point_count, 3);
        assert_eq!(receipt.document.revision, 2);
        assert!(receipt.undo_available);
        assert_eq!(receipt.export.status, "not_requested");
        assert_eq!(receipt.display.status, "not_requested");

        let undone = state.undo(Expected::Any).unwrap();
        assert_eq!(undone.document.revision, 3);
        assert_eq!(state.metadata().unwrap().point_count, 4);
        let report = state.edit_history(Expected::Any).unwrap();
        assert!(report.redo.is_some());
        assert_eq!(report.document.revision, 3);
    }

    #[test]
    fn a_stale_preview_is_refused_rather_than_overwriting_a_newer_revision() {
        let state = scene_state();
        let batch = splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::new(
            splatmcp_core::EditOp::Translate {
                by: [0.0, 1.0, 0.0],
            },
        )]);
        let preview = state.preview_batch(Expected::Any, &batch).unwrap();
        state
            .commit_batch(
                Expected::Any,
                &splatmcp_core::EditBatch::new(vec![splatmcp_core::BatchStep::new(
                    splatmcp_core::EditOp::Translate {
                        by: [0.0, 0.0, 1.0],
                    },
                )]),
                "edit_batch",
            )
            .unwrap();
        let error = state
            .commit_preview(preview.preview_id, Expected::Any, None)
            .unwrap_err();
        assert_eq!(error.code(), "preview_conflict");
        assert_eq!(state.metadata().unwrap().handle.revision, 2);
    }

    #[test]
    fn components_and_selections_report_the_ids_a_tool_would_see() {
        let state = scene_state();
        let created = state.create_component(Expected::Any, "hair").unwrap();
        assert_eq!(created.components.len(), 1);
        assert_eq!(created.document.revision, 2);

        let selection = state
            .select_points(
                Expected::Any,
                &splatmcp_core::SelectionQuery {
                    first: Some(2),
                    ..splatmcp_core::SelectionQuery::all()
                },
            )
            .unwrap();
        assert_eq!(selection.count, 2);
        assert_eq!(selection.sample.len(), 2);
        assert!(selection.sample[0].starts_with("pt-"));

        let members = state
            .set_component_members(
                Expected::Any,
                &splatmcp_core::ComponentId::parse(&created.component_id).unwrap(),
                &splatmcp_core::SelectionQuery {
                    point_ids: selection
                        .sample
                        .iter()
                        .map(|text| splatmcp_core::PointId::parse(text).unwrap())
                        .collect(),
                    ..splatmcp_core::SelectionQuery::all()
                },
            )
            .unwrap();
        assert_eq!(members.components[0].point_count, 2);
        assert_eq!(members.components[0].component_id, created.component_id);
        assert_eq!(
            state.metadata().unwrap().point_count,
            4,
            "membership moves nothing"
        );
    }

    #[test]
    fn a_retained_revision_stays_readable_after_the_display_moves_on() {
        let state = AppState::default();
        let first = state
            .open_ply(
                &ply_of(3),
                Mutation::open("C:/tmp/first.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;
        let handle = first.handle.clone();
        state
            .open_ply(
                &ply_of(7),
                Mutation::open("C:/tmp/second.ply"),
                PlyImportPolicy::Strict,
            )
            .unwrap()
            .metadata;

        // The old revision still reads, exactly, and does not disturb what is displayed.
        let (snapshot, bytes) = state.ply_bytes_for(&handle).unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(
            parse_ply(&bytes, PlyImportPolicy::Strict)
                .unwrap()
                .splat
                .len(),
            3
        );
        assert_eq!(info(&state).unwrap().point_count, 7);

        // An unknown handle is reported as unknown, and an evicted one as expired.
        let foreign = DocumentHandle::new(DocumentId::mint(0xdead, 1), 1);
        assert!(
            state
                .ply_bytes_for(&foreign)
                .unwrap_err()
                .to_string()
                .contains("not available")
        );
    }
}
