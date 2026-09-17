//! The splat the app displays.
//!
//! One document of record: the file the user opened, or the bytes an MCP tool or a Python
//! job committed. Saving re-serialises through `splatmcp-core` instead of echoing the
//! bytes that happened to be loaded.
//!
//! # Identity and revisions
//!
//! Every document carries a stable `document_id` and a `revision` that advances on every
//! accepted change: a manual open, a bridge load, or a Python commit. A Python job states
//! the revision it believes it is editing, and the commit only happens when that revision
//! is still current, so a late job can never overwrite newer work.
//!
//! This is the minimal seam the generation service needs. Task #12 ("stable document
//! identities, revisions and immutable snapshots") owns the full model - content hashes,
//! immutable snapshots and a document registry - and will replace this counter without
//! changing the service's contract.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use splatmcp_core::{Splat, read_ply, write_ply};
use splatmcp_python::arrays::BoundsOut;
use splatmcp_python::script::RecipeRecord;
use splatmcp_python::{CommitOutcome, CommitRequest, DocumentIdentity};

/// A splat plus the name it should be saved under and where it is in time.
pub struct Document {
    pub path: PathBuf,
    pub splat: Splat,
    /// Stable identity of this document across its revisions.
    pub document_id: String,
    /// Content revision, starting at 1 and advancing on every accepted change.
    pub revision: u64,
    /// Recipe that produced this revision, when a Python job produced it.
    pub recipe: Option<RecipeRecord>,
    /// Component the producing job replaced, when it named one.
    pub component_id: Option<String>,
}

impl Document {
    /// Parses PLY bytes and validates them, so an unusable document never enters state.
    ///
    /// Identity and revision are assigned by [`AppState::replace`], which is the only place
    /// that knows what came before.
    pub fn from_ply_bytes(bytes: &[u8], path: PathBuf) -> Result<Self, String> {
        let splat = read_ply(bytes).map_err(|error| error.to_string())?;
        splat.validate().map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            splat,
            document_id: String::new(),
            revision: 0,
            recipe: None,
            component_id: None,
        })
    }

    /// File name used by the save dialog and reported to the viewer.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned()
    }

    /// Canonical PLY bytes of the displayed splat.
    pub fn ply_bytes(&self) -> Result<Vec<u8>, String> {
        write_ply(&self.splat).map_err(|error| error.to_string())
    }

    pub fn info(&self) -> SplatInfo {
        SplatInfo {
            path: self.path.to_string_lossy().to_string(),
            file_name: self.file_name(),
            point_count: self.splat.len(),
            document_id: self.document_id.clone(),
            revision: self.revision,
        }
    }
}

/// Summary of a loaded splat, returned to the frontend after an open or a commit.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct SplatInfo {
    pub path: String,
    pub file_name: String,
    pub point_count: usize,
    pub document_id: String,
    pub revision: u64,
}

/// Identity of the displayed document, for tool replies and the panel.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct DocumentIdentityInfo {
    pub document_id: String,
    pub revision: u64,
    pub point_count: usize,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
}

/// Application state shared by the Tauri commands, the bridge handler and the Python host.
#[derive(Default)]
pub struct AppState {
    document: Mutex<Option<Document>>,
}

impl AppState {
    /// Replaces the displayed document, advancing its revision.
    ///
    /// Loading the same file again keeps the document identity and moves to the next
    /// revision; loading a different file starts a new document at revision 1. Either way a
    /// job that expected the old revision can no longer commit.
    pub fn replace(&self, mut document: Document) -> Result<SplatInfo, String> {
        let mut guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        let (document_id, revision) = match guard.as_ref() {
            Some(previous) if previous.path == document.path => {
                (previous.document_id.clone(), previous.revision + 1)
            }
            _ => (next_document_id(), 1),
        };
        document.document_id = document_id;
        document.revision = revision;
        let info = document.info();
        *guard = Some(document);
        Ok(info)
    }

