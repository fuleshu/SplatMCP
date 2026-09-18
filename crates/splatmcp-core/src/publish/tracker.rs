//! The per-document publication state machine.
//!
//! [`PublicationTracker`] holds one [`DocumentPublication`] per document. Each one records the
//! committed revision, the displayed revision, the request in flight and the history of what
//! happened to earlier requests - and answers the only three questions the app needs:
//!
//! 1. *what should the viewer fetch?* [`DocumentPublication::begin`] mints a request and
//!    supersedes anything older for the same document;
//! 2. *did the viewer draw it?* [`DocumentPublication::acknowledge`] accepts only the request
//!    that is actually in flight, so a late completion cannot claim the screen;
//! 3. *is the display behind the document?* [`DocumentPublication::status`] reports the two
//!    revisions separately, and never derives one from the other.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{
    PUBLICATION_CONTRACT_VERSION, PublicationError, PublicationOutcome, PublicationRequest,
    PublicationSource, PublicationStatus,
};

/// How many superseded or failed requests one document remembers.
///
/// Bounded on purpose: a rapid sequence of edits must not grow this list without limit, and a
/// caller only ever needs to know about the recent ones.
pub(crate) const MAX_HISTORY: usize = 32;
/// How many failed requests are kept, with their reasons.
pub(crate) const MAX_FAILURES: usize = 8;

/// Publication state of one document.
#[derive(Debug, Clone)]
pub struct DocumentPublication {
    document_id: String,
    committed_revision: Option<u64>,
    displayed_revision: Option<u64>,
    /// Token of the newest request for this document.
    next_token: u64,
    /// The request that is expected to be acknowledged, if any.
    pending: Option<PublicationRequest>,
    /// The most recent request and what became of it.
    last: Option<(PublicationRequest, PublicationOutcome)>,
    /// Revisions superseded before they were displayed, newest first.
    skipped: Vec<u64>,
    /// Revisions the viewer could not display, newest first, with the reason.
    failures: Vec<(u64, String)>,
}

