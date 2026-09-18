//! Document identity, revisions and immutable snapshots.
//!
//! One authoritative store owns the scene the app displays; everything else - the viewer,
//! MCP tools, the Python job service - asks that store for an *exact* revision rather than
//! for "the current scene".
//!
//! # Identity
//!
//! A [`DocumentId`] is minted by the store and is unique to the session that minted it
//! (`doc-<session>-<n>`). Two consequences matter:
//!
//! - A **file name or path is never identity**. Paths are provenance: where geometry came
//!   from, and where it was last exported. Renaming or re-saving changes neither.
//! - A handle from an earlier run of the app names a session that no longer exists, so it
//!   cannot silently match a freshly minted document. Handles are only valid for the life of
//!   the process unless a versioned project format deliberately restores them.
//!
//! # Revisions
//!
//! Every accepted *content or component-metadata* change advances the document's revision by
//! one; the revision never goes backwards and is never reused. Exporting, saving, capturing
//! and reading do not advance it: they record provenance (see
//! [`DocumentStore::record_export`]) because neither geometry nor component metadata changed.
//! Undo, once it exists, commits a new revision for the same reason.
//!
//! # Compare and swap
//!
//! A mutation states what it expects ([`Expected`]) and the check happens inside the same
//! critical section as the swap, so two candidates built from the same revision produce one
//! commit and one explicit [`DocumentError::Conflict`] - never a silent last-writer-wins.
//!
//! # Snapshots
//!
//! Reading yields a [`Snapshot`]: an immutable `Arc<Splat>` of one exact revision plus the
//! provenance and metadata that describe it. Because the store keeps content behind `Arc`
//! and mutates with copy-on-write, a snapshot stays readable and unchanged while the editor
//! moves on, and holding one costs nothing until the content actually changes.
//!
//! Retention is bounded ([`RetentionLimits`]) so kept revisions cannot grow without limit;
//! a handle whose revision has been evicted fails with [`DocumentError::SnapshotExpired`],
//! and [`DocumentStore::pin`] keeps one revision resolvable for as long as a caller needs it.

pub mod metadata;
pub mod store;

pub use metadata::{ArtifactChecksum, DocumentMetadata, ExportRecord, RevisionRecord, fingerprint};
pub use store::{DocumentStore, RetentionLimits, RetentionStats, Snapshot, SnapshotPin, now_ms};

use std::fmt;

/// Identity of one document, minted by a [`DocumentStore`] for the session that owns it.
///
/// Opaque on purpose: a caller stores it, compares it and passes it back, and never derives
/// meaning from its text. The rendered form is stable so it can travel in JSON and logs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId(String);

impl DocumentId {
    /// Mints the `index`-th identity of `session`.
    ///
    /// The session stamp is what makes a handle from an earlier run fail loudly instead of
    /// naming whatever document happens to hold the same index this time.
    pub fn mint(session: u64, index: u64) -> Self {
        Self(format!("doc-{session:x}-{index}"))
    }

    /// Reads an identity back from text, or `None` when it was not produced by this format.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("doc-")?;
        let (session, index) = rest.split_once('-')?;
        if session.is_empty() || index.is_empty() {
            return None;
        }
        if !session
            .chars()
            .all(|character| character.is_ascii_hexdigit())
            || !index.chars().all(|character| character.is_ascii_digit())
        {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An exact revision of an exact document: the handle every later request quotes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentHandle {
    pub document_id: DocumentId,
    /// Revision of the content, starting at 1 and advancing on every accepted change.
    pub revision: u64,
}

impl DocumentHandle {
    pub fn new(document_id: DocumentId, revision: u64) -> Self {
        Self {
            document_id,
            revision,
        }
    }
}

impl fmt::Display for DocumentHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.document_id, self.revision)
    }
}

/// What a mutation demands of the document before it is allowed to change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// The displayed document, or a new one when nothing is displayed.
    ///
    /// This is the "act on whatever is shown right now" case: it resolves once, at request
    /// receipt, and the resolved handle is reported back to the caller. It is deliberately
    /// *not* used for work that names a document, because a named document must never
    /// resolve to whichever document is displayed later.
    Any,
    /// The displayed document, which must still be at this revision.
    Revision(u64),
    /// That exact document at that exact revision.
    Handle(DocumentHandle),
}

impl Expected {
    /// The handle this expectation names, when it names one.
    pub fn handle(&self) -> Option<&DocumentHandle> {
        match self {
            Self::Handle(handle) => Some(handle),
            _ => None,
        }
    }

