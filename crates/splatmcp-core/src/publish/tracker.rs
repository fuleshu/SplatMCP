//! The per-document publication state machine.
//!
//! [`PublicationTracker`] holds one [`DocumentPublication`] per document, plus the knowledge
//! that **only one document can be on screen at a time**. Each entry records the committed
//! revision, the displayed revision, the request in flight and what happened to earlier
//! requests, and answers the three questions the app needs:
//!
//! 1. *what should the viewer fetch?* [`DocumentPublication::begin`] mints a request and
//!    supersedes anything older for the same document;
//! 2. *did the viewer draw it?* [`DocumentPublication::acknowledge`] accepts only the request
//!    actually in flight, so a late completion cannot claim the screen;
//! 3. *is the display behind the document?* [`DocumentPublication::status`] reports the two
//!    revisions separately, and never derives one from the other.
//!
//! Three behaviours are worth stating because they are easy to get wrong:
//!
//! - **A commit is recorded even when nothing is published.** A `display:false` edit leaves
//!   `displayed_revision` where it was and moves `committed_revision`, so the status says
//!   "committed, not shown" instead of claiming a hidden revision is current.
//! - **A pending request expires.** A viewer that never answers is a real outcome, and a
//!   request that outlives [`ACK_TIMEOUT_MS`] is recorded as [`PublicationOutcome::TimedOut`]
//!   on the next read - so a stall is reported rather than reported as "pending" forever.
//! - **Switching documents moves the screen.** When a publication for another document is
//!   displayed, that document becomes the active one and the previous document stops claiming
//!   a displayed revision: it is not on screen any more.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{
    PUBLICATION_CONTRACT_VERSION, PublicationError, PublicationNotice, PublicationNoticeOutcome,
    PublicationOutcome, PublicationRequest, PublicationSource, PublicationStatus,
};

/// How many superseded or failed requests one document remembers.
///
/// Bounded on purpose: a rapid sequence of edits must not grow this list without limit, and a
/// caller only ever needs to know about the recent ones.
pub(crate) const MAX_HISTORY: usize = 32;
/// How many failed requests are kept, with their reasons.
pub(crate) const MAX_FAILURES: usize = 8;
/// How many publication notices one document queues before the oldest are dropped.
///
/// A notice is consumed by the app as soon as it is read, so this bound only matters when nothing
/// reads them for a while.
pub(crate) const MAX_NOTICES: usize = 64;