impl DocumentPublication {
    /// A tracker entry for a document nothing is known about yet.
    pub fn new(document_id: impl Into<String>) -> Self {
        Self {
            document_id: document_id.into(),
            committed_revision: None,
            displayed_revision: None,
            next_token: 0,
            pending: None,
            last: None,
            skipped: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn document_id(&self) -> &str {
        &self.document_id
    }

    /// Records that the store now holds `revision`.
    ///
    /// Deliberately does **not** touch `displayed_revision`: committing is not displaying, and
    /// the gap between the two is the whole point of this state machine.
    pub fn committed(&mut self, revision: u64) {
        self.committed_revision = Some(revision);
    }

    /// The revision a frame presented, if any.
    pub fn displayed_revision(&self) -> Option<u64> {
        self.displayed_revision
    }

    /// The newest committed revision the app reported.
    pub fn committed_revision(&self) -> Option<u64> {
        self.committed_revision
    }

    /// True when a request is waiting to be acknowledged.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a publication and returns the request the viewer must fetch.
    ///
    /// Any request already in flight for this document is superseded first and recorded as
    /// [`PublicationOutcome::Skipped`] with the revision that replaced it - never as displayed.
    /// Re-publishing the revision that is already on screen is refused, because it would only
    /// reload geometry the viewer is showing.
    pub fn begin(
        &mut self,
        revision: u64,
        source: PublicationSource,
        frame: bool,
    ) -> Result<PublicationRequest, PublicationError> {
        if source == PublicationSource::Preview && revision == 0 {
            return Err(PublicationError::PreviewIsNotARevision { revision });
        }
        if source == PublicationSource::Committed && self.displayed_revision == Some(revision) {
            return Err(PublicationError::AlreadyDisplayed { revision });
        }
        self.next_token += 1;
        let request = PublicationRequest {
            document_id: self.document_id.clone(),
            revision,
            token: self.next_token,
            source,
            frame,
        };
        if let Some(previous) = self.pending.take() {
            self.last = Some((
                previous.clone(),
                PublicationOutcome::Skipped {
                    superseded_by: Some(revision),
                },
            ));
            self.remember_skipped(previous.revision);
        }
        self.pending = Some(request.clone());
        self.last = Some((request.clone(), PublicationOutcome::Pending));
        Ok(request)
    }

    /// Accepts the viewer's acknowledgement that it displayed one exact request.
    ///
    /// Only the request in flight may be acknowledged. Anything else - an older load finishing
    /// late, a mismatched token, a request for another document - is recorded as a failure in
    /// the history and refused with [`PublicationError::StaleAcknowledgement`], so it can never
    /// become the displayed revision.
    pub fn acknowledge(
        &mut self,
        document_id: &str,
        revision: u64,
        token: u64,
    ) -> Result<PublicationStatus, PublicationError> {
        if document_id != self.document_id {
            return Err(PublicationError::UnknownDocument {
                document_id: document_id.to_owned(),
            });
        }
        let matches = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.matches(document_id, revision, token));
        if !matches {
            let stale_revision = revision;
            let stale_token = token;
            let pending = self.pending.clone();
            self.record_stale(stale_revision, stale_token);
            return Err(PublicationError::StaleAcknowledgement {
                document_id: document_id.to_owned(),
                revision: stale_revision,
                token: stale_token,
                pending: pending.map(Box::new),
            });
        }
        let request = self.pending.take().expect("matched above");
        // A preview candidate is not a revision: the caller sees the frame, but the app's
        // displayed *revision* stays where it was.
        if request.source == PublicationSource::Committed {
            self.displayed_revision = Some(request.revision);
        }
        self.last = Some((request, PublicationOutcome::Displayed));
        Ok(self.status())
    }

    /// Records that the viewer could not display the request in flight.
    ///
    /// The previously displayed revision is left untouched, so a failed publication never
    /// blanks out - or overstates - what is on screen.
    pub fn fail(
        &mut self,
        revision: u64,
        reason: impl Into<String>,
    ) -> Result<PublicationStatus, PublicationError> {
        let reason = reason.into();
        let pending = self.pending.take();
        match pending {
            Some(request) if request.revision == revision => {
                self.failures.insert(0, (revision, reason.clone()));
                self.failures.truncate(MAX_FAILURES);
                self.last = Some((request, PublicationOutcome::Failed(reason)));
            }
            Some(request) => {
                // A failure for something that is not in flight: keep the flight intact and
                // record the stale report rather than dropping the real pending request.
                self.pending = Some(request);
                self.record_stale(revision, 0);
            }
            None => self.record_stale(revision, 0),
        }
        Ok(self.status())
    }

    /// Marks the request in flight as timed out, keeping the displayed revision as it was.
    ///
    /// A viewer that never answers is a real outcome: the app reports `timed_out` rather than
    /// waiting forever or pretending the revision appeared.
    pub fn time_out(&mut self) -> Option<PublicationRequest> {
        let pending = self.pending.take()?;
        self.last = Some((pending.clone(), PublicationOutcome::TimedOut));
        Some(pending)
    }

    /// The current status, with the two revisions kept separate.
    pub fn status(&self) -> PublicationStatus {
        let display_lagging = match (self.committed_revision, self.displayed_revision) {
            (Some(committed), Some(displayed)) => committed != displayed,
            (Some(_), None) => true,
            _ => false,
        };
        PublicationStatus {
            contract_version: PUBLICATION_CONTRACT_VERSION,
            document_id: self.document_id.clone(),
            committed_revision: self.committed_revision,
            displayed_revision: self.displayed_revision,
            pending: self.pending.clone(),
            last: self.last.clone(),
            skipped: self.skipped.clone(),
            failures: self.failures.clone(),
            display_lagging,
        }
    }

    /// True when the viewer is showing exactly the newest committed revision.
    pub fn is_current(&self) -> bool {
        self.status().is_current()
    }

    /// Forgets everything about this document, for a document that was closed.
    pub fn clear(&mut self) {
        self.committed_revision = None;
        self.displayed_revision = None;
        self.pending = None;
        self.last = None;
        self.skipped.clear();
        self.failures.clear();
    }

    fn remember_skipped(&mut self, revision: u64) {
        self.skipped.insert(0, revision);
        self.skipped.truncate(MAX_HISTORY);
    }

    /// Records an acknowledgement or failure that named something not in flight.
    fn record_stale(&mut self, revision: u64, token: u64) {
        let described = if token == 0 {
            format!("revision {revision} failed while a different request was in flight")
        } else {
            format!("revision {revision} acknowledged with stale token {token}")
        };
        self.failures.insert(0, (revision, described));
        self.failures.truncate(MAX_FAILURES);
    }
}

/// One publication tracker per process: it is the app's single answer to "what is on screen?".
pub struct PublicationTracker {
    documents: Mutex<BTreeMap<String, DocumentPublication>>,
}

impl Default for PublicationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicationTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self {
            documents: Mutex::new(BTreeMap::new()),
        }
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, DocumentPublication>>, PublicationError>
    {
        self.documents.lock().map_err(|error| PublicationError::Unavailable {
            reason: error.to_string(),
        })
    }