    /// Short description used in error messages and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Any => "the displayed document".to_owned(),
            Self::Revision(revision) => format!("the displayed document at revision {revision}"),
            Self::Handle(handle) => handle.to_string(),
        }
    }
}

/// Why a revision was produced: what the caller did to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    /// Something else became the displayed document (file open, new document).
    Open,
    /// Geometry arrived from outside (a bridge load, an imported file).
    Import,
    /// An edit changed the existing geometry.
    Edit,
    /// Component metadata changed without touching geometry.
    Component,
    /// A generation job produced the revision.
    Job,
    /// The source of the document was read again.
    Reload,
}

impl MutationKind {
    /// Stable name, used in history records and replies.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Import => "import",
            Self::Edit => "edit",
            Self::Component => "component",
            Self::Job => "job",
            Self::Reload => "reload",
        }
    }

    /// True for every kind: each one changes content or component metadata, so each one
    /// advances the revision. Exporting and reading are not mutations at all.
    pub fn advances_revision(self) -> bool {
        matches!(
            self,
            Self::Open | Self::Import | Self::Edit | Self::Component | Self::Job | Self::Reload
        )
    }
}

impl fmt::Display for MutationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The provenance a mutation wants recorded, and how the caller names it.
///
/// Every field is optional and means "set this when given"; a mutation that does not mention
/// a field leaves the existing value alone. That keeps a small edit from erasing where the
/// document came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub kind: MutationKind,
    /// Operation or job correlation, e.g. `edit_splat`, `job 12`, `open`.
    pub operation: Option<String>,
    /// Named component this revision replaces or creates.
    pub component_id: Option<String>,
    /// File the geometry came from, when it came from one.
    pub source_path: Option<String>,
    /// Name the document should be saved under.
    pub file_name: Option<String>,
    /// Opaque producer record (a recipe), stored verbatim.
    pub recipe: Option<String>,
    /// Timestamp to record; omitted means "now".
    pub at_ms: Option<u64>,
}

impl Mutation {
    /// A mutation of `kind` with nothing else stated.
    pub fn new(kind: MutationKind) -> Self {
        Self {
            kind,
            operation: None,
            component_id: None,
            source_path: None,
            file_name: None,
            recipe: None,
            at_ms: None,
        }
    }

    /// Something else became the displayed document.
    pub fn open(source_path: impl Into<String>) -> Self {
        let source_path = source_path.into();
        Self::new(MutationKind::Open)
            .source(source_path.clone())
            .file_name(file_name_of(&source_path))
            .operation("open")
    }

    /// A new document with no file behind it, e.g. an imported buffer.
    pub fn import(file_name: impl Into<String>) -> Self {
        Self::new(MutationKind::Import)
            .file_name(file_name)
            .operation("import")
    }

    /// An edit of the existing geometry.
    pub fn edit(operation: impl Into<String>) -> Self {
        Self::new(MutationKind::Edit).operation(operation)
    }

    /// A generation job produced the revision.
    pub fn job(operation: impl Into<String>, recipe: Option<String>) -> Self {
        let mutation = Self::new(MutationKind::Job).operation(operation);
        match recipe {
            Some(recipe) => mutation.recipe(recipe),
            None => mutation,
        }
    }

    /// Component metadata changed, with no geometry change.
    pub fn component_change(component_id: impl Into<String>, operation: impl Into<String>) -> Self {
        Self::new(MutationKind::Component)
            .component(component_id)
            .operation(operation)
    }

    /// The source was read again.
    pub fn reload(operation: impl Into<String>) -> Self {
        Self::new(MutationKind::Reload).operation(operation)
    }

    pub fn operation(mut self, operation: impl Into<String>) -> Self {
        self.operation = Some(operation.into());
        self
    }

    pub fn component(mut self, component_id: impl Into<String>) -> Self {
        self.component_id = Some(component_id.into());
        self
    }

    pub fn source(mut self, source_path: impl Into<String>) -> Self {
        self.source_path = Some(source_path.into());
        self
    }

    pub fn file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    pub fn recipe(mut self, recipe: impl Into<String>) -> Self {
        self.recipe = Some(recipe.into());
        self
    }

    /// Records an explicit timestamp, for tests and for replaying a request.
    pub fn at_ms(mut self, at_ms: u64) -> Self {
        self.at_ms = Some(at_ms);
        self
    }
}