    /// Runs `read` against the document, or reports that nothing is loaded.
    pub fn with_document<T>(&self, read: impl FnOnce(&Document) -> T) -> Result<T, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        let document = guard.as_ref().ok_or("no splat is loaded")?;
        Ok(read(document))
    }

    /// Canonical PLY bytes of the displayed splat, or `None` when nothing is loaded.
    pub fn ply_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        match guard.as_ref() {
            Some(document) => document.ply_bytes().map(Some),
            None => Ok(None),
        }
    }

    /// Canonical PLY bytes of one exact revision.
    ///
    /// The viewer asks for the revision it was told about; a mismatch is reported instead of
    /// silently displaying newer geometry.
    pub fn ply_bytes_for_revision(&self, revision: u64) -> Result<Vec<u8>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        let document = guard.as_ref().ok_or("no splat is loaded")?;
        if document.revision != revision {
            return Err(format!(
                "the document is at revision {} but the viewer asked for revision {revision}; \
                 reload or re-issue the job",
                document.revision
            ));
        }
        document.ply_bytes()
    }

    /// Name of the displayed file, used when the viewer asks about it.
    pub fn file_name(&self) -> Result<Option<String>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard.as_ref().map(Document::file_name))
    }

    /// Point count of the displayed splat, for the `document_get_ply` reply.
    pub fn point_count(&self) -> Result<usize, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard
            .as_ref()
            .map(|document| document.splat.len())
            .unwrap_or(0))
    }

    /// Recipe that produced the displayed revision, when a Python job produced it.
    pub fn recipe(&self) -> Result<Option<RecipeRecord>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard.as_ref().and_then(|document| document.recipe.clone()))
    }

    /// Identity of the displayed document, or `None` when nothing is loaded.
    pub fn identity(&self) -> Result<Option<DocumentIdentityInfo>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard.as_ref().map(|document| DocumentIdentityInfo {
            document_id: document.document_id.clone(),
            revision: document.revision,
            point_count: document.splat.len(),
            file_name: document.file_name(),
            component_id: document.component_id.clone(),
        }))
    }

    /// Commits a validated candidate as the next revision.
    ///
    /// The revision comparison and the swap happen under one lock, so two concurrent jobs
    /// for the same revision produce one commit and one explicit conflict rather than a
    /// silent last-writer-wins.
    pub fn commit_candidate(&self, request: CommitRequest) -> Result<CommitOutcome, String> {
        let mut guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        let current = guard.as_ref().map(|document| document.revision).unwrap_or(0);

        if let Some(expected) = request.target.expected_revision {
            if expected != current {
                return Ok(CommitOutcome::Conflict {
                    document_id: request
                        .target
                        .document_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                    expected,
                    actual: current,
                });
            }
        } else if let Some(declared) = request.target.document_id.as_ref() {
            // A caller naming a document must name its revision too; anything else would
            // silently overwrite whatever it happens to be looking at.
            let matches = guard
                .as_ref()
                .is_some_and(|document| &document.document_id == declared);
            if !matches {
                return Err(format!(
                    "document {declared} is not the loaded document; call document_info for its \
                     identity and revision"
                ));
            }
        }

        let point_count = request.splat.len();
        let bounds = request.splat.bounds().map(BoundsOut::from);
        let revision = current + 1;
        let (document_id, path) = match guard.as_ref() {
            Some(previous) => (previous.document_id.clone(), previous.path.clone()),
            None => {
                let file_name = request
                    .target
                    .file_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "generated.ply".to_owned());
                (next_document_id(), crate::paths::documents_dir().join(file_name))
            }
        };

        *guard = Some(Document {
            path,
            splat: request.splat,
            document_id: document_id.clone(),
            revision,
            recipe: Some(request.provenance),
            component_id: request.target.component_id.clone(),
        });

        Ok(CommitOutcome::Committed {
            identity: DocumentIdentity {
                document_id,
                revision,
                point_count,
                bounds,
                component_id: request.target.component_id,
            },
        })
    }
}