/// How long a publication may wait for an acknowledgement before it is reported as timed out.
///
/// The app's contract, not a guess: `renderer_capabilities` publishes the same number, so a
/// caller can see how long "pending" may honestly last.
pub const ACK_TIMEOUT_MS: u64 = 5_000;

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
    /// When the request in flight was announced, for the acknowledgement timeout.
    pending_since_ms: Option<u64>,
    /// The most recent request and what became of it.
    last: Option<(PublicationRequest, PublicationOutcome)>,
    /// Revisions superseded before they were displayed, newest first.
    skipped: Vec<u64>,
    /// Revisions the viewer could not display, newest first, with the reason.
    failures: Vec<(u64, String)>,
    /// Outcomes this document has produced that the rest of the app has not read yet.
    notices: Vec<PublicationNotice>,
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
            pending_since_ms: None,
            last: None,
            skipped: Vec::new(),
            failures: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub fn document_id(&self) -> &str {
        &self.document_id
    }

    /// Records that the store now holds `revision`.
    ///
    /// Deliberately does **not** touch `displayed_revision`: committing is not displaying, and
    /// the gap between the two is the whole point of this state machine. It is called for every
    /// commit, including one nobody asked to display.
    pub fn committed(&mut self, revision: u64) {
        // The pointer only moves forward: a late or out-of-order report of an older revision
        // must never make the document look older than it is.
        self.committed_revision = Some(match self.committed_revision {
            Some(current) => current.max(revision),
            None => revision,
        });
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
        now_ms: u64,
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
        // Publishing a committed revision asserts that it *is* committed: doing so here keeps
        // the two facts consistent even if a caller forgot to record the commit, and the
        // forward-only rule means publishing an older revision cannot move the pointer back.
        if source == PublicationSource::Committed {
            self.committed(revision);
        }
        self.next_token += 1;
        let request = PublicationRequest {
            document_id: self.document_id.clone(),
            revision,
            token: self.next_token,
            source,
            frame,
            started_at_ms: now_ms,
        };
        if let Some(previous) = self.pending.take() {
            self.last = Some((
                previous.clone(),
                PublicationOutcome::Skipped {
                    superseded_by: Some(revision),
                },
            ));
            self.remember_skipped(previous.revision);
            self.notice(previous.revision, PublicationNoticeOutcome::Superseded);
        }
        self.pending = Some(request.clone());
        self.pending_since_ms = Some(now_ms);
        self.last = Some((request.clone(), PublicationOutcome::Pending));
        Ok(request)
    }

    /// Accepts the viewer's acknowledgement that it displayed one exact request.
    ///
    /// Only the request in flight may be acknowledged. Anything else - an older load finishing
    /// late, a mismatched token, a request for another document - is recorded in the failure
    /// history and refused with [`PublicationError::StaleAcknowledgement`], so it can never
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
        self.pending_since_ms = None;
        // A preview candidate is not a revision: the caller sees the frame, but the app's
        // displayed *revision* stays where it was - and no notice is raised for it, because a
        // preview of revision N does not mean revision N's geometry is on screen.
        if request.source == PublicationSource::Committed {
            self.displayed_revision = Some(request.revision);
            self.notice(request.revision, PublicationNoticeOutcome::Displayed);
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
        self.pending_since_ms = None;
        match pending {
            Some(request) if request.revision == revision => {
                self.failures.insert(0, (revision, reason.clone()));
                self.failures.truncate(MAX_FAILURES);
                self.last = Some((request, PublicationOutcome::Failed(reason.clone())));
                self.notice(revision, PublicationNoticeOutcome::Failed(reason));
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
    pub fn time_out(&mut self) -> Option<PublicationRequest> {
        let pending = self.pending.take()?;
        self.pending_since_ms = None;
        self.last = Some((pending.clone(), PublicationOutcome::TimedOut));
        self.notice(pending.revision, PublicationNoticeOutcome::TimedOut);
        Some(pending)
    }

    /// Times the request in flight out when it has waited longer than `timeout_ms`.
    ///
    /// Called on every read and every new publication, so a viewer that never answers cannot
    /// leave a request reading "pending" indefinitely. Returns the request that expired.
    pub fn expire_pending(&mut self, now_ms: u64, timeout_ms: u64) -> Option<PublicationRequest> {
        let since = self.pending_since_ms?;
        if now_ms.saturating_sub(since) <= timeout_ms {
            return None;
        }
        self.time_out()
    }

    /// Drops this document's claim to the screen because another document took it.
    ///
    /// The viewer shows exactly one document. When a publication for another document is
    /// displayed, this entry stops reporting a displayed revision - it is not on screen - while
    /// keeping its committed revision, so the status reads "committed here, showing elsewhere".
    ///
    /// Any request still in flight for this document is dropped as superseded: it can never be
    /// displayed now, and leaving it pending would report a stall that is really a switch.
    pub fn relinquish_screen(&mut self) -> bool {
        let mut changed = self.displayed_revision.take().is_some();
        if let Some(pending) = self.pending.take() {
            self.pending_since_ms = None;
            self.last = Some((pending.clone(), PublicationOutcome::Skipped { superseded_by: None }));
            self.remember_skipped(pending.revision);
            self.notice(pending.revision, PublicationNoticeOutcome::Superseded);
            changed = true;
        }
        changed
    }

    /// Takes the outcomes this document has produced and not yet handed over.
    pub fn take_notices(&mut self) -> Vec<PublicationNotice> {
        std::mem::take(&mut self.notices)
    }

    /// Queues one outcome for the rest of the app, newest last, bounded.
    fn notice(&mut self, revision: u64, outcome: PublicationNoticeOutcome) {
        if self.notices.len() >= MAX_NOTICES {
            self.notices.remove(0);
        }
        self.notices.push(PublicationNotice {
            document_id: self.document_id.clone(),
            revision,
            outcome,
        });
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
            displayed_elsewhere: false,
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
        self.pending_since_ms = None;
        self.last = None;
        self.skipped.clear();
        self.failures.clear();
        self.notices.clear();
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

/// One publication tracker per process: the app's single answer to "what is on screen?".
pub struct PublicationTracker {
    documents: Mutex<TrackerState>,
}

/// Tracker contents: the entries, which of them owns the screen, and the outcomes nobody has read.
#[derive(Default)]
struct TrackerState {
    documents: BTreeMap<String, DocumentPublication>,
    /// The document a publication was last displayed for, if any.
    active: Option<String>,
    /// Outcomes produced since the last read, oldest first, bounded like the per-document queues.
    notices: Vec<PublicationNotice>,
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
            documents: Mutex::new(TrackerState::default()),
        }
    }

    fn locked(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, TrackerState>, PublicationError> {
        self.documents
            .lock()
            .map_err(|error| PublicationError::Unavailable {
                reason: error.to_string(),
            })
    }

    /// Runs `work` against one document's state, creating it on first use.
    fn with<T>(
        &self,
        document_id: &str,
        work: impl FnOnce(&mut DocumentPublication) -> T,
    ) -> Result<T, PublicationError> {
        let mut state = self.locked()?;
        let entry = state
            .documents
            .entry(document_id.to_owned())
            .or_insert_with(|| DocumentPublication::new(document_id));
        let value = work(entry);
        Self::drain(&mut state, document_id);
        Ok(value)
    }

    /// Moves one document's unread outcomes into the tracker's queue.
    fn drain(state: &mut TrackerState, document_id: &str) {
        let Some(entry) = state.documents.get_mut(document_id) else {
            return;
        };
        for notice in entry.take_notices() {
            if state.notices.len() >= MAX_NOTICES {
                state.notices.remove(0);
            }
            state.notices.push(notice);
        }
    }

    /// Times out every request that has waited longer than the acknowledgement timeout.
    ///
    /// Runs on every read, so "pending" is never a permanent state: a viewer that stopped
    /// answering shows up as `timed_out` and a display failure rather than an eternal promise.
    fn sweep_expired(state: &mut TrackerState, now_ms: u64, timeout_ms: u64) {
        let mut expired: Vec<String> = Vec::new();
        for (document_id, entry) in state.documents.iter_mut() {
            if entry.expire_pending(now_ms, timeout_ms).is_some() {
                expired.push(document_id.clone());
            }
        }
        for document_id in expired {
            Self::drain(state, &document_id);
        }
    }

    /// Records that the store now holds `revision` of `document_id`.
    ///
    /// Called for every commit, displayed or not: a hidden commit must still move the
    /// committed revision, or the status would keep claiming the old revision is current.
    pub fn committed(&self, document_id: &str, revision: u64) -> Result<(), PublicationError> {
        self.with(document_id, |entry| entry.committed(revision))
    }

    /// Starts a publication, superseding anything already in flight for that document.
    pub fn begin_at(
        &self,
        now_ms: u64,
        timeout_ms: u64,
        document_id: &str,
        revision: u64,
        source: PublicationSource,
        frame: bool,
    ) -> Result<PublicationRequest, PublicationError> {
        let mut state = self.locked()?;
        Self::sweep_expired(&mut state, now_ms, timeout_ms);
        let entry = state
            .documents
            .entry(document_id.to_owned())
            .or_insert_with(|| DocumentPublication::new(document_id));
        let request = entry.begin(now_ms, revision, source, frame);
        Self::drain(&mut state, document_id);
        request
    }

    /// Accepts an acknowledgement and reports the status it produced.
    ///
    /// A displayed publication makes its document the active one: any other document stops
    /// claiming a displayed revision, because the viewer is showing this one now.
    pub fn acknowledge_at(
        &self,
        now_ms: u64,
        timeout_ms: u64,
        document_id: &str,
        revision: u64,
        token: u64,
    ) -> Result<PublicationStatus, PublicationError> {
        let mut state = self.locked()?;
        Self::sweep_expired(&mut state, now_ms, timeout_ms);
        let entry = state
            .documents
            .get_mut(document_id)
            .ok_or_else(|| PublicationError::UnknownDocument {
                document_id: document_id.to_owned(),
            })?;
        let status = entry.acknowledge(document_id, revision, token)?;
        // The screen moved: every other document gives up its displayed revision, and any request
        // of theirs still in flight - which can never be displayed now - is dropped as superseded.
        if status.displayed_revision.is_some() {
            let others: Vec<String> = state
                .documents
                .keys()
                .filter(|other_id| other_id.as_str() != document_id)
                .cloned()
                .collect();
            for other_id in &others {
                if let Some(other) = state.documents.get_mut(other_id) {
                    other.relinquish_screen();
                }
            }
            state.active = Some(document_id.to_owned());
            for other_id in others {
                Self::drain(&mut state, &other_id);
            }
        }
        Self::drain(&mut state, document_id);
        Ok(status)
    }

    /// Records a failed publication.
    pub fn fail_at(
        &self,
        now_ms: u64,
        timeout_ms: u64,
        document_id: &str,
        revision: u64,
        reason: impl Into<String>,
    ) -> Result<PublicationStatus, PublicationError> {
        let mut state = self.locked()?;
        Self::sweep_expired(&mut state, now_ms, timeout_ms);
        let entry = state
            .documents
            .get_mut(document_id)
            .ok_or_else(|| PublicationError::UnknownDocument {
                document_id: document_id.to_owned(),
            })?;
        let status = entry.fail(revision, reason);
        Self::drain(&mut state, document_id);
        status
    }

    /// Marks the request in flight as timed out.
    pub fn time_out(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationRequest>, PublicationError> {
        self.with(document_id, |entry| entry.time_out())
    }

    /// One document's status, or `None` when nothing is known about it.
    ///
    /// The status also says whether this document is the one on screen
    /// ([`PublicationStatus::displayed_elsewhere`]) so a caller can tell "not displayed yet"
    /// from "another document is displayed".
    pub fn status_at(
        &self,
        now_ms: u64,
        timeout_ms: u64,
        document_id: &str,
    ) -> Result<Option<PublicationStatus>, PublicationError> {
        let mut state = self.locked()?;
        Self::sweep_expired(&mut state, now_ms, timeout_ms);
        Ok(state.documents.get(document_id).map(|entry| {
            let mut status = entry.status();
            status.displayed_elsewhere =
                status.displayed_revision.is_none() && state.active.is_some()
                    && state.active.as_deref() != Some(document_id);
            status
        }))
    }

    /// Takes the outcomes produced since the last read, oldest first.
    ///
    /// The app applies them to the records that announced a display - a job's `display` field, for
    /// instance - so a stored outcome converges instead of freezing at submission time.
    pub fn take_notices(&self) -> Result<Vec<PublicationNotice>, PublicationError> {
        let mut state = self.locked()?;
        Ok(std::mem::take(&mut state.notices))
    }

    /// The document that owns the screen, if any publication has been displayed.
    pub fn active_document(&self) -> Result<Option<String>, PublicationError> {
        Ok(self.locked()?.active.clone())
    }

    /// Forgets a document that was closed.
    pub fn forget(&self, document_id: &str) -> Result<(), PublicationError> {
        let mut state = self.locked()?;
        state.documents.remove(document_id);
        if state.active.as_deref() == Some(document_id) {
            state.active = None;
        }
        Ok(())
    }

    /// Every tracked document's status, for a diagnostics view.
    pub fn all(&self) -> Result<Vec<PublicationStatus>, PublicationError> {
        let state = self.locked()?;
        Ok(state
            .documents
            .values()
            .map(DocumentPublication::status)
            .collect())
    }
}
