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

use serde::Serialize;
use splatmcp_core::document::{DocumentMetadata, DocumentStore, now_ms};
use splatmcp_core::{Splat, SplatError, read_ply, write_ply};

pub use splatmcp_core::document::{
    ArtifactChecksum, DocumentError, DocumentHandle, DocumentId, Expected, Mutation, MutationKind,
    RetentionStats, Snapshot,
};

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

/// Parses PLY bytes and validates them, so an unusable document never enters state.
pub fn parse_ply(bytes: &[u8]) -> Result<Splat, String> {
    let splat = read_ply(bytes).map_err(|error| error.to_string())?;
    splat.validate().map_err(|error| error.to_string())?;
    Ok(splat)
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
}

impl SplatInfo {
    /// Projects bounded metadata onto the flat reply shape.
    pub fn of(metadata: &DocumentMetadata) -> Self {
        Self {
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
    pub bytes: usize,
}

/// Application state: the one authoritative document store this process owns.
#[derive(Default)]
pub struct AppState {
    store: DocumentStore,
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
    ) -> Result<DocumentMetadata, ServiceError> {
        let splat = parse_ply(bytes)?;
        Ok(self.store.open(splat, mutation))
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
    ) -> Result<DocumentMetadata, ServiceError> {
        let splat = parse_ply(bytes)?;
        Ok(self.store.commit(expected, splat, mutation)?)
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
    pub fn reload(&self, expected: Expected) -> Result<DocumentMetadata, ServiceError> {
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
        let splat = parse_ply(&bytes)?;
        Ok(self.store.commit(
            Expected::Handle(handle),
            splat,
            Mutation::reload("reload").source(source),
        )?)
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
    pub fn export(&self, expected: Expected, path: &Path) -> Result<ExportOutcome, ServiceError> {
        let snapshot = self.snapshot(expected)?;
        let pin = self.store.pin(snapshot.handle())?;
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
        self.store.release(&pin);
        Ok(ExportOutcome {
            path: path.to_string_lossy().to_string(),
            document_id: metadata.handle.document_id.to_string(),
            revision: metadata.handle.revision,
            checksum: format!("{}:{}", checksum.algorithm, checksum.hex()),
            bytes: checksum.bytes,
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
                .open_ply(b"not a ply", Mutation::import("x.ply"))
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
                .open_ply(&ply_of(3), Mutation::open("C:/tmp/house.ply"))
                .unwrap(),
        );
        let second = SplatInfo::of(
            &state
                .open_ply(&ply_of(3), Mutation::open("C:/tmp/house.ply"))
                .unwrap(),
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
                )
                .unwrap(),
        );
        assert_eq!(replaced.document_id, second.document_id);
        assert_eq!(replaced.revision, 2);
        assert_eq!(replaced.point_count, 5);
        assert_eq!(replaced.last_operation.as_deref(), Some("edit_splat"));
    }

    #[test]
    fn a_stale_replacement_is_refused_and_leaves_the_document_alone() {
        let state = AppState::default();
        let opened = state
            .open_ply(&ply_of(3), Mutation::import("scene.ply"))
            .unwrap();
        let handle = opened.handle.clone();
        state
            .replace_ply(
                Expected::Handle(handle.clone()),
                &ply_of(4),
                Mutation::edit("edit"),
            )
            .unwrap();

        let error = state
            .replace_ply(Expected::Handle(handle), &ply_of(9), Mutation::edit("edit"))
            .unwrap_err();
        assert!(error.to_string().contains("revision conflict"), "{error}");
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
            .open_ply(&ply_of(4), Mutation::import("scene.ply"))
            .unwrap();
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
        let reloaded = parse_ply(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded.len(), 4);

        // The export is visible as provenance, newest first.
        let metadata = state.metadata().unwrap();
        let export = metadata.last_export().expect("recorded");
        assert_eq!(export.path, path.to_string_lossy());
        assert_eq!(export.revision, opened.handle.revision);

        // A later change still moves exactly one revision.
        let handle = state.active_handle().unwrap();
        let advanced = state
            .replace_ply(Expected::Handle(handle), &ply_of(6), Mutation::edit("edit"))
            .unwrap();
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
            )
            .unwrap();

        // The file changes on disk; reload brings it in without changing identity.
        std::fs::write(&path, ply_of(6)).unwrap();
        let reloaded = SplatInfo::of(&state.reload(Expected::Any).unwrap());
        assert_eq!(reloaded.document_id, opened.handle.document_id.to_string());
        assert_eq!(reloaded.revision, opened.handle.revision + 1);
        assert_eq!(reloaded.point_count, 6);
        assert_eq!(reloaded.last_operation.as_deref(), Some("reload"));

        // A document with no source cannot be reloaded, and says why.
        let state = AppState::default();
        state
            .open_ply(&ply_of(2), Mutation::import("generated.ply"))
            .unwrap();
        let error = state.reload(Expected::Any).unwrap_err().to_string();
        assert!(error.contains("no source file"), "{error}");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_component_change_moves_the_revision_and_keeps_the_geometry() {
        let state = AppState::default();
        let opened = state
            .open_ply(&ply_of(4), Mutation::import("scene.ply"))
            .unwrap();
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
    fn a_retained_revision_stays_readable_after_the_display_moves_on() {
        let state = AppState::default();
        let first = state
            .open_ply(&ply_of(3), Mutation::open("C:/tmp/first.ply"))
            .unwrap();
        let handle = first.handle.clone();
        state
            .open_ply(&ply_of(7), Mutation::open("C:/tmp/second.ply"))
            .unwrap();

        // The old revision still reads, exactly, and does not disturb what is displayed.
        let (snapshot, bytes) = state.ply_bytes_for(&handle).unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(parse_ply(&bytes).unwrap().len(), 3);
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