/// The last path segment of `path`, or the path itself when it has none.
fn file_name_of(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// Where a document's content came from and what happened to it.
///
/// None of this is identity: it is what a caller shows next to an identity so a person can
/// recognise the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// File the geometry was read from, when it came from one.
    pub source_path: Option<String>,
    /// Name the document is saved under.
    pub file_name: String,
    /// Named component of the last mutation, when it named one.
    pub component_id: Option<String>,
    /// Opaque producer record (a recipe), verbatim.
    pub recipe: Option<String>,
    /// Operation or job that produced the newest revision.
    pub last_operation: Option<String>,
    pub created_at_ms: u64,
    /// Last content or component-metadata change; exporting does not move it.
    pub updated_at_ms: u64,
    /// Recent exports, newest first, bounded.
    pub exports: Vec<ExportRecord>,
}

impl Provenance {
    /// Provenance of a brand new document.
    pub fn new(mutation: &Mutation, at_ms: u64) -> Self {
        Self {
            source_path: mutation.source_path.clone(),
            file_name: mutation
                .file_name
                .clone()
                .unwrap_or_else(|| "splat.ply".to_owned()),
            component_id: mutation.component_id.clone(),
            recipe: mutation.recipe.clone(),
            last_operation: Some(
                mutation
                    .operation
                    .clone()
                    .unwrap_or_else(|| mutation.kind.name().to_owned()),
            ),
            created_at_ms: at_ms,
            updated_at_ms: at_ms,
            exports: Vec::new(),
        }
    }

    /// Applies a mutation to this provenance, keeping what the mutation does not mention.
    pub fn apply(&mut self, mutation: &Mutation, at_ms: u64) {
        if let Some(source) = &mutation.source_path {
            self.source_path = Some(source.clone());
        }
        if let Some(name) = &mutation.file_name {
            self.file_name = name.clone();
        }
        if let Some(component) = &mutation.component_id {
            self.component_id = Some(component.clone());
        }
        if let Some(recipe) = &mutation.recipe {
            self.recipe = Some(recipe.clone());
        }
        self.last_operation = Some(
            mutation
                .operation
                .clone()
                .unwrap_or_else(|| mutation.kind.name().to_owned()),
        );
        self.updated_at_ms = at_ms;
    }

    /// True when a producer record is attached.
    pub fn has_recipe(&self) -> bool {
        self.recipe.is_some()
    }

    /// True when the source can be read again.
    pub fn has_source(&self) -> bool {
        self.source_path.is_some()
    }
}

/// Largest number of export records kept per document.
pub const MAX_EXPORTS: usize = 4;

/// Largest number of revision records kept per document.
pub const MAX_HISTORY: usize = 8;

/// Everything that can go wrong when addressing a document, with a stable code per case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentError {
    /// The request needs a displayed document and none is loaded.
    NoDocument,
    /// The named document was never minted by this session, or is not the displayed one.
    ///
    /// A document id from an earlier run of the app lands here: handles do not survive a
    /// restart unless a project format deliberately restores them.
    UnknownDocument {
        document_id: DocumentId,
        active: Option<DocumentId>,
    },
    /// The document matched but had already moved on: nothing was changed.
    Conflict {
        expected: DocumentHandle,
        current: DocumentHandle,
    },
    /// The revision existed but is no longer retained, so it cannot be read.
    SnapshotExpired { handle: DocumentHandle },
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDocument => write!(formatter, "no splat is loaded"),
            Self::UnknownDocument {
                document_id,
                active: Some(active),
            } => write!(
                formatter,
                "document {document_id} is not available (the displayed document is {active})"
            ),
            Self::UnknownDocument {
                document_id,
                active: None,
            } => write!(
                formatter,
                "document {document_id} is not available (no splat is loaded)"
            ),
            Self::Conflict { expected, current } => write!(
                formatter,
                "revision conflict: {expected} was expected but {current} is current"
            ),
            Self::SnapshotExpired { handle } => write!(
                formatter,
                "snapshot {handle} has expired; only the most recent revisions are retained"
            ),
        }
    }
}

impl std::error::Error for DocumentError {}