/// Next document identity, unique within this process.
///
/// A process-local counter is enough for the commit contract: identity only has to tell
/// this app's documents apart while it runs. Task #12 replaces it with a durable identity.
fn next_document_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("doc-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{SplatPoint, write_ply};
    use splatmcp_python::arrays::BatchMetadata;
    use splatmcp_python::executor::SourceSnapshot;
    use splatmcp_python::runtime::RuntimeFingerprint;
    use splatmcp_python::service::TargetSpec;

    fn ply_of(points: usize) -> Vec<u8> {
        let splat = Splat::from_points(
            (0..points)
                .map(|index| {
                    SplatPoint::new(
                        [index as f32, 0.0, 0.0],
                        [0.1, 0.1, 0.1],
                        [0.5, 0.5, 0.5],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        );
        write_ply(&splat).unwrap()
    }

    fn splat_of(points: usize) -> Splat {
        read_ply(&ply_of(points)).unwrap()
    }

    fn commit_request(target: TargetSpec, points: usize) -> CommitRequest {
        CommitRequest {
            target,
            splat: splat_of(points),
            provenance: RecipeRecord::new(
                &splatmcp_python::ScriptSnapshot::inline(
                    "req",
                    "def generate(ctx): pass",
                    "generate",
                    serde_json::json!({}),
                    0,
                )
                .unwrap(),
                "test",
                RuntimeFingerprint {
                    python_version: "3.13.2".to_owned(),
                    python_home: "C:/runtime".to_owned(),
                    packages: Vec::new(),
                },
            ),
        }
    }

    #[test]
    fn a_document_keeps_its_name_and_round_trips_through_ply() {
        let state = AppState::default();
        let document =
            Document::from_ply_bytes(&ply_of(3), PathBuf::from("C:/tmp/thing.ply")).unwrap();
        let info = state.replace(document).unwrap();
        assert_eq!(info.file_name, "thing.ply");
        assert_eq!(info.point_count, 3);
        assert_eq!(info.revision, 1);
        assert!(info.document_id.starts_with("doc-"));
        let bytes = state.ply_bytes().unwrap().unwrap();
        let reparsed = read_ply(&bytes).unwrap();
        assert_eq!(reparsed.len(), 3);
    }

    #[test]
    fn garbage_never_becomes_the_displayed_document() {
        let state = AppState::default();
        assert!(Document::from_ply_bytes(b"not a ply", PathBuf::from("x.ply")).is_err());
        assert!(state.ply_bytes().unwrap().is_none());
        assert!(state.with_document(|_| ()).is_err());
    }

    #[test]
    fn reloading_the_same_file_keeps_its_identity_and_advances_the_revision() {
        let state = AppState::default();
        let path = PathBuf::from("C:/tmp/same.ply");
        let first = state
            .replace(Document::from_ply_bytes(&ply_of(2), path.clone()).unwrap())
            .unwrap();
        let second = state
            .replace(Document::from_ply_bytes(&ply_of(2), path.clone()).unwrap())
            .unwrap();
        assert_eq!(first.document_id, second.document_id);
        assert_eq!(second.revision, 2);

        // A different file is a different document.
        let third = state
            .replace(Document::from_ply_bytes(&ply_of(2), PathBuf::from("C:/tmp/other.ply")).unwrap())
            .unwrap();
        assert_ne!(third.document_id, second.document_id);
        assert_eq!(third.revision, 1);
    }

    #[test]
    fn a_commit_becomes_the_next_revision_and_reports_its_identity() {
        let state = AppState::default();
        state
            .replace(
                Document::from_ply_bytes(&ply_of(2), PathBuf::from("C:/tmp/thing.ply")).unwrap(),
            )
            .unwrap();
        let identity = state.identity().unwrap().unwrap();
        let outcome = state
            .commit_candidate(commit_request(
                TargetSpec::component(&identity.document_id, "spire", identity.revision),
                5,
            ))
            .unwrap();
        match outcome {
            CommitOutcome::Committed { identity } => {
                assert_eq!(identity.revision, 2);
                assert_eq!(identity.point_count, 5);
                assert_eq!(identity.component_id.as_deref(), Some("spire"));
                assert!(identity.bounds.is_some());
            }
            other => panic!("expected a commit, got {other:?}"),
        }
        let stored = state.identity().unwrap().unwrap();
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.point_count, 5);
    }

    #[test]
    fn a_stale_revision_conflicts_instead_of_overwriting() {
        let state = AppState::default();
        state
            .replace(Document::from_ply_bytes(&ply_of(2), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();
        let identity = state.identity().unwrap().unwrap();
        // Another change lands first (a manual load advances the revision).
        state
            .replace(Document::from_ply_bytes(&ply_of(4), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();

        let outcome = state
            .commit_candidate(commit_request(
                TargetSpec::component(&identity.document_id, "spire", identity.revision),
                9,
            ))
            .unwrap();
        match outcome {
            CommitOutcome::Conflict { expected, actual, .. } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 2);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(state.point_count().unwrap(), 4, "the newer content stands");
    }

    #[test]
    fn a_named_document_that_is_not_loaded_is_refused() {
        let state = AppState::default();
        state
            .replace(Document::from_ply_bytes(&ply_of(2), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();
        let error = state
            .commit_candidate(commit_request(TargetSpec::new_document(None), 3))
            .and_then(|outcome| match outcome {
                CommitOutcome::Committed { .. } => Ok(()),
                other => Err(format!("unexpected {other:?}")),
            });
        // A new-document commit is allowed; a named document that is not loaded is not.
        assert!(error.is_ok());

        let mut request = commit_request(TargetSpec::new_document(None), 3);
        request.target.document_id = Some("doc-does-not-exist".to_owned());
        assert!(state.commit_candidate(request).is_err());
    }

    #[test]
    fn bytes_are_only_returned_for_the_current_revision() {
        let state = AppState::default();
        let info = state
            .replace(Document::from_ply_bytes(&ply_of(3), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();
        assert_eq!(state.ply_bytes_for_revision(info.revision).unwrap().len(), state.ply_bytes().unwrap().unwrap().len());
        let error = state.ply_bytes_for_revision(info.revision + 1).unwrap_err();
        assert!(error.contains("asked for revision"), "{error}");
    }

    #[test]
    fn a_job_can_snapshot_the_displayed_document_without_touching_it() {
        // The snapshot the generation service takes is a detached copy; this exercises the
        // same construction the app performs.
        let state = AppState::default();
        let info = state
            .replace(Document::from_ply_bytes(&ply_of(3), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();
        let snapshot = state
            .with_document(|document| SourceSnapshot {
                document_id: document.document_id.clone(),
                revision: document.revision,
                component_id: Some("spire".to_owned()),
                batch: splatmcp_python::arrays::GaussianBatch::from_splat(
                    &document.splat,
                    BatchMetadata::default(),
                ),
            })
            .unwrap();
        assert_eq!(snapshot.document_id, info.document_id);
        assert_eq!(snapshot.revision, info.revision);
        assert_eq!(snapshot.batch.len(), 3);
        // Mutating the copy leaves the document alone.
        let mut copy = snapshot.batch;
        copy.positions[0] = [9.0, 9.0, 9.0];
        assert_eq!(state.point_count().unwrap(), 3);
    }

    #[test]
    fn the_identity_report_matches_the_document() {
        let state = AppState::default();
        assert!(state.identity().unwrap().is_none());
        let info = state
            .replace(Document::from_ply_bytes(&ply_of(2), PathBuf::from("C:/tmp/thing.ply")).unwrap())
            .unwrap();
        let identity = state.identity().unwrap().unwrap();
        assert_eq!(identity.document_id, info.document_id);
        assert_eq!(identity.revision, info.revision);
        assert_eq!(identity.point_count, 2);
        assert_eq!(identity.file_name, "thing.ply");
    }
}