    /// Runs `work` against one document's state, creating it on first use.
    fn with<T>(
        &self,
        document_id: &str,
        work: impl FnOnce(&mut DocumentPublication) -> T,
    ) -> Result<T, PublicationError> {
        let mut documents = self.locked()?;
        let entry = documents
            .entry(document_id.to_owned())
            .or_insert_with(|| DocumentPublication::new(document_id));
        Ok(work(entry))
    }

    /// Records that the store now holds `revision` of `document_id`.
    pub fn committed(&self, document_id: &str, revision: u64) -> Result<(), PublicationError> {
        self.with(document_id, |entry| entry.committed(revision))
    }

    /// Starts a publication, superseding anything already in flight for that document.
    pub fn begin(
        &self,
        document_id: &str,
        revision: u64,
        source: PublicationSource,
        frame: bool,
    ) -> Result<PublicationRequest, PublicationError> {
        self.with(document_id, |entry| entry.begin(revision, source, frame))?
    }

    /// Accepts an acknowledgement and reports the status it produced.
    pub fn acknowledge(
        &self,
        document_id: &str,
        revision: u64,
        token: u64,
    ) -> Result<PublicationStatus, PublicationError> {
        self.with(document_id, |entry| entry.acknowledge(document_id, revision, token))?
    }

    /// Records a failed publication.
    pub fn fail(
        &self,
        document_id: &str,
        revision: u64,
        reason: impl Into<String>,
    ) -> Result<PublicationStatus, PublicationError> {
        self.with(document_id, |entry| entry.fail(revision, reason))?
    }

    /// Marks the request in flight as timed out.
    pub fn time_out(&self, document_id: &str) -> Result<Option<PublicationRequest>, PublicationError> {
        self.with(document_id, |entry| entry.time_out())
    }

    /// One document's status, or `None` when nothing is known about it.
    pub fn status(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationStatus>, PublicationError> {
        let documents = self.locked()?;
        Ok(documents.get(document_id).map(DocumentPublication::status))
    }

    /// Forgets a document that was closed.
    pub fn forget(&self, document_id: &str) -> Result<(), PublicationError> {
        let mut documents = self.locked()?;
        documents.remove(document_id);
        Ok(())
    }

    /// Every tracked document's status, for a diagnostics view.
    pub fn all(&self) -> Result<Vec<PublicationStatus>, PublicationError> {
        let documents = self.locked()?;
        Ok(documents
            .values()
            .map(DocumentPublication::status)
            .collect())
    }
}