impl DocumentError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDocument => "no_document",
            Self::UnknownDocument { .. } => "unknown_document",
            Self::Conflict { .. } => "document_conflict",
            Self::SnapshotExpired { .. } => "snapshot_expired",
        }
    }

    /// True when the failure is a *concurrency* outcome a caller should reconcile, rather
    /// than a caller mistake.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// Current identity and revision, when the store knows them.
    pub fn current(&self) -> Option<&DocumentHandle> {
        match self {
            Self::Conflict { current, .. } => Some(current),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_id_round_trips_and_rejects_foreign_text() {
        let id = DocumentId::mint(0x4f2a, 7);
        assert_eq!(id.as_str(), "doc-4f2a-7");
        assert_eq!(DocumentId::parse(id.as_str()), Some(id.clone()));

        // A path or a bare name is not an identity.
        assert_eq!(DocumentId::parse("C:/tmp/scene.ply"), None);
        assert_eq!(DocumentId::parse("scene"), None);
        assert_eq!(DocumentId::parse("doc-7"), None);
        assert_eq!(DocumentId::parse("doc-4f2a-"), None);
        assert_eq!(DocumentId::parse("doc-zz-1"), None);
    }

    #[test]
    fn handles_describe_themselves_for_replies() {
        let handle = DocumentHandle::new(DocumentId::mint(1, 2), 5);
        assert_eq!(handle.to_string(), "doc-1-2@5");
        assert_eq!(Expected::Handle(handle.clone()).handle(), Some(&handle));
        assert_eq!(
            Expected::Revision(5).describe(),
            "the displayed document at revision 5"
        );
        assert_eq!(Expected::Any.describe(), "the displayed document");
        assert_eq!(Expected::Revision(3).handle(), None);
    }

    #[test]
    fn every_mutation_kind_advances_the_revision() {
        for kind in [
            MutationKind::Open,
            MutationKind::Import,
            MutationKind::Edit,
            MutationKind::Component,
            MutationKind::Job,
            MutationKind::Reload,
        ] {
            assert!(kind.advances_revision(), "{kind}");
            assert!(!kind.name().is_empty());
        }
    }

    #[test]
    fn provenance_keeps_what_a_mutation_does_not_mention() {
        let mut provenance = Provenance::new(&Mutation::open("C:/scenes/house.ply"), 100);
        assert_eq!(provenance.file_name, "house.ply");
        assert_eq!(
            provenance.source_path.as_deref(),
            Some("C:/scenes/house.ply")
        );
        assert_eq!(provenance.created_at_ms, 100);

        // An edit that names nothing keeps the source and the name.
        provenance.apply(&Mutation::edit("edit_splat"), 200);
        assert_eq!(
            provenance.source_path.as_deref(),
            Some("C:/scenes/house.ply")
        );
        assert_eq!(provenance.file_name, "house.ply");
        assert_eq!(provenance.last_operation.as_deref(), Some("edit_splat"));
        assert_eq!(provenance.updated_at_ms, 200);
        assert_eq!(provenance.created_at_ms, 100);

        // A job that names a component sets it and records its recipe.
        provenance.apply(
            &Mutation::job("job 4", Some("{}".to_owned())).component("roof"),
            300,
        );
        assert_eq!(provenance.component_id.as_deref(), Some("roof"));
        assert!(provenance.has_recipe());
        assert_eq!(provenance.last_operation.as_deref(), Some("job 4"));

        // A mutation that names no operation still records its kind.
        provenance.apply(&Mutation::new(MutationKind::Reload), 400);
        assert_eq!(provenance.last_operation.as_deref(), Some("reload"));
    }

    #[test]
    fn a_file_name_is_taken_off_the_path() {
        let mutation = Mutation::open(r"C:\scenes\house.ply");
        assert_eq!(mutation.file_name.as_deref(), Some("house.ply"));
        assert_eq!(file_name_of("flat.ply"), "flat.ply");
    }

    #[test]
    fn error_codes_are_stable_and_a_conflict_carries_the_current_handle() {
        let handle = DocumentHandle::new(DocumentId::mint(1, 1), 3);
        assert_eq!(DocumentError::NoDocument.code(), "no_document");
        let unknown = DocumentError::UnknownDocument {
            document_id: DocumentId::mint(1, 9),
            active: Some(DocumentId::mint(1, 1)),
        };
        assert_eq!(unknown.code(), "unknown_document");
        assert!(unknown.to_string().contains("doc-1-1"));

        let conflict = DocumentError::Conflict {
            expected: DocumentHandle::new(handle.document_id.clone(), 2),
            current: handle.clone(),
        };
        assert_eq!(conflict.code(), "document_conflict");
        assert!(conflict.is_conflict());
        assert_eq!(conflict.current(), Some(&handle));
        assert!(conflict.to_string().contains("doc-1-1@3"));
        assert_eq!(
            DocumentError::SnapshotExpired { handle }.code(),
            "snapshot_expired"
        );
    }
}
